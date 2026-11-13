//! `tenantUsage`: `GET /api/tenant/usage?window=7|30|90&group=job|user|project|provider` (B4 T11, crate::usage::report).
//! Any member; RLS scopes every read to the acting tenant. A bad `window` or `group` is 422 `invalid_query`.
use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::Value;

use crate::error::{ApiError, ApiQuery, ApiResult};
use crate::readmodel::tenant;
use crate::routes::tenants::plan;
use crate::state::AppState;
use crate::tenancy::Member;
use crate::usage::report::{self, Group};

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/tenant/usage", get(usage))
}

#[derive(Deserialize)]
struct UsageQuery {
    window: Option<String>,
    group: Option<String>,
}

fn invalid(what: &str) -> ApiError {
    ApiError::Unprocessable("invalid_query", what.into())
}

async fn usage(
    Member(t): Member,
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<UsageQuery>,
) -> ApiResult<Json<Value>> {
    let window = match q.window.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
        None => 30,
        Some(v) => report::parse_window(v).ok_or_else(|| invalid("window must be 7, 30 or 90."))?,
    };
    let group = match q.group.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
        None => Group::Job,
        Some(v) => Group::parse(v)
            .ok_or_else(|| invalid("group must be job, user, project or provider."))?,
    };
    let mut tx = t.begin(&st.db).await?;
    let row = tenant::read(&mut tx, t.tenant)
        .await?
        .ok_or_else(|| ApiError::Missing("tenant_not_found", format!("no tenant {}", t.tenant)))?;
    let body = report::tenant_usage(
        &mut tx,
        &row,
        plan(&st),
        window,
        group,
        chrono::Utc::now().date_naive(),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(body))
}
