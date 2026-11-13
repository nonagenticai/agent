//! The edge's link to `agent-api`'s internal seam (plan B12 §3) and the seam's wire types, both directions.
//!
//! ```text
//! edge → core  GET  /api/internal/channel/bindings                                  → BindingsResp
//!              POST /api/internal/channel/inbound   InboundReq                        → InboundResp
//!                   (`accept_only: true`: persist + dedupe only → `accepted`, the edge acks the provider, then
//!                    answers with a second call without the flag — the stored message's retry path, R6)
//!              POST /api/internal/channel/staff     StaffReq                          → StaffResp
//!              POST /api/internal/channel/staff-state StaffStateReq                   → StaffStateResp (R9)
//!              POST /api/internal/channel/handover/notice-delivered NoticeDeliveredReq → 204 (R6: the
//!                   customer got the item's handover notice; a resent accept is then `duplicate`, not the notice)
//!              GET  /api/internal/channel/conversations/{id}/messages?tenant=&job_id=&binding_id=&after_ms=
//!                                                                                  → HistoryResp
//!              POST /api/internal/channel/webchat/sessions  WebChatSessionReq        → WebChatSessionResp (B13)
//!              GET  /api/internal/channel/webchat/conversations/{id}/events?tenant=&job_id=&binding_id=
//!                                                                                  → SSE `message` {id, kind, text}
//!              POST /api/internal/channel/webhook-status WebhookStatusReq             → WebhookStatusResp (B7 T15b)
//! core → edge  POST /internal/deliver               DeliverReq                        → DeliverResp
//!              POST /internal/notify-workers        NotifyWorkersReq                  → NotifyWorkersResp
//!              POST /internal/worker-note           WorkerNoteReq                     → 204 (SW-11: an internal note as a private comment on a helpdesk ticket)
//! ```
//!
//! Both directions carry `Authorization: Bearer $EDGE_INTERNAL_KEY` (≥ 32 characters, compared in constant time).
//! The serde types below ARE the wire contract; agent-api's side (`src/conversations/internal.rs`) may share them.
//! Message text is customer data: none of these types is ever logged.
use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::connector::{ConversationRef, Side};
use crate::attachments::AttachmentRef;

/// The shortest accepted `EDGE_INTERNAL_KEY`.
pub const MIN_KEY_LEN: usize = 32;
/// `inbound` may run an LLM answer.
pub const INBOUND_TIMEOUT: Duration = Duration::from_secs(120);
/// `inbound` with `accept_only` (R6: the provider waits for it): no LLM, but a handover notifies the worker
/// channels inline. The default of [`ChannelConnector::accept_timeout`](super::connector::ChannelConnector::accept_timeout);
/// a provider with a short ack deadline bounds it tighter.
pub const ACCEPT_TIMEOUT: Duration = Duration::from_secs(30);
/// Every other seam call.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(10);

// ---- edge → core ---------------------------------------------------------------------------------------------------

/// One enabled `connector_bindings` row, credential opened (`GET /api/internal/channel/bindings`).
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct CoreBinding {
    pub id: Uuid,
    /// `telegram|web_chat|helpdesk` (kinds this edge has no connector for are skipped, not an error).
    pub kind: String,
    pub side: Side,
    pub external_id: String,
    pub tenant: Uuid,
    pub job_id: Uuid,
    pub job_key: String,
    #[serde(default = "prod")]
    pub env: String,
    #[serde(default)]
    pub settings: Value,
    /// The BYO credential (sealed at rest in agent-api); never logged, never in Debug.
    #[serde(default)]
    pub credential: Option<BTreeMap<String, String>>,
}

pub(crate) fn prod() -> String {
    "prod".into()
}

impl fmt::Debug for CoreBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CoreBinding")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("side", &self.side)
            .field("tenant", &self.tenant)
            .field("job_id", &self.job_id)
            .field("job_key", &self.job_key)
            .field("env", &self.env)
            .field(
                "credential",
                &self.credential.as_ref().map(|_| "<redacted>"),
            )
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BindingsResp {
    pub bindings: Vec<CoreBinding>,
}

/// `POST /api/internal/channel/webhook-status` (B7 T15b): what the edge observed of one binding's webhook since it
/// started (crate::channels::webhook_health). Times are epoch milliseconds; `last_error` is a short code (never message
/// text, never a token). agent-api checks the binding (tenant / job / kind) and upserts `channel_webhook_status`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebhookStatusReq {
    pub binding_id: Uuid,
    /// The channel kind (`telegram`).
    pub kind: String,
    pub tenant: Uuid,
    pub job_id: Uuid,
    /// The last update the provider delivered to this binding's webhook; `None` = none since the edge started.
    #[serde(default)]
    pub last_update_at_ms: Option<i64>,
    /// The newest send through the binding's bot that succeeded (0155; absent from an edge older than it).
    #[serde(default)]
    pub last_send_ok_at_ms: Option<i64>,
    /// Sends that failed in a row (a success resets it).
    #[serde(default)]
    pub consecutive_send_failures: u32,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub last_error_at_ms: Option<i64>,
    /// What Telegram itself says about the webhook (its webhook-info call, made by the edge this pass); absent from an
    /// edge older than it and when the call failed, and then `down` is not derivable (0155).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_info: Option<WebhookInfoReport>,
}

/// One binding's webhook registration as Telegram reports it, reduced on the edge: the URL itself is never sent (it
/// carries path ids), only whether one is set and whether it points at the binding's route; the delivery error is a
/// short code (crate::channels::telegram::webhook_error_code), never Telegram's message text.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebhookInfoReport {
    pub url_set: bool,
    /// `None` when the edge cannot tell (no URL set).
    #[serde(default)]
    pub url_matches: Option<bool>,
    #[serde(default)]
    pub pending_update_count: u32,
    #[serde(default)]
    pub last_error_code: Option<String>,
    #[serde(default)]
    pub last_error_at_ms: Option<i64>,
}

/// The status agent-api stored (`ok|degraded|quiet|down`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebhookStatusResp {
    pub status: String,
}

/// The customer behind an inbound message, as the connector vouches for it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WireParty {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub verified: bool,
}

/// `POST /api/internal/channel/inbound`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InboundReq {
    /// The enabled, verified customer binding the message arrived on (required: agent-api checks the row).
    pub binding_id: Uuid,
    /// The channel kind (`ChannelKind::as_str`).
    pub kind: String,
    /// The binding's provider-side id (Telegram bot id, widget key).
    pub external_id: String,
    pub tenant: Uuid,
    pub job_id: Uuid,
    pub job_key: String,
    pub conversation: ConversationRef,
    /// The provider's message id: agent-api dedupes provider retries on it.
    pub message_id: String,
    pub party: WireParty,
    pub text: String,
    /// Web chat on a core binding (B13 §3): the session token from `webchat/sessions` (party id `wc:<session_id>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_token: Option<String>,
    /// Web chat on a core binding: hex `sha256(salt ‖ client ip)` (the per-IP message window).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip_hash: Option<String>,
    /// B7 R6 persist-before-200: store (and dedupe) the message, but do not run the agent's answer; agent-api
    /// answers `accepted` where it would have answered, every other outcome (queued, CSAT, duplicate) as usual. The
    /// edge then calls again without the flag, which answers the stored message (its provider-retry path).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub accept_only: bool,
    /// SW-9: the ids of unlinked customer attachments of this conversation (at most 3) that this message carries;
    /// core links them to the stored message. Web chat only for now.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Uuid>,
}

/// The answer to `inbound`, by `status`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum InboundResp {
    Answered {
        conversation_id: String,
        case_id: String,
        text: String,
    },
    /// The conversation is with a person. `notice` = newly queued: the edge sends the handover notice once.
    Queued {
        conversation_id: String,
        case_id: String,
        item_id: String,
        notice: bool,
    },
    CsatRecorded {
        text: String,
        /// The conversation (B13: lets the edge hand the web chat widget its conversation token on any answer).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        conversation_id: Option<String>,
    },
    /// SW-6: the customer acknowledged the agent's resolution, so the conversation's one CSAT request is asked now.
    /// `text` is the prompt; the edge delivers it as an outbound CSAT request (web chat shows its rating buttons),
    /// not as a plain answer. `conversation_id` as on `CsatRecorded`.
    CsatRequest {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        conversation_id: Option<String>,
    },
    /// A provider retry of a stored message: nothing to send. `conversation_id` as on `CsatRecorded`: a web chat
    /// first message resent after a lost answer still gets its token, and the widget reloads the history.
    Duplicate {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        conversation_id: Option<String>,
    },
    /// `accept_only` (R6): the message is stored and waits for the agent's answer (a second call without the flag).
    Accepted {
        conversation_id: String,
        case_id: String,
    },
}

/// `POST /api/internal/channel/handover/notice-delivered` (R6): the edge delivered item `item_id`'s handover notice
/// on the customer binding `binding_id` (scoped like [`InboundReq`]). Answered 204, also for an unknown or already
/// stamped item.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NoticeDeliveredReq {
    pub binding_id: Uuid,
    pub kind: String,
    pub tenant: Uuid,
    pub job_id: Uuid,
    /// core's conversation the item belongs to: an ack only stamps an item of that conversation
    pub conversation_id: String,
    pub item_id: String,
}

impl NoticeDeliveredReq {
    /// The ack for a notice of `item_id` (in `conversation_id`) that answered `req`.
    pub fn of(req: &InboundReq, (conversation_id, item_id): &(String, String)) -> Self {
        Self {
            binding_id: req.binding_id,
            kind: req.kind.clone(),
            tenant: req.tenant,
            job_id: req.job_id,
            conversation_id: conversation_id.clone(),
            item_id: item_id.clone(),
        }
    }
}

/// The staff member answering in a worker thread.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StaffIdentity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    pub external_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// `POST /api/internal/channel/staff`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StaffReq {
    /// The worker binding the thread belongs to, and its scope (agent-api checks the row).
    pub tenant: Uuid,
    pub job_id: Uuid,
    pub binding_id: Uuid,
    pub kind: String,
    /// The worker thread the reply was posted in (its ref resolves the handover item).
    pub thread: ConversationRef,
    pub staff: StaffIdentity,
    pub message_id: String,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StaffStatus {
    Delivered,
    Stored,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StaffResp {
    pub status: StaffStatus,
    pub item_id: String,
}

/// `POST /api/internal/channel/staff-state` (B7 R9): a worker channel reports that staff closed the handover in its
/// own tool (a Frappe Helpdesk ticket set to Resolved / Closed). agent-api resolves the item and its case and tells
/// the customer; a repeat is `duplicate`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StaffStateReq {
    pub tenant: Uuid,
    pub job_id: Uuid,
    pub binding_id: Uuid,
    pub kind: String,
    /// The worker thread (the ticket) whose ref resolves the handover item.
    pub thread: ConversationRef,
    /// Who changed the state (agent-api resolves staff by email only: an enabled member holding `support_agent`).
    pub staff: StaffIdentity,
    /// Only `resolved` today.
    pub state: String,
    /// The provider's event id (logs only; idempotency is the item's state).
    pub message_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StaffStateStatus {
    /// The item and its case moved to resolved now.
    Resolved,
    /// The item was already resolved: nothing changed, nothing was sent.
    Duplicate,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StaffStateResp {
    pub status: StaffStateStatus,
    pub item_id: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HistoryMessage {
    pub role: String,
    pub text: String,
    pub at_ms: i64,
    /// SW-9: the message's files (refs only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<crate::attachments::AttachmentRef>,
}

/// `GET /api/internal/channel/conversations/{conversation_id}/messages?tenant=&job_id=&binding_id=&after_ms=`:
/// customer-facing lines only (`customer`, `agent`, `staff`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HistoryResp {
    pub messages: Vec<HistoryMessage>,
}

/// The scope of a history read (the web chat binding the conversation runs on).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryScope {
    pub tenant: Uuid,
    pub job_id: Uuid,
    pub binding_id: Uuid,
}

// ---- core → edge ---------------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliverKind {
    Text,
    HandoverNotice,
    CsatRequest,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeliverMessage {
    pub kind: DeliverKind,
    #[serde(default)]
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eta_minutes: Option<u32>,
    /// The staff member's display name on a staff reply (never an email or a sub); web chat shows it, other
    /// channels ignore it. Absent on bot replies and notices.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// SW-9: the files of a staff reply (refs only; web chat carries them on its stream frame, Telegram's edge fetches
    /// the bytes from core). Empty on bot replies and notices.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<crate::attachments::AttachmentRef>,
}

/// `POST /internal/deliver`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeliverReq {
    pub conversation: ConversationRef,
    pub message: DeliverMessage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding_id: Option<Uuid>,
    /// The customer's newest message (unix ms), for providers with a reply window (B13 §3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_customer_at_ms: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeliverResp {
    pub external_id: Option<String>,
}

/// `POST /internal/worker-note` (SW-11): an internal staff note posted to a WORKER thread, never to the customer. Helpdesk
/// only: `thread` is the ticket (`channel` `helpdesk`, `id` the ticket name) and `binding_id` the helpdesk worker
/// binding that opened it; the edge posts `text` as a private HD Ticket Comment. `tenant` and `job_id` are the scope
/// the note is for: the edge refuses (404 `channel_not_configured`) a binding that belongs to another scope.
/// `check_first` (0206: an earlier attempt's outcome is unknown): the edge looks the comment up and posts only when the
/// ticket does not hold it yet. Answers: 204 posted (or already there); 504 `delivery_unknown` (sent, no answer read);
/// 422 `delivery_rejected` (refused: never retried); 404 `channel_not_configured`; 502 `delivery_failed` (try again).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkerNoteReq {
    pub tenant: Uuid,
    pub job_id: Uuid,
    pub thread: ConversationRef,
    pub binding_id: Uuid,
    pub text: String,
    #[serde(default)]
    pub check_first: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExcerptLine {
    pub role: String,
    pub text: String,
}

/// The handover item a worker channel is told about.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkerItem {
    pub id: String,
    pub subject: String,
    pub customer: String,
    pub channel: String,
    pub reason: String,
    pub priority: String,
    #[serde(default)]
    pub excerpt: Vec<ExcerptLine>,
    /// The customer's email when their party has one (an `email` party identity): a ticket's requester (R9).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub customer_email: Option<String>,
}

/// One worker-side binding to notify (Helpdesk `{base_url, service_user}`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkerTarget {
    pub binding_id: Uuid,
    pub kind: String,
    #[serde(default)]
    pub settings: Value,
}

/// `POST /internal/notify-workers`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NotifyWorkersReq {
    pub tenant: Uuid,
    pub job_id: Uuid,
    pub item: WorkerItem,
    pub targets: Vec<WorkerTarget>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NotifyResult {
    pub binding_id: Uuid,
    pub ok: bool,
    /// The worker thread the notification opened; agent-api stores it as a worker `channel_conversation_refs` row.
    pub thread: Option<ConversationRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NotifyWorkersResp {
    pub results: Vec<NotifyResult>,
}

// ---- the client ----------------------------------------------------------------------------------------------------

/// A failed seam call. `Code` is agent-api's `{error}` code (e.g. `provider_not_configured`, `job_not_found`).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CoreError {
    #[error("core unavailable: {0}")]
    Unavailable(String),
    #[error("core refused: {0}")]
    Code(String),
}

/// The internal key, shared by both directions. Debug never shows it.
#[derive(Clone)]
pub struct InternalKey(String);

impl InternalKey {
    /// `None` when shorter than [`MIN_KEY_LEN`].
    pub fn new(k: impl Into<String>) -> Option<Self> {
        let k = k.into();
        (k.len() >= MIN_KEY_LEN).then_some(Self(k))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `Authorization: Bearer <key>` in constant time (HMAC tag comparison).
    pub fn verify(&self, headers: &HeaderMap) -> bool {
        let Some(got) = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
        else {
            return false;
        };
        // `ring::constant_time` is deprecated in ring 0.17.14: compare HMAC tags instead (`hmac::verify` is
        // constant-time), under a key derived from the internal key.
        let k = self.derive("bearer-compare");
        let want = ring::hmac::sign(&k, self.0.as_bytes());
        ring::hmac::verify(&k, got.as_bytes(), want.as_ref()).is_ok()
    }

    /// A key for one purpose (`HMAC-SHA256(key, purpose)`), so a token minted for one use never verifies as another.
    pub fn derive(&self, purpose: &str) -> ring::hmac::Key {
        let root = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, self.0.as_bytes());
        let tag = ring::hmac::sign(&root, purpose.as_bytes());
        ring::hmac::Key::new(ring::hmac::HMAC_SHA256, tag.as_ref())
    }
}

impl fmt::Debug for InternalKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("InternalKey(<redacted>)")
    }
}

/// The edge's client for agent-api's internal seam.
pub struct CoreLink {
    pub base_url: String,
    pub key: InternalKey,
    pub http: reqwest::Client,
}

impl fmt::Debug for CoreLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CoreLink")
            .field("base_url", &self.base_url)
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

impl CoreLink {
    pub fn new(base_url: impl Into<String>, key: InternalKey) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .build()?;
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            key,
            http,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    async fn call<T: serde::de::DeserializeOwned>(
        &self,
        req: reqwest::RequestBuilder,
        timeout: Duration,
    ) -> Result<T, CoreError> {
        let resp = req
            .bearer_auth(self.key.as_str())
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| {
                CoreError::Unavailable(if e.is_timeout() {
                    "timeout".into()
                } else {
                    "connect".into()
                })
            })?;
        let status = resp.status();
        let body = resp
            .bytes()
            .await
            .map_err(|_| CoreError::Unavailable("read".into()))?;
        if !status.is_success() {
            return Err(error_of(status, &body));
        }
        serde_json::from_slice(&body).map_err(|_| CoreError::Unavailable("invalid_body".into()))
    }

    pub async fn bindings(&self) -> Result<Vec<CoreBinding>, CoreError> {
        let r: BindingsResp = self
            .call(
                self.http.get(self.url("/api/internal/channel/bindings")),
                CALL_TIMEOUT,
            )
            .await?;
        Ok(r.bindings)
    }

    pub async fn inbound(&self, req: &InboundReq) -> Result<InboundResp, CoreError> {
        self.call(
            self.http
                .post(self.url("/api/internal/channel/inbound"))
                .json(req),
            INBOUND_TIMEOUT,
        )
        .await
    }

    pub async fn staff(&self, req: &StaffReq) -> Result<StaffResp, CoreError> {
        self.call(
            self.http
                .post(self.url("/api/internal/channel/staff"))
                .json(req),
            CALL_TIMEOUT,
        )
        .await
    }

    pub async fn staff_state(&self, req: &StaffStateReq) -> Result<StaffStateResp, CoreError> {
        self.call(
            self.http
                .post(self.url("/api/internal/channel/staff-state"))
                .json(req),
            CALL_TIMEOUT,
        )
        .await
    }

    /// `handover/notice-delivered`: 2xx with no body expected.
    pub async fn notice_delivered(&self, req: &NoticeDeliveredReq) -> Result<(), CoreError> {
        let resp = self
            .http
            .post(self.url("/api/internal/channel/handover/notice-delivered"))
            .json(req)
            .bearer_auth(self.key.as_str())
            .timeout(CALL_TIMEOUT)
            .send()
            .await
            .map_err(|e| {
                CoreError::Unavailable(if e.is_timeout() {
                    "timeout".into()
                } else {
                    "connect".into()
                })
            })?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let body = resp
            .bytes()
            .await
            .map_err(|_| CoreError::Unavailable("read".into()))?;
        Err(error_of(status, &body))
    }

    pub async fn webhook_status(
        &self,
        req: &WebhookStatusReq,
    ) -> Result<WebhookStatusResp, CoreError> {
        self.call(
            self.http
                .post(self.url("/api/internal/channel/webhook-status"))
                .json(req),
            CALL_TIMEOUT,
        )
        .await
    }

    pub async fn history(
        &self,
        conversation_id: &str,
        scope: HistoryScope,
        after_ms: Option<i64>,
    ) -> Result<HistoryResp, CoreError> {
        let id = utf8_percent(conversation_id);
        let mut path = format!(
            "/api/internal/channel/conversations/{id}/messages?tenant={}&job_id={}&binding_id={}",
            scope.tenant, scope.job_id, scope.binding_id
        );
        if let Some(a) = after_ms {
            path.push_str(&format!("&after_ms={a}"));
        }
        self.call(self.http.get(self.url(&path)), CALL_TIMEOUT)
            .await
    }

    /// The web chat customer stream of `conversation_id` (crate::conversations::webchat_stream): the open SSE
    /// response, its body still to be read. No total timeout (the stream stays open); [`CALL_TIMEOUT`] bounds the
    /// wait for its headers. A refusal is [`CoreError::Code`] (`binding_not_found`, `conversation_not_found`).
    pub async fn webchat_events(
        &self,
        conversation_id: &str,
        scope: HistoryScope,
    ) -> Result<reqwest::Response, CoreError> {
        let id = utf8_percent(conversation_id);
        let path = format!(
            "/api/internal/channel/webchat/conversations/{id}/events?tenant={}&job_id={}&binding_id={}",
            scope.tenant, scope.job_id, scope.binding_id
        );
        let send = self
            .http
            .get(self.url(&path))
            .bearer_auth(self.key.as_str())
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .send();
        let resp = match tokio::time::timeout(CALL_TIMEOUT, send).await {
            Err(_) => return Err(CoreError::Unavailable("timeout".into())),
            Ok(Err(_)) => return Err(CoreError::Unavailable("connect".into())),
            Ok(Ok(r)) => r,
        };
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let body = tokio::time::timeout(CALL_TIMEOUT, resp.bytes())
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        Err(error_of(status, &body))
    }
}

// ---- web chat sessions (plan B13 §3) -------------------------------------------------------------------------------

/// `POST /api/internal/channel/webchat/sessions`.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct WebChatSessionReq {
    pub binding_id: Uuid,
    pub widget_key: String,
    /// The browser's `Origin` (already allow-listed by the edge; agent-api checks the binding's own list).
    pub origin: String,
    /// Hex `sha256(salt ‖ client ip)`: what agent-api keys its limits and stores on.
    pub ip_hash: String,
    /// The visitor's Turnstile token (B14 §2), when the widget solved a challenge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turnstile_token: Option<String>,
    /// The raw client IP, ONLY for Turnstile's `remoteip`: agent-api never stores or logs it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_ip: Option<String>,
    /// The host's signed known-customer assertion (B7): verified by agent-api, never stored or logged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub customer_assertion: Option<String>,
}

impl fmt::Debug for WebChatSessionReq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebChatSessionReq")
            .field("binding_id", &self.binding_id)
            .field("origin", &self.origin)
            .field("turnstile_token", &self.turnstile_token.is_some())
            .field("customer_assertion", &self.customer_assertion.is_some())
            .finish_non_exhaustive()
    }
}

fn yes() -> bool {
    true
}

/// 201 from the sessions route. `session_token` is `<session_id>.<base64url secret>`: the edge reads the session id
/// (the web chat party `wc:<session_id>`) from the token's prefix on every message.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct WebChatSessionResp {
    pub session_id: String,
    pub session_token: String,
    pub expires_at: Value,
    /// `settings.display` of the binding (`{title, greeting, colour}`), passed through to the widget.
    #[serde(default)]
    pub display: Value,
    #[serde(default)]
    pub csat_enabled: bool,
    /// The binding lets customers attach photos and PDFs (`settings.attachments.customer`); an older core that does
    /// not say means yes. The widget hides its paperclip when false.
    #[serde(default = "yes")]
    pub attachments_enabled: bool,
    /// The session opened as a known customer (a verified assertion).
    #[serde(default)]
    pub identified: bool,
    /// The verified customer's display name, for the widget's greeting (identified sessions only).
    #[serde(default)]
    pub greeting_name: Option<String>,
    /// `settings.identity.sign_in_url`, for the widget's "Sign in" button.
    #[serde(default)]
    pub sign_in_url: Option<String>,
}

impl fmt::Debug for WebChatSessionResp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebChatSessionResp")
            .field("session_id", &self.session_id)
            .field("session_token", &"<redacted>")
            .field("csat_enabled", &self.csat_enabled)
            .field("identified", &self.identified)
            .finish_non_exhaustive()
    }
}

/// `POST /api/internal/channel/webchat/session-check` (B7): is the session still admitted (live, not blocked, an
/// identified one inside `session_hours`)? The edge asks before it streams or reads a conversation's history, so a
/// conversation token cannot outlive its session, nor be paired with another conversation's session. 200 `{ok: true}`; 401 `session_invalid`; 403 `session_blocked`.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct WebChatSessionCheckReq {
    pub tenant: Uuid,
    pub job_id: Uuid,
    pub binding_id: Uuid,
    /// The conversation the token named: the session must be that conversation's own.
    pub conversation_id: String,
    pub session_token: String,
}

impl fmt::Debug for WebChatSessionCheckReq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebChatSessionCheckReq")
            .field("binding_id", &self.binding_id)
            .field("session_token", &"<redacted>")
            .finish_non_exhaustive()
    }
}

/// A failed seam call that keeps agent-api's status and `retry_after_s`, for routes whose refusals the edge maps
/// 1:1 to the public answer (web chat sessions and messages) or that decide the provider's ack (R6).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SeamError {
    #[error("core unavailable: {0}")]
    Unavailable(String),
    #[error("core refused ({status}): {code}")]
    Refused {
        status: u16,
        code: String,
        retry_after_s: Option<u64>,
        /// `error.site_key` of a 428 `turnstile_required` (B14 §2), passed on to the widget.
        site_key: Option<String>,
    },
}

impl SeamError {
    /// True when the message may not be stored and the provider should retry (R6: the edge answers 503): core
    /// unreachable, timed out or 5xx. `delivery_failed` (502 from `staff`) is not: the reply is stored, only its
    /// delivery to the customer failed.
    pub fn retryable(&self) -> bool {
        match self {
            SeamError::Unavailable(_) => true,
            SeamError::Refused { status, code, .. } => *status >= 500 && code != "delivery_failed",
        }
    }
}

impl From<SeamError> for CoreError {
    fn from(e: SeamError) -> Self {
        match e {
            SeamError::Unavailable(r) => CoreError::Unavailable(r),
            SeamError::Refused { code, .. } => CoreError::Code(code),
        }
    }
}

impl CoreLink {
    async fn call_seam<T: serde::de::DeserializeOwned>(
        &self,
        req: reqwest::RequestBuilder,
        timeout: Duration,
    ) -> Result<T, SeamError> {
        let resp = req
            .bearer_auth(self.key.as_str())
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| {
                SeamError::Unavailable(if e.is_timeout() {
                    "timeout".into()
                } else {
                    "connect".into()
                })
            })?;
        let status = resp.status();
        let retry_header = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
        let body = resp
            .bytes()
            .await
            .map_err(|_| SeamError::Unavailable("read".into()))?;
        if !status.is_success() {
            return Err(refusal_of(status, retry_header, &body));
        }
        serde_json::from_slice(&body).map_err(|_| SeamError::Unavailable("invalid_body".into()))
    }

    /// The session check behind a conversation's stream and history ([`WebChatSessionCheckReq`]).
    pub async fn webchat_session_check(
        &self,
        req: &WebChatSessionCheckReq,
    ) -> Result<Value, SeamError> {
        self.call_seam(
            self.http
                .post(self.url("/api/internal/channel/webchat/session-check"))
                .json(req),
            CALL_TIMEOUT,
        )
        .await
    }

    pub async fn webchat_session(
        &self,
        req: &WebChatSessionReq,
    ) -> Result<WebChatSessionResp, SeamError> {
        self.call_seam(
            self.http
                .post(self.url("/api/internal/channel/webchat/sessions"))
                .json(req),
            CALL_TIMEOUT,
        )
        .await
    }

    /// R6 persist-before-200: `inbound` with `accept_only` set (whatever `req` says), awaited before the provider is
    /// acked, within `timeout` (the connector's [`accept_timeout`](super::connector::ChannelConnector::accept_timeout); a
    /// timeout is [`SeamError::Unavailable`], so retryable). A refusal keeps its status, so
    /// [`SeamError::retryable`] tells "not stored, 503" from "refused, ack".
    pub async fn accept(
        &self,
        req: &InboundReq,
        timeout: Duration,
    ) -> Result<InboundResp, SeamError> {
        let req = InboundReq {
            accept_only: true,
            ..req.clone()
        };
        self.call_seam(
            self.http
                .post(self.url("/api/internal/channel/inbound"))
                .json(&req),
            timeout,
        )
        .await
    }

    /// `staff`, keeping the refusal's status (R6: the Helpdesk webhook answers 503 when the reply is not stored).
    pub async fn staff_checked(&self, req: &StaffReq) -> Result<StaffResp, SeamError> {
        self.call_seam(
            self.http
                .post(self.url("/api/internal/channel/staff"))
                .json(req),
            CALL_TIMEOUT,
        )
        .await
    }

    /// `staff-state`, keeping the refusal's status (as [`CoreLink::staff_checked`]).
    pub async fn staff_state_checked(
        &self,
        req: &StaffStateReq,
    ) -> Result<StaffStateResp, SeamError> {
        self.call_seam(
            self.http
                .post(self.url("/api/internal/channel/staff-state"))
                .json(req),
            CALL_TIMEOUT,
        )
        .await
    }

    /// `inbound` for web chat: like [`CoreLink::inbound`], but a refusal keeps its status and `retry_after_s`.
    pub async fn webchat_inbound(&self, req: &InboundReq) -> Result<InboundResp, SeamError> {
        self.call_seam(
            self.http
                .post(self.url("/api/internal/channel/inbound"))
                .json(req),
            INBOUND_TIMEOUT,
        )
        .await
    }

    /// SW-9: one customer file of a web chat conversation to core's validate + store
    /// (`POST /api/internal/channel/webchat/attachments`, multipart). The tenant and job are the binding's; core takes
    /// the conversation from the session row and refuses a session that is not that conversation's own. `bytes` is
    /// sent as is: core is the gate (magic bytes, re-encode, limits). A 422 `attachment_refused` is
    /// [`AttachmentUploadError::Refused`] with core's closed `reason`; everything else keeps its status.
    pub async fn webchat_attachment_upload(
        &self,
        scope: HistoryScope,
        conversation_id: &str,
        session_token: &str,
        bytes: Vec<u8>,
        declared_name: &str,
    ) -> Result<AttachmentRef, AttachmentUploadError> {
        let name: String = declared_name
            .chars()
            .filter(|c| !c.is_control())
            .take(200)
            .collect();
        let file = reqwest::multipart::Part::bytes(bytes)
            .file_name(if name.is_empty() {
                "file".to_string()
            } else {
                name
            })
            .mime_str("application/octet-stream")
            .map_err(|_| SeamError::Unavailable("invalid_body".into()))?;
        let form = reqwest::multipart::Form::new()
            .text("tenant", scope.tenant.to_string())
            .text("job_id", scope.job_id.to_string())
            .text("binding_id", scope.binding_id.to_string())
            .text("conversation", conversation_id.to_string())
            .text("session_token", session_token.to_string())
            .part("file", file);
        let resp = self
            .http
            .post(self.url("/api/internal/channel/webchat/attachments"))
            .multipart(form)
            .bearer_auth(self.key.as_str())
            .timeout(UPLOAD_TIMEOUT)
            .send()
            .await
            .map_err(|e| {
                SeamError::Unavailable(if e.is_timeout() {
                    "timeout".into()
                } else {
                    "connect".into()
                })
            })?;
        let status = resp.status();
        let retry_header = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
        let body = resp
            .bytes()
            .await
            .map_err(|_| SeamError::Unavailable("read".into()))?;
        if status.is_success() {
            return serde_json::from_slice(&body)
                .map_err(|_| SeamError::Unavailable("invalid_body".into()).into());
        }
        if let Some(reason) = refused_reason(status, &body) {
            return Err(AttachmentUploadError::Refused { reason });
        }
        Err(refusal_of(status, retry_header, &body).into())
    }

    /// SW-9: the bytes of attachment `att` of `conversation_id` (`GET /api/internal/channel/attachments/{att}`),
    /// which core serves only when the attachment belongs to that conversation and the conversation to the binding of
    /// `scope`: `(media_type, display_name, bytes)`. Anything else is the uniform `Refused { status: 404, .. }`.
    pub async fn webchat_attachment_fetch(
        &self,
        scope: HistoryScope,
        conversation_id: &str,
        att: Uuid,
    ) -> Result<(String, String, Vec<u8>), SeamError> {
        let path = format!(
            "/api/internal/channel/attachments/{att}?tenant={}&job_id={}&binding_id={}&conversation={}",
            scope.tenant,
            scope.job_id,
            scope.binding_id,
            utf8_percent(conversation_id)
        );
        self.fetch_path(&path).await
    }

    /// SW-9 (Telegram): the bytes of attachment `att` of a customer conversation of `scope`'s job, named by the
    /// CHANNEL's conversation id as the deliver request carries it (core resolves it, or accepts its own id):
    /// `(media_type, display_name, bytes)`. Anything core will not serve is the uniform 404.
    pub async fn attachment_fetch(
        &self,
        scope: &crate::interaction::Scope,
        conversation_id: &str,
        att_id: Uuid,
    ) -> Result<(String, String, Vec<u8>), SeamError> {
        let path = format!(
            "/api/internal/channel/attachments/{att_id}?tenant={}&job_id={}&conversation={}",
            scope.tenant,
            scope.job_id,
            utf8_percent(conversation_id)
        );
        self.fetch_path(&path).await
    }

    async fn fetch_path(&self, path: &str) -> Result<(String, String, Vec<u8>), SeamError> {
        let resp = self
            .http
            .get(self.url(path))
            .bearer_auth(self.key.as_str())
            .timeout(UPLOAD_TIMEOUT)
            .send()
            .await
            .map_err(|e| {
                SeamError::Unavailable(if e.is_timeout() {
                    "timeout".into()
                } else {
                    "connect".into()
                })
            })?;
        let status = resp.status();
        let media_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let name = resp
            .headers()
            .get(ATTACHMENT_NAME_HEADER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| {
                base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, v).ok()
            })
            .and_then(|b| String::from_utf8(b).ok())
            .unwrap_or_default();
        let body = resp
            .bytes()
            .await
            .map_err(|_| SeamError::Unavailable("read".into()))?;
        if !status.is_success() {
            return Err(refusal_of(status, None, &body));
        }
        if body.len() > MAX_FETCH_BYTES {
            return Err(SeamError::Unavailable("too_large".into()));
        }
        Ok((media_type, name, body.to_vec()))
    }
}

/// How long an attachment upload or download may take (10 MiB plus validation).
pub const UPLOAD_TIMEOUT: Duration = Duration::from_secs(30);
/// The header on core's attachment bytes that carries the display name: base64url (no padding) of its UTF-8.
pub const ATTACHMENT_NAME_HEADER: &str = "x-attachment-name";
/// A fetched attachment above this is refused (core stores at most 10 MiB).
const MAX_FETCH_BYTES: usize = 11 * 1024 * 1024;

/// A failed attachment upload: core refused the file (`reason` is one of the closed
/// [`crate::attachments::validate::reason`] codes, safe to show a customer), or the call failed otherwise.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AttachmentUploadError {
    #[error("attachment refused: {reason}")]
    Refused { reason: String },
    #[error(transparent)]
    Seam(#[from] SeamError),
}

/// `error.reason` of a 422 `attachment_refused` answer.
fn refused_reason(status: reqwest::StatusCode, body: &[u8]) -> Option<String> {
    if status != reqwest::StatusCode::UNPROCESSABLE_ENTITY {
        return None;
    }
    let v: Value = serde_json::from_slice(body).ok()?;
    let e = v.get("error")?;
    (e.get("code")?.as_str()? == "attachment_refused")
        .then(|| e.get("reason")?.as_str().map(str::to_string))?
}

/// The refusal behind a non-2xx seam answer: the `{error}` code of either envelope, `retry_after_s` from the body
/// (top level or inside `error`) else the `Retry-After` header. A 503 without a code is `unavailable`; any other
/// 5xx without one is [`SeamError::Unavailable`].
fn refusal_of(status: reqwest::StatusCode, retry_header: Option<u64>, body: &[u8]) -> SeamError {
    let v = serde_json::from_slice::<Value>(body).ok();
    let code = v.as_ref().and_then(|v| {
        let e = v.get("error")?;
        e.as_str()
            .or_else(|| e.get("code").and_then(Value::as_str))
            .map(str::to_string)
    });
    let retry_after_s = v
        .as_ref()
        .and_then(|v| {
            v.get("retry_after_s")
                .or_else(|| v.get("error").and_then(|e| e.get("retry_after_s")))
                .and_then(Value::as_u64)
        })
        .or(retry_header);
    let site_key = v
        .as_ref()
        .and_then(|v| v.get("error")?.get("site_key")?.as_str())
        .map(str::to_string);
    let status_u16 = status.as_u16();
    match code.filter(|c| !c.is_empty()) {
        Some(code) => SeamError::Refused {
            status: status_u16,
            code,
            retry_after_s,
            site_key,
        },
        None if status == reqwest::StatusCode::SERVICE_UNAVAILABLE => SeamError::Refused {
            status: status_u16,
            code: "unavailable".into(),
            retry_after_s,
            site_key,
        },
        None if status.is_server_error() => SeamError::Unavailable(format!("status {status_u16}")),
        None => SeamError::Refused {
            status: status_u16,
            code: format!("status_{status_u16}"),
            retry_after_s,
            site_key,
        },
    }
}

/// `{error: "code"}` or agent-api's envelope `{error: {code}}`; otherwise the status.
fn error_of(status: reqwest::StatusCode, body: &[u8]) -> CoreError {
    let code = serde_json::from_slice::<Value>(body).ok().and_then(|v| {
        let e = v.get("error")?;
        e.as_str()
            .or_else(|| e.get("code").and_then(Value::as_str))
            .map(str::to_string)
    });
    match code {
        Some(c) if !c.is_empty() => CoreError::Code(c),
        _ if status.is_server_error() => {
            CoreError::Unavailable(format!("status {}", status.as_u16()))
        }
        _ => CoreError::Code(format!("status_{}", status.as_u16())),
    }
}

/// Path-segment encoding (ids are uuids or `wc_…`; anything else is escaped).
fn utf8_percent(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn inbound_answers_parse_by_status() {
        let a: InboundResp = serde_json::from_value(
            json!({"status": "answered", "conversation_id": "c", "case_id": "CS-1", "text": "hi"}),
        )
        .unwrap();
        assert!(matches!(a, InboundResp::Answered { .. }));
        let q: InboundResp =
            serde_json::from_value(json!({"status": "queued", "conversation_id": "c",
            "case_id": "CS-1", "item_id": "HO-1", "notice": true}))
            .unwrap();
        assert!(matches!(q, InboundResp::Queued { notice: true, .. }));
        let d: InboundResp = serde_json::from_value(json!({"status": "duplicate"})).unwrap();
        assert_eq!(
            d,
            InboundResp::Duplicate {
                conversation_id: None
            }
        );
        let c: InboundResp =
            serde_json::from_value(json!({"status": "csat_recorded", "text": "thanks"})).unwrap();
        assert!(matches!(c, InboundResp::CsatRecorded { .. }));
    }

    #[test]
    fn accept_only_is_omitted_when_false_and_accepted_parses() {
        let a: InboundResp = serde_json::from_value(
            json!({"status": "accepted", "conversation_id": "c", "case_id": "CS-1"}),
        )
        .unwrap();
        assert!(matches!(a, InboundResp::Accepted { .. }));
        let r: InboundReq =
            serde_json::from_value(json!({"binding_id": Uuid::nil(), "kind": "telegram",
            "external_id": "1", "tenant": Uuid::nil(), "job_id": Uuid::nil(), "job_key": "k",
            "conversation": {"channel": "telegram", "id": "42"}, "message_id": "9",
            "party": {"id": "42", "verified": true}, "text": "hi"}))
            .unwrap();
        assert!(!r.accept_only, "an older edge's call answers as before");
        assert!(
            serde_json::to_value(&r)
                .unwrap()
                .get("accept_only")
                .is_none()
        );
    }

    #[test]
    fn retryable_is_unreachable_or_5xx_but_not_a_stored_undelivered_reply() {
        let refused = |status: u16, code: &str| SeamError::Refused {
            status,
            code: code.into(),
            retry_after_s: None,
            site_key: None,
        };
        assert!(SeamError::Unavailable("connect".into()).retryable());
        assert!(refused(503, "unavailable").retryable());
        assert!(refused(500, "internal").retryable());
        assert!(!refused(502, "delivery_failed").retryable());
        assert!(!refused(404, "binding_not_found").retryable());
        assert!(!refused(403, "not_staff").retryable());
        assert_eq!(
            CoreError::from(refused(403, "not_staff")),
            CoreError::Code("not_staff".into())
        );
    }

    #[test]
    fn error_codes_come_from_either_envelope() {
        let s = reqwest::StatusCode::CONFLICT;
        assert_eq!(
            error_of(s, br#"{"error":"job_not_found"}"#),
            CoreError::Code("job_not_found".into())
        );
        assert_eq!(
            error_of(s, br#"{"error":{"code":"provider_not_configured"}}"#),
            CoreError::Code("provider_not_configured".into())
        );
        assert!(matches!(
            error_of(reqwest::StatusCode::BAD_GATEWAY, b"nope"),
            CoreError::Unavailable(_)
        ));
    }

    #[test]
    fn key_is_long_redacted_and_compared_exactly() {
        assert!(InternalKey::new("short").is_none());
        let k = InternalKey::new("k".repeat(32)).unwrap();
        assert!(!format!("{k:?}").contains("kkkk"));
        let mut h = HeaderMap::new();
        assert!(!k.verify(&h));
        h.insert(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {}", "k".repeat(32)).parse().unwrap(),
        );
        assert!(k.verify(&h));
        h.insert(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {}", "k".repeat(33)).parse().unwrap(),
        );
        assert!(!k.verify(&h));
    }

    #[test]
    fn credentials_never_show_in_debug() {
        let b: CoreBinding = serde_json::from_value(json!({"id": Uuid::nil(), "kind": "telegram",
            "side": "worker", "external_id": "x", "tenant": Uuid::nil(), "job_id": Uuid::nil(),
            "job_key": "k", "credential": {"app_password": "s3cr3t"}}))
        .unwrap();
        assert_eq!(b.env, "prod");
        assert!(!format!("{b:?}").contains("s3cr3t"));
    }
}
