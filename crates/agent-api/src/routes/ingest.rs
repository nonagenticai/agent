//! `/api/knowledge/ingest-jobs`: strata_ingest's jobs for the tenant (B3 T6; plan §5.3). Ported from
//! care-bus-agents@96c608c `routes/ingest.rs`; shapes = `web/src/lib/api/types/tenant.ts` (`IngestJob`,
//! `IngestJobDeleted`, `IngestJobDeleting`), refusals = the fixture (`web/src/lib/api/areas/tenant.ts` T8). Every
//! view is strata's job mapped by `strata::ingest::ui_view`: the store is the agent store, never the strata id, and a
//! job's `source` / `options` objects never reach the UI.
//!
//! - `GET /api/knowledge/ingest-jobs?status=&store_id=&before=` (member) → `Paged<IngestJob>`, newest first, 100 per
//!   page (strata's cursor). 422 `invalid_status`; 404 `store_not_found`; an unbound store answers an empty page.
//! - `GET /api/knowledge/ingest-jobs/{id}` (member) → `IngestJob`; another tenant's job is strata's 404 `not_found`.
//! - `POST …/{id}/retry` (knowledge_manager) → `IngestJob`. 409 `slot_not_configured` (with `slot`) when a failed
//!   step's provider slot is still empty in the working slots (a slot no row names is the deployment default, which
//!   strata resolves). A failed api / datatable job is re-run as a new source run (T8: strata keeps no credentials;
//!   409 `source_gone`, `run_in_progress`, `resubmit_refused`) and answers the new job. Otherwise strata's retry, 409
//!   `conflict` for any job but a failed one.
//! - `POST …/{id}/cancel` (knowledge_manager) → `IngestJob` (`cancelled` / `cancelling`); 409 `conflict` once ended.
//! - `DELETE …/{id}` (knowledge_manager) → 200 `IngestJobDeleted` or 202 `IngestJobDeleting`, as strata answers.
//!   409 `owned_by_active_run` (with `run_id`) while a running source run follows the job or its crawl / sitemap
//!   parent; strata's 409 `conflict`, 409 `child_of_parent_job` (with `parent_job_id`) and 502
//!   `upstream_unavailable` pass through.
use std::collections::HashMap;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::audit::{self, Event};
use crate::error::{ApiError, ApiPath, ApiQuery, ApiResult};
use crate::knowledge::runs::{self, StartReq};
use crate::knowledge::stores;
use crate::state::AppState;
use crate::strata::ingest::{self, ListQuery};
use crate::strata::{self, Plane, Service, StrataStoreId};
use crate::tenancy::{JobCtx, Member, Requires, TenantCtx, role};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/knowledge/ingest-jobs", get(list))
        .route("/api/knowledge/ingest-jobs/{id}", get(one).delete(remove))
        .route("/api/knowledge/ingest-jobs/{id}/retry", post(retry))
        .route("/api/knowledge/ingest-jobs/{id}/cancel", post(cancel))
}

/// Jobs per page (fixture `paged(rows, {limit: 100})`).
const LIST_LIMIT: i64 = 100;
/// `created_by` of a job no direct upload names (a source run's job, a job made outside agent).
const SYSTEM: &str = "system";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListParams {
    /// `''` = any status.
    #[serde(default)]
    status: Option<String>,
    /// An agent store id; `''` = every store.
    #[serde(default)]
    store_id: Option<String>,
    /// strata's `next` of the previous page.
    #[serde(default)]
    before: Option<Uuid>,
}

/// What maps strata's jobs onto agent's names: strata store → agent store, direct upload → who uploaded.
struct Names {
    stores: HashMap<StrataStoreId, Uuid>,
    uploads: HashMap<Uuid, String>,
}

impl Names {
    fn view(&self, job: &Value) -> Value {
        let store = ingest::store_id(job).and_then(|s| self.stores.get(&s).copied());
        let by = ingest::upload_id(job)
            .and_then(|u| self.uploads.get(&u))
            .map_or(SYSTEM, String::as_str);
        ingest::ui_view(job, store, by)
    }
}

async fn names(st: &AppState, t: &TenantCtx, jobs: &[Value]) -> ApiResult<Names> {
    let mut ups: Vec<Uuid> = jobs.iter().filter_map(ingest::upload_id).collect();
    ups.sort_unstable();
    ups.dedup();
    let mut tx = t.begin(&st.db).await?;
    let all = stores::all(&mut tx).await?;
    let uploads: Vec<(Uuid, String)> = if ups.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as("SELECT id, created_by FROM knowledge_upload WHERE id = ANY($1)")
            .bind(&ups)
            .fetch_all(&mut *tx)
            .await?
    };
    tx.commit().await?;
    Ok(Names {
        stores: all
            .iter()
            .filter_map(|s| s.strata().map(|sid| (sid, s.id)))
            .collect(),
        uploads: uploads.into_iter().collect(),
    })
}

async fn view_of(st: &AppState, t: &TenantCtx, job: &Value) -> ApiResult<Value> {
    let n = names(st, t, std::slice::from_ref(job)).await?;
    Ok(n.view(job))
}

/// strata_ingest's data plane for the tenant (the provisioning gate, in a transaction of its own).
async fn plane<'a>(st: &'a AppState, t: &TenantCtx) -> ApiResult<Plane<'a>> {
    let mut tx = t.begin(&st.db).await?;
    let p = strata::gate(st, &mut tx, t.tenant, Service::Ingest).await?;
    tx.commit().await?;
    Ok(p)
}

/// One `ingest_job` audit row in its own transaction (ingest jobs are tenant-level: no job key).
async fn record(st: &AppState, t: &TenantCtx, e: Event) -> ApiResult<()> {
    let mut tx = t.begin(&st.db).await?;
    audit::record(&mut tx, &t.actor(), e).await?;
    tx.commit().await?;
    Ok(())
}

/// `ingestJobs`.
async fn list(
    Member(t): Member,
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<ListParams>,
) -> ApiResult<Json<Value>> {
    let status = q.status.filter(|s| !s.is_empty());
    if let Some(s) = &status
        && !ingest::JOB_STATUSES.contains(&s.as_str())
    {
        return Err(ApiError::Unprocessable(
            "invalid_status",
            format!("status must be one of {}.", ingest::JOB_STATUSES.join(", ")),
        ));
    }
    let store = match q.store_id.as_deref().filter(|s| !s.is_empty()) {
        None => None,
        Some(raw) => {
            let id = Uuid::parse_str(raw).map_err(|_| stores::store_not_found())?;
            let mut tx = t.begin(&st.db).await?;
            let s = stores::by_id(&mut tx, id, false)
                .await?
                .ok_or_else(stores::store_not_found)?;
            tx.commit().await?;
            match s.strata() {
                Some(sid) => Some(sid),
                // not created in strata yet: it holds no job
                None => return Ok(Json(json!({"items": [], "next": null}))),
            }
        }
    };
    let p = plane(&st, &t).await?;
    let lq = ListQuery {
        store_id: store,
        status,
        limit: Some(LIST_LIMIT),
        before: q.before,
    };
    let body = ingest::list(&st.http, &p, &lq).await?;
    let jobs: Vec<Value> = body
        .get("jobs")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let n = names(&st, &t, &jobs).await?;
    let items: Vec<Value> = jobs.iter().map(|j| n.view(j)).collect();
    Ok(Json(json!({"items": items, "next": body.get("next")})))
}

/// `ingestJob`.
async fn one(
    Member(t): Member,
    State(st): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<Json<Value>> {
    let p = plane(&st, &t).await?;
    let job = ingest::get(&st.http, &p, id).await?;
    Ok(Json(view_of(&st, &t, &job).await?))
}

/// The first slot of a failed step of `job` that the working slots hold empty (`configured = false`).
async fn empty_slot(st: &AppState, t: &TenantCtx, job: &Value) -> ApiResult<Option<String>> {
    let mut slots: Vec<String> = job
        .get("steps")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|s| s.get("status").and_then(Value::as_str) == Some("failed"))
        .filter_map(|s| s.get("kind").and_then(Value::as_str))
        .filter_map(ingest::step_slot)
        .map(str::to_string)
        .collect();
    slots.sort_unstable();
    slots.dedup();
    if slots.is_empty() {
        return Ok(None);
    }
    let mut tx = t.begin(&st.db).await?;
    let empty: Option<String> = sqlx::query_scalar(
        "SELECT slot FROM provider_slot WHERE slot = ANY($1) AND NOT configured ORDER BY slot LIMIT 1",
    )
    .bind(&slots)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(empty)
}

/// `retryIngestJob`.
async fn retry(
    Requires(t, _): Requires<role::KnowledgeManager>,
    State(st): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<Json<Value>> {
    let p = plane(&st, &t).await?;
    let job = ingest::get(&st.http, &p, id).await?;
    if job.get("status").and_then(Value::as_str) == Some("failed")
        && let Some(slot) = empty_slot(&st, &t, &job).await?
    {
        return Err(ApiError::Conflict(
            "slot_not_configured",
            format!(
                "The {slot} slot is still empty; configure it on Providers → Ingest slots first, or the retry \
                 fails the same way."
            ),
        )
        .with("slot", slot));
    }
    // T8: a failed api / datatable job is re-run as a new run of its source (strata keeps no credentials)
    if job.get("status").and_then(Value::as_str) == Some("failed")
        && let Some((job_id, job_key, source)) = runs::source_of_job(&st, &t, &job).await?
    {
        return resubmit(&st, &t, id, job_id, job_key, source).await;
    }
    let job = ingest::retry(&st.http, &p, id).await?;
    let view = view_of(&st, &t, &job).await?;
    record(
        &st,
        &t,
        Event::new("ingest.retried", "ingest_job", id.to_string())
            .detail("status", view["status"].clone()),
    )
    .await?;
    Ok(Json(view))
}

/// A failed feed job re-run as a new source run (`runs::start` in the source's job, trigger `retry`): the new
/// job's view. A run strata_ingest refused outright answers 409 `resubmit_refused` with its `run_id`.
async fn resubmit(
    st: &AppState,
    t: &TenantCtx,
    id: Uuid,
    job_id: Uuid,
    job_key: String,
    source: Uuid,
) -> ApiResult<Json<Value>> {
    let ctx = JobCtx {
        t: t.clone(),
        job_id,
        job_key,
    };
    let req = StartReq {
        trigger: "retry",
        ..StartReq::default()
    };
    let started = runs::start(st, &ctx, source, req).await?;
    let run = started.run;
    let Some(new_id) = run
        .ingest_job_ids
        .first()
        .and_then(|j| Uuid::parse_str(j).ok())
    else {
        let why = run
            .error
            .as_ref()
            .and_then(|e| e["message"].as_str())
            .unwrap_or("no job was created")
            .to_string();
        return Err(ApiError::Conflict(
            "resubmit_refused",
            format!("The source run was refused: {why}"),
        )
        .with("run_id", run.id.to_string()));
    };
    let p = plane(st, t).await?;
    let new = ingest::get(&st.http, &p, new_id).await?;
    let view = view_of(st, t, &new).await?;
    record(
        st,
        t,
        Event::new("ingest.retried", "ingest_job", id.to_string())
            .detail("status", view["status"].clone())
            .detail("resubmitted_as", new_id.to_string())
            .detail("run_id", run.id.to_string()),
    )
    .await?;
    Ok(Json(view))
}

/// `cancelIngestJob`.
async fn cancel(
    Requires(t, _): Requires<role::KnowledgeManager>,
    State(st): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<Json<Value>> {
    let p = plane(&st, &t).await?;
    let job = ingest::cancel(&st.http, &p, id).await?;
    let view = view_of(&st, &t, &job).await?;
    record(
        &st,
        &t,
        Event::new("ingest.cancelled", "ingest_job", id.to_string())
            .detail("status", view["status"].clone()),
    )
    .await?;
    Ok(Json(view))
}

/// The running source run (T7 `source_run`) that follows job `id` or its crawl / sitemap `parent`. A finished run is
/// never changed by a delete (its outcome is stored), so it does not count.
async fn active_run(
    st: &AppState,
    t: &TenantCtx,
    id: Uuid,
    parent: Option<Uuid>,
) -> ApiResult<Option<Uuid>> {
    let ids: Vec<String> = std::iter::once(id)
        .chain(parent)
        .map(|u| u.to_string())
        .collect();
    let mut tx = t.begin(&st.db).await?;
    let run: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM source_run WHERE status = 'running' AND ingest_job_ids::text[] && $1::text[] \
          ORDER BY id LIMIT 1",
    )
    .bind(&ids)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(run)
}

/// `deleteIngestJob`: 200 (gone) or 202 (strata finishes it; the client polls `ingestJob` until 404).
async fn remove(
    Requires(t, _): Requires<role::KnowledgeManager>,
    State(st): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<Response> {
    let p = plane(&st, &t).await?;
    let job = ingest::get(&st.http, &p, id).await?;
    if let Some(run) = active_run(&st, &t, id, ingest::parent_job_id(&job)).await? {
        return Err(ApiError::Conflict(
            "owned_by_active_run",
            format!(
                "This job belongs to source run {run}, which is still running. Delete it once that run has \
                 finished."
            ),
        )
        .with("run_id", run.to_string()));
    }
    let r = ingest::delete(&st.http, &p, id).await?;
    let accepted = r.status == StatusCode::ACCEPTED;
    record(
        &st,
        &t,
        Event::new("ingest.deleted", "ingest_job", id.to_string())
            .detail("status", r.body["status"].clone())
            .detail("accepted", accepted)
            .detail(
                "documents_retained",
                r.body["document_ids"].as_array().map_or(0, Vec::len),
            ),
    )
    .await?;
    Ok((r.status, Json(r.body)).into_response())
}
