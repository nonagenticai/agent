//! The `agent-edge` pipeline and router (public, `chat.nonagentic.ai`; no admin route, no Keycloak). Every provider
//! webhook takes one path: `authenticate → ingest → ack`, and in the background `dedupe → seam → deliver`:
//!
//! ```text
//! POST|GET /api/channels/{kind}/webhook ─► ChannelConnector::authenticate ─► ::ingest
//!    Reply(r)                → r (handshake / ignored event)
//!    (worker connector of the kind, first) Staff{ack, staff} in a worker-bound channel → ack now; per message spawn:
//!                              staff email → agent-api `staff` → a fixed confirmation in the thread
//!    Messages{ack, inbound} → ack now; per new message spawn: Typing → InteractionRequest (channel = kind, party, ConversationRef,
//!                              recent turns) → InteractionBackend::respond → collect → ::deliver(Text)
//! ```
//!
//! The answer goes out through `deliver` rather than the HTTP response, because providers time out in seconds and because a later message (a staff reply after a handover, D104) takes the same way out.
//!
//! B12: with `EDGE_CORE_URL` + `EDGE_INTERNAL_KEY` set ([`EdgeState::core`]) the background step is one
//! `POST /api/internal/channel/inbound` to agent-api instead of the local backend + memory (agent-api stores the
//! conversation, dedupes on the provider message id and decides answer / handover / CSAT). B7 R6: a connector with
//! [`ChannelConnector::persist_before_ack`] (Telegram) has the message
//! stored first — `inbound` with `accept_only`, awaited before the ack within the connector's
//! [`ChannelConnector::accept_timeout`]; core cannot store it → 503, the provider retries — and only the answer (a second `inbound` call) runs in the background. Web chat
//! stores synchronously on its own route. agent-api reaches the
//! customer later through the key-gated `POST /internal/deliver` (web chat: accepted here, then streamed from
//! agent-api, crate::channels::webchat) and the worker side through
//! `POST /internal/notify-workers` (crate::channels::core). BYO webhooks arrive on
//! `/api/channels/{kind}/webhook/{binding_id}` with their binding preselected.
use std::collections::{BTreeMap, HashMap};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use serde_json::{Value, json};
use uuid::Uuid;

use super::attachments_wire::{InboundUploadMeta, UploadOutcome, upload_inbound};
use super::bindings::{Binding, BindingStore};
use super::connector::{
    Blob, ChannelConnector, ChannelError, ChannelKind, ChannelStatus, ConversationRef, Inbound,
    InboundMedia, Ingress, IngressRequest, Outbound, Side, StaffInbound, outbound_text,
    refusal_notice,
};
use super::core::{
    CoreError, CoreLink, DeliverKind, DeliverReq, DeliverResp, InboundReq, InboundResp,
    NoticeDeliveredReq, NotifyResult, NotifyWorkersReq, NotifyWorkersResp, SeamError,
    StaffIdentity, StaffReq, StaffResp, WireParty, WorkerNoteReq,
};
use super::memory::{ConversationMemory, Dedupe};
use super::webchat::{WebChat, new_event_id};
use super::webhook_health::WebhookHealth;
use crate::interaction::model::{BackendError, InteractionRequest, Message, Party, Role};
use crate::interaction::{InteractionBackend, collect};

/// A webhook body above this is 413 before any parsing (provider webhooks are well under it).
pub const MAX_WEBHOOK_BYTES: usize = 256 * 1024;

/// Fixed customer-facing texts. Never echo provider errors to a customer.
pub const NOT_CONNECTED: &str =
    "This workspace is not connected to an agent yet. Ask your administrator to connect it.";
pub const NOT_AVAILABLE: &str = "The agent is not available for this workspace yet.";
pub const TRY_LATER: &str = "Sorry, I could not answer just now. Please try again in a moment.";
/// A per-binding connector with the fingerprint of the binding it was built from.
type BindingConnector = (u64, Arc<dyn ChannelConnector>);

/// The connectors this edge serves, by kind and side (only configured ones are registered).
#[derive(Default)]
pub struct Registry {
    by_kind: BTreeMap<(ChannelKind, Side), Arc<dyn ChannelConnector>>,
    /// Per-binding connectors (B12 §5: a BYO credential, per-binding settings), keyed by binding id with a
    /// fingerprint of what they were built from; rebuilt when the binding changes.
    per_binding: Mutex<HashMap<Uuid, BindingConnector>>,
}

impl Registry {
    pub fn register(&mut self, c: Arc<dyn ChannelConnector>) {
        self.by_kind.insert((c.kind(), c.side()), c);
    }

    /// The customer-side connector of a kind (webhooks, deliveries to customers).
    pub fn get(&self, kind: ChannelKind) -> Option<Arc<dyn ChannelConnector>> {
        self.get_side(kind, Side::Customer)
    }

    pub fn get_side(&self, kind: ChannelKind, side: Side) -> Option<Arc<dyn ChannelConnector>> {
        self.by_kind.get(&(kind, side)).cloned()
    }

    pub fn statuses(&self) -> Vec<ChannelStatus> {
        self.by_kind.values().map(|c| c.status()).collect()
    }

    /// `c` for `binding` (B12 §5): the connector [`ChannelConnector::for_binding`] builds from the binding's BYO
    /// credential and settings (cached), else `c` itself (no binding, an env binding, another kind, or nothing the
    /// connector reads). Ingest of a BYO webhook, the answer after it and `/internal/deliver` all go through here.
    pub fn for_binding(
        &self,
        c: Arc<dyn ChannelConnector>,
        binding: Option<&Binding>,
    ) -> Arc<dyn ChannelConnector> {
        let Some((b, id)) = binding.and_then(|b| b.id.map(|id| (b, id))) else {
            return c;
        };
        if b.kind != c.kind() {
            return c;
        }
        let mut h = DefaultHasher::new();
        b.credential.hash(&mut h);
        b.settings.to_string().hash(&mut h);
        (c.side() == Side::Customer).hash(&mut h);
        let fpr = h.finish();
        let mut cache = self
            .per_binding
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some((f, d)) = cache.get(&id)
            && *f == fpr
        {
            return d.clone();
        }
        match c.for_binding(b) {
            Some(d) => {
                cache.insert(id, (fpr, d.clone()));
                d
            }
            None => {
                cache.remove(&id);
                c
            }
        }
    }

    /// Drop the cached connectors of bindings `live` no longer has (deleted or disabled since). Cheap unless the
    /// cache holds more entries than `live` has core bindings, so the cache never outgrows the live set by more than
    /// the one entry a swap can leave behind.
    pub fn evict_stale(&self, live: &BindingStore) {
        let mut cache = self
            .per_binding
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if cache.len() > live.core_len() {
            cache.retain(|id, _| live.by_id(*id).is_some());
        }
    }
}

/// [`Registry::for_binding`] after evicting the cached connectors of bindings that are gone.
fn connector_for(
    st: &EdgeState,
    c: Arc<dyn ChannelConnector>,
    binding: Option<&Binding>,
) -> Arc<dyn ChannelConnector> {
    st.registry.evict_stale(&st.bindings);
    st.registry.for_binding(c, binding)
}

#[derive(Clone)]
pub struct EdgeState {
    pub registry: Arc<Registry>,
    pub backend: Arc<dyn InteractionBackend>,
    pub memory: Arc<ConversationMemory>,
    pub dedupe: Arc<Dedupe>,
    /// How long one answer may take before the customer gets [`TRY_LATER`].
    pub answer_deadline: Duration,
    /// The phase-A web chat (`/api/webchat/*`, `/webchat/widget.js`); `None` = those routes are 404.
    pub webchat: Option<Arc<WebChat>>,
    /// Env bindings ∪ core bindings (B12).
    pub bindings: Arc<BindingStore>,
    /// agent-api's internal seam (B12); `None` = phase A (local backend + memory, `/internal/*` 503).
    pub core: Option<Arc<CoreLink>>,
    /// Telegram webhook health as observed here (B7 T15b): updates received and send outcomes per binding, reported
    /// to core by crate::channels::webhook_health::spawn_reporter.
    pub health: Arc<WebhookHealth>,
}

impl EdgeState {
    pub fn new(registry: Registry, backend: Arc<dyn InteractionBackend>) -> Self {
        Self {
            registry: Arc::new(registry),
            backend,
            memory: Arc::new(ConversationMemory::default()),
            dedupe: Arc::new(Dedupe::default()),
            answer_deadline: Duration::from_secs(120),
            webchat: None,
            bindings: Arc::new(BindingStore::default()),
            core: None,
            health: Arc::new(WebhookHealth::default()),
        }
    }
}

/// The edge's whole surface. Test `edge_serves_no_admin_route` pins that nothing else is mounted.
pub fn router(st: EdgeState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/api/channels/{kind}/webhook", any(webhook))
        .route(
            "/api/channels/{kind}/webhook/{binding_id}",
            any(byo_webhook),
        )
        .route("/internal/deliver", post(deliver))
        .route("/internal/notify-workers", post(notify_workers))
        .route("/internal/worker-note", post(worker_note))
        .merge(super::webchat::routes())
        .with_state(st)
}

async fn health() -> axum::Json<Value> {
    axum::Json(json!({"status": "ok"}))
}

/// Always ready once listening (no database in phase A); lists the configured kinds and binding counts, never a
/// secret or a binding's ids.
async fn ready(State(st): State<EdgeState>) -> axum::Json<Value> {
    let channels: Vec<Value> = st
        .registry
        .statuses()
        .into_iter()
        .map(|s| json!({"kind": s.kind.as_str(), "side": format!("{:?}", s.side).to_lowercase(), "bindings": s.bindings}))
        .collect();
    axum::Json(
        json!({"status": "ready", "channels": channels, "web_chat": st.webchat.is_some(),
        "core": st.core.is_some(), "bindings": st.bindings.len()}),
    )
}

async fn webhook(
    State(st): State<EdgeState>,
    Path(kind): Path<String>,
    method: Method,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    body: Bytes,
) -> Response {
    run_webhook(st, kind, None, method, headers, query, body).await
}

/// A BYO bot / app's webhook: the binding is named in the path (an unknown, disabled or other-kind id is 404).
async fn byo_webhook(
    State(st): State<EdgeState>,
    Path((kind, binding_id)): Path<(String, String)>,
    method: Method,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    body: Bytes,
) -> Response {
    // Frappe Helpdesk is worker-side only, with its own signed payloads (crate::channels::helpdesk, B7 R9)
    if kind == ChannelKind::Helpdesk.as_str() {
        return super::helpdesk::webhook(st, &binding_id, method, headers, body).await;
    }
    let binding = Uuid::parse_str(&binding_id)
        .ok()
        .and_then(|id| st.bindings.by_id(id))
        .filter(|b| b.kind.as_str() == kind && b.side == Side::Customer);
    let Some(binding) = binding else {
        return not_configured();
    };
    run_webhook(st, kind, Some(binding), method, headers, query, body).await
}

async fn run_webhook(
    st: EdgeState,
    kind: String,
    binding: Option<Binding>,
    method: Method,
    headers: HeaderMap,
    query: Option<String>,
    body: Bytes,
) -> Response {
    let Some(kind) = ChannelKind::parse(&kind) else {
        return not_configured();
    };
    let Some(connector) = st.registry.get(kind) else {
        return not_configured();
    };
    // a BYO bot / app verifies with (and answers through) its own credential
    let connector = connector_for(&st, connector, binding.as_ref());
    if body.len() > MAX_WEBHOOK_BYTES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            axum::Json(json!({"error": {"code": "payload_too_large"}})),
        )
            .into_response();
    }
    let req = IngressRequest {
        method,
        headers,
        query,
        body,
        binding,
    };
    let r = handle(&st, connector, &req).await;
    // B7 T15b webhook health: an update that passed authentication (acked, or 503 because core could not store it)
    if kind == ChannelKind::Telegram
        && let Some(id) = req.binding.as_ref().and_then(|b| b.id)
        && matches!(r, Ok(_) | Err(ChannelError::Unavailable(_)))
    {
        st.health.update_received(id);
    }
    match r {
        Ok(r) => r,
        Err(e) => {
            tracing::info!(channel = kind.as_str(), error = %e, "channel_webhook_refused");
            metrics::counter!("agent_edge_webhook_refused_total", "channel" => kind.as_str())
                .increment(1);
            e.into_response()
        }
    }
}

fn not_configured() -> Response {
    (
        StatusCode::NOT_FOUND,
        axum::Json(json!({"error": {"code": "channel_not_configured"}})),
    )
        .into_response()
}

/// `authenticate → ingest`, then the ack; a message is answered in the background. With
/// [`ChannelConnector::persist_before_ack`] and core, each message is stored in core before the ack (R6); a message
/// core could not store makes the whole call 503 (nothing of it is dropped as seen, so the provider's resend is
/// processed; messages of it already stored come back from core as duplicates).
pub async fn handle(
    st: &EdgeState,
    connector: Arc<dyn ChannelConnector>,
    req: &IngressRequest,
) -> Result<Response, super::connector::ChannelError> {
    let auth = connector.authenticate(req).await?;
    if let Some(r) = staff_ingress(st, connector.kind(), req, &auth).await {
        return Ok(r);
    }
    match connector.ingest(req, &auth).await? {
        Ingress::Reply(r) => Ok(r.into_response()),
        // Only a worker-side connector yields staff messages; a customer connector that did is acked and dropped.
        Ingress::Staff { ack, .. } => Ok(ack.into_response()),
        Ingress::Messages { ack, inbound } => {
            let items = inbound.into_iter().map(|m| (m, None)).collect();
            handle_messages(st, connector, req, ack, items).await
        }
        Ingress::Media {
            ack,
            inbound,
            media,
        } => handle_messages(st, connector, req, ack, vec![(*inbound, Some(media))]).await,
    }
}

/// The messages of one authenticated call (see [`handle`]); `media` is the photo or file a message carries (SW-9).
async fn handle_messages(
    st: &EdgeState,
    connector: Arc<dyn ChannelConnector>,
    req: &IngressRequest,
    ack: super::connector::EdgeReply,
    inbound: Vec<(Inbound, Option<InboundMedia>)>,
) -> Result<Response, super::connector::ChannelError> {
    let kind = connector.kind();
    let persist = connector.persist_before_ack();
    let mut not_stored: Option<String> = None;
    for (mut m, media) in inbound {
        // once one message could not be stored the call is 503: the rest is left for the provider's resend
        if not_stored.is_some() || !st.dedupe.first_time(kind, &m.external_id) {
            continue;
        }
        // A BYO webhook is signed with its own binding's secret, so it speaks only for that binding's
        // workspace: a message naming another one is dropped (acked, never processed), never matched to
        // the other workspace's binding — that would let one tenant inject into another's.
        if let Some(b) = req.binding.as_ref()
            && !byo_owns(b, kind, &m)
        {
            tracing::warn!(channel = kind.as_str(), binding_id = ?b.id, "channel_byo_workspace_mismatch");
            metrics::counter!("agent_edge_byo_mismatch_total", "channel" => kind.as_str())
                .increment(1);
            continue;
        }
        metrics::counter!("agent_edge_messages_total", "channel" => kind.as_str()).increment(1);
        let binding = resolve_binding(st, kind, req.binding.as_ref(), &m);
        let binding_id = binding.as_ref().and_then(|b| b.id);
        if m.scope.is_none() {
            m.scope = binding.as_ref().map(|b| b.scope.clone());
        }
        // the answer goes out with the binding's own credential when it has one
        let c = connector_for(st, connector.clone(), binding.as_ref());
        // SW-9: a photo or file is downloaded and stored in core first; a refused one still reaches staff, as text.
        // A chat with no conversation yet is stored first (`receive_media` accepts the message, then uploads).
        let mut attachments: Vec<Uuid> = Vec::new();
        let mut pre: Option<Result<InboundResp, SeamError>> = None;
        let mut notice: Option<&'static str> = None;
        if let Some(media) = media {
            let got = receive_media(st, &c, binding.as_ref(), &m, &media, persist).await;
            if let Some(t) = got.text {
                m.text = t;
            }
            attachments = got.ids;
            pre = got.pre;
            notice = got.notice;
        }
        // R6: stored in core before the ack, answered after it
        if persist
            && let Some(core) = st.core.clone()
            && let Some(mut req) = inbound_req(kind, &m, binding.as_ref())
        {
            req.attachments = attachments;
            // The message was already stored by the pre-accept (new chat): without files to link its answer is the
            // outcome; with files, the same message is accepted again with them (core links on a resend, stores
            // nothing twice, runs no turn: the one answer call below is the only turn).
            let accepted = match pre {
                Some(Ok(r))
                    if req.attachments.is_empty()
                        || !matches!(
                            r,
                            InboundResp::Accepted { .. } | InboundResp::Queued { .. }
                        ) =>
                {
                    Ok(r)
                }
                Some(Err(e)) => Err(e),
                _ => core.accept(&req, connector.accept_timeout()).await,
            };
            match accepted {
                Ok(accepted) => {
                    let conv = m.conversation;
                    let health = st.health.clone();
                    tokio::spawn(async move {
                        if let Some(n) = notice {
                            let out = Some(Outbound::Text(n.into()));
                            deliver_out(c.clone(), &conv, out, &health, Some(req.binding_id), None)
                                .await;
                        }
                        after_accept(&core, c, conv, req, accepted, &health).await
                    });
                }
                Err(e) if e.retryable() => {
                    st.dedupe.forget(kind, &m.external_id);
                    tracing::warn!(channel = kind.as_str(), error = %e, "channel_accept_unavailable");
                    metrics::counter!("agent_edge_accept_unavailable_total", "channel" => kind.as_str())
                            .increment(1);
                    not_stored = Some(e.to_string());
                }
                // refused for good (no such binding, job): acked, the customer gets the fixed text
                Err(e) => {
                    let (out, _) = outbound_of(kind, Err(e.into()));
                    let health = st.health.clone();
                    tokio::spawn(async move {
                        deliver_out(c, &m.conversation, out, &health, binding_id, None).await
                    });
                }
            }
            continue;
        }
        let st = st.clone();
        tokio::spawn(async move {
            if let Some(n) = notice {
                let out = Some(Outbound::Text(n.into()));
                deliver_out(
                    c.clone(),
                    &m.conversation,
                    out,
                    &st.health,
                    binding_id,
                    None,
                )
                .await;
            }
            match st.core.clone() {
                Some(core) => answer_via_core(&core, c, m, binding, &st.health).await,
                None => answer(&st, c, m).await,
            }
        });
    }
    match not_stored {
        Some(e) => Err(ChannelError::Unavailable(e)),
        None => Ok(ack.into_response()),
    }
}

/// Worker side (D104, B12): one webhook serves both sides of a kind (one bot per kind). When the kind has
/// a worker connector, it reads the call first; staff replies in a thread of a channel with a worker binding go to
/// agent-api's `staff` route (in the background, after the ack). Anything else — not a thread reply, or a channel
/// without a worker binding — takes the customer path unchanged. `None` = not a staff call.
async fn staff_ingress(
    st: &EdgeState,
    kind: ChannelKind,
    req: &IngressRequest,
    auth: &super::connector::IngressAuth,
) -> Option<Response> {
    let worker = st.registry.get_side(kind, Side::Worker)?;
    let Ok(Ingress::Staff { ack, staff }) = worker.ingest(req, auth).await else {
        return None;
    };
    let mut bound: Vec<(StaffInbound, Binding, Uuid)> = Vec::new();
    for m in staff {
        match worker_binding(st, kind, &m.target) {
            // A BYO webhook is signed by one tenant's app: it may reach only that tenant's worker channels.
            Some(b)
                if req
                    .binding
                    .as_ref()
                    .is_some_and(|p| p.scope.tenant != b.scope.tenant) =>
            {
                tracing::warn!(channel = kind.as_str(), "channel_byo_staff_tenant_mismatch");
            }
            Some(b) => {
                // worker bindings come from core, so they carry an id; an env binding is customer-side only
                if let Some(id) = b.id {
                    bound.push((m, b, id));
                }
            }
            None => {}
        }
    }
    if bound.is_empty() {
        return None;
    }
    for (m, b, binding_id) in bound {
        // A new post in a worker channel is staff talking among themselves: never the customer path, never core.
        if !m.in_thread || !st.dedupe.first_time(kind, &m.external_id) {
            continue;
        }
        metrics::counter!("agent_edge_staff_messages_total", "channel" => kind.as_str())
            .increment(1);
        let Some(core) = st.core.clone() else {
            // Worker bindings come from core; without it there is nowhere to send a staff reply.
            continue;
        };
        let w = worker.clone();
        tokio::spawn(async move { staff_via_core(&core, w, m, b, binding_id).await });
    }
    Some(ack.into_response())
}

/// The worker binding of a target: keyed by the target id, or naming it in `settings` (`channel_id` / `channel`).
pub fn worker_binding(st: &EdgeState, kind: ChannelKind, target: &str) -> Option<Binding> {
    st.bindings.find(kind, Side::Worker, target).or_else(|| {
        st.bindings
            .of_kind(kind, Side::Worker)
            .into_iter()
            .find(|b| {
                ["channel_id", "channel"]
                    .iter()
                    .any(|k| b.settings.get(*k).and_then(Value::as_str) == Some(target))
            })
    })
}

/// One staff reply → agent-api's `staff` → a short fixed confirmation in the thread. agent-api resolves staff by
/// email only, so an unresolvable email is answered here and never sent. Never logs message text.
pub async fn staff_via_core(
    core: &CoreLink,
    worker: Arc<dyn ChannelConnector>,
    m: StaffInbound,
    binding: Binding,
    binding_id: Uuid,
) {
    let r = match staff_req(worker.as_ref(), &m, &binding, binding_id).await {
        None => None,
        Some(req) => Some(core.staff(&req).await),
    };
    confirm_staff(worker, &m, r).await;
}

/// The `staff` call for one staff message; `None` when the worker connector cannot tell the staff member's email
/// (agent-api resolves staff by email only, so nothing is sent).
pub async fn staff_req(
    worker: &dyn ChannelConnector,
    m: &StaffInbound,
    binding: &Binding,
    binding_id: Uuid,
) -> Option<StaffReq> {
    let email = worker.staff_email(m).await?;
    Some(StaffReq {
        kind: worker.kind().as_str().into(),
        tenant: binding.scope.tenant,
        job_id: binding.scope.job_id,
        thread: m.thread.clone(),
        staff: StaffIdentity {
            email: Some(email),
            external_id: m.staff_external_id.clone(),
            name: m.staff_name.clone(),
        },
        message_id: m.external_id.clone(),
        text: m.text.clone(),
        binding_id,
    })
}

/// The fixed confirmation in the worker thread for agent-api's answer to a staff message (`None` = the email was
/// unknown, nothing was sent). Never logs message text.
pub async fn confirm_staff(
    worker: Arc<dyn ChannelConnector>,
    m: &StaffInbound,
    r: Option<Result<StaffResp, CoreError>>,
) {
    let kind = worker.kind();
    let confirmation = match &r {
        None => Some(super::workers::UNVERIFIED),
        Some(r) => {
            if let Err(e) = r {
                tracing::info!(channel = kind.as_str(), error = %e, "channel_staff_refused");
            }
            super::workers::confirmation(r)
        }
    };
    let Some(text) = confirmation else {
        return;
    };
    if let Err(e) = worker
        .deliver(&m.reply_to, Outbound::Text(text.into()))
        .await
    {
        tracing::warn!(channel = kind.as_str(), error = %e, "channel_staff_confirm_failed");
    }
}

/// True when a message that arrived on BYO binding `b`'s webhook belongs to it: same kind, and the message names no
/// workspace or exactly `b`'s (e.g. a Telegram bot id).
fn byo_owns(b: &Binding, kind: ChannelKind, m: &Inbound) -> bool {
    b.kind == kind && m.workspace.as_deref().is_none_or(|w| w == b.external_id)
}

/// The binding a message belongs to: the BYO path's (the caller dropped a message it does not own, see
/// [`byo_owns`]), else the store's customer binding for its workspace. A preselected binding never falls back to
/// the store.
fn resolve_binding(
    st: &EdgeState,
    kind: ChannelKind,
    preselected: Option<&Binding>,
    m: &Inbound,
) -> Option<Binding> {
    if let Some(b) = preselected {
        return byo_owns(b, kind, m).then(|| b.clone());
    }
    let ws = m.workspace.as_deref()?;
    st.bindings.find(kind, Side::Customer, ws)
}

/// agent-api's `inbound` request for a message; `None` when there is nothing to send it on (no job scope, or an
/// env (phase-A) binding without an id: agent-api accepts a message only on a known, verified binding).
pub fn inbound_req(
    kind: ChannelKind,
    inbound: &Inbound,
    binding: Option<&Binding>,
) -> Option<InboundReq> {
    let scope = inbound.scope.as_ref()?;
    let binding_id = binding.and_then(|b| b.id)?;
    Some(InboundReq {
        binding_id,
        kind: kind.as_str().into(),
        external_id: binding
            .map(|b| b.external_id.clone())
            .or_else(|| inbound.workspace.clone())
            .unwrap_or_default(),
        tenant: scope.tenant,
        job_id: scope.job_id,
        job_key: scope.job_key.clone(),
        conversation: inbound.conversation.clone(),
        message_id: inbound.external_id.clone(),
        party: WireParty {
            id: inbound.identity.subject.clone(),
            name: inbound.identity.display_name.clone(),
            verified: inbound.identity.verified,
        },
        text: inbound.text.clone(),
        session_token: None,
        ip_hash: None,
        accept_only: false,
        attachments: Vec::new(),
    })
}

/// What [`receive_media`] made of a message's photo or file.
struct MediaOutcome {
    /// Replaces the message text when the file was not accepted.
    text: Option<String>,
    /// The stored attachments to link to the message.
    ids: Vec<Uuid>,
    /// Set when the chat had no conversation yet: core's answer to the `accept_only` call that stored the message
    /// first (an upload needs its conversation). `None`: nothing was sent to core's `inbound` yet.
    pre: Option<Result<InboundResp, SeamError>>,
    /// Set when a file was refused: the customer is told, directly, in plain words (never the file or an id).
    notice: Option<&'static str>,
}

/// SW-9: the photo or file an inbound message carries: downloaded through the connector (capped at 10 MB), validated
/// and stored by core, and the stored attachment's id returned for the message. A file that is not accepted for any
/// reason (too large, the wrong type, core down) stores nothing and replaces the message text with a short note
/// ("(photo — not accepted: it is over 10 MB)", after the caption if there was one), so staff and the agent still see
/// that something was sent. Never panics, never logs the file, its name, its handle or an URL.
///
/// Order: the upload is tried first (the conversation exists). A brand-new chat answers `conversation_unknown`
/// (an attachment is stored against a conversation row): the message is then stored FIRST with `accept_only` (the
/// persist-before-answer path: stored, deduped, no turn) and the upload repeated. The caller then accepts the same
/// message again with the attachment ids (core links on a resend) and runs the one answer call. A refused file after
/// the pre-accept leaves the stored text as it was (caption or "(photo)").
async fn receive_media(
    st: &EdgeState,
    c: &Arc<dyn ChannelConnector>,
    binding: Option<&Binding>,
    m: &Inbound,
    media: &InboundMedia,
    persist: bool,
) -> MediaOutcome {
    let kind = c.kind();
    let refuse = |reason: &str, pre: Option<Result<InboundResp, SeamError>>| {
        metrics::counter!("agent_edge_media_total", "channel" => kind.as_str(), "outcome" => "refused")
            .increment(1);
        MediaOutcome {
            // a message already stored by the pre-accept keeps its text; the customer's notice tells the rest
            text: pre.is_none().then(|| media.refused_text(reason)),
            ids: Vec::new(),
            pre,
            notice: Some(refusal_notice(reason)),
        }
    };
    if let Some(reason) = media.skip {
        return refuse(reason, None);
    }
    let (Some(core), Some(binding_id), Some(scope)) = (
        st.core.as_ref(),
        binding.and_then(|b| b.id),
        m.scope.as_ref(),
    ) else {
        return refuse("unavailable", None);
    };
    let file = match c.download_media(media).await {
        Ok(f) => f,
        Err(reason) => return refuse(reason, None),
    };
    let meta = InboundUploadMeta {
        binding_id,
        kind: kind.as_str().into(),
        tenant: scope.tenant,
        job_id: scope.job_id,
        conversation: m.conversation.clone(),
        message_id: m.external_id.clone(),
        party_id: m.identity.subject.clone(),
        declared_name: file.declared_name,
    };
    let mut pre = None;
    let mut r = upload_inbound(core, &meta, file.bytes.clone()).await;
    if persist
        && matches!(&r, Err(CoreError::Code(code)) if code == "conversation_unknown")
        && let Some(req) = inbound_req(kind, m, binding)
    {
        let stored = core.accept(&req, c.accept_timeout()).await;
        let ok = matches!(
            stored,
            Ok(InboundResp::Accepted { .. }) | Ok(InboundResp::Queued { .. })
        );
        pre = Some(stored);
        if ok {
            r = upload_inbound(core, &meta, file.bytes).await;
        } else {
            // not stored (retryable) or answered some other way: the caller settles it with `pre`
            return MediaOutcome {
                text: None,
                ids: Vec::new(),
                pre,
                notice: None,
            };
        }
    }
    match r {
        Ok(UploadOutcome::Stored(r)) => {
            metrics::counter!("agent_edge_media_total", "channel" => kind.as_str(), "outcome" => "stored")
                .increment(1);
            MediaOutcome {
                text: None,
                ids: vec![r.id],
                pre,
                notice: None,
            }
        }
        Ok(UploadOutcome::Refused { reason }) => refuse(&reason, pre),
        Err(e) => {
            tracing::warn!(channel = kind.as_str(), error = %e, "channel_media_upload_failed");
            refuse("unavailable", pre)
        }
    }
}

/// B12: one inbound message → agent-api's `inbound` → at most one delivery. Never logs customer text.
pub async fn answer_via_core(
    core: &CoreLink,
    connector: Arc<dyn ChannelConnector>,
    inbound: Inbound,
    binding: Option<Binding>,
    health: &WebhookHealth,
) {
    let kind = connector.kind();
    let conv = inbound.conversation.clone();
    let binding_id = binding.as_ref().and_then(|b| b.id);
    let (out, ack) = match inbound_req(kind, &inbound, binding.as_ref()) {
        None => (Some(Outbound::Text(NOT_CONNECTED.into())), None),
        Some(req) => {
            let _ = connector.deliver(&conv, Outbound::Typing).await;
            let (out, notice) = outbound_of(kind, core.inbound(&req).await);
            (out, notice.map(|i| NoticeDeliveredReq::of(&req, &i)))
        }
    };
    deliver_out(
        connector,
        &conv,
        out,
        health,
        binding_id,
        ack.map(|a| (core, a)),
    )
    .await;
}

/// R6, after the ack: a message core stored and left for the agent (`accepted`) is answered now — a second
/// `inbound` call without `accept_only`, which answers the stored message — and any other outcome of the accept
/// (queued, CSAT, duplicate) is delivered as [`answer_via_core`] would have. Never logs customer text.
pub async fn after_accept(
    core: &CoreLink,
    connector: Arc<dyn ChannelConnector>,
    conv: ConversationRef,
    mut req: InboundReq,
    accepted: InboundResp,
    health: &WebhookHealth,
) {
    let kind = connector.kind();
    let binding_id = req.binding_id;
    let (out, notice) = match accepted {
        InboundResp::Accepted { .. } => {
            let _ = connector.deliver(&conv, Outbound::Typing).await;
            req.accept_only = false;
            // linked to the message by the accept call; the answer call must not link them again
            req.attachments.clear();
            outbound_of(kind, core.inbound(&req).await)
        }
        other => outbound_of(kind, Ok(other)),
    };
    let ack = notice.map(|i| (core, NoticeDeliveredReq::of(&req, &i)));
    deliver_out(connector, &conv, out, health, Some(binding_id), ack).await;
}

/// The binding a send counts for in the webhook health (B7 T15b): Telegram core bindings only.
fn tracked(kind: ChannelKind, binding_id: Option<Uuid>) -> Option<Uuid> {
    binding_id.filter(|_| kind == ChannelKind::Telegram)
}

/// Deliver `out` (if any) to the customer; a failure is logged and counted (and, for a Telegram binding, recorded in
/// its webhook health). `ack`: `out` is that item's handover notice — once delivered, core is told (R6: a resent
/// accept is then `duplicate`, so no edge pod sends it again); a failed ack is logged and counted, never fatal (the
/// worst case is the notice once more on a resend).
async fn deliver_out(
    connector: Arc<dyn ChannelConnector>,
    conv: &ConversationRef,
    out: Option<Outbound>,
    health: &WebhookHealth,
    binding_id: Option<Uuid>,
    ack: Option<(&CoreLink, NoticeDeliveredReq)>,
) {
    let Some(out) = out else {
        return;
    };
    let kind = connector.kind();
    let r = connector.deliver(conv, out).await;
    if let Some(id) = tracked(kind, binding_id) {
        health.sent(id, &r);
    }
    if let Err(e) = r {
        tracing::warn!(channel = kind.as_str(), error = %e, "channel_deliver_failed");
        metrics::counter!("agent_edge_deliver_failed_total", "channel" => kind.as_str())
            .increment(1);
        return;
    }
    if let Some((core, req)) = ack
        && let Err(e) = core.notice_delivered(&req).await
    {
        tracing::warn!(channel = kind.as_str(), error = %e, "channel_notice_ack_failed");
        metrics::counter!("agent_edge_notice_ack_failed_total", "channel" => kind.as_str())
            .increment(1);
    }
}

/// What the customer gets for agent-api's answer (`None` = nothing: a retry, or a message queued for a person who
/// was already told), and the (conversation, handover item) whose notice it is (acked to core once delivered, see
/// [`deliver_out`]).
pub fn outbound_of(
    kind: ChannelKind,
    r: Result<InboundResp, CoreError>,
) -> (Option<Outbound>, Option<(String, String)>) {
    match r {
        Ok(InboundResp::Answered { text, .. }) | Ok(InboundResp::CsatRecorded { text, .. }) => {
            (Some(Outbound::Text(text)), None)
        }
        // SW-6: the same outbound the deliver path uses for a CSAT request
        Ok(InboundResp::CsatRequest { text, .. }) => (Some(Outbound::CsatRequest { text }), None),
        Ok(InboundResp::Queued {
            notice: true,
            conversation_id,
            item_id,
            ..
        }) => (
            Some(Outbound::HandoverNotice { eta_minutes: None }),
            Some((conversation_id, item_id)).filter(|(c, i)| !c.is_empty() && !i.is_empty()),
        ),
        // `accepted` answers only an `accept_only` call, which goes through [`after_accept`]
        Ok(InboundResp::Queued { notice: false, .. })
        | Ok(InboundResp::Duplicate { .. })
        | Ok(InboundResp::Accepted { .. }) => (None, None),
        Err(CoreError::Code(c)) if not_available_code(&c) => {
            (Some(Outbound::Text(NOT_AVAILABLE.into())), None)
        }
        Err(e) => {
            tracing::warn!(channel = kind.as_str(), error = %e, "channel_core_failed");
            (Some(Outbound::Text(TRY_LATER.into())), None)
        }
    }
}

/// agent-api codes that mean "this job cannot answer here", not "try again". `job_not_live` (D109: no live release,
/// or a live release with no runtime design) only clears when someone releases the job, so retrying never helps.
pub fn not_available_code(c: &str) -> bool {
    matches!(
        c,
        "provider_not_configured" | "job_not_found" | "job_not_live"
    )
}

// ---- the internal seam (agent-api → edge) ------------------------------------------------------------------------

fn seam_error(status: StatusCode, code: &'static str) -> Response {
    (status, axum::Json(json!({"error": {"code": code}}))).into_response()
}

/// Bearer `EDGE_INTERNAL_KEY`: unset → 503 `edge_not_configured`, mismatch → 401 `unauthorized`.
fn gate(st: &EdgeState, headers: &HeaderMap) -> Option<Response> {
    let Some(core) = &st.core else {
        return Some(seam_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "edge_not_configured",
        ));
    };
    (!core.key.verify(headers)).then(|| seam_error(StatusCode::UNAUTHORIZED, "unauthorized"))
}

/// `POST /internal/deliver`: one message to a customer conversation (a staff reply, a handover notice, a CSAT ask).
async fn deliver(State(st): State<EdgeState>, headers: HeaderMap, body: Bytes) -> Response {
    if let Some(r) = gate(&st, &headers) {
        return r;
    }
    let Ok(mut req) = serde_json::from_slice::<DeliverReq>(&body) else {
        return seam_error(StatusCode::BAD_REQUEST, "invalid_body");
    };
    let refs = std::mem::take(&mut req.message.attachments);
    let msg = match req.message.kind {
        DeliverKind::Text => Outbound::Text(req.message.text),
        DeliverKind::HandoverNotice => Outbound::HandoverNotice {
            eta_minutes: req.message.eta_minutes,
        },
        DeliverKind::CsatRequest => Outbound::CsatRequest {
            text: req.message.text,
        },
    };
    let Some(kind) = ChannelKind::parse(&req.conversation.channel) else {
        return not_configured();
    };
    if kind == ChannelKind::WebChat {
        // no provider to call: agent-api publishes the accepted message on the conversation's stream under this id,
        // and every edge replica's customer stream reads it from there (crate::conversations::webchat_stream)
        if st.webchat.is_none() {
            return not_configured();
        }
        let external_id = outbound_text(&msg).map(|_| new_event_id());
        return axum::Json(DeliverResp { external_id }).into_response();
    }
    // SW-9: files go out on Telegram only; every other channel refuses by name, never silently drops the file
    if !refs.is_empty() && kind != ChannelKind::Telegram {
        tracing::info!(
            channel = kind.as_str(),
            "channel_deliver_unsupported_attachment"
        );
        return seam_error(StatusCode::UNPROCESSABLE_ENTITY, "unsupported_attachment");
    }
    let Some(c) = st.registry.get(kind) else {
        return not_configured();
    };
    // the binding the conversation arrived on: its BYO credential (and settings) are used on the way out too
    let binding = req
        .binding_id
        .and_then(|id| st.bindings.by_id(id))
        .filter(|b| b.kind == kind && b.side == Side::Customer);
    let c = connector_for(&st, c, binding.as_ref());
    if !refs.is_empty() {
        return deliver_attachments(&st, &c, binding.as_ref(), &req.conversation, msg, &refs).await;
    }
    let r = c
        .deliver_proactive(&req.conversation, msg, req.last_customer_at_ms)
        .await;
    if let Some(id) = tracked(kind, binding.as_ref().and_then(|b| b.id)) {
        st.health.sent(id, &r);
    }
    match r {
        Ok(r) => axum::Json(DeliverResp {
            external_id: r.external_id,
        })
        .into_response(),
        Err(e) => {
            tracing::warn!(channel = kind.as_str(), error = %e, "channel_deliver_failed");
            metrics::counter!("agent_edge_deliver_failed_total", "channel" => kind.as_str())
                .increment(1);
            seam_error(StatusCode::BAD_GATEWAY, "delivery_failed")
        }
    }
}

/// SW-9: a staff reply with files to a Telegram customer. Every file's bytes are fetched from core first (a failure
/// sends nothing), then each goes out as `Outbound::Attachment`; the reply text rides on the first file as its caption
/// (the connector puts it in its own message first when it is too long for one). Bytes and names are never logged.
async fn deliver_attachments(
    st: &EdgeState,
    c: &Arc<dyn ChannelConnector>,
    binding: Option<&Binding>,
    conv: &ConversationRef,
    msg: Outbound,
    refs: &[crate::attachments::AttachmentRef],
) -> Response {
    let kind = c.kind();
    let (Some(core), Some(binding)) = (st.core.as_ref(), binding) else {
        return seam_error(StatusCode::SERVICE_UNAVAILABLE, "attachments_unavailable");
    };
    let mut files = Vec::with_capacity(refs.len());
    for r in refs {
        match core.attachment_fetch(&binding.scope, &conv.id, r.id).await {
            Ok(f) => files.push(f),
            Err(e) => {
                tracing::warn!(channel = kind.as_str(), error = %e, "channel_attachment_fetch_failed");
                return seam_error(StatusCode::BAD_GATEWAY, "attachment_unavailable");
            }
        }
    }
    let mut caption = outbound_text(&msg).filter(|t| !t.trim().is_empty());
    let mut external_id = None;
    for (media_type, display_name, bytes) in files {
        let out = Outbound::Attachment {
            media_type,
            display_name,
            bytes: Blob(bytes),
            caption: caption.take(),
        };
        let r = c.deliver(conv, out).await;
        if let Some(id) = tracked(kind, binding.id) {
            st.health.sent(id, &r);
        }
        match r {
            Ok(r) => external_id = external_id.or(r.external_id),
            Err(e) => {
                tracing::warn!(channel = kind.as_str(), error = %e, "channel_deliver_failed");
                metrics::counter!("agent_edge_deliver_failed_total", "channel" => kind.as_str())
                    .increment(1);
                return seam_error(StatusCode::BAD_GATEWAY, "delivery_failed");
            }
        }
    }
    axum::Json(DeliverResp { external_id }).into_response()
}

/// `POST /internal/worker-note` (SW-11): an internal staff note to a worker thread, as a private comment on the helpdesk
/// ticket. Its own path, not `/internal/deliver` (which is customer-side only): helpdesk worker bindings only, the
/// binding must be an enabled-and-loaded helpdesk worker binding; anything else is 404 `channel_not_configured`.
async fn worker_note(State(st): State<EdgeState>, headers: HeaderMap, body: Bytes) -> Response {
    if let Some(r) = gate(&st, &headers) {
        return r;
    }
    let Ok(req) = serde_json::from_slice::<WorkerNoteReq>(&body) else {
        return seam_error(StatusCode::BAD_REQUEST, "invalid_body");
    };
    let kind = ChannelKind::Helpdesk;
    let binding = st
        .bindings
        .by_id(req.binding_id)
        .filter(|b| b.kind == kind && b.side == Side::Worker)
        // the binding must belong to the scope the note is for (a binding id alone must not post into another tenant's ticket)
        .filter(|b| b.scope.tenant == req.tenant && b.scope.job_id == req.job_id);
    let (Some(binding), Some(c), true) = (
        binding,
        st.registry.get_side(kind, Side::Worker),
        req.thread.channel == kind.as_str(),
    ) else {
        return not_configured();
    };
    let c = connector_for(&st, c, Some(&binding));
    // a retry after an attempt whose outcome is unknown (0206): the ticket may hold the note already
    if req.check_first {
        match c.worker_note_exists(&req.thread, &req.text).await {
            Ok(true) => return StatusCode::NO_CONTENT.into_response(),
            Ok(false) | Err(ChannelError::Unsupported) => {}
            Err(e) => {
                tracing::warn!(channel = kind.as_str(), error = %e, "channel_worker_note_lookup_failed");
                return note_error(&e, false);
            }
        }
    }
    // the connector's own note call, not `deliver`: a note whose text is the "Sent to the customer." confirmation is
    // still a note, and a connector without a loaded binding is an error, never a silent 204
    match c.worker_note(&req.thread, &req.text).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(ChannelError::Unsupported) => not_configured(),
        Err(e) => {
            tracing::warn!(channel = kind.as_str(), error = %e, "channel_worker_note_failed");
            metrics::counter!("agent_edge_deliver_failed_total", "channel" => kind.as_str())
                .increment(1);
            note_error(&e, true)
        }
    }
}

/// The `/internal/worker-note` answer of a failed call (`WorkerNoteReq`): 504 `delivery_unknown` when the note was sent
/// (`sent`: the post, not the lookup) and no answer was read; 422 `delivery_rejected` when the ticket refused it (a 4xx
/// other than 408/429, or a binding the connector cannot use), never worth a retry; else 502 `delivery_failed`.
fn note_error(e: &ChannelError, sent: bool) -> Response {
    match e {
        ChannelError::Unavailable(why) if sent && (why == "timeout" || why == "read") => {
            seam_error(StatusCode::GATEWAY_TIMEOUT, "delivery_unknown")
        }
        ChannelError::Upstream(why)
            if sent && (why == "invalid_answer" || why == "answer_too_large") =>
        {
            seam_error(StatusCode::GATEWAY_TIMEOUT, "delivery_unknown")
        }
        ChannelError::Upstream(why)
            if why.starts_with("status 4") && why != "status 408" && why != "status 429" =>
        {
            seam_error(StatusCode::UNPROCESSABLE_ENTITY, "delivery_rejected")
        }
        ChannelError::Unauthenticated(_)
        | ChannelError::Forbidden(_)
        | ChannelError::Invalid(_) => {
            seam_error(StatusCode::UNPROCESSABLE_ENTITY, "delivery_rejected")
        }
        _ => seam_error(StatusCode::BAD_GATEWAY, "delivery_failed"),
    }
}

/// `POST /internal/notify-workers`: tell each worker target about a handover item; one result per target.
async fn notify_workers(State(st): State<EdgeState>, headers: HeaderMap, body: Bytes) -> Response {
    if let Some(r) = gate(&st, &headers) {
        return r;
    }
    let Ok(req) = serde_json::from_slice::<NotifyWorkersReq>(&body) else {
        return seam_error(StatusCode::BAD_REQUEST, "invalid_body");
    };
    let mut results = Vec::with_capacity(req.targets.len());
    for t in &req.targets {
        let connector =
            ChannelKind::parse(&t.kind).and_then(|k| st.registry.get_side(k, Side::Worker));
        let r = match connector {
            None => Err("channel_not_configured"),
            Some(c) => match c.notify_worker(t, &req.item).await {
                Ok(thread) => Ok(thread),
                Err(ChannelError::Unsupported) => Err("unsupported"),
                Err(e) => {
                    tracing::warn!(channel = %t.kind, error = %e, "channel_notify_worker_failed");
                    Err("delivery_failed")
                }
            },
        };
        results.push(match r {
            Ok(thread) => NotifyResult {
                binding_id: t.binding_id,
                ok: true,
                thread,
                error: None,
            },
            Err(code) => NotifyResult {
                binding_id: t.binding_id,
                ok: false,
                thread: None,
                error: Some(code.into()),
            },
        });
    }
    axum::Json(NotifyWorkersResp { results }).into_response()
}

/// One inbound message → one delivered answer (or a fixed text). Never panics, never logs customer text.
pub async fn answer(st: &EdgeState, connector: Arc<dyn ChannelConnector>, inbound: Inbound) {
    let kind = connector.kind();
    let conv = inbound.conversation.clone();
    let text = match reply_text(st, kind, &inbound, connector.as_ref()).await {
        Ok(t) => t,
        Err(fixed) => fixed.to_string(),
    };
    if let Err(e) = connector.deliver(&conv, Outbound::Text(text)).await {
        tracing::warn!(channel = kind.as_str(), error = %e, "channel_deliver_failed");
        metrics::counter!("agent_edge_deliver_failed_total", "channel" => kind.as_str())
            .increment(1);
    }
}

async fn reply_text(
    st: &EdgeState,
    kind: ChannelKind,
    inbound: &Inbound,
    connector: &dyn ChannelConnector,
) -> Result<String, &'static str> {
    let Some(scope) = &inbound.scope else {
        return Err(NOT_CONNECTED);
    };
    // Best effort: a typing indicator that fails changes nothing.
    let _ = connector
        .deliver(&inbound.conversation, Outbound::Typing)
        .await;
    // History is per tenant + conversation: two workspaces never share turns, even if a provider reuses an id.
    let key = format!("{}/{}", scope.tenant, inbound.conversation.id);
    let mut input = st.memory.history(kind, &key);
    input.push(Message::text(Role::User, inbound.text.clone()));
    let mut req = InteractionRequest::new(scope, input);
    req.channel = kind.as_str().into();
    req.party = Some(Party::customer(
        inbound.identity.subject.clone(),
        inbound.identity.display_name.clone(),
    ));
    req.conversation = Some(inbound.conversation.clone());
    if req.validate().is_err() {
        // Over the input bound with history: retry once with this message alone.
        req.input = vec![Message::text(Role::User, inbound.text.clone())];
        if req.validate().is_err() {
            return Err(TRY_LATER);
        }
    }
    match tokio::time::timeout(st.answer_deadline, collect(st.backend.respond(req))).await {
        Ok(Ok(r)) if !r.text.trim().is_empty() => {
            st.memory.record(kind, &key, &inbound.text, &r.text);
            Ok(r.text)
        }
        Ok(Ok(_)) => Err(TRY_LATER),
        Ok(Err(BackendError::NotConfigured(_))) => Err(NOT_AVAILABLE),
        Ok(Err(e)) => {
            tracing::warn!(channel = kind.as_str(), error = %e, "channel_answer_failed");
            Err(TRY_LATER)
        }
        Err(_) => {
            tracing::warn!(channel = kind.as_str(), "channel_answer_timeout");
            Err(TRY_LATER)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queued(item_id: &str, notice: bool) -> Result<InboundResp, CoreError> {
        Ok(InboundResp::Queued {
            conversation_id: "c-1".into(),
            case_id: "CASE-1".into(),
            item_id: item_id.into(),
            notice,
        })
    }

    /// Only a `queued` with the notice names an item to ack; nothing else does.
    #[test]
    fn outbound_of_names_the_notice_item() {
        let k = ChannelKind::Telegram;
        assert_eq!(
            outbound_of(k, queued("HO-1", true)),
            (
                Some(Outbound::HandoverNotice { eta_minutes: None }),
                Some(("c-1".to_string(), "HO-1".to_string()))
            )
        );
        assert_eq!(outbound_of(k, queued("HO-1", false)), (None, None));
        // no item id: the notice still goes out, there is nothing to ack
        assert_eq!(
            outbound_of(k, queued("", true)),
            (Some(Outbound::HandoverNotice { eta_minutes: None }), None)
        );
        let answered = Ok(InboundResp::Answered {
            conversation_id: "c-1".into(),
            case_id: "CASE-1".into(),
            text: "hi".into(),
        });
        assert_eq!(
            outbound_of(k, answered),
            (Some(Outbound::Text("hi".into())), None)
        );
        assert_eq!(
            outbound_of(
                k,
                Ok(InboundResp::Duplicate {
                    conversation_id: None
                })
            ),
            (None, None)
        );
        assert_eq!(
            outbound_of(k, Err(CoreError::Unavailable("timeout".into()))),
            (Some(Outbound::Text(TRY_LATER.into())), None)
        );
    }

    /// A job that cannot answer here gets the not-available reply, never an endless "try again later"; any other
    /// refusal is still "try again".
    #[test]
    fn not_available_codes_answer_not_available() {
        let k = ChannelKind::Telegram;
        for c in ["provider_not_configured", "job_not_found", "job_not_live"] {
            assert!(not_available_code(c), "{c}");
            assert_eq!(
                outbound_of(k, Err(CoreError::Code(c.into()))),
                (Some(Outbound::Text(NOT_AVAILABLE.into())), None),
                "{c}"
            );
        }
        assert!(!not_available_code("rate_limited"));
        assert_eq!(
            outbound_of(k, Err(CoreError::Code("rate_limited".into()))),
            (Some(Outbound::Text(TRY_LATER.into())), None)
        );
    }
}
