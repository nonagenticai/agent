//! The generator's transactional state machine. Routes choose the authority and scope before calling it.
//! Platform advances create untagged revisions; tenant advances leave the job unchanged until Package.
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;

use super::guard::B8Hooks;
use super::llm::{self, Drafter, StepContext};
use super::run::{RunRow, Scope};
use super::{accept, findings, store, usage};
use crate::audit::{self, Actor, Event, PlatformEvent};
use crate::design::{DesignOf, JobRef};
use crate::error::{ApiError, ApiResult};
use crate::journey::model::{GeneratorStepKey, is_slug};
use crate::platform_db;
use crate::state::AppState;
use crate::tenancy::JobCtx;

fn conflict(code: &'static str, message: &str) -> ApiError {
    ApiError::Conflict(code, message.into())
}

pub fn assert_open(run: &RunRow) -> ApiResult<()> {
    match run.status.as_str() {
        "running" => Ok(()),
        "packaged" => Err(conflict(
            "run_packaged",
            "This run has already been packaged.",
        )),
        _ => Err(conflict("run_abandoned", "This run was abandoned.")),
    }
}

pub async fn platform_run(c: &mut PgConnection, id: i64) -> ApiResult<RunRow> {
    let run = store::lock_run(c, id).await?;
    if run.scope != "platform" {
        return Err(conflict(
            "not_a_platform_run",
            "Drive this run from its job's discovery page.",
        ));
    }
    assert_open(&run)?;
    Ok(run)
}

pub async fn tenant_run(c: &mut PgConnection, j: &JobCtx, id: i64) -> ApiResult<RunRow> {
    let run = store::lock_run(c, id).await?;
    if run.scope != "tenant" || run.tenant_id != j.t.tenant || run.job_id != Some(j.job_id) {
        return Err(store::run_not_found(&id.to_string()));
    }
    assert_open(&run)?;
    Ok(run)
}

pub async fn event(
    c: &mut PgConnection,
    run: &RunRow,
    actor: &Actor,
    action: &'static str,
    details: Value,
) -> ApiResult<()> {
    if run.scope == "platform" {
        audit::record_platform(
            c,
            actor,
            PlatformEvent::new(action, run.id.to_string())
                .detail("run", run.id)
                .detail("result", details),
        )
        .await?;
    } else {
        audit::record(
            c,
            actor,
            Event::new(action, "generator_run", run.id.to_string())
                .job(&run.job_key)
                .detail("run", run.id)
                .detail("result", details),
        )
        .await?;
    }
    Ok(())
}

pub async fn detail(c: &mut PgConnection, run: &RunRow) -> ApiResult<Value> {
    let d = store::detail(&mut *c, run.id).await?;
    let mut view = super::run::detail_json(&d, None);
    let f = findings::all(c, &d, false).await?;
    view["red"] = json!(findings::reds(&f).len());
    view["findings"] = json!(f);
    Ok(view)
}

pub async fn context(
    c: &mut PgConnection,
    st: &AppState,
    run: &RunRow,
    hint: Option<String>,
) -> ApiResult<StepContext> {
    let d = store::detail(&mut *c, run.id).await?;
    let items = store::items_full(&mut *c, run.id).await?;
    let (objects, operations) = if run.scope == "platform" {
        let model = crate::platform::templates::head_model(&mut *c).await?;
        let names = |kind| {
            model
                .rows
                .iter()
                .filter(|r| r.kind == kind && r.op != agent_model::kinds::Op::Tombstone)
                .map(|r| r.key.clone())
                .collect::<Vec<_>>()
        };
        (
            names(agent_model::kinds::Kind::Object),
            names(agent_model::kinds::Kind::Operation),
        )
    } else {
        let job = JobRef {
            id: run
                .job_id
                .ok_or_else(|| store::run_not_found(&run.id.to_string()))?,
            key: run.job_key.clone(),
        };
        let b = crate::job_design::base::at(
            c,
            &st.design,
            st.compiler.as_ref(),
            &job,
            &DesignOf::Draft,
        )
        .await?;
        let objects = b
            .operations
            .iter()
            .map(|o| o.object.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let operations = b
            .operations
            .iter()
            .map(|o| format!("{}.{}", o.object, o.verb))
            .collect();
        (objects, operations)
    };
    Ok(StepContext {
        industry: run.industry.clone(), department: run.department.clone(), job: run.job_key.clone(),
        description: run.description.clone(), hint, canonical_objects: objects, canonical_operations: operations,
        stages: d.stages.iter().filter(|s| !s.out_of_scope).map(|s| s.stage.clone()).collect(),
        accepted: json!(items.iter().filter(|i| matches!(i.decision.as_str(), "accepted" | "renamed") && i.step != run.current_step)
            .map(|i| json!({"kind":i.kind,"title":i.title,"payload":i.payload,"example":i.example})).collect::<Vec<_>>()),
    })
}

/// The trimmed hint (F-54); blank = none; more than 2000 characters is 422 `hint_too_long`.
pub fn checked_hint(hint: Option<String>) -> ApiResult<Option<String>> {
    let hint = hint.map(|h| h.trim().to_owned()).filter(|h| !h.is_empty());
    if hint
        .as_ref()
        .is_some_and(|h| h.chars().count() > llm::MAX_HINT)
    {
        return Err(ApiError::Unprocessable(
            "hint_too_long",
            "A hint takes at most 2000 characters.".into(),
        ));
    }
    Ok(hint)
}

/// Model calls are injectable for deterministic tests. Their items and metering commit in the same transaction.
#[allow(clippy::too_many_arguments)]
pub async fn draft(
    c: &mut PgConnection,
    st: &AppState,
    run: &RunRow,
    actor: &Actor,
    by: &str,
    hint: Option<String>,
    drafter: &dyn Drafter,
) -> ApiResult<Value> {
    assert_open(run)?;
    let step = GeneratorStepKey::parse(&run.current_step)
        .filter(|s| s.drafts())
        .ok_or_else(|| conflict("nothing_to_draft", "This step has no drafting call."))?;
    let hint = checked_hint(hint)?;
    let ctx = context(&mut *c, st, run, hint.clone()).await?;
    let drafted = llm::draft(drafter, step, &ctx, st.cfg.generator.draft_timeout).await?;
    if drafted.items.is_empty() {
        return Err(conflict("nothing_to_draft", "The model proposed no items."));
    }
    let items: Vec<_> = drafted
        .items
        .iter()
        .map(|i| store::NewItem {
            kind: &i.kind,
            title: &i.title,
            detail: &i.detail,
            example: i.example.as_ref(),
            payload: &i.payload,
            new_object: i.new_object.as_deref(),
            new_operation: None,
            confidence: i.confidence,
        })
        .collect();
    store::insert_items(&mut *c, run.id, step.as_str(), &items).await?;
    sqlx::query("UPDATE generator_step SET hint=$3 WHERE run_id=$1 AND step=$2")
        .bind(run.id)
        .bind(step.as_str())
        .bind(hint.as_deref())
        .execute(&mut *c)
        .await?;
    // Platform authority is temporarily left for tenant-owned provider usage. The tenant GUC remains local to
    // this transaction, so usage and items either both commit or both roll back.
    if run.scope == "platform" {
        platform_db::leave(&mut *c).await?;
    }
    crate::tenancy::set_tenant(&mut *c, run.tenant_id).await?;
    let tokens = usage::meter(
        &mut *c,
        &st.cfg.metering,
        usage::RunRef {
            id: run.id,
            job_id: run.job_id,
        },
        step,
        by,
        &drafted,
    )
    .await?;
    if run.scope == "platform" {
        platform_db::enter(&mut *c).await?;
    }
    event(
        &mut *c,
        run,
        actor,
        "generator.step_draft",
        json!({"step":step.as_str(),"items":items.len(),"consumed":tokens,"hint":hint.is_some()}),
    )
    .await?;
    store::touch(&mut *c, run.id).await?;
    detail(c, run).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    pub decision: String,
    pub note: Option<String>,
}

pub async fn decide(
    c: &mut PgConnection,
    run: &RunRow,
    actor: &Actor,
    item: &str,
    decision: Decision,
) -> ApiResult<Value> {
    assert_open(run)?;
    let i = store::item_for_update(&mut *c, run.id, item).await?;
    let scope =
        Scope::parse(&run.scope).ok_or_else(|| store::run_not_found(&run.id.to_string()))?;
    let steps = super::run::steps_for(scope);
    let position = |key: &str| steps.iter().position(|s| s.as_str() == key);
    if position(&i.step)
        .zip(position(&run.current_step))
        .is_none_or(|(a, b)| a > b)
    {
        return Err(conflict(
            "step_not_reached",
            "This step has not been reached.",
        ));
    }
    if !["accepted", "merged", "renamed", "dropped", "pending"]
        .contains(&decision.decision.as_str())
    {
        return Err(ApiError::Unprocessable(
            "invalid_decision",
            "Choose accept, merge, rename, drop or pending.".into(),
        ));
    }
    let note = decision
        .note
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if note.is_some_and(|n| n.chars().count() > 2000) {
        return Err(ApiError::Unprocessable(
            "invalid_note",
            "A decision note takes at most 2000 characters.".into(),
        ));
    }
    if decision.decision == "renamed" {
        let name = note.ok_or_else(|| {
            ApiError::Unprocessable(
                "note_required",
                "Give the renamed item its new name.".into(),
            )
        })?;
        if i.kind == "activity"
            && !name
                .split_once(':')
                .is_some_and(|(stage, slug)| is_slug(stage) && is_slug(slug))
        {
            return Err(ApiError::Unprocessable(
                "invalid_key",
                "An activity key is stage:activity in lower_snake_case.".into(),
            ));
        }
    }
    if decision.decision == "merged" {
        let target = note.ok_or_else(|| {
            ApiError::Unprocessable(
                "merge_target_required",
                "Name another item in this step.".into(),
            )
        })?;
        let other = store::item_for_update(&mut *c, run.id, target).await?;
        if target == item
            || other.step != i.step
            || matches!(other.decision.as_str(), "merged" | "dropped")
        {
            return Err(ApiError::Unprocessable(
                "invalid_merge_target",
                "Merge into a different live item of the same step.".into(),
            ));
        }
    }
    sqlx::query("UPDATE generator_item SET decision=$3,decision_note=$4,title=CASE WHEN $3='renamed' THEN $4 ELSE title END WHERE run_id=$1 AND item_id=$2")
        .bind(run.id).bind(item).bind(&decision.decision).bind(note).execute(&mut *c).await?;
    store::touch(&mut *c, run.id).await?;
    event(
        &mut *c,
        run,
        actor,
        "generator.item_decide",
        json!({"item":item,"decision":decision.decision}),
    )
    .await?;
    detail(c, run).await
}

pub async fn stage_scope(
    c: &mut PgConnection,
    run: &RunRow,
    actor: &Actor,
    stage: &str,
    out_of_scope: bool,
) -> ApiResult<Value> {
    assert_open(run)?;
    let changed =
        sqlx::query("UPDATE generator_stage SET out_of_scope=$3 WHERE run_id=$1 AND stage=$2")
            .bind(run.id)
            .bind(stage)
            .bind(out_of_scope)
            .execute(&mut *c)
            .await?
            .rows_affected();
    if changed == 0 {
        return Err(ApiError::Missing(
            "stage_not_found",
            "This run has no such stage.".into(),
        ));
    }
    store::touch(&mut *c, run.id).await?;
    event(
        &mut *c,
        run,
        actor,
        "generator.stage_scope",
        json!({"stage":stage,"out_of_scope":out_of_scope}),
    )
    .await?;
    detail(c, run).await
}

pub async fn advance(c: &mut PgConnection, run: &RunRow, actor: &Actor) -> ApiResult<Value> {
    assert_open(run)?;
    let scope =
        Scope::parse(&run.scope).ok_or_else(|| store::run_not_found(&run.id.to_string()))?;
    let steps = super::run::steps_for(scope);
    let at = steps
        .iter()
        .position(|s| s.as_str() == run.current_step)
        .ok_or_else(|| conflict("invalid_step", "Unknown generator step."))?;
    let next = steps
        .get(at + 1)
        .ok_or_else(|| conflict("use_tag", "Package this run to finish it."))?;
    let items = store::items_full(&mut *c, run.id).await?;
    if items
        .iter()
        .any(|i| i.step == run.current_step && i.decision == "pending")
    {
        return Err(conflict(
            "undecided_items",
            "Decide every item of this step before advancing.",
        ));
    }
    let d = store::detail(&mut *c, run.id).await?;
    if findings::from_validate(&run.current_step) {
        let f = findings::all(&mut *c, &d, false).await?;
        if !findings::reds(&f).is_empty() {
            return Err(conflict(
                "validation_failed",
                "Resolve the red findings before advancing.",
            )
            .with("findings", json!(f)));
        }
    }
    let mut written = 0;
    if scope == Scope::Platform {
        crate::platform::db::lock_family(&mut *c, &accept::dept_layer(run)).await?;
        let result = accept::write_step(&mut *c, actor, run, steps[at], &items, &d.stages).await?;
        if !result.skipped.is_empty() {
            return Err(ApiError::Unprocessable(
                "invalid_accepted_items",
                "Some accepted items cannot be written.".into(),
            )
            .with("items", json!(result.skipped)));
        }
        written = result.written;
        if written > 0 {
            crate::platform::db::tags::tag_family(
                &mut *c,
                actor,
                &accept::dept_layer(run),
                "Generator draft",
                &accept::author(run.id),
                &B8Hooks,
                true,
            )
            .await?;
        }
    }
    sqlx::query("UPDATE generator_step SET status='done',summary=$3 WHERE run_id=$1 AND step=$2")
        .bind(run.id)
        .bind(&run.current_step)
        .bind(format!("{written} revisions written"))
        .execute(&mut *c)
        .await?;
    sqlx::query("UPDATE generator_step SET status='current' WHERE run_id=$1 AND step=$2")
        .bind(run.id)
        .bind(next.as_str())
        .execute(&mut *c)
        .await?;
    sqlx::query("UPDATE generator_run SET current_step=$2,updated_at=now() WHERE id=$1")
        .bind(run.id)
        .bind(next.as_str())
        .execute(&mut *c)
        .await?;
    event(
        &mut *c,
        run,
        actor,
        "generator.advance",
        json!({"from_step":run.current_step,"to_step":next.as_str(),"written":written}),
    )
    .await?;
    detail(c, run).await
}
