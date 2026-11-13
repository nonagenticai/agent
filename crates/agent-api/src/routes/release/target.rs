//! The job's template selection (B2 T7, spec §4, D65): `releaseTarget` and `saveReleaseTarget`.
//!
//! - `GET /api/jobs/{job}/target` — any member: `JobTarget` (the draft selection, who set it and when, what makes it
//!   stale, the selection options, the history newest first).
//! - `PUT /api/jobs/{job}/target` `{selection, expected_updated_at}` — `release_manager`: the selection is saved to
//!   the DRAFT (D54, kind `selection`, page J14) and audited `job.selection_update {old, new, change}`. 409
//!   `selection_changed` when `expected_updated_at` is not the stored `updated_at`; 422 `invalid_selection`.
//!   A move to another industry/department also reconciles `jobs.pins` (the selection's families at their tags, the
//!   obsolete industry/department pins dropped; `job.pins_update`); a job with any release refuses that move
//!   409 `hierarchy_immutable` (apply writes the release's pins back).
//!
//! A port of the fixture `releaseTarget` / `saveReleaseTarget` (`web/src/lib/api/areas/release.ts`) and
//! `describeChange`. Every handler runs `ensure_job` first in its job-scoped transaction; `design::track` locks the
//! job, records the ledger row and bumps `draft_rev` (the handler never does).
use agent_model::compile::Selection;
use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;

use super::draft::Names;
use crate::audit::{self, Event};
use crate::design::{self, DesignOf, DraftRef, DraftWrite, JobRef, Label};
use crate::error::{ApiError, ApiJson, ApiResult};
use crate::platform::templates::{self, TemplatePin};
use crate::release::bundle::SelectionOptions;
use crate::release::compile::{TemplateCompiler, check_selection, selection_error};
use crate::state::AppState;
use crate::tenancy::{JobCtx, JobMember, JobRequires, role};

pub fn routes() -> Router<AppState> {
    Router::new().route(
        "/api/jobs/{job}/target",
        get(release_target).put(save_release_target),
    )
}

/// Fixture `SELECTION_VALUE_KEYS`: the selection values `describeChange` lists after the departments.
const SELECTION_VALUE_KEYS: [&str; 3] = ["area", "channel", "region"];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SaveTarget {
    selection: Selection,
    /// The `selection_updated_at` the client last read; `null` while no selection was ever saved.
    #[serde(default)]
    expected_updated_at: Option<String>,
}

#[derive(sqlx::FromRow)]
struct TargetRow {
    selection: Option<Value>,
    updated_by: Option<String>,
    updated_at: Option<DateTime<Utc>>,
}

#[derive(sqlx::FromRow)]
struct HistoryRow {
    at: DateTime<Utc>,
    actor: String,
    change: String,
    old: Option<Value>,
    new: Option<Value>,
}

fn internal(what: String) -> ApiError {
    ApiError::Internal(anyhow::anyhow!(what))
}

fn job_ref(j: &JobCtx) -> JobRef {
    JobRef {
        id: j.job_id,
        key: j.job_key.clone(),
    }
}

fn iso(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

fn parse_selection(v: Value) -> ApiResult<Selection> {
    serde_json::from_value(v)
        .map_err(|e| internal(format!("job_target.selection is unreadable: {e}")))
}

fn to_json(s: &Selection) -> ApiResult<Value> {
    serde_json::to_value(s).map_err(|e| internal(format!("a selection does not serialise: {e}")))
}

/// A null selection field as the fixture's template string shows it.
fn shown(v: &Option<String>) -> &str {
    v.as_deref().unwrap_or("null")
}

/// Fixture `canonicalSelection`: the lists sorted, except the departments, whose order is meaningful (D115).
fn canonical(s: &Selection) -> Selection {
    let mut c = s.clone();
    c.areas.sort();
    c.dimensions.channel.sort();
    c
}

/// Fixture `sameSelection`.
fn same_selection(a: &Selection, b: &Selection) -> bool {
    canonical(a) == canonical(b)
}

fn diff_values(parts: &mut Vec<String>, label: &str, a: &[String], b: &[String]) {
    for x in b {
        if !a.contains(x) {
            parts.push(format!("{label} + {x}"));
        }
    }
    for x in a {
        if !b.contains(x) {
            parts.push(format!("{label} − {x}"));
        }
    }
}

/// Fixture `describeChange`: `area + logistics · channel − telegram`.
fn describe_change(old: Option<&Selection>, next: &Selection) -> String {
    let Some(old) = old else {
        return "first selection".to_string();
    };
    let mut parts: Vec<String> = Vec::new();
    if old.profile != next.profile {
        parts.push(format!(
            "profile {} → {}",
            shown(&old.profile),
            shown(&next.profile)
        ));
    }
    if old.industry != next.industry {
        parts.push(format!("industry {} → {}", old.industry, next.industry));
    }
    diff_values(
        &mut parts,
        "department",
        &old.departments,
        &next.departments,
    );
    // D115: the order is part of the selection (primary first)
    if old.departments != next.departments
        && old.departments.len() == next.departments.len()
        && old.departments.iter().all(|d| next.departments.contains(d))
    {
        parts.push(format!(
            "department order {} → {}",
            old.departments.join(", "),
            next.departments.join(", ")
        ));
    }
    for d in SELECTION_VALUE_KEYS {
        diff_values(&mut parts, d, old.values(d), next.values(d));
    }
    if parts.is_empty() {
        "no change".to_string()
    } else {
        parts.join(" · ")
    }
}

/// `expected` (the client's `expected_updated_at`) names the stored `updated_at`: both absent, or the same instant.
/// Anything unparseable is a mismatch (fixture: `expected !== t.updated_at`).
fn same_instant(expected: Option<&str>, at: Option<DateTime<Utc>>) -> bool {
    match (expected, at) {
        (None, None) => true,
        (Some(s), Some(t)) => {
            DateTime::parse_from_rfc3339(s.trim()).is_ok_and(|d| d.with_timezone(&Utc) == t)
        }
        _ => false,
    }
}

async fn target_row(c: &mut PgConnection, job: &JobRef) -> ApiResult<TargetRow> {
    sqlx::query_as::<_, TargetRow>(
        "SELECT selection, updated_by, updated_at FROM job_target WHERE job_id = $1",
    )
    .bind(job.id)
    .fetch_optional(&mut *c)
    .await?
    .ok_or_else(|| internal(format!("job {} has no job_target row", job.key)))
}

/// The `JobTarget` view (fixture `releaseTarget`), over the selection `options` the caller already read.
async fn target_view(
    c: &mut PgConnection,
    options: SelectionOptions,
    job: &JobRef,
) -> ApiResult<Value> {
    let row = target_row(&mut *c, job).await?;
    let sel = row.selection.clone().map(parse_selection).transpose()?;
    let error = selection_error(&options, sel.as_ref());
    let mut names = Names::default();
    let by_name = names.of(&mut *c, row.updated_by.as_deref()).await?;
    let rows = sqlx::query_as::<_, HistoryRow>(
        "SELECT at, actor, change, old, new FROM job_target_history WHERE job_id = $1 ORDER BY id DESC",
    )
    .bind(job.id)
    .fetch_all(&mut *c)
    .await?;
    let mut history = Vec::with_capacity(rows.len());
    for h in rows {
        let actor_name = names.of(&mut *c, Some(h.actor.as_str())).await?;
        history.push(json!({
            "at": iso(h.at),
            "actor": h.actor,
            "actor_name": actor_name,
            "action": "job.selection_update",
            "change": h.change,
            "old": h.old,
            "new": h.new,
        }));
    }
    Ok(json!({
        "job": job.key,
        "selection": row.selection,
        "selection_updated_by": row.updated_by,
        "selection_updated_by_name": by_name,
        "selection_updated_at": row.updated_at.map(iso),
        "selection_error": error,
        "options": options,
        "history": history,
    }))
}

async fn release_target(
    JobMember(j): JobMember,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let options = TemplateCompiler::options(st.compiler.as_ref(), &mut tx).await?;
    let v = target_view(&mut tx, options, &jr).await?;
    tx.commit().await?;
    Ok(Json(v))
}

async fn save_release_target(
    JobRequires(j, _): JobRequires<role::ReleaseManager>,
    State(st): State<AppState>,
    ApiJson(b): ApiJson<SaveTarget>,
) -> ApiResult<Json<Value>> {
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    // serialise with every other write of the job's design before reading the row the check compares
    sqlx::query("SELECT 1 FROM jobs WHERE id = $1 FOR UPDATE")
        .bind(jr.id)
        .execute(&mut *tx)
        .await?;
    let row = target_row(&mut tx, &jr).await?;
    if !same_instant(b.expected_updated_at.as_deref(), row.updated_at) {
        return Err(ApiError::Conflict(
            "selection_changed",
            "Someone else changed the selection; reload and try again.".into(),
        ));
    }
    let next = b.selection;
    // D106/D115: the hierarchy, then the pins invariant against what the job runs (`jobs.pins`: one
    // `industry/<i>` pin and a pin per selected department, none of another industry), then the options
    templates::hierarchy_refusal(&next)?;
    // D111: a region of the platform's vocabulary (422 `unknown_region`), before the options' generic check
    templates::region_refusal(&mut tx, &next).await?;
    let (pins_raw, has_release): (Value, bool) = sqlx::query_as(
        "SELECT pins, EXISTS (SELECT 1 FROM job_release WHERE job_id = $1) FROM jobs WHERE id = $1",
    )
    .bind(jr.id)
    .fetch_one(&mut *tx)
    .await?;
    let pins: Vec<TemplatePin> = serde_json::from_value(pins_raw.clone())
        .map_err(|e| internal(format!("jobs.pins of {} is unreadable: {e}", jr.key)))?;
    let old = row.selection.map(parse_selection).transpose()?;
    // Only a move to another industry/department reconciles the stack; any other edit keeps the pins exactly as they
    // are (a released job stays editable for areas, channel and region).
    let hierarchy_changed = old
        .as_ref()
        .is_none_or(|o| o.industry != next.industry || o.departments != next.departments);
    let mut next_pins = if hierarchy_changed {
        templates::reconcile_pins(&pins, &next)
    } else {
        pins.clone()
    };
    // a job without a template pins nothing to check against (it compiles nothing either)
    let pins_changed = !pins.is_empty() && next_pins != pins;
    if !pins.is_empty() && !pins_changed {
        templates::issues_refusal(&templates::pin_issues(&next, &pins))?;
    }
    if pins_changed {
        // A release copies its own pins back into `jobs.pins` on apply (release/apply.rs:327), so moving the stack
        // of a job that has ANY release would be overwritten — refuse before anything is written.
        if has_release {
            return Err(ApiError::Conflict(
                "hierarchy_immutable",
                format!(
                    "{} has a release, so its template stack cannot change industry or department here.",
                    jr.key
                ),
            ));
        }
        // A family the selection added was cut on a stack of its own (a department on its base): raise each family
        // the selection now pins to the version that tag records, as a cut's `adopt_pins` would. Families the
        // selection does not need — a sibling department, every other base department — are not added.
        let mut completed = templates::complete_pins(&mut tx, next_pins, &next).await?;
        let added: Vec<TemplatePin> = completed
            .iter()
            .filter(|p| !pins.iter().any(|q| q.layer == p.layer))
            .cloned()
            .collect();
        for a in &added {
            for s in st.compiler.stack(&mut tx, a).await? {
                if let Some(p) = completed.iter_mut().find(|p| p.layer == s.layer) {
                    p.version = p.version.max(s.version);
                }
            }
        }
        // an untagged family is not pinnable: the selection is refused exactly as it was before
        templates::issues_refusal(&templates::pin_issues(&next, &completed))?;
        match templates::compile_pinned(
            &mut tx,
            &completed,
            &next,
            crate::release::compile::job_key(&completed),
        )
        .await?
        {
            Ok(_) => {}
            Err(errors) => return Err(templates::compile_refusal(&errors)),
        }
        next_pins = completed;
    }
    let options = TemplateCompiler::options(st.compiler.as_ref(), &mut tx).await?;
    check_selection(&options, &next)?;
    let selection_changed = old.as_ref().is_none_or(|o| !same_selection(o, &next));
    let pins_new_json = serde_json::to_value(&next_pins)
        .map_err(|e| internal(format!("pins of {} cannot be stored: {e}", jr.key)))?;
    if selection_changed || pins_changed {
        let sub = j.t.caller.sub.clone();
        if selection_changed {
            // the ledger label reads against what Production runs (the live selection, else the stored one)
            let live_snap = design::value(
                &mut tx,
                &st.design,
                &jr,
                &DesignOf::Live,
                "selection",
                &DraftRef::new(),
            )
            .await?;
            let live = match live_snap.filter(|v| !v.is_null()) {
                Some(v) => Some(parse_selection(v)?),
                None => old.clone(),
            };
            let change = describe_change(old.as_ref(), &next);
            let label = format!("Selection: {}", describe_change(live.as_ref(), &next));
            let old_json = old.as_ref().map(to_json).transpose()?;
            let new_json = to_json(&next)?;
            sqlx::query(
                "INSERT INTO job_target_history (job_id, actor, change, old, new) \
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(jr.id)
            .bind(&sub)
            .bind(&change)
            .bind(&old_json)
            .bind(&new_json)
            .execute(&mut *tx)
            .await?;
            let w = DraftWrite {
                area: "selection",
                page: "J14",
                kind: "selection",
                r#ref: DraftRef::new(),
                label: Label::Now(label),
            };
            let (job_id, sel, by, pins_next) = (
                jr.id,
                new_json.clone(),
                sub,
                pins_changed.then(|| pins_new_json.clone()),
            );
            // B3 T10: the kinds are design data of this same write — a stack move re-materialises them (every target
            // write does; it is idempotent, and kinds also depend on the selection, never on the pins alone)
            let kinds_job = jr.clone();
            let kinds_pins = next_pins.clone();
            let kinds_template_selection = next.clone();
            design::track::<(), _>(&mut tx, &st.design, &j, w, move |c| {
                Box::pin(async move {
                    sqlx::query(
                        "UPDATE job_target SET selection = $2, updated_by = $3, updated_at = clock_timestamp() \
                          WHERE job_id = $1",
                    )
                    .bind(job_id)
                    .bind(sel)
                    .bind(by)
                    .execute(&mut *c)
                    .await?;
                    if let Some(p) = pins_next {
                        sqlx::query("UPDATE jobs SET pins = $2 WHERE id = $1")
                            .bind(job_id)
                            .bind(p)
                            .execute(&mut *c)
                            .await?;
                        sqlx::query(
                            "UPDATE template_upgrade_request SET status = 'withdrawn' \
                              WHERE job_id = $1 AND status = 'open'",
                        )
                        .bind(job_id)
                        .execute(&mut *c)
                        .await?;
                    }
                    crate::knowledge::kinds::materialise_from_template(
                        &mut *c,
                        &kinds_job,
                        &kinds_pins,
                        Some(&kinds_template_selection),
                    )
                    .await?;
                    Ok::<(), ApiError>(())
                })
            })
            .await?;
            let mut ev = Event::new("job.selection_update", "job", jr.key.clone())
                .job(&jr.key)
                .detail("old", old_json)
                .detail("new", new_json)
                .detail("change", change);
            if pins_changed {
                ev = ev
                    .detail("pins_old", pins_raw.clone())
                    .detail("pins_new", pins_new_json.clone());
            }
            audit::record(&mut tx, &j.t.actor(), ev).await?;
        } else {
            // only the stack changed (the selection is the same): a draft change like any other, but `track`
            // sees the selection value unchanged and so neither bumps `draft_rev` nor nudges — do both here.
            let w = DraftWrite {
                area: "selection",
                page: "J14",
                kind: "selection",
                r#ref: DraftRef::new(),
                label: Label::Now("Template stack updated".into()),
            };
            let (job_id, pins_next) = (jr.id, pins_new_json.clone());
            // the selection is unchanged, but the pins are not: the kinds follow the new template's stack
            let kinds_job = jr.clone();
            let kinds_pins = next_pins.clone();
            let kinds_template_selection = next.clone();
            design::track::<(), _>(&mut tx, &st.design, &j, w, move |c| {
                Box::pin(async move {
                    sqlx::query("UPDATE jobs SET pins = $2 WHERE id = $1")
                        .bind(job_id)
                        .bind(pins_next)
                        .execute(&mut *c)
                        .await?;
                    crate::knowledge::kinds::materialise_from_template(
                        &mut *c,
                        &kinds_job,
                        &kinds_pins,
                        Some(&kinds_template_selection),
                    )
                    .await?;
                    sqlx::query(
                        "UPDATE template_upgrade_request SET status = 'withdrawn' \
                          WHERE job_id = $1 AND status = 'open'",
                    )
                    .bind(job_id)
                    .execute(&mut *c)
                    .await?;
                    Ok::<(), ApiError>(())
                })
            })
            .await?;
            sqlx::query("UPDATE jobs SET draft_rev = draft_rev + 1 WHERE id = $1")
                .bind(jr.id)
                .execute(&mut *tx)
                .await?;
            crate::runtime::design::nudge_on_commit(
                &mut tx,
                &[jr.id],
                crate::runtime::scope::Env::Test,
            )
            .await?;
            audit::record(
                &mut tx,
                &j.t.actor(),
                Event::new("job.pins_update", "job", jr.key.clone())
                    .job(&jr.key)
                    .detail("old", pins_raw)
                    .detail("new", pins_new_json),
            )
            .await?;
        }
    }
    let v = target_view(&mut tx, options, &jr).await?;
    tx.commit().await?;
    Ok(Json(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(areas: &[&str], channel: &[&str]) -> Selection {
        let mut s = Selection {
            profile: Some("retail_ecommerce".into()),
            industry: "retail".into(),
            departments: vec!["industry_dept/retail/customer_service".into()],
            ..Selection::default()
        };
        s.areas = areas.iter().map(|a| a.to_string()).collect();
        s.dimensions.channel = channel.iter().map(|c| c.to_string()).collect();
        s
    }

    #[test]
    fn describe_change_matches_the_fixture() {
        let a = sel(&["order-support"], &["chat", "telegram"]);
        assert_eq!(describe_change(None, &a), "first selection");
        assert_eq!(describe_change(Some(&a), &a), "no change");
        let b = sel(&["order-support", "logistics"], &["chat"]);
        assert_eq!(
            describe_change(Some(&a), &b),
            "area + logistics · channel − telegram"
        );
        let mut c = b.clone();
        c.profile = None;
        assert_eq!(
            describe_change(Some(&b), &c),
            "profile retail_ecommerce → null"
        );
    }

    #[test]
    fn a_department_reorder_is_a_change() {
        let mut a = sel(&[], &["chat"]);
        a.departments.push("industry_dept/retail/returns".into());
        let mut b = a.clone();
        b.departments.reverse();
        assert!(
            !same_selection(&a, &b),
            "D115: the order is part of the selection"
        );
        assert_eq!(
            describe_change(Some(&a), &b),
            "department order industry_dept/retail/customer_service, industry_dept/retail/returns → \
             industry_dept/retail/returns, industry_dept/retail/customer_service"
        );
    }

    #[test]
    fn same_selection_ignores_list_order() {
        let a = sel(&["faq", "order-support"], &["telegram", "chat"]);
        let b = sel(&["order-support", "faq"], &["chat", "telegram"]);
        assert!(same_selection(&a, &b));
        assert!(!same_selection(&a, &sel(&["faq"], &["chat"])));
    }

    #[test]
    fn expected_updated_at_compares_instants() {
        let t = DateTime::parse_from_rfc3339("2026-10-20T10:00:00.123456Z")
            .unwrap()
            .with_timezone(&Utc);
        assert!(same_instant(None, None));
        assert!(same_instant(Some(iso(t).as_str()), Some(t)));
        assert!(same_instant(
            Some("2026-10-20T12:00:00.123456+02:00"),
            Some(t)
        ));
        assert!(!same_instant(None, Some(t)));
        assert!(!same_instant(Some("2026-10-20T10:00:00Z"), Some(t)));
        assert!(!same_instant(Some("yesterday"), Some(t)));
        assert!(!same_instant(Some(""), None));
    }
}
