//! The tenant admin's overview and home (B1 T15): `GET /api/tenant/overview` (`TenantOverview`: the tenant, its licence
//! verdict, the Integrations panel, the data-plane blocks, jobs and member counts) and `GET /api/tenant/home`
//! (`TenantHome`: attention, job lanes, the agents' last 7 days). Any member.
use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::error::{ApiError, ApiResult};
use crate::readmodel::{agent_status, home, jobs, tenant};
use crate::routes::tenants::plan;
use crate::state::AppState;
use crate::tenancy::{Member, TenantCtx};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/tenant/overview", get(overview))
        .route("/api/tenant/home", get(home_page))
}

/// The acting tenant's row (it exists: the request resolved it).
async fn row(conn: &mut sqlx::PgConnection, t: &TenantCtx) -> ApiResult<tenant::TenantRow> {
    tenant::read(conn, t.tenant)
        .await?
        .ok_or_else(|| ApiError::Missing("tenant_not_found", format!("no tenant {}", t.tenant)))
}

async fn overview(Member(t): Member, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let on_bus = agent_status::bus_up(&st).await;
    let mut tx = t.begin(&st.db).await?;
    let r = row(&mut tx, &t).await?;
    let sub = (!t.caller.super_admin).then_some(t.caller.sub.as_str());
    let (_, verdict, _) = tenant::licence(&mut tx, &r, plan(&st), sub).await?;
    let (integrations, blocks) = home::integrations(&mut tx).await?;
    let jobs = jobs::summaries(&mut tx, on_bus).await?;
    let members = home::member_counts(&mut tx).await?;
    tx.commit().await?;
    Ok(Json(json!({
        "tenant": tenant::tenant_json(&r, plan(&st)),
        "licence": verdict,
        "integrations": integrations,
        "blocks": blocks,
        "jobs": jobs,
        "members": members,
    })))
}

async fn home_page(Member(t): Member, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let lifespan = i64::try_from(st.identity.cfg.invite_email_lifespan_seconds).unwrap_or(i64::MAX);
    let on_bus = agent_status::bus_up(&st).await;
    let mut tx = t.begin(&st.db).await?;
    let r = row(&mut tx, &t).await?;
    let attention = home::attention(&mut tx, &r, lifespan).await?;
    let pulses = home::pulses(&mut tx, &st, on_bus).await?;
    let activity = home::activity(&mut tx, 7).await?;
    tx.commit().await?;
    Ok(Json(json!({
        "attention": attention,
        "jobs": pulses,
        "activity": activity,
    })))
}
