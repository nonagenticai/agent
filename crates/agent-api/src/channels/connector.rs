//! The generic channel connector interface (B9 T1, D96; two-sided per D104, B11). A connector turns one provider's
//! webhook into an [`Inbound`] customer (or worker) message and delivers [`Outbound`] messages back to a
//! [`ConversationRef`]. It knows nothing about the runtime: the edge (crate::channels::edge) puts every inbound
//! through the interaction seam (D101) and hands the answer to [`ChannelConnector::deliver`].
//!
//! Identity mapping (party resolution, B9 T3) is not a connector concern. Message text is customer data: never in a
//! log line, a metric label or an error message.
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::bindings::Binding;
use super::core::{ACCEPT_TIMEOUT, WorkerItem, WorkerTarget};
use super::telegram::WebhookInfo;
pub use crate::interaction::model::ConversationRef;
use crate::interaction::model::Scope;

/// Every connector kind the edge can route. A new kind = a new impl + (phase B) a platform config screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ChannelKind {
    WebChat,
    Telegram,
    /// Frappe Helpdesk, worker side only (B12, R9): a handover opens an HD Ticket; agent replies and resolve/close
    /// come back through a signed Frappe Webhook (crate::channels::helpdesk).
    Helpdesk,
}

impl ChannelKind {
    /// The interaction `channel` value and the webhook path segment (`/api/channels/{kind}/webhook`).
    pub fn as_str(self) -> &'static str {
        match self {
            ChannelKind::WebChat => "web_chat",
            ChannelKind::Telegram => "telegram",
            ChannelKind::Helpdesk => "helpdesk",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        [
            ChannelKind::WebChat,
            ChannelKind::Telegram,
            ChannelKind::Helpdesk,
        ]
        .into_iter()
        .find(|k| k.as_str() == s)
    }
}

/// Which side of an escalated conversation a connector serves (D104): customers, or the tenant's service workers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Customer,
    Worker,
}

/// One provider webhook call, as the edge received it. The body is the raw bytes, because every provider signs them.
#[derive(Clone, Debug)]
pub struct IngressRequest {
    pub method: Method,
    pub headers: HeaderMap,
    /// The raw query string (a provider's verification GET may carry its challenge there).
    pub query: Option<String>,
    pub body: Bytes,
    /// The binding a BYO webhook path (`/api/channels/{kind}/webhook/{binding_id}`) preselected (B12); connectors may
    /// ignore it, the edge uses it to scope the call's messages.
    pub binding: Option<Binding>,
}

impl IngressRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    /// One query parameter, percent-decoded.
    pub fn query_param(&self, name: &str) -> Option<String> {
        self.query.as_deref().and_then(|q| {
            q.split('&').find_map(|pair| {
                let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                (percent_decode(k) == name).then(|| percent_decode(v))
            })
        })
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => match (hex(b[i + 1]), hex(b[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h << 4 | l);
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(c: u8) -> Option<u8> {
    (c as char).to_digit(16).map(|d| d as u8)
}

/// What [`ChannelConnector::authenticate`] proved about the caller: verified claims the connector's `ingest` needs
/// (e.g. a signed token's claims). Never message text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IngressAuth {
    pub claims: BTreeMap<String, String>,
}

/// The external identity behind an inbound message (a Telegram user id, a web chat session).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalIdentity {
    pub channel: ChannelKind,
    pub subject: String,
    pub display_name: Option<String>,
    /// True when the provider vouches for the subject (every phase-A kind: the webhook is signed).
    pub verified: bool,
}

/// One message from the provider, ready for the seam.
#[derive(Clone, Debug, PartialEq)]
pub struct Inbound {
    /// The provider's message id (dedupe key; providers retry). It is the seam's `message_id` (B12).
    pub external_id: String,
    /// The provider-side workspace the message came from, the binding key (e.g. a Telegram bot
    /// id); `None` when the provider did not name one. The edge resolves core bindings with it (B12).
    pub workspace: Option<String>,
    pub text: String,
    pub identity: ExternalIdentity,
    /// The job the provider-side workspace is bound to; `None` = not bound (the edge answers "not connected").
    pub scope: Option<Scope>,
    /// Where answers go.
    pub conversation: ConversationRef,
}

/// One staff message posted in a worker thread (D104, B12): a reply under a handover card in a worker channel. The edge
/// sends it to agent-api's `staff` route (never to the runtime) when [`StaffInbound::target`] has a worker binding.
#[derive(Clone, Debug, PartialEq)]
pub struct StaffInbound {
    /// The provider's message id (dedupe key; agent-api's `message_id`).
    pub external_id: String,
    /// The worker target the thread lives in (e.g. a helpdesk site): the worker binding's key.
    pub target: String,
    /// The provider workspace the channel belongs to (if any) when the event names it: a
    /// pending worker binding's `external_id` for `verify <code>`.
    pub workspace: Option<String>,
    /// The worker thread (the `ConversationRef` `notify_worker` returned); it resolves the handover item.
    pub thread: ConversationRef,
    /// Where the edge posts its short confirmation ("Sent to the customer.").
    pub reply_to: ConversationRef,
    /// The provider's user id (the provider's own).
    pub staff_id: String,
    /// A stable directory id when the provider has one (if any); else the provider user id.
    pub staff_external_id: String,
    pub staff_name: Option<String>,
    /// The email the activity itself carried (rarely); [`ChannelConnector::staff_email`] resolves it otherwise.
    pub staff_email: Option<String>,
    pub text: String,
    /// False for a new top-level post in the channel (not a reply under a card): in a worker-bound channel the edge
    /// acks and drops it, so staff chatter never reaches the customer path.
    pub in_thread: bool,
}

/// A direct HTTP answer to the provider.
#[derive(Clone, Debug, PartialEq)]
pub struct EdgeReply {
    pub status: StatusCode,
    pub content_type: Option<&'static str>,
    pub body: Bytes,
}

impl EdgeReply {
    /// 200, empty body: the provider's ack.
    pub fn ok() -> Self {
        Self {
            status: StatusCode::OK,
            content_type: None,
            body: Bytes::new(),
        }
    }

    pub fn text(body: impl Into<String>) -> Self {
        Self {
            status: StatusCode::OK,
            content_type: Some("text/plain; charset=utf-8"),
            body: Bytes::from(body.into()),
        }
    }

    pub fn json(v: &serde_json::Value) -> Self {
        Self {
            status: StatusCode::OK,
            content_type: Some("application/json"),
            body: Bytes::from(v.to_string()),
        }
    }
}

impl IntoResponse for EdgeReply {
    fn into_response(self) -> Response {
        let mut r = (self.status, self.body).into_response();
        if let Some(ct) = self.content_type {
            r.headers_mut().insert(
                axum::http::header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static(ct),
            );
        }
        r
    }
}

/// What a webhook call turned into.
#[derive(Clone, Debug, PartialEq)]
pub enum Ingress {
    /// Answer the provider directly; nothing enters the platform (a verification handshake, an ignored event: a bot's
    /// own message, an edit, a status update).
    Reply(EdgeReply),
    /// One or more messages (Meta batches): ack the provider with `ack` now (providers time out in seconds), answer
    /// each later through `deliver`.
    Messages {
        ack: EdgeReply,
        inbound: Vec<Inbound>,
    },
    /// One customer message that carries a photo or a file (SW-9): the edge downloads it through the connector
    /// ([`ChannelConnector::download_media`]), hands the bytes to core for validation and storage, and sends the stored
    /// attachment's id with the message. Everything else about the message is as in [`Ingress::Messages`].
    Media {
        ack: EdgeReply,
        inbound: Box<Inbound>,
        media: InboundMedia,
    },
    /// Staff replies in worker threads (worker-side connectors only): ack now, hand each to agent-api's `staff` route.
    Staff {
        ack: EdgeReply,
        staff: Vec<StaffInbound>,
    },
}

/// A message out to a conversation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outbound {
    Text(String),
    /// "The agent is typing" where the provider has it; a no-op elsewhere.
    Typing,
    /// The conversation was handed to a human (D104); `eta_minutes` from the handover policy when known.
    HandoverNotice {
        eta_minutes: Option<u32>,
    },
    /// "How did we do? Reply 1–5" after a resolved handover (B12); the text comes from agent-api.
    CsatRequest {
        text: String,
    },
    /// A staff-attached photo or PDF (SW-9) for a connector that uploads files (Telegram). `caption` is the reply text
    /// (markdown, as in [`Outbound::Text`]); the connector decides whether it fits as a caption. Connectors without
    /// file support never see it: the edge refuses with `unsupported_attachment` first.
    Attachment {
        media_type: String,
        display_name: String,
        bytes: Blob,
        caption: Option<String>,
    },
}

/// The bytes of a file in flight. `Debug` shows the size only, so a logged [`Outbound`] never dumps a file.
#[derive(Clone, PartialEq, Eq)]
pub struct Blob(pub Vec<u8>);

impl fmt::Debug for Blob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Blob({} bytes)", self.0.len())
    }
}

/// Which kind of media an inbound message carried.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKind {
    Photo,
    File,
}

impl MediaKind {
    /// The message text of a message that is only media.
    pub fn placeholder(self) -> &'static str {
        match self {
            MediaKind::Photo => "(photo)",
            MediaKind::File => "(file)",
        }
    }

    fn label(self) -> &'static str {
        match self {
            MediaKind::Photo => "photo",
            MediaKind::File => "file",
        }
    }
}

/// A photo or file the provider says an inbound message carries (not downloaded yet).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboundMedia {
    pub kind: MediaKind,
    /// The provider's handle for the file (Telegram `file_id`). Not secret, but never logged.
    pub file_id: String,
    /// The provider's file name, only ever a hint for the display name.
    pub declared_name: Option<String>,
    /// The provider's caption (the message text); empty when there is none.
    pub caption: String,
    /// Set when the connector already knows the file is not acceptable (e.g. `too_large`): nothing is downloaded.
    pub skip: Option<&'static str>,
}

impl InboundMedia {
    /// The message text when the file was not accepted: a short plain-words reason, after the caption if there is one.
    pub fn refused_text(&self, reason: &str) -> String {
        let note = format!(
            "({} — not accepted: {})",
            self.kind.label(),
            friendly_refusal(reason)
        );
        if self.caption.trim().is_empty() {
            note
        } else {
            format!("{}\n{note}", self.caption.trim_end())
        }
    }
}

/// A downloaded inbound file, size-capped, not yet validated (core's validation decides what it is).
#[derive(Clone, PartialEq, Eq)]
pub struct MediaFile {
    pub bytes: Vec<u8>,
    pub declared_name: String,
}

impl fmt::Debug for MediaFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MediaFile({} bytes)", self.bytes.len())
    }
}

/// Plain words for a refusal reason (`crate::attachments::validate::reason`, plus the edge's own).
pub fn friendly_refusal(reason: &str) -> &'static str {
    match reason {
        "too_large" => "it is over 10 MB",
        "unsupported_type" => "only photos (JPEG, PNG, WebP) and PDF files can be received",
        "too_many_pixels" => "the picture is too large",
        "animated" => "animated images cannot be received",
        "malformed_image" | "malformed_pdf" => "the file could not be read",
        "active_pdf" => "the PDF contains active content",
        "encrypted_pdf" => "the PDF is password protected",
        "quota" => "there is no storage left for it",
        "attachments_off" => "photos and PDFs are turned off here",
        _ => "it could not be received",
    }
}

/// The short message a customer gets for a file that was not accepted: the same wording as the web chat widget's
/// refusals (`webchat_widget.js` `REFUSALS`). Never names the file, an id or the provider's handle.
pub fn refusal_notice(reason: &str) -> &'static str {
    match reason {
        "unsupported_type" => {
            "That file type is not supported. Please send a photo (JPEG, PNG or WebP) or a PDF."
        }
        "too_large" | "payload_too_large" => "That file is too big. The limit is 10 MB.",
        "too_many_pixels" => "That image is too large. Please send a smaller photo.",
        "animated" => "Animated images are not supported. Please send a still photo.",
        "malformed_image" => "We could not read that image. Please try another one.",
        "malformed_pdf" => "We could not read that PDF. Please try another one.",
        "active_pdf" => {
            "That PDF cannot be accepted because it contains active content. Please send a plain PDF or a photo."
        }
        "encrypted_pdf" => {
            "That PDF is password protected. Please remove the password and try again."
        }
        "quota" | "attachment_quota" => {
            "We cannot take more files right now. Please try again later."
        }
        "attachments_off" => {
            "Photos and PDFs are turned off for this chat. Please type your message instead."
        }
        "malware" => "This file can't be accepted.",
        "scan_unavailable" | "storage_unavailable" | "unavailable" => {
            "We can't accept files right now, please try again later."
        }
        "rate_limited" => "You have sent a lot of files. Please wait a while and try again.",
        _ => "That file could not be accepted. Please try another one.",
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeliveryReceipt {
    /// The provider's id of the delivered message, when it returns one.
    pub external_id: Option<String>,
}

/// Configuration state for `/ready` and the (phase B) admin screen. Never a secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelStatus {
    pub kind: ChannelKind,
    pub side: Side,
    /// Provider-side workspaces bound to a job.
    pub bindings: usize,
}

/// Errors map to the edge's HTTP answers. Messages are codes and sizes, never customer text.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ChannelError {
    #[error("unauthenticated: {0}")]
    Unauthenticated(&'static str),
    #[error("forbidden: {0}")]
    Forbidden(&'static str),
    #[error("rate limited")]
    RateLimited { retry_after_s: u64 },
    #[error("unavailable: {0}")]
    Unavailable(String),
    #[error("invalid: {0}")]
    Invalid(&'static str),
    /// A delivery the provider refused or could not take (status code / provider error code, no text).
    #[error("upstream: {0}")]
    Upstream(String),
    /// The connector does not implement this operation (e.g. a customer-only connector asked to notify workers).
    #[error("unsupported")]
    Unsupported,
}

impl IntoResponse for ChannelError {
    fn into_response(self) -> Response {
        let (status, code) = match &self {
            ChannelError::Unauthenticated(c) => (StatusCode::UNAUTHORIZED, *c),
            ChannelError::Forbidden(c) => (StatusCode::FORBIDDEN, *c),
            ChannelError::RateLimited { .. } => (StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
            ChannelError::Unavailable(_) => (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
            ChannelError::Invalid(c) => (StatusCode::UNPROCESSABLE_ENTITY, *c),
            ChannelError::Upstream(_) => (StatusCode::BAD_GATEWAY, "upstream_error"),
            ChannelError::Unsupported => (StatusCode::NOT_IMPLEMENTED, "unsupported"),
        };
        let mut r = (status, axum::Json(json!({"error": {"code": code}}))).into_response();
        if let ChannelError::RateLimited { retry_after_s } = self
            && let Ok(v) = axum::http::HeaderValue::from_str(&retry_after_s.to_string())
        {
            r.headers_mut().insert(axum::http::header::RETRY_AFTER, v);
        }
        r
    }
}

/// One provider. Object-safe (the edge holds `Arc<dyn ChannelConnector>`), so the async methods return boxed futures.
pub trait ChannelConnector: Send + Sync {
    fn kind(&self) -> ChannelKind;

    fn side(&self) -> Side {
        Side::Customer
    }

    /// Verify the provider's signature / token. 401 / 403 on failure; nothing else is read before this passes.
    fn authenticate<'a>(
        &'a self,
        req: &'a IngressRequest,
    ) -> BoxFuture<'a, Result<IngressAuth, ChannelError>>;

    /// Parse an authenticated call into a handshake answer or a message.
    fn ingest<'a>(
        &'a self,
        req: &'a IngressRequest,
        auth: &'a IngressAuth,
    ) -> BoxFuture<'a, Result<Ingress, ChannelError>>;

    /// Send one message to a conversation (the answer after the ack, a staff reply after a handover).
    fn deliver<'a>(
        &'a self,
        conv: &'a ConversationRef,
        msg: Outbound,
    ) -> BoxFuture<'a, Result<DeliveryReceipt, ChannelError>>;

    /// A proactive delivery (agent-api's `/internal/deliver`: a staff reply, a handover notice, a CSAT ask) with the
    /// time of the customer's newest message when agent-api knows it (B13 §3). Connectors whose provider limits
    /// proactive messages (a provider's messaging window) override it; the default is [`ChannelConnector::deliver`].
    fn deliver_proactive<'a>(
        &'a self,
        conv: &'a ConversationRef,
        msg: Outbound,
        _last_customer_at_ms: Option<i64>,
    ) -> BoxFuture<'a, Result<DeliveryReceipt, ChannelError>> {
        self.deliver(conv, msg)
    }

    /// B12 §5: this connector for one binding: the binding's BYO credential (the provider's own field names, e.g.
    /// Telegram `bot_token`) in place of the platform's env credential, plus any per-binding settings the connector
    /// reads. `None` (the default) = use the platform connector unchanged. The edge caches the result per binding.
    fn for_binding(&self, _binding: &Binding) -> Option<Arc<dyn ChannelConnector>> {
        None
    }

    fn status(&self) -> ChannelStatus;

    /// B7 R6 persist-before-200: the edge stores each inbound message in core (`inbound` with `accept_only`) before
    /// it acks the provider, answers 503 when core cannot store it (the provider retries), and runs the answer in
    /// the background afterwards. Every provider that retries an un-acked delivery (Telegram) sets it; a provider with a short ack deadline
    /// bounds the store with [`ChannelConnector::accept_timeout`] instead of acking first. `false` (the default) = ack
    /// first, store and answer in the background (a message is lost when core fails after the ack).
    fn persist_before_ack(&self) -> bool {
        false
    }

    /// R6: how long the edge waits for core to store a message before it answers 503 (the provider retries, and
    /// the in-process dedupe lets the retry through). Must sit inside the provider's ack deadline: a short provider ack deadline. Default [`ACCEPT_TIMEOUT`].
    fn accept_timeout(&self) -> Duration {
        ACCEPT_TIMEOUT
    }

    /// Worker side (D104, B12): post a handover item to a worker target (a helpdesk ticket queue) and return the thread it opened, whose replies come back as staff messages. Customer-only connectors
    /// keep the default.
    fn notify_worker<'a>(
        &'a self,
        _target: &'a WorkerTarget,
        _item: &'a WorkerItem,
    ) -> BoxFuture<'a, Result<Option<ConversationRef>, ChannelError>> {
        Box::pin(async { Err(ChannelError::Unsupported) })
    }

    /// Worker side (SW-11): post an internal staff note to the worker thread `thread` (a private ticket comment),
    /// verbatim: it is not a customer delivery, so no delivery-only filtering applies. The connector must be the
    /// binding's ([`ChannelConnector::for_binding`]); one without a binding fails. The default is
    /// [`ChannelError::Unsupported`].
    fn worker_note<'a>(
        &'a self,
        _thread: &'a ConversationRef,
        _text: &'a str,
    ) -> BoxFuture<'a, Result<(), ChannelError>> {
        Box::pin(async { Err(ChannelError::Unsupported) })
    }

    /// Worker side (SW-11): whether the worker thread `thread` already holds the note `text` that
    /// [`ChannelConnector::worker_note`] posts (a retry after an attempt whose outcome is unknown). The default is
    /// [`ChannelError::Unsupported`].
    fn worker_note_exists<'a>(
        &'a self,
        _thread: &'a ConversationRef,
        _text: &'a str,
    ) -> BoxFuture<'a, Result<bool, ChannelError>> {
        Box::pin(async { Err(ChannelError::Unsupported) })
    }

    /// Worker side: the staff member's email (agent-api resolves staff by email only). The default is what the
    /// message itself carried; worker connectors look it up in the provider's directory (cached).
    fn staff_email<'a>(&'a self, m: &'a StaffInbound) -> BoxFuture<'a, Option<String>> {
        Box::pin(std::future::ready(m.staff_email.clone()))
    }

    /// SW-9: download the file an inbound message carries ([`Ingress::Media`]), through this connector's own client and
    /// credential, capped at 10 MB while streaming. The error is a reason code (`too_large`, `download_failed`),
    /// never provider text or an URL. The default is `unsupported`.
    fn download_media<'a>(
        &'a self,
        _media: &'a InboundMedia,
    ) -> BoxFuture<'a, Result<MediaFile, &'static str>> {
        Box::pin(async { Err("unsupported") })
    }

    /// B7 T15b: the provider's own, read-only view of this binding's webhook registration (Telegram's webhook-info
    /// call, crate::channels::telegram::TelegramConnector::webhook_info). Reading it claims nothing (C1 holds); the
    /// default is [`ChannelError::Unsupported`].
    fn read_webhook_info(&self) -> BoxFuture<'_, Result<WebhookInfo, ChannelError>> {
        Box::pin(async { Err(ChannelError::Unsupported) })
    }
}

/// The fixed customer-facing text for an outbound that is not plain text (connectors without a native rendering).
pub fn outbound_text(msg: &Outbound) -> Option<String> {
    match msg {
        Outbound::Text(t) => Some(t.clone()),
        Outbound::Typing => None,
        Outbound::HandoverNotice {
            eta_minutes: Some(m),
        } => Some(format!(
            "I've passed this conversation to a colleague. Expect a reply within about {m} minutes."
        )),
        Outbound::HandoverNotice { eta_minutes: None } => {
            Some("I've passed this conversation to a colleague, who will reply here.".into())
        }
        Outbound::CsatRequest { text } => Some(text.clone()),
        Outbound::Attachment { caption, .. } => caption.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_round_trip_and_are_valid_interaction_channels() {
        for k in [ChannelKind::WebChat, ChannelKind::Telegram] {
            assert_eq!(ChannelKind::parse(k.as_str()), Some(k));
            assert!(crate::interaction::model::valid_channel(k.as_str()));
        }
        assert_eq!(ChannelKind::parse("telegram"), Some(ChannelKind::Telegram));
        assert_eq!(ChannelKind::parse("fax"), None);
    }

    #[test]
    fn query_params_are_percent_decoded() {
        let r = IngressRequest {
            method: Method::GET,
            headers: HeaderMap::new(),
            query: Some("hub.mode=subscribe&hub.verify_token=a%20b+c&hub.challenge=123&x".into()),
            body: Bytes::new(),
            binding: None,
        };
        assert_eq!(r.query_param("hub.mode").as_deref(), Some("subscribe"));
        assert_eq!(r.query_param("hub.verify_token").as_deref(), Some("a b c"));
        assert_eq!(r.query_param("hub.challenge").as_deref(), Some("123"));
        assert_eq!(r.query_param("x").as_deref(), Some(""));
        assert_eq!(r.query_param("nope"), None);
    }

    #[test]
    fn handover_notice_has_customer_text() {
        assert!(
            outbound_text(&Outbound::HandoverNotice {
                eta_minutes: Some(5)
            })
            .unwrap()
            .contains("5 minutes")
        );
        assert_eq!(outbound_text(&Outbound::Typing), None);
    }
}
