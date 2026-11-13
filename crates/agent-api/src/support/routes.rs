//! The staff workspace's own routes (SW T7). The owner commands, Send and the transcript stay with the conversation
//! routes (crate::conversations::routes).
use axum::Router;
use axum::routing::{get, post};

use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/support/inbox", get(super::inbox::support_inbox))
        .route("/api/support/events", get(super::events::support_events))
        .route(
            "/api/jobs/{job}/conversations/{id}/presence",
            post(super::presence::conversation_presence),
        )
        .route(
            "/api/support/routing",
            get(super::routing::get_routing).put(super::routing::put_routing),
        )
        .route(
            "/api/jobs/{job}/staff",
            get(super::job_staff::get_staff).put(super::job_staff::put_staff),
        )
        .route("/api/support/csat", get(super::csat_report::get_csat))
        .route(
            "/api/jobs/{job}/canned-replies",
            get(super::canned::canned_replies).post(super::canned::create_canned_reply),
        )
        .route(
            "/api/jobs/{job}/canned-replies/{id}",
            axum::routing::delete(super::canned::delete_canned_reply),
        )
}
