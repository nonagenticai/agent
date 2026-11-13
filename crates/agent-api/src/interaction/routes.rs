//! The interaction routes: three wire shapes over one backend stream. Every route is `JobCaller` (a job API key,
//! crate::interaction::apikeys, or, as `JobMember`, any member of the
//! job's tenant, an observer included: a conversation with the job changes no tenant data), so the job is resolved in
//! the caller's tenant (404 `job_not_found` for another tenant's job or an unknown key).
//!
//! - `POST /api/jobs/{job}/interactions` — the native shape (crate::interaction::shapes::native).
//! - `POST /api/jobs/{job}/responses` — the OpenAI Responses shape (crate::interaction::shapes::responses).
//! - `POST /api/jobs/{job}/chat/completions` — the OpenAI chat-completions shape
//!   (crate::interaction::shapes::chat_completions).
//! - `GET /api/jobs/{job}/interactions/webhook-secret` — `integrations_manager`: the job's callback signing secret
//!   (crate::interaction::webhook); 409 `webhooks_not_configured` without `INTERACTION_WEBHOOK_KEY`.
//! - `GET /api/jobs/{job}/interactions/deliveries/{id}` — `JobCaller`: one async interaction's delivery state.
//!
//! A native body with `callback_url` is async: 202 with the object `queued`, the result POSTed to the URL later
//! (crate::interaction::webhook). 422 `stream_with_callback` with `stream: true`, 409 `webhooks_not_configured`, 422
//! `invalid_callback_url` (not public https), then the input limits, then 429 `too_many_pending_interactions`.
//!
//! With `stream: true` the answer is `text/event-stream` (keep-alive comments every 15 s). For a request with a party (D109, A4: any role,
//! an API key included) its readiness (live release with a runtime design, a serving runtime) is checked first and the opening frames go
//! out at once, before the runtime's turn. Otherwise the first backend event is
//! awaited before the stream starts, so a backend that cannot answer at all (409 `job_not_live` for a party
//! request of a job with no live release, D109; 409 `provider_not_configured`, 503
//! `interaction_backend_unavailable`, 502 `interaction_upstream_error`) is the normal error JSON; a failure after that is the shape's failure frame. No
//! database transaction is open while the backend answers.
use std::convert::Infallible;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use futures_util::stream;
use serde_json::{Value, json};
use uuid::Uuid;

use super::apikeys::JobCaller;
use super::backend::collect;
use super::model::{BackendError, DEFAULT_CHANNEL, InteractionRequest, Scope};
use super::shapes::chat_completions::{self, ChatBody, ChatEncoder};
use super::shapes::native::{self, NativeBody, NativeEncoder};
use super::shapes::responses::{self, ResponsesBody, ResponsesEncoder};
use super::shapes::{Encoder, Frame};
use super::webhook;
use crate::error::{ApiError, ApiJson, ApiPath, ApiResult};
use crate::state::AppState;
use crate::tenancy::{JobRequires, begin_scoped, role};

/// Names the channel of an OpenAI-shaped request (default `api`).
pub const CHANNEL_HEADER: &str = "x-interaction-channel";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/jobs/{job}/interactions", post(interactions))
        .route("/api/jobs/{job}/responses", post(responses))
        .route("/api/jobs/{job}/chat/completions", post(chat_completions))
        .route(
            "/api/jobs/{job}/interactions/webhook-secret",
            get(webhook_secret),
        )
        .route(
            "/api/jobs/{job}/interactions/deliveries/{id}",
            get(delivery),
        )
}

async fn interactions(
    j: JobCaller,
    State(st): State<AppState>,
    ApiJson(mut b): ApiJson<NativeBody>,
) -> ApiResult<Response> {
    let callback = b.callback_url.take();
    let (mut req, stream) = native::to_request(b, &Scope::from(&j));
    j.stamp(&mut req);
    match callback {
        Some(url) => queue(&st, req, stream, url).await,
        None => {
            let enc = NativeEncoder::new(&req);
            answer(&st, req, stream, enc).await
        }
    }
}

/// The async path: check, store the queued row (crate::interaction::webhook::enqueue; 429 past the tenant's pending
/// cap), kick the worker, answer 202 `queued`. The worker answers and delivers, also after a restart.
async fn queue(
    st: &AppState,
    req: InteractionRequest,
    stream: bool,
    url: String,
) -> ApiResult<Response> {
    if stream {
        return Err(ApiError::Unprocessable(
            "stream_with_callback",
            "stream and callback_url cannot be combined".into(),
        ));
    }
    let w = st.webhooks.clone().ok_or_else(webhooks_not_configured)?;
    webhook::check_callback_url(&url, w.allow_private)
        .await
        .map_err(|why| ApiError::Unprocessable("invalid_callback_url", why))?;
    req.validate()?;
    let enc = NativeEncoder::new(&req);
    let queued = enc.queued(&url);
    // who asked: the party (an API key's `api:<key id>`, or the body's own), the channel and the caller's thread key
    let caller = json!({
        "party": req.party.as_ref().map(|p| p.id.clone()),
        "channel": req.channel,
        "conversation": req.conversation.as_ref().map(|c| c.id.clone()),
    });
    webhook::enqueue(&st.db, &req, &caller, &url, queued["created_at"].as_str()).await?;
    w.kick.notify_one();
    Ok((StatusCode::ACCEPTED, Json(queued)).into_response())
}

/// `id, state, attempts, last_error, created_at, updated_at` of one `interaction_deliveries` row.
type DeliveryRow = (
    Uuid,
    String,
    i32,
    Option<String>,
    DateTime<Utc>,
    DateTime<Utc>,
);

/// One async interaction's delivery state (`int_<id>` or the bare uuid): `{id, state, attempts, last_error,
/// created_at, updated_at}`; 404 `delivery_not_found` for an unknown id or another job's.
async fn delivery(
    j: JobCaller,
    State(st): State<AppState>,
    ApiPath((_, id)): ApiPath<(String, String)>,
) -> ApiResult<Json<Value>> {
    let raw = id.strip_prefix("int_").unwrap_or(&id);
    let id = Uuid::parse_str(raw).map_err(|_| delivery_not_found())?;
    let mut tx = begin_scoped(&st.db, j.tenant).await?;
    let row: Option<DeliveryRow> = sqlx::query_as(
        "SELECT id, state, attempts, last_error, created_at, updated_at FROM interaction_deliveries \
          WHERE id = $1 AND job_id = $2",
    )
    .bind(id)
    .bind(j.job_id)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    let (id, state, attempts, last_error, created_at, updated_at) =
        row.ok_or_else(delivery_not_found)?;
    Ok(Json(json!({
        "id": format!("int_{id}"),
        "state": state,
        "attempts": attempts,
        "last_error": last_error,
        "created_at": created_at,
        "updated_at": updated_at,
    })))
}

fn delivery_not_found() -> ApiError {
    ApiError::Missing("delivery_not_found", "no such async interaction".into())
}

/// The job's callback signing secret (Standard Webhooks `whsec_…`), derived from `INTERACTION_WEBHOOK_KEY`.
async fn webhook_secret(
    JobRequires(j, _): JobRequires<role::IntegrationsManager>,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    let w = st.webhooks.as_ref().ok_or_else(webhooks_not_configured)?;
    let secret = w.job_secret(j.t.tenant, j.job_id);
    Ok(Json(json!({
        "secret": webhook::presented_secret(&secret),
        "scheme": webhook::SCHEME,
        "signature_header": webhook::SIGNATURE_HEADER,
    })))
}

fn webhooks_not_configured() -> ApiError {
    ApiError::Conflict(
        "webhooks_not_configured",
        "async interactions (callback_url) are not configured on this platform".into(),
    )
}

async fn responses(
    j: JobCaller,
    State(st): State<AppState>,
    headers: HeaderMap,
    ApiJson(b): ApiJson<ResponsesBody>,
) -> ApiResult<Response> {
    let mut t = responses::to_request(b, &Scope::from(&j), channel(&headers)?)?;
    j.stamp(&mut t.req);
    if let Some(previous) = t.previous {
        let cref = crate::runtime::backend::previous_conversation(&st, &t.req, previous).await?;
        t.req.conversation = Some(cref);
    }
    let enc = ResponsesEncoder::new(&t);
    answer(&st, t.req, t.stream, enc).await
}

async fn chat_completions(
    j: JobCaller,
    State(st): State<AppState>,
    headers: HeaderMap,
    ApiJson(b): ApiJson<ChatBody>,
) -> ApiResult<Response> {
    let mut t = chat_completions::to_request(b, &Scope::from(&j), channel(&headers)?)?;
    j.stamp(&mut t.req);
    let enc = ChatEncoder::new(&t);
    answer(&st, t.req, t.stream, enc).await
}

/// `X-Interaction-Channel`, else `api` (validated with the rest of the request).
fn channel(h: &HeaderMap) -> ApiResult<String> {
    match h.get(CHANNEL_HEADER) {
        None => Ok(DEFAULT_CHANNEL.into()),
        Some(v) => v.to_str().map(|s| s.trim().to_string()).map_err(|_| {
            ApiError::Unprocessable(
                "invalid_channel",
                "channel must match ^[a-z][a-z0-9_]{0,31}$".into(),
            )
        }),
    }
}

/// Validate, call the backend, answer in the shape of `enc`: the whole object, or the stream of frames.
async fn answer<E: Encoder>(
    st: &AppState,
    req: InteractionRequest,
    stream: bool,
    mut enc: E,
) -> ApiResult<Response> {
    req.validate()?;
    let runtime = super::is_runtime_caller(&req);
    if runtime && stream {
        // D109: a refusal (409 `job_not_live`, 503) is still the normal error JSON, checked before the stream
        crate::runtime::backend::runtime_ready(st, &req).await?;
    }
    let mut events = super::respond(st, req);
    if !stream {
        let r = collect(events).await?;
        return Ok(Json(enc.complete(&r)).into_response());
    }
    if runtime {
        // the runtime answers after its turn (up to its reply wait): the opening frames go out at once, keep-alive
        // comments every 15 s until the reply
        let head = enc.start();
        let frames = stream::iter(head).chain(events.flat_map(move |ev| stream::iter(enc.on(ev))));
        let sse = Sse::new(frames.map(|f| Ok::<_, Infallible>(event(f))))
            .keep_alive(KeepAlive::default());
        return Ok(sse.into_response());
    }
    let first = events.next().await.unwrap_or_else(|| {
        Err(BackendError::Upstream(
            "the stream ended before any event".into(),
        ))
    })?;
    let head = enc.start();
    let frames = stream::iter(head).chain(
        stream::once(async move { Ok(first) })
            .chain(events)
            .flat_map(move |ev| stream::iter(enc.on(ev))),
    );
    let sse =
        Sse::new(frames.map(|f| Ok::<_, Infallible>(event(f)))).keep_alive(KeepAlive::default());
    Ok(sse.into_response())
}

fn event(f: Frame) -> Event {
    let e = Event::default().data(f.data);
    match f.event {
        Some(name) => e.event(name),
        None => e,
    }
}
