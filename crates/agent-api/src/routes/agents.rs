//! J23 bus agents (B4 T14): `busAgents` (`GET /api/jobs/{job}/agents`, 503 `unavailable` without Redis) and
//! `agentFleet` (`GET /api/jobs/{job}/agents/fleet`, database only). Any member. The numbers are crate::bus_admin's:
//! the interaction, orchestrator and runner consumer groups, plus the R1 post-gate from its own heartbeats.
use axum::Router;
use axum::extract::State;
use axum::response::Json;
use axum::routing::get;
use serde_json::Value;

use crate::bus_admin;
use crate::design::JobRef;
use crate::error::ApiResult;
use crate::page::paged;
use crate::state::AppState;
use crate::tenancy::JobMember;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/jobs/{job}/agents", get(bus_agents))
        .route("/api/jobs/{job}/agents/fleet", get(agent_fleet))
}

/// `busAgents`.
async fn bus_agents(JobMember(j): JobMember, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(paged(bus_admin::agents(&st, &j).await?, None)))
}

/// `agentFleet`.
async fn agent_fleet(
    JobMember(j): JobMember,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    let job = JobRef {
        id: j.job_id,
        key: j.job_key.clone(),
    };
    let mut tx = j.begin(&st.db).await?;
    let v = bus_admin::fleet(&mut tx, &job).await?;
    tx.commit().await?;
    Ok(Json(v))
}
