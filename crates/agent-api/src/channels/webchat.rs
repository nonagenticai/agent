//! The phase-A customer web chat on the public edge (`chat.nonagentic.ai`, plan B11; B9 T4–T8 later). Stateless:
//! the widget keeps the conversation in the visitor's browser (sessionStorage) and sends its recent turns with each
//! message, so this module stores nothing but in-process rate-limit buckets. B9 adds sessions, Party and CSAT behind
//! the same widget and URLs.
//!
//! ```text
//! POST    /api/webchat/sessions   {widget_key, turnstile_token?} → 201 {session_token, expires_at, display,
//!                                 csat_enabled} (B13); 428 turnstile_required {site_key} | 403 turnstile_failed (B14)
//! OPTIONS /api/webchat/sessions   CORS preflight
//! POST    /api/webchat/messages   {widget_key, visitor_id, messages:[{role,text}], stream?} → {text} | SSE delta…done
//!                                 (`Authorization: Bearer <session_token>`; required on a core binding, B13)
//! OPTIONS /api/webchat/messages   CORS preflight (exact allow-listed origins, no cookies)
//! GET     /webchat/widget.js      the embeddable widget
//! GET     /webchat/demo?key=wk_…  a page embedding the widget for one configured key (the owner's live test);
//!                                 `#assertion=<jwt>` in its fragment (an admin test as a known customer, minted by
//!                                 core's `…/channel-bindings/{id}/test-session`) is handed to the widget by demo.js
//! GET     /webchat/demo.js        the demo page's tiny bridge (same origin, so the strict CSP needs no inline script)
//! ```
//!
//! Env: `WEBCHAT_BINDINGS` (widget key → job, crate::channels::bindings), `WEBCHAT_ALLOWED_ORIGINS` (comma list of
//! `https://host[:port]` or `http://localhost[:port]`), optional `WEBCHAT_PUBLIC_URL` (the edge's own origin, default
//! [`DEFAULT_PUBLIC_URL`], always allowed so the demo page works). Unconfigured → every route here is 404
//! `web_chat_not_configured`.
//!
//! Customer text is data: it never reaches a log line or a metric label. A client IP lives only as a salted sha256
//! key of the in-memory limiter.
//!
//! B12, with the core seam configured (crate::channels::core): the message route sends only the last message to
//! agent-api's `inbound` and answers JSON `{status, text, conversation_id, token}` (never SSE); the first answer
//! issues the conversation id (`wc_` + the first 32 hex of sha256(session id), B13; `wc_` + uuid v7 without a
//! session) and a token (`<core conversation id>.<HMAC>`, key derived from
//! `EDGE_INTERNAL_KEY` for purpose `webchat`) that the widget keeps in sessionStorage and sends back. Later messages
//! (staff replies, notices, CSAT) arrive on
//!
//! ```text
//! GET /api/webchat/conversations/{id}/events?token=              SSE `: connected`, then `message` {id, kind, text,
//!                                                                name?}; keep-alive 15 s
//! GET /api/webchat/conversations/{id}/messages?token=&after_ms=  the stored history (reconnect), from core
//! ```
//!
//! The stream is agent-api's (crate::conversations::webchat_stream): after the token check the edge proxies
//! `GET /api/internal/channel/webchat/conversations/{core id}/events` and re-emits only well-formed `message`
//! events, so any edge replica serves any customer (the edge keeps no fan-out and has no Redis). `/internal/deliver`
//! of a web chat only accepts the message and names its id; agent-api publishes it. Core unreachable → a 200 stream
//! that asks the browser to retry in 5 s and ends; core silent for [`UPSTREAM_IDLE`] (it sends a keep-alive every
//! 15 s) or its stream ended → the stream ends and the browser reconnects (the history read covers any gap). At most
//! [`MAX_STREAMS_PER_CONVERSATION`] streams per conversation per replica (a reload plus a second tab); one more is
//! 429 `rate_limited` with `Retry-After`, before core is asked. An env
//! (`WEBCHAT_BINDINGS`) key has no core conversation and so no stream. Web chat bindings may also come from core
//! (`settings.allowed_origins`).
//!
//! B13 sessions: on a core binding the session route asks agent-api (`POST /api/internal/channel/webchat/sessions`
//! with the origin and an `ip_hash`), which stores the session and its durable limits in Postgres; the token is
//! `<session_id>.<base64url secret>` and every message carries it as a Bearer, with party `wc:<session_id>`.
//! agent-api's 401/403/429/503 pass through 1:1 (429 with `Retry-After`). An env (`WEBCHAT_BINDINGS`) key gets a
//! stateless edge-signed token of the same shape (per-process key: a restart invalidates it and the widget opens a
//! new session) and keeps the in-process limits only. The in-process limits stay the first line on both paths.
//! `ip_hash` = hex `sha256(salt ‖ ip)`, salt `WEBCHAT_IP_HASH_SALT`, else derived from `EDGE_INTERNAL_KEY`.
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::convert::Infallible;
use std::hash::Hash;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, RawQuery, State};
use axum::extract::{DefaultBodyLimit, FromRequest, Multipart, Path, Request};
use axum::http::header::{
    ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN,
    ACCESS_CONTROL_EXPOSE_HEADERS, ACCESS_CONTROL_MAX_AGE, AUTHORIZATION, CACHE_CONTROL,
    CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_SECURITY_POLICY, CONTENT_TYPE, ORIGIN,
    REFERRER_POLICY, RETRY_AFTER, VARY, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::{Extension, Router};
use base64::Engine;
use futures_util::StreamExt;
use futures_util::stream::{self, BoxStream};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use super::bindings;
use super::connector::{ChannelKind, ConversationRef, Outbound, Side, outbound_text};
use super::core::{
    AttachmentUploadError, CoreError, CoreLink, HistoryScope, InboundReq, InboundResp, InternalKey,
    SeamError, WebChatSessionCheckReq, WebChatSessionReq, WireParty,
};
use super::edge::{EdgeState, not_available_code};
use crate::attachments::AttachmentRef;
use crate::error::ApiError;
use crate::interaction::collect;
use crate::interaction::model::{
    BackendError, BackendEvent, InteractionRequest, Message, Party, Role, Scope,
};

/// The interaction channel of every web chat request.
pub const CHANNEL: &str = "web_chat";
/// The edge's public origin when `WEBCHAT_PUBLIC_URL` is unset.
pub const DEFAULT_PUBLIC_URL: &str = "https://chat.nonagentic.ai";
/// A message body above this is 413 before parsing.
pub const MAX_BODY_BYTES: usize = 64 * 1024;
/// The most recent messages forwarded per request; older ones are dropped.
pub const MAX_HISTORY: usize = 20;
/// D100 defaults: per client IP 30 messages / 10 min and 10 new visitor ids / hour; per widget key 300 / min.
pub const IP_MESSAGES: (u32, Duration) = (30, Duration::from_secs(10 * 60));
pub const IP_NEW_VISITORS: (u32, Duration) = (10, Duration::from_secs(60 * 60));
pub const WIDGET_MESSAGES: (u32, Duration) = (300, Duration::from_secs(60));
/// Limiter keys kept per bucket map; beyond it idle buckets are dropped, and if that is not enough the map restarts
/// (fail open: a flood of distinct keys must not lock out every new visitor; the widget bucket still bounds cost).
pub const MAX_LIMITER_KEYS: usize = 100_000;

const WIDGET_JS: &str = include_str!("webchat_widget.js");
/// Replaced in [`WIDGET_JS`] by the configured public URL (the widget's API base when its own `src` is unreadable).
const PUBLIC_URL_PLACEHOLDER: &str = "__WEBCHAT_PUBLIC_URL__";
/// The demo page's CSP; Cloudflare's challenge origin is allowed for a binding with Turnstile on (B14 §2).
const DEMO_CSP: &str = "default-src 'self'; script-src 'self' https://challenges.cloudflare.com; \
     frame-src https://challenges.cloudflare.com; connect-src 'self'; img-src 'self' blob:; style-src 'self' 'unsafe-inline'";

#[derive(Clone, Debug)]
pub struct WebChatConfig {
    /// Widget key (`wk_` + 24 × `[a-z2-7]`) → the job it answers as.
    pub bindings: BTreeMap<String, Scope>,
    /// Exact origins allowed to call the API (lowercase, no trailing slash).
    pub allowed_origins: BTreeSet<String>,
    /// The edge's own origin (no trailing slash); implicitly allowed.
    pub public_url: String,
}

/// `(config, missing names)`. Off (None) unless `WEBCHAT_BINDINGS` (≥ 1 binding) and `WEBCHAT_ALLOWED_ORIGINS` are
/// both set; a set but malformed value fails the boot by name, never echoing the value.
pub fn config_from(
    trimmed: &impl Fn(&str) -> Option<String>,
) -> Result<(Option<WebChatConfig>, Vec<&'static str>)> {
    let public_url = match trimmed("WEBCHAT_PUBLIC_URL") {
        None => DEFAULT_PUBLIC_URL.to_string(),
        Some(v) => normalize_origin(&v).ok_or_else(|| {
            anyhow!("WEBCHAT_PUBLIC_URL must be an origin like https://chat.example.com (no path)")
        })?,
    };
    let raw_bindings = trimmed("WEBCHAT_BINDINGS");
    let bindings = bindings::parse("WEBCHAT_BINDINGS", raw_bindings.as_deref())?;
    if raw_bindings.is_some() && bindings.is_empty() {
        anyhow::bail!("WEBCHAT_BINDINGS must bind at least one widget key");
    }
    if !bindings.keys().all(|k| valid_widget_key(k)) {
        anyhow::bail!(
            "WEBCHAT_BINDINGS: every external_id must be a widget key: wk_ followed by 24 characters of [a-z2-7]"
        );
    }
    let allowed_origins = match trimmed("WEBCHAT_ALLOWED_ORIGINS") {
        None => None,
        Some(raw) => {
            let mut set = BTreeSet::new();
            for entry in raw.split(',').map(str::trim).filter(|e| !e.is_empty()) {
                let o = normalize_origin(entry).ok_or_else(|| {
                    anyhow!(
                        "WEBCHAT_ALLOWED_ORIGINS: every entry must be an exact origin, https://host[:port] or \
                         http://localhost[:port]"
                    )
                })?;
                set.insert(o);
            }
            if set.is_empty() {
                anyhow::bail!("WEBCHAT_ALLOWED_ORIGINS must name at least one origin");
            }
            Some(set)
        }
    };
    let mut missing = Vec::new();
    if bindings.is_empty() {
        missing.push("WEBCHAT_BINDINGS");
    }
    let Some(allowed_origins) = allowed_origins else {
        missing.push("WEBCHAT_ALLOWED_ORIGINS");
        return Ok((None, missing));
    };
    if !missing.is_empty() {
        return Ok((None, missing));
    }
    Ok((
        Some(WebChatConfig {
            bindings,
            allowed_origins,
            public_url,
        }),
        missing,
    ))
}

/// `wk_` + 24 characters of `[a-z2-7]` (base32, lowercase).
pub fn valid_widget_key(k: &str) -> bool {
    k.strip_prefix("wk_")
        .is_some_and(|r| r.len() == 24 && r.bytes().all(|b| matches!(b, b'a'..=b'z' | b'2'..=b'7')))
}

/// `^[A-Za-z0-9_-]{8,64}$`.
pub fn valid_visitor_id(v: &str) -> bool {
    (8..=64).contains(&v.len())
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The origin in its canonical form (lowercase, no trailing slash), or None unless it is `https://host[:port]` or
/// `http://localhost[:port]`.
pub fn normalize_origin(raw: &str) -> Option<String> {
    let o = raw.trim().to_ascii_lowercase();
    let o = o.strip_suffix('/').unwrap_or(&o).to_string();
    let (rest, https) = match o.strip_prefix("https://") {
        Some(r) => (r, true),
        None => (o.strip_prefix("http://")?, false),
    };
    let (host, port) = match rest.rsplit_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (rest, None),
    };
    if let Some(p) = port {
        let ok = !p.is_empty()
            && p.len() <= 5
            && p.bytes().all(|b| b.is_ascii_digit())
            && p.parse::<u32>().is_ok_and(|n| (1..=65_535).contains(&n));
        if !ok {
            return None;
        }
    }
    let host_ok = if https {
        valid_host(host)
    } else {
        host == "localhost"
    };
    host_ok.then_some(o)
}

fn valid_host(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 253
        && h.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

/// The token key's purpose (crate::channels::core::InternalKey::derive).
pub const TOKEN_PURPOSE: &str = "webchat";
/// The purpose the `ip_hash` salt is derived for when `WEBCHAT_IP_HASH_SALT` is unset.
pub const IP_HASH_PURPOSE: &str = "webchat-ip-hash";
/// How long an env-binding session claims to live (stateless: reported to the widget, not enforced).
pub const ENV_SESSION_TTL_HOURS: i64 = 24;
/// The longest accepted session token.
const MAX_SESSION_TOKEN: usize = 256;

/// The session id of a `<session_id>.<base64url secret>` token (`[A-Za-z0-9_-]{8,64}` before the first `.`), or
/// None when the token is not of that shape.
pub fn session_id_of(token: &str) -> Option<&str> {
    if token.len() > MAX_SESSION_TOKEN {
        return None;
    }
    let (id, secret) = token.split_once('.')?;
    let secret_ok = !secret.is_empty()
        && secret
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    (valid_visitor_id(id) && secret_ok).then_some(id)
}

/// `Authorization: Bearer <token>`.
fn bearer(headers: &HeaderMap) -> Option<String> {
    headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}
/// SSE keep-alive comment interval.
pub const KEEP_ALIVE: Duration = Duration::from_secs(15);
/// The longest agent-api's stream may stay silent (three of its keep-alives) before the edge ends the customer's.
pub const UPSTREAM_IDLE: Duration = Duration::from_secs(45);
/// The `retry` an unreachable core's stream asks of the browser.
pub const UPSTREAM_RETRY: Duration = Duration::from_secs(5);
/// Unparsed bytes of agent-api's stream above this end it (one event is a few KiB).
const UPSTREAM_MAX_BUFFER: usize = 256 * 1024;
/// Concurrent customer streams of one conversation on one edge replica.
pub const MAX_STREAMS_PER_CONVERSATION: usize = 2;
/// The `Retry-After` of a stream refused for that cap.
const STREAM_RETRY_S: u64 = 5;
/// A staff display name on an SSE event is cut to this many characters.
const MAX_NAME_CHARS: usize = 80;
/// The first SSE frame, sent the moment the stream opens: proxies (Cloudflare, istio) and hyper hold the response
/// head until a body chunk exists, so without it the stream only "connects" with the first keep-alive.
pub const SSE_OPEN_COMMENT: &str = "connected";

/// `wc_` + a uuid v7 (32 lowercase hex characters).
pub fn new_conversation_id() -> String {
    format!("wc_{}", Uuid::now_v7().simple())
}

/// The conversation id of a session's messages sent without a conversation token: `wc_` + the first 32 hex
/// characters of `sha256(session id)`. A first message resent before the widget holds its token (its answer failed or
/// was lost), or two first messages at once, so land in the session's one conversation instead of minting new ones.
pub fn session_conversation_id(session_id: &str) -> String {
    let d = ring::digest::digest(&ring::digest::SHA256, session_id.as_bytes());
    format!("wc_{}", &hex::encode(d.as_ref())[..32])
}

pub fn valid_conversation_id(s: &str) -> bool {
    s.strip_prefix("wc_")
        .is_some_and(|r| r.len() == 32 && r.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

fn token_mac(key: &InternalKey, conversation_id: &str, core_id: &str) -> ring::hmac::Tag {
    ring::hmac::sign(
        &key.derive(TOKEN_PURPOSE),
        format!("wc|{conversation_id}|{core_id}").as_bytes(),
    )
}

/// `<core conversation id>.<base64url HMAC>`: binds the web chat id to agent-api's conversation.
pub fn issue_token(key: &InternalKey, conversation_id: &str, core_id: &str) -> String {
    let mac = token_mac(key, conversation_id, core_id);
    format!(
        "{core_id}.{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac.as_ref())
    )
}

/// The core conversation id a valid token names; `None` for any mismatch (constant-time).
pub fn verify_token(key: &InternalKey, conversation_id: &str, token: &str) -> Option<String> {
    let (core_id, mac) = token.rsplit_once('.')?;
    let mac = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(mac)
        .ok()?;
    ring::hmac::verify(
        &key.derive(TOKEN_PURPOSE),
        format!("wc|{conversation_id}|{core_id}").as_bytes(),
        &mac,
    )
    .ok()?;
    (!core_id.is_empty()).then(|| core_id.to_string())
}

/// One message pushed to a web chat conversation's SSE stream.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HubEvent {
    pub id: String,
    /// `text` | `handover_notice` | `csat_request`.
    pub kind: &'static str,
    pub text: String,
    /// The staff member's display name on a staff reply (never an email or a sub); absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// SW-9: the files of a staff reply (refs only; the widget fetches the bytes through the edge).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<AttachmentRef>,
}

/// The id of a message the edge accepted for a web chat (`/internal/deliver`): `m_` + a uuid v7.
pub fn new_event_id() -> String {
    format!("m_{}", Uuid::now_v7().simple())
}

/// The `data` of one SSE event block of agent-api's stream as a [`HubEvent`]; `None` for a comment, a keep-alive or
/// anything malformed (dropped, never forwarded).
pub fn upstream_event(block: &str) -> Option<HubEvent> {
    let data: Vec<&str> = block
        .lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .map(|d| d.strip_prefix(' ').unwrap_or(d))
        .collect();
    if data.is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(&data.join("\n")).ok()?;
    let kind = match v["kind"].as_str()? {
        "text" => "text",
        "handover_notice" => "handover_notice",
        "csat_request" => "csat_request",
        _ => return None,
    };
    // a staff reply of only files has no text
    let attachments: Vec<AttachmentRef> = v["attachments"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| serde_json::from_value::<AttachmentRef>(x.clone()).ok())
                .filter(|r| ALLOWED_MEDIA.contains(&r.media_type.as_str()))
                .take(MAX_REFS_PER_EVENT)
                .collect()
        })
        .unwrap_or_default();
    let text = match v["text"].as_str().filter(|t| !t.is_empty()) {
        Some(t) => t,
        None if !attachments.is_empty() => "",
        None => return None,
    };
    Some(HubEvent {
        // an SSE id never carries a control character (axum refuses one)
        id: v["id"]
            .as_str()
            .filter(|i| !i.chars().any(char::is_control))
            .unwrap_or_default()
            .to_string(),
        kind,
        text: text.to_string(),
        name: v["name"]
            .as_str()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(|n| n.chars().take(MAX_NAME_CHARS).collect()),
        attachments,
    })
}

/// The media types an attachment may have on the wire (core's re-encoded images and safe PDFs).
const ALLOWED_MEDIA: [&str; 4] = ["image/jpeg", "image/png", "image/webp", "application/pdf"];
/// Refs relayed per stream event (a staff reply carries at most 5).
const MAX_REFS_PER_EVENT: usize = 5;
/// An attachment upload body above this is refused before it is buffered (core caps the file at 10 MiB).
pub const MAX_UPLOAD_BYTES: usize = 11 * 1024 * 1024;
/// Attachments one customer message carries.
pub const MAX_MESSAGE_ATTACHMENTS: usize = 3;

/// The longest sign-in URL relayed to the widget.
const MAX_SIGN_IN_URL: usize = 2048;

/// B7: `Some(url)` when `block` is agent-api's `sign_in` event (SSE event name `sign_in`, or a `kind` of `sign_in`),
/// `url` being the binding's sign-in URL when it sent a well-formed plain http(s) one (never a `javascript:` link).
/// `None` for any other block.
pub fn upstream_sign_in(block: &str) -> Option<Option<String>> {
    let named = block
        .lines()
        .filter_map(|l| l.strip_prefix("event:"))
        .any(|e| e.trim() == "sign_in");
    let data: Vec<&str> = block
        .lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .map(|d| d.strip_prefix(' ').unwrap_or(d))
        .collect();
    let v: serde_json::Value = serde_json::from_str(&data.join("\n")).unwrap_or_default();
    if !named && v["kind"].as_str() != Some("sign_in") {
        return None;
    }
    Some(
        v["url"]
            .as_str()
            .map(str::trim)
            .filter(|u| {
                u.len() <= MAX_SIGN_IN_URL
                    && (u.starts_with("https://") || u.starts_with("http://"))
                    && !u.chars().any(|c| c.is_control() || c.is_whitespace())
            })
            .map(str::to_string),
    )
}

/// The next complete event block (up to a blank line) of `buf`, removed from it.
fn next_block(buf: &mut Vec<u8>) -> Option<String> {
    let at = buf.windows(2).position(|w| w == b"\n\n")?;
    let block: Vec<u8> = buf.drain(..at + 2).collect();
    Some(String::from_utf8_lossy(&block[..at]).into_owned())
}

/// The configured web chat: its bindings, the CORS allow-list and the in-process limiters.
pub struct WebChat {
    cfg: WebChatConfig,
    /// The allow-list plus the edge's own origin.
    origins: BTreeSet<String>,
    widget_js: Bytes,
    /// Per-process salt for the IP keys (a bare sha256 of an IPv4 address is trivially reversed).
    salt: [u8; 16],
    /// `WEBCHAT_IP_HASH_SALT` (stable across restarts, so agent-api's durable per-IP limits hold).
    ip_hash_salt: Option<Vec<u8>>,
    /// Per-process key of the env-binding session tokens.
    session_key: ring::hmac::Key,
    limits: Mutex<Limits>,
    /// Open customer streams per conversation id ([`StreamSlot`]).
    streams: Mutex<HashMap<String, usize>>,
}

/// One open customer stream of a conversation; dropping it frees the slot.
pub struct StreamSlot {
    wc: Arc<WebChat>,
    conversation_id: String,
}

impl Drop for StreamSlot {
    fn drop(&mut self) {
        let mut m = self.wc.streams.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = m.get_mut(&self.conversation_id) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                m.remove(&self.conversation_id);
            }
        }
    }
}

impl WebChat {
    /// A stream slot of `conversation_id`, or `None` while it already has [`MAX_STREAMS_PER_CONVERSATION`].
    pub fn open_stream(self: &Arc<Self>, conversation_id: &str) -> Option<StreamSlot> {
        let mut m = self.streams.lock().unwrap_or_else(|e| e.into_inner());
        let n = m.entry(conversation_id.to_string()).or_insert(0);
        if *n >= MAX_STREAMS_PER_CONVERSATION {
            return None;
        }
        *n += 1;
        Some(StreamSlot {
            wc: self.clone(),
            conversation_id: conversation_id.to_string(),
        })
    }

    pub fn new(cfg: WebChatConfig) -> Self {
        let mut origins = cfg.allowed_origins.clone();
        origins.insert(cfg.public_url.clone());
        let widget_js = Bytes::from(WIDGET_JS.replace(PUBLIC_URL_PLACEHOLDER, &cfg.public_url));
        Self {
            cfg,
            origins,
            widget_js,
            salt: uuid::Uuid::new_v4().into_bytes(),
            ip_hash_salt: std::env::var("WEBCHAT_IP_HASH_SALT")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .map(String::into_bytes),
            session_key: {
                let mut k = [0u8; 32];
                k[..16].copy_from_slice(&uuid::Uuid::new_v4().into_bytes());
                k[16..].copy_from_slice(&uuid::Uuid::new_v4().into_bytes());
                ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &k)
            },
            limits: Mutex::new(Limits::default()),
            streams: Mutex::new(HashMap::new()),
        }
    }

    pub fn config(&self) -> &WebChatConfig {
        &self.cfg
    }

    /// The embed snippet for one widget key.
    pub fn snippet(&self, widget_key: &str) -> String {
        format!(
            r#"<script src="{}/webchat/widget.js" data-widget="{widget_key}" async></script>"#,
            self.cfg.public_url
        )
    }

    fn origin_allowed(&self, origin: &str) -> bool {
        self.origins.contains(origin)
    }

    fn ip_key(&self, ip: &str) -> [u8; 32] {
        let mut c = ring::digest::Context::new(&ring::digest::SHA256);
        c.update(&self.salt);
        c.update(ip.as_bytes());
        let mut k = [0u8; 32];
        k.copy_from_slice(c.finish().as_ref());
        k
    }

    /// Hex `sha256(salt ‖ ip)` for agent-api: salt `WEBCHAT_IP_HASH_SALT`, else derived from the internal key, else
    /// (no core link) the per-process salt.
    pub fn ip_hash(&self, core: Option<&CoreLink>, ip: &str) -> String {
        let derived;
        let salt: &[u8] = match (&self.ip_hash_salt, core) {
            (Some(s), _) => s.as_slice(),
            (None, Some(c)) => {
                derived = ring::hmac::sign(&c.key.derive(IP_HASH_PURPOSE), b"salt");
                derived.as_ref()
            }
            (None, None) => &self.salt[..],
        };
        let mut c = ring::digest::Context::new(&ring::digest::SHA256);
        c.update(salt);
        c.update(ip.as_bytes());
        hex::encode(c.finish().as_ref())
    }

    fn session_mac(&self, widget_key: &str, session_id: &str) -> ring::hmac::Tag {
        ring::hmac::sign(
            &self.session_key,
            format!("wcs|{widget_key}|{session_id}").as_bytes(),
        )
    }

    /// An env-binding session: `(session id, "<session id>.<base64url HMAC>")`.
    pub fn issue_session(&self, widget_key: &str) -> (String, String) {
        let id = Uuid::new_v4().simple().to_string();
        let mac = self.session_mac(widget_key, &id);
        let token = format!(
            "{id}.{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac.as_ref())
        );
        (id, token)
    }

    /// The session id of a token this process issued for `widget_key`; None otherwise (constant-time).
    pub fn verify_session(&self, widget_key: &str, token: &str) -> Option<String> {
        let id = session_id_of(token)?;
        let (_, mac) = token.split_once('.')?;
        let mac = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(mac)
            .ok()?;
        ring::hmac::verify(
            &self.session_key,
            format!("wcs|{widget_key}|{id}").as_bytes(),
            &mac,
        )
        .ok()?;
        Some(id.to_string())
    }

    /// One new session from this IP (the in-process first line; agent-api holds the durable count on core).
    fn admit_session(&self, ip: &str) -> Result<(), u64> {
        self.check_session(ip)?;
        self.take_session(ip);
        Ok(())
    }

    /// Whether this IP may open a session now, taking nothing: the pre-filter before a core round-trip whose answer
    /// (428 `turnstile_required`, 403) must not cost the visitor a session.
    fn check_session(&self, ip: &str) -> Result<(), u64> {
        let ip = self.ip_key(ip);
        let mut l = self.limits.lock().unwrap_or_else(|e| e.into_inner());
        l.ip_visitors.check(&ip, Instant::now())
    }

    /// Charge this IP one session (after core issued it). Concurrent creates that all passed [`Self::check_session`]
    /// may drive the bucket below zero, which only lengthens the wait.
    fn take_session(&self, ip: &str) {
        let ip = self.ip_key(ip);
        let mut l = self.limits.lock().unwrap_or_else(|e| e.into_inner());
        l.ip_visitors.take(&ip);
    }

    /// Take one message from every bucket that applies, or none and the seconds to wait. `visitor` None = the
    /// message carries a session, whose creation already took the new-visitor token.
    fn admit(&self, ip: &str, widget_key: &str, visitor: Option<&str>) -> Result<(), u64> {
        let ip = self.ip_key(ip);
        let now = Instant::now();
        let mut l = self.limits.lock().unwrap_or_else(|e| e.into_inner());
        let widget = widget_key.to_string();
        let seen_key = (ip, visitor.unwrap_or_default().to_string());
        let new_visitor = visitor.is_some()
            && !l
                .seen
                .get(&seen_key)
                .is_some_and(|t| now.duration_since(*t) < IP_NEW_VISITORS.1);
        let mut wait = 0u64;
        for w in [
            l.ip_messages.check(&ip, now).err(),
            l.widget_messages.check(&widget, now).err(),
            if new_visitor {
                l.ip_visitors.check(&ip, now).err()
            } else {
                None
            },
        ]
        .into_iter()
        .flatten()
        {
            wait = wait.max(w);
        }
        if wait > 0 {
            return Err(wait);
        }
        l.ip_messages.take(&ip);
        l.widget_messages.take(&widget);
        if new_visitor {
            l.ip_visitors.take(&ip);
            if l.seen.len() >= MAX_LIMITER_KEYS && !l.seen.contains_key(&seen_key) {
                l.seen
                    .retain(|_, t| now.duration_since(*t) < IP_NEW_VISITORS.1);
                if l.seen.len() >= MAX_LIMITER_KEYS {
                    l.seen.clear();
                }
            }
        }
        if visitor.is_some() {
            l.seen.insert(seen_key, now);
        }
        Ok(())
    }
}

struct Limits {
    ip_messages: Buckets<[u8; 32]>,
    ip_visitors: Buckets<[u8; 32]>,
    widget_messages: Buckets<String>,
    /// (ip key, visitor id) → last seen: a visitor seen within the hour is not new.
    seen: HashMap<([u8; 32], String), Instant>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            ip_messages: Buckets::new(IP_MESSAGES),
            ip_visitors: Buckets::new(IP_NEW_VISITORS),
            widget_messages: Buckets::new(WIDGET_MESSAGES),
            seen: HashMap::new(),
        }
    }
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

/// Token buckets: `capacity` tokens, refilled continuously over `window`.
struct Buckets<K> {
    capacity: f64,
    window: Duration,
    map: HashMap<K, Bucket>,
}

impl<K: Hash + Eq + Clone> Buckets<K> {
    fn new((capacity, window): (u32, Duration)) -> Self {
        Self {
            capacity: f64::from(capacity),
            window,
            map: HashMap::new(),
        }
    }

    /// Refill the key's bucket; `Err(seconds)` when it holds less than one token.
    fn check(&mut self, k: &K, now: Instant) -> Result<(), u64> {
        if self.map.len() >= MAX_LIMITER_KEYS && !self.map.contains_key(k) {
            let window = self.window;
            self.map.retain(|_, b| now.duration_since(b.last) < window);
            if self.map.len() >= MAX_LIMITER_KEYS {
                self.map.clear();
            }
        }
        let (capacity, per_sec) = (self.capacity, self.capacity / self.window.as_secs_f64());
        let b = self.map.entry(k.clone()).or_insert(Bucket {
            tokens: capacity,
            last: now,
        });
        b.tokens = (b.tokens + now.duration_since(b.last).as_secs_f64() * per_sec).min(capacity);
        b.last = now;
        if b.tokens >= 1.0 {
            Ok(())
        } else {
            Err(((1.0 - b.tokens) / per_sec).ceil().max(1.0) as u64)
        }
    }

    fn take(&mut self, k: &K) {
        if let Some(b) = self.map.get_mut(k) {
            b.tokens -= 1.0;
        }
    }
}

/// The web chat routes (merged into the edge router before its state is attached).
pub fn routes() -> Router<EdgeState> {
    Router::new()
        .route("/api/webchat/sessions", post(sessions).options(preflight))
        .route("/api/webchat/messages", post(messages).options(preflight))
        .route(
            "/api/webchat/attachments",
            post(upload_attachment)
                .options(preflight)
                .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES)),
        )
        .route(
            "/api/webchat/conversations/{id}/attachments/{att}",
            get(fetch_attachment).options(preflight),
        )
        .route("/api/webchat/conversations/{id}/events", get(events))
        .route("/api/webchat/conversations/{id}/messages", get(history))
        .route("/webchat/widget.js", get(widget_js))
        .route("/webchat/demo", get(demo))
        .route("/webchat/demo.js", get(demo_js))
}

fn error(status: StatusCode, code: &'static str) -> Response {
    if status != StatusCode::NOT_FOUND {
        metrics::counter!("agent_edge_webchat_refused_total", "code" => code).increment(1);
    }
    (status, Json(json!({"error": {"code": code}}))).into_response()
}

fn not_configured() -> Response {
    error(StatusCode::NOT_FOUND, "web_chat_not_configured")
}

/// The request's `Origin` when it is allowed: the env allow-list (plus the edge's own origin), or any core web chat
/// binding's `settings.allowed_origins` (the message route then checks the named binding's own list).
fn allowed_origin(st: &EdgeState, wc: &WebChat, headers: &HeaderMap) -> Option<HeaderValue> {
    let o = headers.get(ORIGIN)?;
    let s = o.to_str().ok()?;
    let ok = wc.origin_allowed(s)
        || st
            .bindings
            .of_kind(ChannelKind::WebChat, Side::Customer)
            .iter()
            .any(|b| b.allowed_origins().iter().any(|a| a == s));
    ok.then(|| o.clone())
}

fn with_cors(mut r: Response, origin: HeaderValue) -> Response {
    let h = r.headers_mut();
    h.insert(ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    h.append(VARY, HeaderValue::from_static("Origin"));
    h.insert(
        ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static("Retry-After"),
    );
    r
}

async fn preflight(State(st): State<EdgeState>, headers: HeaderMap) -> Response {
    let Some(wc) = st.webchat.as_deref() else {
        return not_configured();
    };
    let Some(origin) = allowed_origin(&st, wc, &headers) else {
        return error(StatusCode::FORBIDDEN, "origin_not_allowed");
    };
    let mut r = StatusCode::NO_CONTENT.into_response();
    let h = r.headers_mut();
    h.insert(ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    h.append(VARY, HeaderValue::from_static("Origin"));
    h.insert(
        ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    h.insert(
        ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("content-type, authorization"),
    );
    h.insert(ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("600"));
    r
}

fn rate_limited(wait: u64) -> Response {
    let mut r = error(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
    r.headers_mut()
        .insert(RETRY_AFTER, HeaderValue::from(wait.max(1)));
    r
}

/// agent-api's web chat refusals, 1:1 (B13 §3): 401 `widget_unknown` | `session_invalid`, 403 `origin_not_allowed` |
/// `session_blocked`, 429 `rate_limited` (+ `Retry-After`), 503 `unavailable`. None for anything else (the caller's
/// own mapping: an unknown 401/403 code is a seam fault, not the visitor's).
fn seam_refusal(status: u16, code: &str, retry_after_s: Option<u64>) -> Option<Response> {
    match (status, code) {
        (401, "widget_unknown") => Some(error(StatusCode::UNAUTHORIZED, "widget_unknown")),
        (401, "session_invalid") => Some(error(StatusCode::UNAUTHORIZED, "session_invalid")),
        // B7: a refused customer assertion, passed on unchanged (the widget asks its host once more, then goes anonymous)
        (401, "assertion_invalid") => Some(error(StatusCode::UNAUTHORIZED, "assertion_invalid")),
        (401, "assertion_replayed") => Some(error(StatusCode::UNAUTHORIZED, "assertion_replayed")),
        (403, "origin_not_allowed") => Some(error(StatusCode::FORBIDDEN, "origin_not_allowed")),
        (403, "session_blocked") => Some(error(StatusCode::FORBIDDEN, "session_blocked")),
        // SW-9: a file id that is not the conversation's own unsent upload, or too many of them
        (404, "not_found") => Some(error(StatusCode::NOT_FOUND, "not_found")),
        (422, "invalid_attachments" | "too_many_attachments") => Some(error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_attachments",
        )),
        (413, "attachment_quota") => Some(error(StatusCode::PAYLOAD_TOO_LARGE, "attachment_quota")),
        (413, _) => Some(error(StatusCode::PAYLOAD_TOO_LARGE, "too_large")),
        (429, _) => Some(rate_limited(retry_after_s.unwrap_or(60))),
        (503, _) => Some(error(StatusCode::SERVICE_UNAVAILABLE, "unavailable")),
        _ => None,
    }
}

/// The longest `turnstile_token` relayed (Cloudflare's are about 2 KiB).
const MAX_TURNSTILE_TOKEN: usize = 4096;

/// agent-api's Turnstile answers on session create (B14 §2), 1:1: 428 `turnstile_required {site_key}` (the widget
/// renders the challenge with that key and retries) and 403 `turnstile_failed`.
fn turnstile_refusal(status: u16, code: &str, site_key: Option<&str>) -> Option<Response> {
    match (status, code) {
        (428, "turnstile_required") => {
            metrics::counter!("agent_edge_webchat_refused_total", "code" => "turnstile_required")
                .increment(1);
            Some(
                (
                    StatusCode::PRECONDITION_REQUIRED,
                    Json(json!({"error": {"code": "turnstile_required", "site_key": site_key}})),
                )
                    .into_response(),
            )
        }
        (403, "turnstile_failed") => Some(error(StatusCode::FORBIDDEN, "turnstile_failed")),
        _ => None,
    }
}

#[derive(Deserialize)]
struct SessionBody {
    widget_key: String,
    /// B14 §2: the token of a solved Turnstile challenge, after a 428 `turnstile_required`.
    #[serde(default)]
    turnstile_token: Option<String>,
    /// B7: the tenant's signed customer assertion (a JWT), verified by core only. Never logged, never stored here.
    #[serde(default)]
    customer_assertion: Option<String>,
}

/// The longest `customer_assertion` relayed (core's `webchat_identity::MAX_ASSERTION_BYTES`).
const MAX_ASSERTION_BYTES: usize = 2048;

/// `POST /api/webchat/sessions {widget_key, turnstile_token?, customer_assertion?}` → 201 `{session_token, expires_at, display,
/// csat_enabled, identified, greeting_name?, sign_in_url?}`; 428 `turnstile_required {site_key}` / 403
/// `turnstile_failed` on a binding with Turnstile on; 401 `assertion_invalid` | `assertion_replayed` from core.
async fn sessions(
    State(st): State<EdgeState>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let Some(wc) = st.webchat.clone() else {
        return not_configured();
    };
    let Some(origin) = allowed_origin(&st, &wc, &headers) else {
        return error(StatusCode::FORBIDDEN, "origin_not_allowed");
    };
    let peer = peer.map(|Extension(ConnectInfo(a))| a);
    let r = handle_session(&st, &wc, peer, &headers, body).await;
    with_cors(r, origin)
}

async fn handle_session(
    st: &EdgeState,
    wc: &WebChat,
    peer: Option<SocketAddr>,
    headers: &HeaderMap,
    body: Body,
) -> Response {
    let Ok(raw) = axum::body::to_bytes(body, 8 * 1024).await else {
        return error(StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large");
    };
    let Ok(b) = serde_json::from_slice::<SessionBody>(&raw) else {
        return error(StatusCode::BAD_REQUEST, "invalid_body");
    };
    let turnstile_token = b.turnstile_token.as_deref().map(str::trim);
    if turnstile_token.is_some_and(|t| t.is_empty() || t.len() > MAX_TURNSTILE_TOKEN) {
        return error(StatusCode::BAD_REQUEST, "invalid_body");
    }
    let assertion = b.customer_assertion.as_deref();
    if assertion.is_some_and(|a| a.is_empty() || a.len() > MAX_ASSERTION_BYTES) {
        return error(StatusCode::BAD_REQUEST, "invalid_body");
    }
    let origin = headers
        .get(ORIGIN)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let ip = client_ip(headers, peer);
    if wc.cfg.bindings.contains_key(&b.widget_key) {
        // an env binding has no core to verify with: an assertion is never honoured, so it is refused like a bad one
        if assertion.is_some() {
            return error(StatusCode::UNAUTHORIZED, "assertion_invalid");
        }
        if let Err(wait) = wc.admit_session(&ip) {
            return rate_limited(wait);
        }
        let (_, token) = wc.issue_session(&b.widget_key);
        let expires_at =
            (chrono::Utc::now() + chrono::TimeDelta::hours(ENV_SESSION_TTL_HOURS)).to_rfc3339();
        metrics::counter!("agent_edge_webchat_sessions_total", "source" => "env").increment(1);
        return (
            StatusCode::CREATED,
            Json(
                json!({"session_token": token, "expires_at": expires_at, "display": {},
                "csat_enabled": false, "identified": false}),
            ),
        )
            .into_response();
    }
    // a binding created since the last periodic pull: one rate-limited pull right away, not a 404 for up to 60 s
    let found = match st.core.as_deref() {
        Some(core) if valid_widget_key(&b.widget_key) => {
            st.bindings
                .find_or_refresh(ChannelKind::WebChat, Side::Customer, &b.widget_key, core)
                .await
        }
        _ => st
            .bindings
            .find(ChannelKind::WebChat, Side::Customer, &b.widget_key),
    };
    let Some(bd) = found else {
        return error(StatusCode::NOT_FOUND, "widget_not_found");
    };
    let (Some(binding_id), Some(core)) = (bd.id, st.core.as_deref()) else {
        return error(StatusCode::NOT_FOUND, "widget_not_found");
    };
    if !wc.origin_allowed(&origin) && !bd.allowed_origins().contains(&origin) {
        return error(StatusCode::FORBIDDEN, "origin_not_allowed");
    }
    // A pure check here (flood pre-filter); the token is taken only once core issued the session, so a 428
    // `turnstile_required` round-trip and its retry cost one session, not two.
    if let Err(wait) = wc.check_session(&ip) {
        return rate_limited(wait);
    }
    let req = WebChatSessionReq {
        binding_id,
        widget_key: b.widget_key.clone(),
        origin,
        ip_hash: wc.ip_hash(Some(core), &ip),
        turnstile_token: turnstile_token.map(str::to_string),
        // siteverify's `remoteip` only; agent-api keeps the hash
        client_ip: (ip != "unknown").then(|| ip.clone()),
        customer_assertion: assertion.map(str::to_string),
    };
    match core.webchat_session(&req).await {
        Ok(s) if session_id_of(&s.session_token) == Some(s.session_id.as_str()) => {
            wc.take_session(&ip);
            metrics::counter!("agent_edge_webchat_sessions_total", "source" => "core").increment(1);
            (
                StatusCode::CREATED,
                Json(
                    json!({"session_token": s.session_token, "expires_at": s.expires_at,
                    "display": s.display, "csat_enabled": s.csat_enabled, "attachments": s.attachments_enabled,
                    "identified": s.identified,
                    "greeting_name": s.greeting_name, "sign_in_url": s.sign_in_url}),
                ),
            )
                .into_response()
        }
        Ok(_) => {
            tracing::warn!(channel = CHANNEL, "webchat_session_token_shape");
            error(StatusCode::BAD_GATEWAY, "upstream_error")
        }
        Err(SeamError::Refused {
            status,
            code,
            retry_after_s,
            site_key,
        }) => turnstile_refusal(status, &code, site_key.as_deref())
            .or_else(|| seam_refusal(status, &code, retry_after_s))
            .unwrap_or_else(|| {
                tracing::warn!(channel = CHANNEL, status, code = %code, "webchat_session_failed");
                error(StatusCode::BAD_GATEWAY, "upstream_error")
            }),
        Err(SeamError::Unavailable(r)) => {
            tracing::warn!(channel = CHANNEL, reason = %r, "webchat_session_failed");
            error(StatusCode::SERVICE_UNAVAILABLE, "unavailable")
        }
    }
}

#[derive(Deserialize)]
struct WireBody {
    widget_key: String,
    visitor_id: String,
    messages: Vec<WireMessage>,
    #[serde(default)]
    stream: bool,
    /// B12: the conversation id + token the first core answer issued.
    #[serde(default)]
    conversation_id: Option<String>,
    #[serde(default)]
    token: Option<String>,
    /// B12: the widget's id for this message (`[A-Za-z0-9_-]{8,64}`), agent-api's retry dedupe key.
    #[serde(default)]
    message_id: Option<String>,
    /// SW-9: ids of the customer's uploaded files this message carries (at most [`MAX_MESSAGE_ATTACHMENTS`]).
    #[serde(default)]
    attachments: Vec<Uuid>,
}

#[derive(Deserialize)]
struct WireMessage {
    role: String,
    text: String,
}

/// `CF-Connecting-IP` (set by Cloudflare, which overwrites a client's own), else the socket peer, else `unknown`.
/// `X-Forwarded-For` is ignored: a visitor sets it freely, so trusting it would let one IP pose as many and slip the
/// per-IP limits.
fn client_ip(headers: &HeaderMap, peer: Option<SocketAddr>) -> String {
    if let Some(ip) = headers
        .get("cf-connecting-ip")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        return ip.to_string();
    }
    peer.map_or_else(|| "unknown".to_string(), |a| a.ip().to_string())
}

async fn messages(
    State(st): State<EdgeState>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let Some(wc) = st.webchat.clone() else {
        return not_configured();
    };
    let Some(origin) = allowed_origin(&st, &wc, &headers) else {
        return error(StatusCode::FORBIDDEN, "origin_not_allowed");
    };
    let peer = peer.map(|Extension(ConnectInfo(a))| a);
    let r = handle_message(&st, &wc, peer, &headers, body).await;
    with_cors(r, origin)
}

async fn handle_message(
    st: &EdgeState,
    wc: &WebChat,
    peer: Option<SocketAddr>,
    headers: &HeaderMap,
    body: Body,
) -> Response {
    let declared = headers
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok());
    if declared.is_some_and(|n| n > MAX_BODY_BYTES) {
        return error(StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large");
    }
    let Ok(raw) = axum::body::to_bytes(body, MAX_BODY_BYTES).await else {
        return error(StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large");
    };
    let Ok(b) = serde_json::from_slice::<WireBody>(&raw) else {
        return error(StatusCode::BAD_REQUEST, "invalid_body");
    };
    let (scope, binding_id) = match wc.cfg.bindings.get(&b.widget_key) {
        Some(s) => (s.clone(), None),
        None => {
            let Some(bd) = st
                .bindings
                .find(ChannelKind::WebChat, Side::Customer, &b.widget_key)
            else {
                return error(StatusCode::NOT_FOUND, "widget_not_found");
            };
            let origin = headers
                .get(ORIGIN)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default();
            if !wc.origin_allowed(origin) && !bd.allowed_origins().iter().any(|a| a == origin) {
                return error(StatusCode::FORBIDDEN, "origin_not_allowed");
            }
            (bd.scope, bd.id)
        }
    };
    let scope = &scope;
    if !valid_visitor_id(&b.visitor_id) {
        return error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_visitor_id");
    }
    // B13: `(session id, token)`. A core binding requires a session (agent-api checks it); an env key accepts its
    // own edge-signed token, or none (a widget cached from before sessions).
    let presented = bearer(headers);
    let session = match (binding_id, presented) {
        (Some(_), Some(t)) => match session_id_of(&t).map(str::to_string) {
            Some(id) => Some((id, t)),
            None => return error(StatusCode::UNAUTHORIZED, "session_invalid"),
        },
        (Some(_), None) => return error(StatusCode::UNAUTHORIZED, "session_invalid"),
        (None, Some(t)) => match wc.verify_session(&b.widget_key, &t) {
            Some(id) => Some((id, t)),
            None => return error(StatusCode::UNAUTHORIZED, "session_invalid"),
        },
        (None, None) => None,
    };
    let ip = client_ip(headers, peer);
    let visitor = session.is_none().then_some(b.visitor_id.as_str());
    if let Err(wait) = wc.admit(&ip, &b.widget_key, visitor) {
        return rate_limited(wait);
    }
    let party_id = match &session {
        Some((id, _)) => format!("wc:{id}"),
        None => format!("wc:{}", b.visitor_id),
    };
    if b.attachments.len() > MAX_MESSAGE_ATTACHMENTS
        || (!b.attachments.is_empty() && st.core.is_none())
    {
        return error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_attachments");
    }
    let last_ok = b
        .messages
        .last()
        .is_some_and(|m| m.role == "user" && !m.text.trim().is_empty());
    if !last_ok {
        return error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_messages");
    }
    if let Some(core) = st.core.as_deref() {
        let wire = WireSession {
            session_id: session.as_ref().map(|s| s.0.clone()),
            party_id,
            token: session.map(|s| s.1),
            ip_hash: wc.ip_hash(Some(core), &ip),
        };
        return core_message(core, &b, scope, binding_id, wire).await;
    }
    let mut input = Vec::with_capacity(b.messages.len());
    for m in b.messages {
        let role = match m.role.as_str() {
            "user" => Role::User,
            "assistant" => Role::Assistant,
            _ => return error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_messages"),
        };
        if !m.text.trim().is_empty() {
            input.push(Message::text(role, m.text));
        }
    }
    let excess = input.len().saturating_sub(MAX_HISTORY);
    input.drain(..excess);
    let mut req = InteractionRequest::new(scope, input);
    req.channel = CHANNEL.into();
    req.party = Some(Party::customer(party_id, None));
    // Over the input bound with history: drop the oldest turns until it fits (the last message alone may still be
    // 413).
    loop {
        match req.validate() {
            Ok(()) => break,
            Err(ApiError::TooLarge(..)) if req.input.len() > 1 => {
                req.input.remove(0);
            }
            Err(e) => return e.into_response(),
        }
    }
    metrics::counter!("agent_edge_messages_total", "channel" => CHANNEL).increment(1);
    let events = st.backend.respond(req);
    if b.stream {
        return stream_answer(events, st.answer_deadline);
    }
    match tokio::time::timeout(st.answer_deadline, collect(events)).await {
        Ok(Ok(r)) => Json(json!({"text": r.text})).into_response(),
        Ok(Err(e)) => {
            let code = public_code(&e);
            tracing::warn!(channel = CHANNEL, code, error = %e, "channel_answer_failed");
            let (status, _) = ApiError::from(e).status_and_code();
            error(status, code)
        }
        Err(_) => {
            tracing::warn!(channel = CHANNEL, "channel_answer_timeout");
            error(StatusCode::GATEWAY_TIMEOUT, "timeout")
        }
    }
}

/// What the edge vouches for on a core web chat message (B13).
struct WireSession {
    /// The session id (core bindings always carry one).
    session_id: Option<String>,
    /// `wc:<session id>` (or `wc:<visitor id>` without a session).
    party_id: String,
    token: Option<String>,
    ip_hash: String,
}

/// B12: the last message through agent-api's `inbound`; JSON `{status, text, conversation_id, token}`.
async fn core_message(
    core: &CoreLink,
    b: &WireBody,
    scope: &Scope,
    binding_id: Option<Uuid>,
    wire: WireSession,
) -> Response {
    // agent-api accepts web chat only on a known binding (an env widget key has none)
    let Some(binding_id) = binding_id else {
        return error(StatusCode::NOT_FOUND, "widget_not_found");
    };
    let presented = match (&b.conversation_id, &b.token) {
        (Some(c), Some(t)) => {
            let core_id = valid_conversation_id(c)
                .then(|| verify_token(&core.key, c, t))
                .flatten();
            // the token names the binding it was issued on: it is valid on that widget only
            let Some(core_id) = core_id.filter(|id| {
                id.rsplit_once('~').map(|(_, b)| b) == Some(binding_id.to_string().as_str())
            }) else {
                return error(StatusCode::FORBIDDEN, "invalid_token");
            };
            Some((c.clone(), core_id, t.clone()))
        }
        _ => None,
    };
    // no conversation token yet: the session's own id (a resent first message reuses it), else a fresh one
    let conversation_id = match (&presented, &wire.session_id) {
        (Some(p), _) => p.0.clone(),
        (None, Some(sid)) => session_conversation_id(sid),
        (None, None) => new_conversation_id(),
    };
    let message_id = b
        .message_id
        .clone()
        .filter(|m| valid_visitor_id(m))
        .unwrap_or_else(|| Uuid::new_v4().simple().to_string());
    let text = b
        .messages
        .last()
        .map(|m| m.text.clone())
        .unwrap_or_default();
    let req = InboundReq {
        binding_id,
        kind: CHANNEL.into(),
        external_id: b.widget_key.clone(),
        tenant: scope.tenant,
        job_id: scope.job_id,
        job_key: scope.job_key.clone(),
        conversation: ConversationRef {
            channel: CHANNEL.into(),
            id: conversation_id.clone(),
            route: BTreeMap::from([("widget_key".to_string(), b.widget_key.clone())]),
        },
        message_id,
        party: WireParty {
            id: wire.party_id,
            name: None,
            verified: false,
        },
        text,
        session_token: wire.token,
        ip_hash: Some(wire.ip_hash),
        accept_only: false,
        attachments: b.attachments.clone(),
    };
    metrics::counter!("agent_edge_messages_total", "channel" => CHANNEL).increment(1);
    let mut kind: Option<&'static str> = None;
    let (status, text, core_id) = match core.webchat_inbound(&req).await {
        Ok(InboundResp::Answered {
            conversation_id,
            text,
            ..
        }) => ("answered", Some(text), Some(conversation_id)),
        Ok(InboundResp::Queued {
            conversation_id,
            notice,
            ..
        }) => (
            "queued",
            notice
                .then(|| outbound_text(&Outbound::HandoverNotice { eta_minutes: None }))
                .flatten(),
            Some(conversation_id),
        ),
        // B13: these carry the conversation too, so the widget gets its token on every answer (a first message
        // resent after a lost answer comes back `duplicate`: with the token it opens its stream and reloads history)
        Ok(InboundResp::CsatRecorded {
            text,
            conversation_id,
        }) => ("csat_recorded", Some(text), conversation_id),
        // SW-6: the rating prompt; `kind` tells the widget to show its rating buttons
        Ok(InboundResp::CsatRequest {
            text,
            conversation_id,
        }) => {
            kind = Some("csat_request");
            ("csat_request", Some(text), conversation_id)
        }
        Ok(InboundResp::Duplicate { conversation_id }) => ("duplicate", None, conversation_id),
        // only an `accept_only` call (R6) is answered `accepted`; web chat never sends one
        Ok(InboundResp::Accepted {
            conversation_id, ..
        }) => ("accepted", None, Some(conversation_id)),
        Err(SeamError::Refused { code, .. }) if not_available_code(&code) => {
            return error(StatusCode::SERVICE_UNAVAILABLE, "not_available");
        }
        Err(SeamError::Refused {
            status,
            code,
            retry_after_s,
            ..
        }) => {
            if let Some(r) = seam_refusal(status, &code, retry_after_s) {
                return r;
            }
            tracing::warn!(channel = CHANNEL, status, code = %code, "channel_core_failed");
            return error(StatusCode::BAD_GATEWAY, "upstream_error");
        }
        Err(SeamError::Unavailable(r)) if r == "timeout" => {
            tracing::warn!(channel = CHANNEL, "channel_answer_timeout");
            return error(StatusCode::GATEWAY_TIMEOUT, "timeout");
        }
        Err(e) => {
            tracing::warn!(channel = CHANNEL, error = %e, "channel_core_failed");
            return error(StatusCode::BAD_GATEWAY, "upstream_error");
        }
    };
    let token = match core_id {
        Some(id) => Some(issue_token(
            &core.key,
            &conversation_id,
            &format!("{id}~{binding_id}"),
        )),
        None => presented.map(|p| p.2),
    };
    let conversation_id = token.as_ref().map(|_| conversation_id);
    Json(
        json!({"status": status, "text": text, "kind": kind, "conversation_id": conversation_id, "token": token}),
    )
    .into_response()
}

/// One raw (unencoded) query parameter: tokens are base64url + `.` + an id, `after_ms` digits.
fn raw_param(q: Option<&str>, name: &str) -> Option<String> {
    q?.split('&').find_map(|p| {
        let (k, v) = p.split_once('=')?;
        (k == name).then(|| v.to_string())
    })
}

/// The conversation's core id when the web chat is on with a core link, the id is well formed and the token
/// verifies; otherwise the error answer. A present but foreign `Origin` is 403.
fn conversation_access(
    st: &EdgeState,
    headers: &HeaderMap,
    id: &str,
    query: Option<&str>,
) -> Result<(String, Option<HeaderValue>), (StatusCode, &'static str)> {
    let (Some(wc), Some(core)) = (st.webchat.as_deref(), st.core.as_deref()) else {
        return Err((StatusCode::NOT_FOUND, "web_chat_not_configured"));
    };
    let origin = match headers.get(ORIGIN) {
        None => None,
        Some(_) => match allowed_origin(st, wc, headers) {
            Some(o) => Some(o),
            None => return Err((StatusCode::FORBIDDEN, "origin_not_allowed")),
        },
    };
    let core_id = valid_conversation_id(id)
        .then(|| raw_param(query, "token"))
        .flatten()
        .and_then(|t| verify_token(&core.key, id, &t));
    match core_id {
        Some(c) => Ok((c, origin)),
        None => Err((StatusCode::FORBIDDEN, "invalid_token")),
    }
}

fn cors_opt(r: Response, origin: Option<HeaderValue>) -> Response {
    match origin {
        Some(o) => with_cors(r, o),
        None => r,
    }
}

/// The core conversation id and binding scope a verified token carries (`<core conversation id>~<binding id>`);
/// `None` when its binding is no longer known.
fn core_scope(st: &EdgeState, core_id: &str) -> Option<(String, HistoryScope)> {
    let (c, b) = core_id.rsplit_once('~')?;
    let b = st.bindings.by_id(Uuid::parse_str(b).ok()?)?;
    Some((
        c.to_string(),
        HistoryScope {
            tenant: b.scope.tenant,
            job_id: b.scope.job_id,
            binding_id: b.id?,
        },
    ))
}

/// Whether the admin currently allows customer files on the binding a conversation token (`<core id>~<binding id>`)
/// names, from the edge's binding cache. An unknown binding answers true: the history route refuses it anyway, and the
/// upload route is the authority on files.
fn customer_files_on(st: &EdgeState, token_core_id: &str) -> bool {
    token_core_id
        .rsplit_once('~')
        .and_then(|(_, b)| Uuid::parse_str(b).ok())
        .and_then(|b| st.bindings.by_id(b))
        .map(|b| {
            crate::conversations::attachment_settings::Switches::of("web_chat", &b.settings)
                .customer
        })
        .unwrap_or(true)
}

/// A conversation token must not outlive its session: the stream and history also prove the SESSION. The session
/// token is the Bearer (a `fetch`) or `?session=` (an `EventSource` cannot set a header); core checks it is known on
/// this binding, unexpired, not blocked and, for an identified session, inside `session_hours`. `Err` is the
/// answer to send.
#[allow(clippy::result_large_err)]
async fn session_proof(
    core: &CoreLink,
    headers: &HeaderMap,
    query: Option<&str>,
    scope: &HistoryScope,
    conversation_id: &str,
) -> Result<(), Response> {
    let token = bearer(headers)
        .or_else(|| raw_param(query, "session"))
        .filter(|t| session_id_of(t).is_some());
    let Some(session_token) = token else {
        return Err(error(StatusCode::UNAUTHORIZED, "session_invalid"));
    };
    let req = WebChatSessionCheckReq {
        tenant: scope.tenant,
        job_id: scope.job_id,
        binding_id: scope.binding_id,
        conversation_id: conversation_id.to_string(),
        session_token,
    };
    match core.webchat_session_check(&req).await {
        Ok(_) => Ok(()),
        Err(SeamError::Refused {
            status,
            code,
            retry_after_s,
            ..
        }) => Err(seam_refusal(status, &code, retry_after_s).unwrap_or_else(|| {
            tracing::warn!(channel = CHANNEL, status, code = %code, "webchat_session_check_failed");
            error(StatusCode::BAD_GATEWAY, "upstream_error")
        })),
        Err(SeamError::Unavailable(r)) => {
            tracing::warn!(channel = CHANNEL, reason = %r, "webchat_session_check_failed");
            Err(error(StatusCode::SERVICE_UNAVAILABLE, "unavailable"))
        }
    }
}

/// `POST /api/webchat/attachments`: multipart `file`, `conversation` (the id) and `token` (the conversation token),
/// session token as `Authorization: Bearer`. A body over [`MAX_UPLOAD_BYTES`] is refused from its `Content-Length`
/// before a byte is read, and the body stream is capped at the same size. Core validates and stores the file
/// (crate::conversations::webchat_attachments): 201 `{id, media_type, display_name, byte_size, width?, height?}`;
/// 422 `attachment_refused {reason}`, 413 `attachment_quota` | `too_large`, 429, 401 `session_invalid`, 403.
async fn upload_attachment(State(st): State<EdgeState>, req: Request) -> Response {
    let Some(wc) = st.webchat.as_deref() else {
        return not_configured();
    };
    let headers = req.headers().clone();
    let Some(origin) = allowed_origin(&st, wc, &headers) else {
        return error(StatusCode::FORBIDDEN, "origin_not_allowed");
    };
    with_cors(handle_upload(&st, &headers, req).await, origin)
}

async fn handle_upload(st: &EdgeState, headers: &HeaderMap, req: Request) -> Response {
    let declared = headers
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok());
    if declared.is_some_and(|n| n > MAX_UPLOAD_BYTES) {
        return error(StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large");
    }
    let Ok(mut mp) = Multipart::from_request(req, &()).await else {
        return error(StatusCode::BAD_REQUEST, "invalid_body");
    };
    let (mut conversation, mut token, mut file) = (None, None, None);
    loop {
        let field = match mp.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => {
                return error(StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large");
            }
            Err(_) => return error(StatusCode::BAD_REQUEST, "invalid_body"),
        };
        let name = field.name().unwrap_or_default().to_string();
        match name.as_str() {
            "file" if file.is_none() => {
                let fname = field.file_name().unwrap_or_default().to_string();
                match field.bytes().await {
                    Ok(b) => file = Some((fname, b.to_vec())),
                    Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => {
                        return error(StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large");
                    }
                    Err(_) => return error(StatusCode::BAD_REQUEST, "invalid_body"),
                }
            }
            "conversation" | "token" => match field.text().await {
                Ok(t) if t.len() <= 512 => {
                    if name == "token" {
                        token = Some(t);
                    } else {
                        conversation = Some(t);
                    }
                }
                _ => return error(StatusCode::BAD_REQUEST, "invalid_body"),
            },
            _ => {}
        }
    }
    let (Some(conversation), Some(token), Some((fname, bytes))) = (conversation, token, file)
    else {
        return error(StatusCode::BAD_REQUEST, "invalid_body");
    };
    if token.contains(['&', '#', ' ']) {
        return error(StatusCode::FORBIDDEN, "invalid_token");
    }
    let (core_id, origin) =
        match conversation_access(st, headers, &conversation, Some(&format!("token={token}"))) {
            Ok(a) => a,
            Err((status, code)) => return error(status, code),
        };
    drop(origin);
    let Some(core) = st.core.as_deref() else {
        return not_configured();
    };
    let Some((core_id, scope)) = core_scope(st, &core_id) else {
        return error(StatusCode::NOT_FOUND, "widget_not_found");
    };
    if let Err(r) = session_proof(core, headers, None, &scope, &core_id).await {
        return r;
    }
    let Some(session_token) = bearer(headers) else {
        return error(StatusCode::UNAUTHORIZED, "session_invalid");
    };
    match core
        .webchat_attachment_upload(scope, &core_id, &session_token, bytes, &fname)
        .await
    {
        Ok(r) => (StatusCode::CREATED, Json(r)).into_response(),
        Err(AttachmentUploadError::Refused { reason }) => {
            metrics::counter!("agent_edge_webchat_refused_total", "code" => "attachment_refused")
                .increment(1);
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({"error": {"code": "attachment_refused", "reason": reason}})),
            )
                .into_response()
        }
        Err(AttachmentUploadError::Seam(SeamError::Refused {
            status,
            code,
            retry_after_s,
            ..
        })) => seam_refusal(status, &code, retry_after_s).unwrap_or_else(|| {
            tracing::warn!(channel = CHANNEL, status, code = %code, "webchat_attachment_upload_failed");
            error(StatusCode::BAD_GATEWAY, "upstream_error")
        }),
        Err(AttachmentUploadError::Seam(SeamError::Unavailable(r))) => {
            tracing::warn!(channel = CHANNEL, reason = %r, "webchat_attachment_upload_failed");
            error(StatusCode::SERVICE_UNAVAILABLE, "unavailable")
        }
    }
}

/// `GET /api/webchat/conversations/{id}/attachments/{att}?token=`: the file's bytes, proxied from core, for the
/// holder of the conversation token AND the session (`Authorization: Bearer <session token>`). Always
/// `nosniff`, `private, no-store` and a sandboxing CSP; inline only for images, a PDF is a download.
async fn fetch_attachment(
    State(st): State<EdgeState>,
    Path((id, att)): Path<(String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let (core_id, origin) = match conversation_access(&st, &headers, &id, query.as_deref()) {
        Ok(a) => a,
        Err((status, code)) => return error(status, code),
    };
    let Some(core) = st.core.as_deref() else {
        return not_configured();
    };
    let Ok(att) = Uuid::parse_str(&att) else {
        return cors_opt(error(StatusCode::NOT_FOUND, "not_found"), origin);
    };
    let Some((core_id, scope)) = core_scope(&st, &core_id) else {
        return cors_opt(error(StatusCode::NOT_FOUND, "widget_not_found"), origin);
    };
    if let Err(r) = session_proof(core, &headers, query.as_deref(), &scope, &core_id).await {
        return cors_opt(r, origin);
    }
    let r = match core.webchat_attachment_fetch(scope, &core_id, att).await {
        Ok((media_type, name, bytes)) if ALLOWED_MEDIA.contains(&media_type.as_str()) => {
            attachment_response(&media_type, &name, bytes)
        }
        Ok(_) => {
            tracing::warn!(channel = CHANNEL, "webchat_attachment_media_type_refused");
            error(StatusCode::BAD_GATEWAY, "upstream_error")
        }
        Err(SeamError::Refused { status: 404, .. }) => error(StatusCode::NOT_FOUND, "not_found"),
        Err(SeamError::Refused {
            status,
            code,
            retry_after_s,
            ..
        }) => seam_refusal(status, &code, retry_after_s).unwrap_or_else(|| {
            tracing::warn!(channel = CHANNEL, status, code = %code, "webchat_attachment_fetch_failed");
            error(StatusCode::BAD_GATEWAY, "upstream_error")
        }),
        Err(SeamError::Unavailable(r)) => {
            tracing::warn!(channel = CHANNEL, reason = %r, "webchat_attachment_fetch_failed");
            error(StatusCode::SERVICE_UNAVAILABLE, "unavailable")
        }
    };
    cors_opt(r, origin)
}

/// The bytes of an attachment with the headers every one carries (design §2).
fn attachment_response(media_type: &str, name: &str, bytes: Vec<u8>) -> Response {
    let ascii: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut encoded = String::new();
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.') {
            encoded.push(b as char);
        } else {
            encoded.push_str(&format!("%{b:02X}"));
        }
    }
    let disposition = if media_type == "application/pdf" {
        format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")
    } else {
        "inline".to_string()
    };
    let mut r = (StatusCode::OK, bytes).into_response();
    let h = r.headers_mut();
    if let Ok(v) = HeaderValue::from_str(media_type) {
        h.insert(CONTENT_TYPE, v);
    }
    if let Ok(v) = HeaderValue::from_str(&disposition) {
        h.insert(CONTENT_DISPOSITION, v);
    }
    h.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    h.insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("sandbox; default-src 'none'"),
    );
    r
}

/// `GET /api/webchat/conversations/{id}/events?token=`: SSE `message` events for later messages, proxied from
/// agent-api's stream of the conversation (see the module doc).
async fn events(
    State(st): State<EdgeState>,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let (core_id, origin) = match conversation_access(&st, &headers, &id, query.as_deref()) {
        Ok(a) => a,
        Err((status, code)) => return error(status, code),
    };
    let Some(core) = st.core.as_deref() else {
        return not_configured();
    };
    let Some((core_id, scope)) = core_scope(&st, &core_id) else {
        return cors_opt(error(StatusCode::NOT_FOUND, "widget_not_found"), origin);
    };
    if let Err(r) = session_proof(core, &headers, query.as_deref(), &scope, &core_id).await {
        // core unreachable: the same answer as an unreachable stream below, a 200 that asks the browser to retry
        if r.status() == StatusCode::SERVICE_UNAVAILABLE {
            let retry = stream::once(std::future::ready(Ok::<_, Infallible>(
                Event::default().retry(UPSTREAM_RETRY),
            )));
            return cors_opt(Sse::new(retry).into_response(), origin);
        }
        return cors_opt(r, origin);
    }
    let Some(slot) = st.webchat.as_ref().and_then(|wc| wc.open_stream(&id)) else {
        return cors_opt(rate_limited(STREAM_RETRY_S), origin);
    };
    let open = stream::once(std::future::ready(
        Event::default().comment(SSE_OPEN_COMMENT),
    ));
    let frames = match core.webchat_events(&core_id, scope).await {
        Ok(resp) => stream::unfold(Some((resp, Vec::new())), |state| async move {
            let (mut resp, mut buf) = state?;
            loop {
                if let Some(block) = next_block(&mut buf) {
                    // B7: the agent asks an unidentified visitor to sign in (a structured event, not a message)
                    if let Some(url) = upstream_sign_in(&block) {
                        let e = Event::default()
                            .event("sign_in")
                            .data(json!({ "url": url }).to_string());
                        return Some((e, Some((resp, buf))));
                    }
                    let Some(ev) = upstream_event(&block) else {
                        continue;
                    };
                    let e = Event::default()
                        .event("message")
                        .id(ev.id.clone())
                        .data(json!(ev).to_string());
                    return Some((e, Some((resp, buf))));
                }
                // ended, failed, silent past three keep-alives or flooding: end; the browser reconnects
                match tokio::time::timeout(UPSTREAM_IDLE, resp.chunk()).await {
                    Ok(Ok(Some(c))) if buf.len() + c.len() <= UPSTREAM_MAX_BUFFER => {
                        buf.extend_from_slice(&c)
                    }
                    _ => return None,
                }
            }
        })
        .boxed(),
        Err(CoreError::Code(c)) if c == "conversation_not_found" || c == "binding_not_found" => {
            return cors_opt(
                error(StatusCode::NOT_FOUND, "conversation_not_found"),
                origin,
            );
        }
        Err(e) => {
            tracing::warn!(channel = CHANNEL, error = %e, "channel_stream_failed");
            stream::once(std::future::ready(Event::default().retry(UPSTREAM_RETRY))).boxed()
        }
    };
    // the slot is the stream's: freed when it ends or the customer goes away
    let frames = open.chain(frames).map(move |e| {
        let _slot = &slot;
        Ok::<_, Infallible>(e)
    });
    let mut r = Sse::new(frames)
        .keep_alive(KeepAlive::new().interval(KEEP_ALIVE))
        .into_response();
    let h = r.headers_mut();
    h.insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    h.insert(
        CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-store"),
    );
    h.insert("x-accel-buffering", HeaderValue::from_static("no"));
    cors_opt(r, origin)
}

/// `GET /api/webchat/conversations/{id}/messages?token=&after_ms=`: the stored history, from core.
async fn history(
    State(st): State<EdgeState>,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let (core_id, origin) = match conversation_access(&st, &headers, &id, query.as_deref()) {
        Ok(a) => a,
        Err((status, code)) => return error(status, code),
    };
    let Some(core) = st.core.as_deref() else {
        return not_configured();
    };
    let after_ms = raw_param(query.as_deref(), "after_ms").and_then(|v| v.parse::<i64>().ok());
    // the token carries `<core conversation id>~<binding id>`; the binding gives the scope agent-api checks
    let customer_files = customer_files_on(&st, &core_id);
    let Some((core_id, scope)) = core_scope(&st, &core_id) else {
        return cors_opt(error(StatusCode::NOT_FOUND, "widget_not_found"), origin);
    };
    if let Err(r) = session_proof(core, &headers, query.as_deref(), &scope, &core_id).await {
        return cors_opt(r, origin);
    }
    let r = match core.history(&core_id, scope, after_ms).await {
        Ok(h) => {
            // a reloaded tab learns the admin's current customer-files switch here (it has no session open to read it from)
            let mut body = serde_json::to_value(&h).unwrap_or_else(|_| json!({}));
            if let Some(o) = body.as_object_mut() {
                o.insert("attachments".into(), json!(customer_files));
            }
            Json(body).into_response()
        }
        Err(e) => {
            tracing::warn!(channel = CHANNEL, error = %e, "channel_history_failed");
            error(StatusCode::BAD_GATEWAY, "upstream_error")
        }
    };
    cors_opt(r, origin)
}

/// The customer-facing code of a backend failure (no operator detail).
fn public_code(e: &BackendError) -> &'static str {
    match e {
        BackendError::NotConfigured(_) | BackendError::NotLive(_) => "not_available",
        BackendError::Unavailable(_) => "unavailable",
        BackendError::Upstream(_) => "upstream_error",
    }
}

fn error_event(code: &'static str) -> Event {
    Event::default()
        .event("error")
        .data(json!({"code": code}).to_string())
}

/// SSE: `delta` `{text}` …, then `done` `{}`; or one `error` `{code}`. The deadline bounds the whole answer.
fn stream_answer(
    events: BoxStream<'static, Result<BackendEvent, BackendError>>,
    deadline: Duration,
) -> Response {
    let until = tokio::time::Instant::now() + deadline;
    let frames = stream::unfold(Some(events), move |state| async move {
        let mut events = state?;
        loop {
            match tokio::time::timeout_at(until, events.next()).await {
                Err(_) => {
                    tracing::warn!(channel = CHANNEL, "channel_answer_timeout");
                    return Some((error_event("timeout"), None));
                }
                Ok(None) => return Some((error_event("upstream_error"), None)),
                Ok(Some(Err(e))) => {
                    let code = public_code(&e);
                    tracing::warn!(channel = CHANNEL, code, error = %e, "channel_answer_failed");
                    return Some((error_event(code), None));
                }
                Ok(Some(Ok(BackendEvent::Done { .. }))) => {
                    return Some((Event::default().event("done").data("{}"), None));
                }
                Ok(Some(Ok(BackendEvent::Delta(t)))) if t.is_empty() => continue,
                Ok(Some(Ok(BackendEvent::Delta(t)))) => {
                    let e = Event::default()
                        .event("delta")
                        .data(json!({"text": t}).to_string());
                    return Some((e, Some(events)));
                }
            }
        }
    });
    Sse::new(frames.map(Ok::<_, Infallible>))
        .keep_alive(KeepAlive::default())
        .into_response()
}

async fn widget_js(State(st): State<EdgeState>) -> Response {
    let Some(wc) = st.webchat.as_deref() else {
        return not_configured();
    };
    (
        [
            (CONTENT_TYPE, "application/javascript; charset=utf-8"),
            (CACHE_CONTROL, "public, max-age=300"),
            (X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        wc.widget_js.clone(),
    )
        .into_response()
}

/// Whether `key` is a web chat binding of this edge (env, or core: one rate-limited pull for a binding created since
/// the last periodic pull, as `handle_session`), so a fresh binding's Try-it link works at once.
async fn demo_key_known(st: &EdgeState, wc: &WebChat, key: &str) -> bool {
    wc.cfg.bindings.contains_key(key)
        || match st.core.as_deref() {
            Some(core) => st
                .bindings
                .find_or_refresh(ChannelKind::WebChat, Side::Customer, key, core)
                .await
                .is_some(),
            None => st
                .bindings
                .find(ChannelKind::WebChat, Side::Customer, key)
                .is_some(),
        }
}

/// The demo page's bridge for an admin test as a known customer. The assertion rides in the URL FRAGMENT (never sent to
/// a server, never in a log), is moved into the widget's identify hook once, and is removed from the address bar so a
/// reload or a copied link carries nothing. The hook answers the assertion the first time it is asked and null after
/// (the assertion is single-use anyway). Without a fragment the page stays an anonymous visitor's.
const DEMO_JS: &str = r#"(function () {
  var a = null;
  try {
    var m = /^#assertion=([A-Za-z0-9_.-]{1,2048})$/.exec(window.location.hash);
    if (m) {
      a = m[1];
      window.history.replaceState(null, "", window.location.pathname + window.location.search);
    }
  } catch (e) {}
  if (a) {
    window.nonagenticChat = {
      identity: function () {
        var v = a;
        a = null;
        return Promise.resolve(v);
      }
    };
  }
})();
"#;

async fn demo_js(State(st): State<EdgeState>) -> Response {
    if st.webchat.is_none() {
        return not_configured();
    }
    (
        [
            (CONTENT_TYPE, "application/javascript; charset=utf-8"),
            (CACHE_CONTROL, "public, max-age=300"),
            (X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        DEMO_JS,
    )
        .into_response()
}

async fn demo(State(st): State<EdgeState>, RawQuery(query): RawQuery) -> Response {
    let Some(wc) = st.webchat.as_deref() else {
        return not_configured();
    };
    let key = query
        .as_deref()
        .and_then(|q| q.split('&').find_map(|p| p.strip_prefix("key=")))
        .filter(|k| valid_widget_key(k));
    let Some(key) = key else {
        return error(StatusCode::NOT_FOUND, "widget_not_found");
    };
    if !demo_key_known(&st, wc, key).await {
        return error(StatusCode::NOT_FOUND, "widget_not_found");
    }
    let title = "Web chat demo";
    // `key` is `wk_[a-z2-7]{24}`: safe in an attribute without escaping.
    let html = format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<style>
  body {{ font: 16px/1.5 system-ui, sans-serif; margin: 0; padding: 32px 16px; background: #f6f7f9; color: #1d2330; }}
  main {{ max-width: 640px; margin: 0 auto; }}
  @media (prefers-color-scheme: dark) {{ body {{ background: #14171c; color: #e6e8ec; }} }}
</style>
</head>
<body>
<main>
<h1>{title}</h1>
<p>This page embeds the web chat widget. Open it with the button at the bottom right.</p>
</main>
<script src="/webchat/demo.js"></script>
<script src="/webchat/widget.js" data-widget="{key}" async></script>
</body>
</html>
"#
    );
    (
        [
            (CONTENT_TYPE, "text/html; charset=utf-8"),
            (CONTENT_SECURITY_POLICY, DEMO_CSP),
            (X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (REFERRER_POLICY, "no-referrer"),
            (CACHE_CONTROL, "no-store"),
        ],
        html,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_in_blocks_pass_through_with_a_plain_url_only() {
        let block = |u: &str| format!("event: sign_in\ndata: {{\"url\":{u}}}");
        assert_eq!(
            upstream_sign_in(&block("\"https://shop.example/login\"")),
            Some(Some("https://shop.example/login".to_string()))
        );
        assert_eq!(upstream_sign_in(&block("null")), Some(None));
        assert_eq!(
            upstream_sign_in(&block("\"javascript:alert(1)\"")),
            Some(None)
        );
        // any other block is not a sign-in
        assert_eq!(
            upstream_sign_in("event: message\ndata: {\"kind\":\"text\",\"text\":\"x\"}"),
            None
        );
    }

    #[test]
    fn origins_normalize_or_refuse() {
        assert_eq!(
            normalize_origin(" HTTPS://Shop.Example.com/ ").as_deref(),
            Some("https://shop.example.com")
        );
        assert!(normalize_origin("https://a.b:8443").is_some());
        assert!(normalize_origin("http://localhost:5173").is_some());
        for bad in [
            "http://shop.example.com",
            "https://",
            "https://a..b",
            "https://a.b/path",
            "https://a.b:0",
            "https://a.b:99999",
            "ftp://a.b",
            "https://-a.b",
            "*",
        ] {
            assert!(normalize_origin(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn keys_and_visitors() {
        assert!(valid_widget_key("wk_abcdefghijklmnopqrstuvwx"));
        assert!(valid_widget_key("wk_234567abcdefghijklmnopqr"));
        assert!(!valid_widget_key("wk_abcdefghijklmnopqrstuvw1"));
        assert!(!valid_widget_key("wk_ABCDEFGHIJKLMNOPQRSTUVWX"));
        assert!(!valid_widget_key("wk_short"));
        assert!(valid_visitor_id("a1b2c3d4"));
        assert!(!valid_visitor_id("short"));
        assert!(!valid_visitor_id("has space 123"));
    }

    #[test]
    fn bucket_refuses_past_capacity_with_a_wait() {
        let mut b = Buckets::<u8>::new((2, Duration::from_secs(60)));
        let now = Instant::now();
        for _ in 0..2 {
            assert!(b.check(&1, now).is_ok());
            b.take(&1);
        }
        let wait = b.check(&1, now).unwrap_err();
        assert!((25..=31).contains(&wait), "{wait}");
        assert!(b.check(&2, now).is_ok());
    }

    #[test]
    fn client_ip_trusts_cloudflare_or_the_peer_never_forwarded_for() {
        let peer: SocketAddr = "192.0.2.9:4000".parse().unwrap();
        let mut h = HeaderMap::new();
        h.insert(
            "x-forwarded-for",
            HeaderValue::from_static("203.0.113.1, 10.0.0.1"),
        );
        assert_eq!(client_ip(&h, Some(peer)), "192.0.2.9");
        assert_eq!(client_ip(&h, None), "unknown");
        h.insert("cf-connecting-ip", HeaderValue::from_static("198.51.100.4"));
        assert_eq!(client_ip(&h, Some(peer)), "198.51.100.4");
    }

    #[test]
    fn a_session_names_one_conversation() {
        let id = session_conversation_id("0123456789abcdef");
        assert!(valid_conversation_id(&id), "{id}");
        assert_eq!(id, session_conversation_id("0123456789abcdef"));
        assert_ne!(id, session_conversation_id("0123456789abcdeg"));
    }

    #[test]
    fn a_sign_in_block_is_recognised_by_event_name_or_kind() {
        let named = "event: sign_in\ndata: {\"url\":\"https://shop.example.com/login\"}";
        assert_eq!(
            upstream_sign_in(named),
            Some(Some("https://shop.example.com/login".to_string()))
        );
        let kind = "data: {\"kind\":\"sign_in\"}";
        assert_eq!(upstream_sign_in(kind), Some(None));
        assert_eq!(
            upstream_sign_in("event: sign_in\ndata: {\"url\":\"\"}"),
            Some(None)
        );
        // a message is not a sign-in, and neither is a keep-alive
        assert_eq!(
            upstream_sign_in("event: message\ndata: {\"kind\":\"text\",\"text\":\"hi\"}"),
            None
        );
        assert_eq!(upstream_sign_in(": keep-alive"), None);
    }
}
