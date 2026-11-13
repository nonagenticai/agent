//! Release conflicts, the read side and the resolve branch (B2 T13, spec §4, 05 §4.6, J16, D54). Ports of the fixture
//! `releaseConflicts`, `conflictBoard` / `conflictCard`, `resolveConflict` and `liveDecision`
//! (`web/src/lib/api/areas/release.ts`), which is the accepted contract for codes, messages and shapes.
//!
//! - `GET /api/jobs/{job}/release-conflicts?status=open|all` — any member: `Paged<ReleaseConflict>`, newest
//!   detection first (`status` absent = `open`; anything else is 422 `invalid_status`).
//! - `GET /api/jobs/{job}/release-conflicts/board?status=open|all` — any member: `ConflictBoard` (J16 cards: base
//!   before → after, `mine`, the stored `cause`, `impact` from [`impact`], where the decision is in draft → publish,
//!   and the counts `open` (undecided) / `decided_in_draft`).
//! - `resolveConflict` is `POST /api/jobs/{job}/refinements/{revert|rebase}` `{conflict}`: the refine route
//!   (`routes::release::refine`) branches to [`resolve`]. The choice is a DRAFT change (D54) run through
//!   `overlay::refine`, which saves `decision` on the place's open conflicts (`draft_key` = `overlay:<object>:<key>`
//!   or `setting:<key>`) and links it to the ledger row; the conflict stays `open` until the release carrying that
//!   draft is applied (`release::conflicts::on_apply`). Discarding the draft clears the decision
//!   (`design::drafts`).
//!
//! A decision is "live" (fixture `liveDecision`) while something still carries it to Production: its unpublished
//! ledger row, or an approved cut that bundled it. A decision nothing carries does not count: the conflict is
//! undecided and may be resolved again.
use std::collections::BTreeMap;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::draft::{Names, iso};
use crate::design::{self, JobRef, Registry, ledger};
use crate::error::{ApiError, ApiQuery, ApiResult};
use crate::page::paged;
use crate::release::compile::TemplateCompiler;
use crate::release::overlay::{self, JOB_SETTING, Op};
use crate::state::AppState;
use crate::tenancy::{JobCtx, JobMember};

/// Fixture `edit_path`: the design page (relative to the job, `jobPath(job, edit_path)`) where the refinement can be
/// edited instead.
pub const EDIT_PATH: &str = "/refinements";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/jobs/{job}/release-conflicts", get(release_conflicts))
        .route(
            "/api/jobs/{job}/release-conflicts/board",
            get(conflict_board),
        )
}

fn job_ref(j: &JobCtx) -> JobRef {
    JobRef {
        id: j.job_id,
        key: j.job_key.clone(),
    }
}

#[derive(Deserialize)]
struct StatusQuery {
    status: Option<String>,
}

/// `open` (also when absent) → `false`, `all` → `true`; anything else is 422 `invalid_status`.
fn wants_all(status: Option<&str>) -> ApiResult<bool> {
    match status {
        None | Some("open") => Ok(false),
        Some("all") => Ok(true),
        Some(_) => Err(ApiError::Unprocessable(
            "invalid_status",
            "status must be open or all.".to_string(),
        )),
    }
}

/// One `release_conflict` row.
#[derive(sqlx::FromRow)]
struct ConflictRow {
    id: i64,
    kind: String,
    object: String,
    key: String,
    tenant_row: String,
    detail: String,
    base_hash_before: Option<String>,
    base_hash_after: Option<String>,
    base_before: Option<Value>,
    base_after: Option<Value>,
    cause: Option<Value>,
    raised_by_cut: Option<String>,
    release_id: String,
    detected_at: DateTime<Utc>,
    status: String,
    resolution: Option<String>,
    resolved_by: Option<String>,
    resolved_at: Option<DateTime<Utc>>,
    decision: Option<Value>,
}

/// The job's conflicts (`all` = every status, else the open ones), newest detection first, ties newest id first
/// (fixture sort).
async fn rows(c: &mut PgConnection, job: Uuid, all: bool) -> ApiResult<Vec<ConflictRow>> {
    Ok(sqlx::query_as::<_, ConflictRow>(
        "SELECT id, kind, object, key, tenant_row, detail, base_hash_before, base_hash_after, base_before, \
                base_after, cause, raised_by_cut, release_id, detected_at, status, resolution, resolved_by, \
                resolved_at, decision \
           FROM release_conflict \
          WHERE job_id = $1 AND ($2 OR status = 'open') \
          ORDER BY detected_at DESC, id DESC",
    )
    .bind(job)
    .bind(all)
    .fetch_all(&mut *c)
    .await?)
}

/// Fixture `stripConflict`: the `ReleaseConflict` of a row, `decision.by_name` and `resolved_by_name` filled in.
async fn conflict_json(
    c: &mut PgConnection,
    names: &mut Names,
    r: &ConflictRow,
) -> ApiResult<Map<String, Value>> {
    let decision = match &r.decision {
        Some(Value::Object(d)) => {
            let mut d = d.clone();
            let by = d.get("by").and_then(Value::as_str).map(str::to_owned);
            let by_name = names.of(&mut *c, by.as_deref()).await?;
            d.insert("by_name".to_string(), Value::String(by_name));
            Value::Object(d)
        }
        _ => Value::Null,
    };
    let mut m = Map::new();
    m.insert("id".to_string(), json!(r.id));
    m.insert("kind".to_string(), json!(r.kind));
    m.insert("object".to_string(), json!(r.object));
    m.insert("key".to_string(), json!(r.key));
    m.insert("tenant_row".to_string(), json!(r.tenant_row));
    m.insert("detail".to_string(), json!(r.detail));
    m.insert("base_hash_before".to_string(), json!(r.base_hash_before));
    m.insert("base_hash_after".to_string(), json!(r.base_hash_after));
    m.insert("release_id".to_string(), json!(r.release_id));
    m.insert("detected_at".to_string(), json!(iso(r.detected_at)));
    m.insert("status".to_string(), json!(r.status));
    m.insert("resolution".to_string(), json!(r.resolution));
    m.insert("resolved_by".to_string(), json!(r.resolved_by));
    if let Some(sub) = r.resolved_by.as_deref() {
        let n = names.of(&mut *c, Some(sub)).await?;
        m.insert("resolved_by_name".to_string(), Value::String(n));
    }
    m.insert("resolved_at".to_string(), json!(r.resolved_at.map(iso)));
    m.insert("decision".to_string(), decision);
    m.insert("raised_by_cut".to_string(), json!(r.raised_by_cut));
    Ok(m)
}

/// Where a decision is on its way to Production (fixture `liveDecision`).
struct LiveDecision {
    /// The unpublished ledger row carrying it (`d-<id>`) and its label.
    row: Option<(String, String)>,
    /// The first approved (cut, not applied) release that bundled it.
    cut: Option<String>,
}

/// Fixture `liveDecision`: an OPEN conflict's decision while its ledger row is unpublished or an approved cut bundled
/// it; `None` for any other conflict (no decision, or nothing carries it any more).
async fn live_decision(
    c: &mut PgConnection,
    job: &JobRef,
    status: &str,
    decision: Option<&Value>,
) -> ApiResult<Option<LiveDecision>> {
    if status != "open" {
        return Ok(None);
    }
    let Some(key) = decision
        .and_then(|d| d.get("draft_key"))
        .and_then(Value::as_str)
    else {
        return Ok(None);
    };
    let row = ledger::find(&mut *c, job, key).await?;
    let cut = ledger::approved_cuts(&mut *c, job, key)
        .await?
        .into_iter()
        .next();
    if row.is_none() && cut.is_none() {
        return Ok(None);
    }
    Ok(Some(LiveDecision {
        row: row.map(|r| (format!("d-{}", r.id), r.label)),
        cut,
    }))
}

/// J16 customer impact of an open conflict (`ConflictImpact {conversations_30d, effect}`): `None` until B4 provides
/// case counts by place. B4 fills this hook; the board calls it for every card.
pub async fn impact(
    _c: &mut PgConnection,
    _job: &JobRef,
    _object: &str,
    _key: &str,
) -> ApiResult<Option<Value>> {
    Ok(None)
}

/// The tenant's refinement content at the place now (fixture `mine`): the overlay row's item (null for a hide or
/// no row), or `{<key>: value}` for a job setting override (value null for a hide).
async fn mine(c: &mut PgConnection, job: Uuid, object: &str, key: &str) -> ApiResult<Value> {
    if object == JOB_SETTING {
        let v: Option<Option<String>> = sqlx::query_scalar(
            "SELECT value FROM job_setting_override WHERE job_id = $1 AND key = $2",
        )
        .bind(job)
        .bind(key)
        .fetch_optional(&mut *c)
        .await?;
        return Ok(match v {
            Some(v) => {
                let mut m = Map::new();
                m.insert(key.to_string(), json!(v));
                Value::Object(m)
            }
            None => Value::Null,
        });
    }
    let item: Option<Option<Value>> = sqlx::query_scalar(
        "SELECT item FROM job_overlay_row WHERE job_id = $1 AND object = $2 AND key = $3",
    )
    .bind(job)
    .bind(object)
    .bind(key)
    .fetch_optional(&mut *c)
    .await?;
    Ok(item.flatten().unwrap_or(Value::Null))
}

/// What the base rows are: the row itself, or its parent entity for a `parent_missing` attribute.
fn base_subject(r: &ConflictRow) -> String {
    if r.kind == "parent_missing"
        && let Some((entity, _)) = r.key.split_once('.')
    {
        return entity.to_string();
    }
    r.key.clone()
}

/// Fixture `conflictCard`.
async fn card(
    c: &mut PgConnection,
    job: &JobRef,
    names: &mut Names,
    r: &ConflictRow,
    live: Option<&LiveDecision>,
) -> ApiResult<Value> {
    let mut m = conflict_json(&mut *c, names, r).await?;
    let draft = live.map(|d| {
        let choice = r
            .decision
            .as_ref()
            .and_then(|v| v.get("choice"))
            .and_then(Value::as_str);
        let verb = if choice == Some("rebased") {
            "Kept"
        } else {
            "Reverted"
        };
        let (id, label) = match &d.row {
            Some((id, label)) => (Some(id.clone()), label.clone()),
            None => (None, format!("{verb} {}", r.key)),
        };
        json!({"id": id, "label": label, "in_release": d.cut})
    });
    let published_in = if r.status == "resolved" {
        r.decision
            .as_ref()
            .and_then(|v| v.get("published_in"))
            .cloned()
            .unwrap_or(Value::Null)
    } else {
        Value::Null
    };
    m.insert("base_subject".to_string(), json!(base_subject(r)));
    m.insert("base_before".to_string(), json!(r.base_before));
    m.insert("base_after".to_string(), json!(r.base_after));
    m.insert(
        "mine".to_string(),
        mine(&mut *c, job.id, &r.object, &r.key).await?,
    );
    m.insert("cause".to_string(), json!(r.cause));
    m.insert(
        "impact".to_string(),
        json!(impact(&mut *c, job, &r.object, &r.key).await?),
    );
    m.insert("draft".to_string(), json!(draft));
    m.insert("published_in".to_string(), published_in);
    m.insert("edit_path".to_string(), json!(EDIT_PATH));
    Ok(Value::Object(m))
}

async fn release_conflicts(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<StatusQuery>,
) -> ApiResult<Json<Value>> {
    let all = wants_all(q.status.as_deref())?;
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let mut names = Names::default();
    let mut items = Vec::new();
    let found = rows(&mut tx, jr.id, all).await?;
    for r in &found {
        items.push(Value::Object(conflict_json(&mut tx, &mut names, r).await?));
    }
    tx.commit().await?;
    Ok(Json(paged(items, None)))
}

async fn conflict_board(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<StatusQuery>,
) -> ApiResult<Json<Value>> {
    let all = wants_all(q.status.as_deref())?;
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    // every row once: the counts are over the open ones whatever `status` filters the cards to
    let every = rows(&mut tx, jr.id, true).await?;
    let mut live: BTreeMap<i64, LiveDecision> = BTreeMap::new();
    let open = every.iter().filter(|r| r.status == "open").count();
    for r in every.iter().filter(|r| r.status == "open") {
        if let Some(d) = live_decision(&mut tx, &jr, &r.status, r.decision.as_ref()).await? {
            live.insert(r.id, d);
        }
    }
    let mut names = Names::default();
    let mut items = Vec::new();
    for r in every.iter().filter(|r| all || r.status == "open") {
        items.push(card(&mut tx, &jr, &mut names, r, live.get(&r.id)).await?);
    }
    let draft_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM draft_ledger WHERE job_id = $1")
            .bind(jr.id)
            .fetch_one(&mut *tx)
            .await?;
    let current_release: Option<String> =
        sqlx::query_scalar("SELECT current_release_id FROM jobs WHERE id = $1")
            .bind(jr.id)
            .fetch_one(&mut *tx)
            .await?;
    tx.commit().await?;
    let decided = live.len();
    Ok(Json(json!({
        "items": items,
        "open": open - decided,
        "decided_in_draft": decided,
        "blocks_publish": false,
        "current_release": current_release,
        "draft_count": draft_count,
    })))
}

#[derive(sqlx::FromRow)]
struct ResolveRow {
    object: String,
    key: String,
    status: String,
    decision: Option<Value>,
}

/// `resolveConflict` (fixture): `POST /api/jobs/{job}/refinements/{revert|rebase}` `{conflict}`, in the caller's
/// job-scoped transaction (after `ensure_job`). Returns the job's `draft_rev` after the write. Refusals:
/// - 422 `invalid_op` (`hide` / `unhide` do not resolve a conflict);
/// - 404 `conflict_not_found`;
/// - 409 `conflict_not_open` (superseded or resolved), `edit_on_journey_page` (an `activity` conflict: the Journey
///   page resolves it), `conflict_decided` (a decision still carried to Production);
/// - every refusal of `overlay::refine` for the conflict's place.
pub async fn resolve(
    c: &mut PgConnection,
    reg: &Registry,
    compiler: &dyn TemplateCompiler,
    j: &JobCtx,
    op: Op,
    id: i64,
) -> ApiResult<i64> {
    if !matches!(op, Op::Revert | Op::Rebase) {
        return Err(ApiError::Unprocessable(
            "invalid_op",
            "Resolve a conflict with revert (accept the base) or rebase (keep mine).".to_string(),
        ));
    }
    // the job lock `overlay::refine` takes, taken first so the checks below and the write see one state
    sqlx::query("SELECT 1 FROM jobs WHERE id = $1 FOR UPDATE")
        .bind(j.job_id)
        .execute(&mut *c)
        .await?;
    let row = sqlx::query_as::<_, ResolveRow>(
        "SELECT object, key, status, decision FROM release_conflict WHERE job_id = $1 AND id = $2",
    )
    .bind(j.job_id)
    .bind(id)
    .fetch_optional(&mut *c)
    .await?;
    let Some(r) = row else {
        return Err(ApiError::Missing(
            "conflict_not_found",
            format!("No conflict #{id}."),
        ));
    };
    if r.status != "open" {
        return Err(ApiError::Conflict(
            "conflict_not_open",
            format!("Conflict #{id} is {}.", r.status),
        ));
    }
    // Q-R3 (a): an activity's conflict (F-43, B2's `base_removed` / `base_changed`) is resolved on the Journey page
    overlay::edit_on_journey_page(&r.object)?;
    let jr = job_ref(j);
    if live_decision(&mut *c, &jr, &r.status, r.decision.as_ref())
        .await?
        .is_some()
    {
        return Err(ApiError::Conflict(
            "conflict_decided",
            format!("Conflict #{id} is already decided in the draft; undo that decision first."),
        ));
    }
    // J16 (D54): the choice is a draft change; the conflict records it and stays open until the release carrying it
    // is applied
    overlay::refine(c, reg, compiler, j, op, &r.object, &r.key).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_is_open_or_all() {
        assert!(!wants_all(None).unwrap());
        assert!(!wants_all(Some("open")).unwrap());
        assert!(wants_all(Some("all")).unwrap());
        assert_eq!(
            wants_all(Some("closed")).unwrap_err().code(),
            "invalid_status"
        );
    }
}
