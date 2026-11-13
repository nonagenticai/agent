//! `POST /api/jobs/{job}/conversations/{id}/presence` `{state: "viewing" | "typing"}` (SW T7): a staff member tells
//! colleagues they are looking at or typing in a conversation. 204; publishes a `presence` frame
//! `{conversation_id, job, sub, name, state}` on the job's feeds (the tenant stream carries it, crate::support::events).
//! A `typing` frame is published without `name` (no lookup per keystroke burst); the tenant stream fills it in.
//! Presence itself is not stored: the web expires an entry after its last frame.
//!
//! `viewing` also marks the conversation read for the caller: Redis key `support:seen:{sub}:{job}:{conversation}`
//! (under the environment prefix, `AGENT_PREFIX`) = now (unix ms), 7-day TTL; the inbox's `unread` compares it with
//! the last customer message (crate::support::inbox). Without Redis nothing is marked (and nothing reads unread).
//!
//! Who: any of the staff roles (crate::support::require_worker, checked before the body is used: 403 `forbidden`,
//! `missing_role: support_agent`). 404 `conversation_not_found`; 422 `invalid_presence` for another state.
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::conversations::{now_ms, staff, store};
use crate::error::{ApiError, ApiJson, ApiPath, ApiResult};
use crate::interaction::Scope;
use crate::runtime::Bus;
use crate::state::AppState;
use crate::tenancy::JobMember;

/// How long a `viewing` mark is kept.
pub const SEEN_TTL_SECS: u64 = 7 * 24 * 60 * 60;
/// The most the mark's SET waits on Redis.
const SET_TIMEOUT: Duration = Duration::from_secs(2);

/// The seen mark of (`sub`, job `job_key`, `conversation_id`) under `root`'s environment prefix.
pub fn seen_key(root: &Bus, sub: &str, job_key: &str, conversation_id: &str) -> String {
    root.root_key(&format!("support:seen:{sub}:{job_key}:{conversation_id}"))
}

/// Mark the conversation seen by `sub` now. Best effort: a Redis failure is logged (ids only).
pub async fn mark_seen(st: &AppState, sub: &str, job_key: &str, conversation_id: &str) {
    let Some(root) = st.redis.as_ref() else {
        return;
    };
    let mut c = root.conn();
    let mut cmd = redis::cmd("SET");
    cmd.arg(seen_key(root, sub, job_key, conversation_id))
        .arg(now_ms())
        .arg("EX")
        .arg(SEEN_TTL_SECS);
    match tokio::time::timeout(SET_TIMEOUT, cmd.query_async::<()>(&mut c)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!(
            conversation = conversation_id,
            error = %format!("{e:#}"),
            "support_seen_not_marked"
        ),
        Err(_) => tracing::warn!(
            conversation = conversation_id,
            "support_seen_mark_timed_out"
        ),
    }
}

/// `POST /api/jobs/{job}/conversations/{id}/presence` (see the module doc).
pub async fn conversation_presence(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiPath((_, id)): ApiPath<(String, String)>,
    ApiJson(raw): ApiJson<Value>,
) -> ApiResult<StatusCode> {
    super::require_worker(&j.t)?;
    let state: &'static str = match raw.get("state").and_then(Value::as_str) {
        Some("viewing") => "viewing",
        Some("typing") => "typing",
        _ => {
            return Err(ApiError::Unprocessable(
                "invalid_presence",
                "state is viewing or typing.".into(),
            ));
        }
    };
    let sub = j.t.caller.sub.clone();
    let mut tx = j.begin(&st.db).await?;
    store::conversation_or_404(&mut tx, &id).await?;
    // `typing` repeats while the member types: no name lookup (the tenant stream names it, crate::support::events)
    let name = if state == "viewing" {
        Some(store::member_name(&mut tx, &sub).await?)
    } else {
        None
    };
    tx.commit().await?;
    if state == "viewing" {
        mark_seen(&st, &sub, &j.job_key, &id).await;
    }
    let mut data = json!({
        "conversation_id": id, "job": j.job_key, "sub": sub, "state": state,
    });
    if let Some(n) = name {
        data["name"] = json!(n);
    }
    staff::publish(&st, &Scope::from(&j), &id, "presence", data).await;
    Ok(StatusCode::NO_CONTENT)
}
