//! `/health` (liveness: the process answers) and `/ready` (readiness: the runtime pool reaches Postgres and the
//! platform seed applied without a conflict). Unauthenticated; they carry no data.
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router, extract::State};
use serde_json::{Value, json};

use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
}

async fn health() -> Json<Value> {
    Json(json!({"status": "ok"}))
}

/// 200 `ready`; 503 `not_ready` when the database does not answer.
async fn ready(State(st): State<AppState>) -> (StatusCode, Json<Value>) {
    let db_ok = sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(&st.db)
        .await
        .is_ok();
    let signup = if st.signup.is_some() { "on" } else { "dark" };
    let db = if db_ok { "ok" } else { "down" };
    if db_ok {
        (
            StatusCode::OK,
            Json(json!({"status": "ready", "db": db, "signup": signup})),
        )
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status": "not_ready", "db": db, "signup": signup})),
        )
    }
}
