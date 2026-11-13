//! The customer side of a web chat conversation's later messages (B7): a staff reply, a handover notice or a CSAT
//! ask the edge accepted (`POST /internal/deliver`) is published here, on the job's fan-out (crate::sse_hub: Redis
//! Pub/Sub when `REDIS_URL` is set, so every agent-api replica sees it; else this process's hub), and agent-edge's
//! customer stream reads it back through
//!
//! ```text
//! GET /api/internal/channel/webchat/conversations/{conversation_id}/events?tenant=&job_id=&binding_id=   (EdgeKey)
//!     SSE `message` {id, kind, text, name?} (SSE id = `id`), opened by an `open` comment, keep-alive 15 s
//! ```
//!
//! so any edge replica serves any customer: the edge holds no fan-out of its own (and has no Redis). The binding is
//! checked like the history read (an enabled, verified web chat customer binding of that tenant and job) and the
//! conversation must have arrived on it, else 404 `binding_not_found` / `conversation_not_found`. Frames are lossy
//! like every Pub/Sub frame: the message is stored first and the widget re-reads the history on every (re)connect.
//! The stream ends when the Redis link drops or the subscriber lags; the edge then ends the customer's stream and
//! the browser reconnects. These frames (event `webchat`) never reach the staff feeds. Text is customer data: never
//! logged. Only what the edge accepted is published here, so a staff note (SW T3, never delivered) never is; the
//! staff feeds' `message` frames (notes included) are not `webchat` frames and never reach this stream.
//!
//! Bounds: at most [`MAX_STREAMS`] streams per process ([`STREAM_SLOTS`]; a full process answers 503 `unavailable`
//! with `Retry-After`), and a stream holds no database connection: the binding and conversation are checked once,
//! up front, and the connection goes back to the pool before the first frame. The edge caps streams per
//! conversation (crate::channels::webchat).
use std::convert::Infallible;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::HeaderValue;
use axum::http::header::CACHE_CONTROL;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::{StreamExt, stream};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Semaphore;
use uuid::Uuid;

use super::edge_link::DeliverKind;
use super::internal::{self, EdgeKey};
use super::store;
use crate::attachments::AttachmentRef;
use crate::channels::connector::{Outbound, outbound_text};
use crate::channels::webchat::CHANNEL;
use crate::error::{ApiError, ApiPath, ApiQuery, ApiResult};
use crate::interaction::{ConversationRef, Scope};
use crate::runtime::{Env, Scope as BusScope};
use crate::sse_hub;
use crate::state::AppState;

/// The hub event of a web chat delivery (in crate::sse_hub::CHAT_EVENTS; kept off the staff feeds).
pub const EVENT: &str = "webchat";
/// The hub event of the sign-in prompt (B4 visitor gate): the customer stream sends it as SSE `sign_in`
/// `{"url": <sign_in_url>|null}`. Kept off the staff feeds like [`EVENT`].
pub const SIGN_IN_EVENT: &str = "webchat_sign_in";
const KEEPALIVE: Duration = Duration::from_secs(15);
/// Open web chat streams per agent-api process.
pub const MAX_STREAMS: usize = 1000;
/// The `Retry-After` of a full process.
const FULL_RETRY_S: u64 = 5;
/// One permit per open stream, held by the stream until it ends.
pub static STREAM_SLOTS: Semaphore = Semaphore::const_new(MAX_STREAMS);

pub fn routes() -> Router<AppState> {
    Router::new().route(
        "/api/internal/channel/webchat/conversations/{conversation_id}/events",
        get(events),
    )
}

/// The customer-facing kind of a delivery, as the widget reads it.
fn wire_kind(kind: DeliverKind) -> &'static str {
    match kind {
        DeliverKind::Text => "text",
        DeliverKind::HandoverNotice => "handover_notice",
        DeliverKind::CsatRequest => "csat_request",
    }
}

/// After the edge accepted `kind`/`text` for `cref` (its `external_id`, the event id): publish it to the
/// conversation's customer stream when `cref` is a web chat. A no-op for any other channel. The text is what the
/// edge's connectors send for the same message (crate::channels::connector::outbound_text), so a handover notice
/// reads the same on every channel. `name` is a staff reply's customer-facing author name
/// (crate::conversations::staff::customer_facing_name); a blank one is dropped.
#[allow(clippy::too_many_arguments)]
pub async fn published(
    st: &AppState,
    s: &Scope,
    conversation_id: &str,
    cref: &ConversationRef,
    kind: DeliverKind,
    text: &str,
    name: Option<&str>,
    external_id: &str,
) {
    published_with_attachments(
        st,
        s,
        conversation_id,
        cref,
        kind,
        text,
        name,
        external_id,
        &[],
    )
    .await;
}

/// [`published`] for a staff reply that carries files (SW-9): the frame gains `attachments` (refs only; the widget
/// fetches the bytes through the edge). A reply of only files (no text) is still published.
#[allow(clippy::too_many_arguments)]
pub async fn published_with_attachments(
    st: &AppState,
    s: &Scope,
    conversation_id: &str,
    cref: &ConversationRef,
    kind: DeliverKind,
    text: &str,
    name: Option<&str>,
    external_id: &str,
    attachments: &[AttachmentRef],
) {
    if cref.channel != CHANNEL {
        return;
    }
    let msg = match kind {
        DeliverKind::Text => Outbound::Text(text.to_string()),
        DeliverKind::HandoverNotice => Outbound::HandoverNotice { eta_minutes: None },
        DeliverKind::CsatRequest => Outbound::CsatRequest {
            text: text.to_string(),
        },
    };
    let text = outbound_text(&msg).unwrap_or_default();
    if text.is_empty() && attachments.is_empty() {
        return;
    }
    let id = if external_id.is_empty() {
        format!("m_{}", Uuid::now_v7().simple())
    } else {
        external_id.to_string()
    };
    let scope = match BusScope::new(s.tenant, s.job_key.clone(), Env::Prod) {
        Ok(scope) => scope,
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "webchat_stream_scope_invalid");
            return;
        }
    };
    let mut data = json!({"id": id, "kind": wire_kind(kind), "text": text});
    if let Some(n) = name.map(str::trim).filter(|n| !n.is_empty()) {
        data["name"] = json!(n);
    }
    if !attachments.is_empty() {
        data["attachments"] = json!(attachments);
    }
    sse_hub::publish(st, &scope, s.job_id, conversation_id, EVENT, data).await;
}

/// The binding's `settings.identity.sign_in_url` when it is a plain http(s) URL, else `None`.
pub fn clean_sign_in_url(settings: &serde_json::Value) -> Option<String> {
    let u = settings.pointer("/identity/sign_in_url")?.as_str()?.trim();
    let ok = u.len() <= 2048
        && (u.starts_with("https://") || u.starts_with("http://"))
        && !u.chars().any(|c| c.is_control() || c.is_whitespace());
    ok.then(|| u.to_string())
}

#[derive(Deserialize)]
struct StreamQuery {
    tenant: Uuid,
    job_id: Uuid,
    binding_id: Uuid,
}

/// The conversation arrived on the query's binding (as the history read checks). Its own transaction, committed
/// before it returns: no stream holds a database connection.
async fn on_binding(st: &AppState, q: &StreamQuery, conversation_id: &str) -> ApiResult<bool> {
    let mut tx = store::begin_job(&st.db, q.tenant, q.job_id).await?;
    let found: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM channel_conversation_refs \
                         WHERE conversation_id = $1 AND side = 'customer' AND route->>'binding_id' = $2)",
    )
    .bind(conversation_id)
    .bind(q.binding_id.to_string())
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(found)
}

/// `GET /api/internal/channel/webchat/conversations/{conversation_id}/events` (see the module doc).
async fn events(
    _k: EdgeKey,
    State(st): State<AppState>,
    ApiPath(conversation_id): ApiPath<String>,
    ApiQuery(q): ApiQuery<StreamQuery>,
) -> ApiResult<Response> {
    // first, so a full process costs no database round trip
    let Ok(slot) = STREAM_SLOTS.try_acquire() else {
        return Err(ApiError::Unavailable(
            "unavailable",
            "Too many open web chat streams; try again shortly.".into(),
            Some(FULL_RETRY_S),
        ));
    };
    let job_key = internal::checked_binding(
        &st,
        q.binding_id,
        CHANNEL,
        "customer",
        q.tenant,
        q.job_id,
        None,
    )
    .await?;
    if !on_binding(&st, &q, &conversation_id).await? {
        return Err(ApiError::Missing(
            "conversation_not_found",
            "No such conversation on this binding.".into(),
        ));
    }
    // the hub first, then the bridge: nothing published meanwhile is missed
    let rx = st.events.subscribe();
    let up = sse_hub::upstream_for(&st, q.tenant, q.job_id, &job_key).await;
    let frames = sse_hub::frames(
        rx,
        up,
        q.tenant,
        q.job_id,
        Some(conversation_id),
        Vec::new(),
    )
    .filter(|f| std::future::ready(f.event == EVENT || f.event == SIGN_IN_EVENT));
    let open = stream::once(std::future::ready(Event::default().comment("open")));
    // the slot is the stream's: it is freed when the stream ends or the edge goes away
    let events = open.chain(frames.map(move |f| {
        let _slot = &slot;
        if f.event == SIGN_IN_EVENT {
            return Event::default()
                .event("sign_in")
                .data(json!({"url": f.data["url"]}).to_string());
        }
        let id = f.data["id"]
            .as_str()
            .filter(|i| !i.chars().any(char::is_control))
            .unwrap_or_default()
            .to_string();
        Event::default()
            .event("message")
            .id(id)
            .data(f.data.to_string())
    }));
    let mut r = Sse::new(events.map(Ok::<_, Infallible>))
        .keep_alive(KeepAlive::new().interval(KEEPALIVE))
        .into_response();
    r.headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_kinds_are_the_widgets() {
        assert_eq!(wire_kind(DeliverKind::Text), "text");
        assert_eq!(wire_kind(DeliverKind::HandoverNotice), "handover_notice");
        assert_eq!(wire_kind(DeliverKind::CsatRequest), "csat_request");
        assert!(sse_hub::CHAT_EVENTS.contains(&EVENT));
        assert!(sse_hub::CHAT_EVENTS.contains(&SIGN_IN_EVENT));
    }

    #[test]
    fn sign_in_url_is_a_plain_http_url_or_none() {
        let of = |u: serde_json::Value| clean_sign_in_url(&json!({"identity": {"sign_in_url": u}}));
        assert_eq!(
            of(json!("https://shop.example/login")).as_deref(),
            Some("https://shop.example/login")
        );
        assert_eq!(of(json!("javascript:alert(1)")), None);
        assert_eq!(of(json!("https://a.example/x y")), None);
        assert_eq!(of(json!(5)), None);
        assert_eq!(clean_sign_in_url(&json!({})), None);
    }
}
