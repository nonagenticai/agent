//! Releases (B2, D62): the compiled bundle, its hashes, and the cut / apply / rollback engine over the design
//! snapshot (`crate::design`).
pub mod apply;
pub mod approve;
pub mod bundle;
pub mod compile;
pub mod conflicts;
pub mod derive;
pub mod follow;
pub mod hash;
pub mod hooks;
pub mod mint;
pub mod overlay;
pub mod readiness_rules;
pub mod rule_authoring;
pub mod rules;
pub mod rules_eval;
pub mod summary;
pub mod targets;

use axum::http::StatusCode;
use serde_json::Value;

use crate::error::ApiError;

/// The publish gate's refusal (D67): 409 `release_not_ready` whose error object carries the `blocking` readiness
/// items, i.e. `{"error": {"code": "release_not_ready", "message": …, "blocking": [ReadinessItem…]}}`.
/// The message names the release and its gaps (fixture `ReleaseError`), so each call site writes its own.
pub fn release_not_ready(message: impl Into<String>, blocking: Vec<Value>) -> ApiError {
    ApiError::detailed(StatusCode::CONFLICT, "release_not_ready", message)
        .with("blocking", Value::Array(blocking))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;
    use http_body_util::BodyExt;
    use serde_json::json;

    #[tokio::test]
    async fn release_not_ready_body_carries_blocking() {
        let item = json!({
            "code": "route_not_ready",
            "area": "routes",
            "dependency": "case",
            "slot": "read"
        });
        let msg = "cfg-2026-10-20.1 is not ready: case.read.";
        let e = release_not_ready(msg, vec![item.clone()]);
        assert_eq!(
            e.status_and_code(),
            (StatusCode::CONFLICT, "release_not_ready")
        );
        let resp = e.into_response();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let b = resp.into_body().collect().await.unwrap().to_bytes();
        let v: Value = serde_json::from_slice(&b).unwrap();
        assert_eq!(
            v,
            json!({"error": {"code": "release_not_ready", "message": msg, "blocking": [item]}})
        );
    }
}
