//! `GET /api/platform/signup-launch` (B1 T14, super_admin, always mounted): the early-access dark-launch switch (P3,
//! K21) — whether signup is on, each prerequisite by NAME with `set|missing` (never a value), and the six routes the
//! switch mounts with their guards.
use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::error::ApiResult;
use crate::state::AppState;
use crate::tenancy::SuperAdmin;

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/platform/signup-launch", get(signup_launch))
}

/// (env name, key, label, what it does).
const ITEMS: [(&str, &str, &str, &str); 8] = [
    (
        "EARLY_ACCESS_SIGNUP_ENABLED",
        "enabled",
        "Signup switch",
        "Mounts the signup routes; off = they are the unknown-route 404",
    ),
    (
        "SIGNUP_INTAKE_AZP",
        "intake_client",
        "Intake client",
        "Keycloak client of the website / chat intake handler (default agent-signup-intake)",
    ),
    (
        "SIGNUP_DECIDER_AZP",
        "decider_client",
        "Decider client",
        "Keycloak client of chat-bridge's Telegram decision door (default chat-bridge-signup-decider)",
    ),
    (
        "TURNSTILE_SECRET",
        "turnstile",
        "Bot check",
        "Verifies a relayed Turnstile token when set",
    ),
    (
        "TELEGRAM_BOT_TOKEN",
        "telegram_bot",
        "Telegram bot",
        "Posts new requests and decisions",
    ),
    (
        "TELEGRAM_SIGNUP_CHAT_ID",
        "telegram_chat",
        "Telegram chat",
        "The forum the requests are posted to",
    ),
    (
        "TELEGRAM_SIGNUP_THREAD_ID",
        "telegram_topic",
        "Telegram topic",
        "The forum topic (never 167, the probe alarms)",
    ),
    (
        "SIGNUP_TELEGRAM_RESIDENCY",
        "telegram_residency",
        "Telegram approval residency",
        "eu or us: where a workspace approved from Telegram lives (required with Telegram)",
    ),
];

/// The routes the switch mounts.
const ROUTES: [(&str, &str, &str); 7] = [
    (
        "POST",
        "/api/signup-approvals/intake",
        "intake service account",
    ),
    ("GET", "/api/signup-approvals", "super_admin"),
    ("POST", "/api/signup-approvals/invite", "super_admin"),
    ("POST", "/api/signup-approvals/{id}/approve", "super_admin"),
    ("POST", "/api/signup-approvals/{id}/reject", "super_admin"),
    ("POST", "/api/signup-approvals/{id}/retry", "super_admin"),
    (
        "POST",
        "/api/signup-approvals/{id}/telegram-decision",
        "decider service account",
    ),
];

async fn signup_launch(
    SuperAdmin(_): SuperAdmin,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    let set = |name: &str| {
        st.cfg
            .signup_launch
            .iter()
            .any(|(k, present)| *k == name && *present)
    };
    let items: Vec<Value> = ITEMS
        .iter()
        .map(|(name, key, label, detail)| {
            // The client names have defaults: set unless overridden is still "set".
            let present = set(name) || matches!(*key, "intake_client" | "decider_client");
            json!({"key": key, "label": label, "name": name,
                   "status": if present { "set" } else { "missing" }, "detail": detail})
        })
        .collect();
    Ok(Json(json!({
        "enabled": st.signup.is_some(),
        "items": items,
        "routes": ROUTES
            .iter()
            .map(|(m, p, g)| json!({"method": m, "path": p, "guard": g}))
            .collect::<Vec<_>>(),
    })))
}
