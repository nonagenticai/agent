//! `GET /api/jobs/{job}/status` (`AgentStatus`, console redesign T6): the one status of the agent. Any job member
//! reads. The rule and the numbers are in [`crate::readmodel::agent_status`].
use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};

use crate::design::JobRef;
use crate::error::ApiResult;
use crate::readmodel::agent_status::{self, AgentStatus};
use crate::state::AppState;
use crate::tenancy::JobMember;

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/jobs/{job}/status", get(status))
}

async fn status(
    JobMember(j): JobMember,
    State(st): State<AppState>,
) -> ApiResult<Json<AgentStatus>> {
    let job = JobRef {
        id: j.job_id,
        key: j.job_key.clone(),
    };
    Ok(Json(agent_status::agent_status(&st, &j, &job).await?))
}
