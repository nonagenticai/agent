//! `GET /api/search?q=` (console redesign follow-up): the ⌘K palette's case hits across the tenant's jobs.
//!
//! Any member of the tenant may read every job's cases (`JobMember` is `Member` plus "the job exists"), so the
//! tenant-wide search is `Member` too, with RLS (`app.current_tenant`) as the isolation. The match is the per-job
//! cases route's: a case-insensitive substring over the case id, customer name, title, activity or outcome.
//! A case is only unique within its job, so every hit carries the job key and the console `href`.
//! Response: `{"cases": [{job, job_name, case_id, title, customer_name, status, href}]}`, newest first, at most 10.
use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::Row;

use crate::conversations::store;
use crate::error::{ApiError, ApiQuery, ApiResult};
use crate::state::AppState;
use crate::tenancy::Member;

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/search", get(search))
}

/// Fewer characters than this match nearly everything: the answer is empty.
const MIN_CHARS: usize = 2;
const MAX_CHARS: usize = 200;
const LIMIT: i64 = 10;

#[derive(Deserialize)]
struct SearchQuery {
    q: Option<String>,
}

async fn search(
    Member(t): Member,
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<SearchQuery>,
) -> ApiResult<Json<Value>> {
    let text = q.q.as_deref().map(str::trim).unwrap_or_default();
    let n = text.chars().count();
    if n > MAX_CHARS {
        return Err(ApiError::Unprocessable(
            "invalid_query",
            "q is at most 200 characters".into(),
        ));
    }
    store::require_clean([text])?;
    if n < MIN_CHARS {
        return Ok(Json(json!({"cases": []})));
    }
    let needle = text.to_lowercase();
    let mut tx = t.begin(&st.db).await?;
    let rows = sqlx::query(
        "SELECT c.case_id, j.key AS job, j.label AS job_name, COALESCE(c.title, '') AS title, \
                c.customer_name, c.status \
           FROM agent_case c JOIN jobs j ON j.id = c.job_id AND j.tenant_id = c.tenant_id \
          WHERE c.tenant_id = $1 \
            AND (strpos(lower(c.case_id), $2) > 0 \
                 OR strpos(lower(COALESCE(c.customer_name, '')), $2) > 0 \
                 OR strpos(lower(COALESCE(c.title, '')), $2) > 0 \
                 OR strpos(lower(COALESCE(c.activity, '')), $2) > 0 \
                 OR strpos(lower(COALESCE(c.outcome, '')), $2) > 0) \
          ORDER BY c.updated_at DESC, j.key, substr(c.case_id, 4)::int8 DESC \
          LIMIT $3",
    )
    .bind(t.tenant)
    .bind(&needle)
    .bind(LIMIT)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let cases: Vec<Value> = rows
        .iter()
        .map(|r| {
            let job: String = r.get("job");
            let case_id: String = r.get("case_id");
            let customer: Option<String> = r.get("customer_name");
            json!({
                "job": job,
                "job_name": r.get::<String, _>("job_name"),
                "case_id": case_id,
                "title": r.get::<String, _>("title"),
                "customer_name": customer.filter(|c| !c.is_empty()),
                "status": r.get::<String, _>("status"),
                "href": format!("/admin/jobs/{job}/cases/{case_id}"),
            })
        })
        .collect();
    Ok(Json(json!({"cases": cases})))
}
