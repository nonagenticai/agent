//! `GET /api/inbox[?job=<key>]` (console redesign T7): everything that needs a person, with who owns it and when it
//! is due. Any member of the tenant.
//!
//! - no `job`: the tenant items ([`home::attention`]) plus the attention of every job of the tenant
//!   ([`job_attention`]); the tenant is the one `X-Active-Tenant-Id` / the member's home resolved, so the jobs of
//!   another tenant are never listed (RLS);
//! - `job=<key>`: that job's attention plus the tenant items about that job; 404 `job_not_found` for another key.
//!
//! Order: severity (blocking, action, info), then `due_at` ascending with no due time last, then `at` descending.
//! Response: `{items: AttentionItem[], counts: {blocking, action, info}}`. The counts of one job equal the `urgent`
//! and `review` of its status (`GET /api/jobs/{job}/status`): both read [`job_attention`].
use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use futures_util::{StreamExt, stream};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::design::JobRef;
use crate::error::{ApiError, ApiQuery, ApiResult};
use crate::readmodel::agent_status::bus_up;
use crate::readmodel::jobs::JobRow;
use crate::readmodel::{home, job_attention, jobs, tenant};
use crate::state::AppState;
use crate::tenancy::{JobCtx, Member};

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/inbox", get(inbox))
}

/// How many jobs' attention is collected at once (each holds a pool connection only for its own reads).
const JOBS_AT_ONCE: usize = 4;

#[derive(Deserialize)]
struct InboxQuery {
    job: Option<String>,
}

async fn inbox(
    Member(t): Member,
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<InboxQuery>,
) -> ApiResult<Json<Value>> {
    let scope = q.job.as_deref().filter(|k| !k.is_empty());
    let lifespan = i64::try_from(st.identity.cfg.invite_email_lifespan_seconds).unwrap_or(i64::MAX);
    let mut tx = t.begin(&st.db).await?;
    let row = tenant::read(&mut tx, t.tenant)
        .await?
        .ok_or_else(|| ApiError::Missing("tenant_not_found", format!("no tenant {}", t.tenant)))?;
    let mut items: Vec<Value> = home::attention(&mut tx, &row, lifespan).await?;
    let all: Vec<JobRow> = jobs::list(&mut tx).await?;
    tx.commit().await?;

    let selected: Vec<JobRow> = match scope {
        None => all,
        Some(key) => {
            let one: Vec<JobRow> = all.into_iter().filter(|j| j.key == key).collect();
            if one.is_empty() {
                return Err(ApiError::Missing(
                    "job_not_found",
                    format!("no job {key:?}"),
                ));
            }
            items.retain(|i| i["job"] == key);
            one
        }
    };
    // one PING for the whole page; the jobs are collected a few at a time, each in its own job-scoped transaction
    // (RLS), and `buffered` keeps the jobs' order so the response does not depend on which finished first
    let up = !selected.is_empty() && bus_up(&st).await;
    // each future owns its job and its own handles, so the stream borrows nothing from this frame
    let per_job: Vec<ApiResult<Vec<Value>>> = stream::iter(selected)
        .map(|j| {
            let (st, t) = (st.clone(), t.clone());
            async move {
                let ctx = JobCtx {
                    t,
                    job_id: j.id,
                    job_key: j.key.clone(),
                };
                let job = JobRef {
                    id: j.id,
                    key: j.key,
                };
                job_attention::job_attention(&st, &ctx, &job, up).await
            }
        })
        .buffered(JOBS_AT_ONCE)
        .collect()
        .await;
    for one in per_job {
        items.extend(one?);
    }
    job_attention::rank_sort(&mut items);
    let counts = job_attention::counts(&items);
    Ok(Json(json!({"items": items, "counts": counts})))
}
