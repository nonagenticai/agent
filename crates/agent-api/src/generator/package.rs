//! B8 T11 / T10b: Package. The last step of a run.
//!
//! - PLATFORM (`tagGeneratorRun`): the run's findings are re-computed over the grid as written (rules 1–6 over the
//!   untagged revisions, `findings::all(.., at_package = true)`); any red is 409 `validation_failed` and no tag is
//!   written. Else every output layer with untagged revisions is cut through B6's `tag_family` (the family's
//!   provisional `v0` becomes the real `@vN` in the same transaction, P7); a layer with nothing to tag is left
//!   out. The run becomes `packaged`, `tagged` = the tags cut, all steps `done`, one `generator.tag` platform audit
//!   row.
//! - TENANT (`packageJobGeneratorRun`, D97): the accepted journey items become refinements of THIS job's draft
//!   (D8: an extension for a new activity, `hide` for a template activity the run dropped), through the J4
//!   refinement writers; never a layer row. Nothing is live (D54). A row the refinement rules refuse is skipped and
//!   listed in the step summary.
use serde_json::{Value, json};
use sqlx::PgConnection;

use super::run::RunRow;
use super::{accept, drive, findings, store};
use crate::audit::Actor;
use crate::design::{DesignOf, JobRef};
use crate::error::{ApiError, ApiResult};
use crate::journey::model::Actor as JourneyActor;
use crate::journey::refine::{self, NewExtension};
use crate::platform::hooks::GeneratorGuard;
use crate::state::AppState;
use crate::tenancy::JobCtx;

fn conflict(code: &'static str, msg: &str) -> ApiError {
    ApiError::Conflict(code, msg.into())
}

/// Common to both scopes: the run is at Package and carries no red finding.
async fn ready(c: &mut PgConnection, run: &RunRow) -> ApiResult<()> {
    if run.current_step != "package" {
        return Err(conflict(
            "not_at_package",
            "Advance the run to Package first.",
        ));
    }
    let d = store::detail(&mut *c, run.id).await?;
    let f = findings::all(&mut *c, &d, true).await?;
    if !findings::reds(&f).is_empty() {
        return Err(conflict(
            "validation_failed",
            "Resolve the red findings before packaging.",
        )
        .with("findings", json!(f)));
    }
    Ok(())
}

async fn finish(
    c: &mut PgConnection,
    run: &RunRow,
    tagged: &str,
    output: &[String],
    summary: &str,
) -> ApiResult<()> {
    sqlx::query("UPDATE generator_step SET status = 'done' WHERE run_id = $1")
        .bind(run.id)
        .execute(&mut *c)
        .await?;
    sqlx::query("UPDATE generator_step SET summary = $3 WHERE run_id = $1 AND step = $2")
        .bind(run.id)
        .bind(&run.current_step)
        .bind(summary)
        .execute(&mut *c)
        .await?;
    sqlx::query("UPDATE generator_run SET status = 'packaged', tagged = $2, output = $3, updated_at = now() WHERE id = $1")
        .bind(run.id)
        .bind(tagged)
        .bind(output)
        .execute(&mut *c)
        .await?;
    Ok(())
}

/// `tagGeneratorRun`. The caller holds the run (`drive::platform_run`) in a platform transaction.
pub async fn tag_platform(
    c: &mut PgConnection,
    guard: &dyn GeneratorGuard,
    run: &RunRow,
    actor: &Actor,
    by: &str,
) -> ApiResult<Value> {
    drive::assert_open(run)?;
    ready(&mut *c, run).await?;
    let mut cut: Vec<String> = Vec::new();
    let mut kept: Vec<String> = Vec::new();
    for out in &run.output {
        let Some((family, _)) = out.split_once('@') else {
            // an approved operation (`operation:<key>`) listed on the run: not a layer
            kept.push(out.clone());
            continue;
        };
        crate::platform::db::lock_family(&mut *c, family).await?;
        let note = format!("Generator run #{}", run.id);
        match crate::platform::db::tags::tag_family(
            &mut *c,
            actor,
            family,
            &note,
            &accept::author(run.id),
            guard,
            false,
        )
        .await
        {
            Ok(t) => cut.push(format!("{family}@v{}", t.version)),
            Err(e) if e.code() == "nothing_to_tag" => {}
            Err(e) => return Err(e),
        }
    }
    let tagged = if cut.is_empty() {
        "no changes".to_owned()
    } else {
        cut.join(", ")
    };
    let mut output = cut.clone();
    output.extend(kept);
    finish(&mut *c, run, &tagged, &output, &format!("tagged {tagged}")).await?;
    drive::event(
        &mut *c,
        run,
        actor,
        "generator.tag",
        json!({"tagged": cut, "by": by}),
    )
    .await?;
    drive::detail(c, run).await
}

/// `packageJobGeneratorRun`. The caller holds the run (`drive::tenant_run`) in the job's scoped transaction.
pub async fn package_tenant(
    c: &mut PgConnection,
    st: &AppState,
    j: &JobCtx,
    run: &RunRow,
    actor: &Actor,
) -> ApiResult<Value> {
    drive::assert_open(run)?;
    ready(&mut *c, run).await?;
    let job = JobRef {
        id: j.job_id,
        key: j.job_key.clone(),
    };
    crate::design::ensure_job(&mut *c, &st.design, &job).await?;
    let bases = crate::job_design::base::at(
        &mut *c,
        &st.design,
        st.compiler.as_ref(),
        &job,
        &DesignOf::Draft,
    )
    .await?;
    let items = store::items_full(&mut *c, run.id).await?;
    let (mut saved, mut skipped) = (0usize, Vec::<String>::new());
    for it in items
        .iter()
        .filter(|i| i.step == "journey" && i.kind == "activity")
    {
        let payload = it.payload.clone().unwrap_or(Value::Null);
        let Some(key) = super::accept::activity_key(
            &it.decision,
            &it.title,
            payload.get("key").and_then(Value::as_str),
        ) else {
            skipped.push(format!("{}: not stage:activity", it.title.trim()));
            continue;
        };
        let text = |k: &str| {
            payload
                .get(k)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        let result = match it.decision.as_str() {
            "accepted" | "renamed" if bases.activity(&key).is_none() => {
                let Some((stage, slug)) = key.split_once(':') else {
                    skipped.push(format!("{key}: not stage:activity"));
                    continue;
                };
                let actor_of = text("actor")
                    .and_then(|a| serde_json::from_value::<JourneyActor>(json!(a)).ok())
                    .unwrap_or(JourneyActor::Customer);
                refine::add_extension(
                    &mut *c,
                    &st.design,
                    st.compiler.as_ref(),
                    j,
                    NewExtension {
                        stage: stage.to_owned(),
                        slug: slug.to_owned(),
                        label: text("label").unwrap_or_else(|| slug.replace('_', " ")),
                        actor: actor_of,
                        trigger: text("trigger").unwrap_or_default(),
                    },
                )
                .await
            }
            "dropped" if bases.activity(&key).is_some() => {
                refine::hide(&mut *c, &st.design, st.compiler.as_ref(), j, &key).await
            }
            _ => continue,
        };
        match result {
            Ok(_) => saved += 1,
            Err(e)
                if matches!(
                    e.status_and_code().0,
                    axum::http::StatusCode::CONFLICT
                        | axum::http::StatusCode::UNPROCESSABLE_ENTITY
                        | axum::http::StatusCode::NOT_FOUND
                ) =>
            {
                skipped.push(format!("{key}: {}", e.code()));
            }
            Err(e) => return Err(e),
        }
    }
    let summary = if skipped.is_empty() {
        format!("{saved} refinements saved")
    } else {
        format!("{saved} refinements saved; skipped: {}", skipped.join("; "))
    };
    finish(&mut *c, run, "refinements saved", &run.output, &summary).await?;
    drive::event(
        &mut *c,
        run,
        actor,
        "generator.tag",
        json!({"saved": saved, "skipped": skipped}),
    )
    .await?;
    drive::detail(c, run).await
}
