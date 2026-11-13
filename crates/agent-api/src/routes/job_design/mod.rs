//! The job-design routes (B5): J11 ontology, J12 context spaces + procedures, J7 gates, J8 toolsets. Each submodule
//! owns its routes; this module merges them and holds what they share. The paths
//! `/api/jobs/{job}/{ontology,context-spaces,procedures,gates,toolsets}…` are static, so they win over B2's
//! `GET /api/jobs/{job}/{object}` catch-all (`routes/release/refine.rs`, plan R12).
//!
//! Bodies: an unknown or extra field, a missing field or a wrong type is 422 `invalid_body` (plan "Global
//! constraints"); a body that is not JSON at all stays B1's 400 `invalid_body` ([`ApiJson`](crate::error::ApiJson)).
use axum::Router;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::design::JobRef;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use crate::tenancy::JobCtx;

pub mod gates;
pub mod ontology;
pub mod procedures;
pub mod toolsets;

pub fn routes() -> Router<AppState> {
    Router::new()
        .merge(ontology::routes())
        .merge(procedures::routes())
        .merge(gates::routes())
        .merge(toolsets::routes())
}

/// Decode a JSON body into a B5 input type: 422 `invalid_body` when it does not fit.
pub fn body<T: DeserializeOwned>(v: Value) -> ApiResult<T> {
    serde_json::from_value(v).map_err(|e| {
        ApiError::Unprocessable(
            "invalid_body",
            format!("The request body is not valid: {e}"),
        )
    })
}

pub fn job_ref(j: &JobCtx) -> JobRef {
    JobRef {
        id: j.job_id,
        key: j.job_key.clone(),
    }
}
