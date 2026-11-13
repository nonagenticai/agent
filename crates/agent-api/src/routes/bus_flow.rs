//! J24 message path (B4 T14): `busFlow` (`GET /api/jobs/{job}/bus-flow?env=prod|test`, any member). `env` defaults to
//! `prod`; anything else is 422 `invalid_query`; without Redis 503 `unavailable`. Built by crate::bus_admin::flow.
use axum::Router;
use axum::extract::State;
use axum::response::Json;
use axum::routing::get;
use serde::Deserialize;
use serde_json::Value;

use crate::bus_admin;
use crate::error::{ApiQuery, ApiResult};
use crate::state::AppState;
use crate::tenancy::JobMember;

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/jobs/{job}/bus-flow", get(bus_flow))
}

#[derive(Deserialize)]
struct FlowQuery {
    env: Option<String>,
}

/// `busFlow`.
async fn bus_flow(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<FlowQuery>,
) -> ApiResult<Json<Value>> {
    let env = bus_admin::env_param(q.env.as_deref())?;
    let root = bus_admin::root(&st)?;
    Ok(Json(bus_admin::flow(root, &j, env).await?))
}
