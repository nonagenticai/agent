//! Frappe Helpdesk as a worker-side channel (B12, B7 ruling R9): agent ↔ Frappe both ways.
//!
//! ```text
//! out  agent-api handover ─► edge /internal/notify-workers ─► Helpdesk::notify_worker
//!        POST {base_url}/api/resource/HD Ticket  {subject, description, raised_by, custom_agent_key = item id}
//!        Authorization: token <api_key>:<api_secret>        → thread = ConversationRef{helpdesk, <ticket name>}
//! in   Frappe Webhook ─► POST /api/channels/helpdesk/webhook/{binding_id}
//!        X-Frappe-Webhook-Signature = base64(HMAC-SHA256(webhook_secret, raw body))
//!        {"event":"reply", ...}  Communication after_insert, Sent      → agent-api `staff` (the customer gets it)
//!        {"event":"status", ...} HD Ticket on_update, Resolved|Closed → agent-api `staff-state` (item + case
//!                                                                       resolved, the customer is told)
//! ```
//!
//! SW-5: agent-api runs both as the workspace's own commands, for the agent account the Helpdesk user's email maps to:
//! a reply takes the conversation first (Take) and goes through the one Send path; a close is that Take and a
//! Resolve. So a Helpdesk reply or close shows in the workspace, and the workspace's owner rules hold here: while a
//! colleague owns the conversation the reply or close is refused (409 `owner_changed`) and the ticket gets a comment
//! saying so (crate::channels::workers::OWNED_ELSEWHERE, [`OWNED_ELSEWHERE_CLOSE`]).
//!
//! A binding (`connector_bindings`, kind `helpdesk`, side `worker`) carries `settings {base_url, service_user}` (an email)
//! and the sealed credential `{api_key, api_secret, webhook_secret}`. Every call to Frappe goes through
//! crate::netguard with the edge's `CONNECTOR_PRIVATE_TARGETS` (an in-cluster Frappe is a private target, admitted
//! per tenant by the operator) and the binding's tenant. Message text is customer data: never logged.
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use futures_util::future::BoxFuture;
use ring::hmac;
use serde_json::{Value, json};
use uuid::Uuid;

use super::bindings::{Binding, BindingStore};
use super::connector::{
    ChannelConnector, ChannelError, ChannelKind, ChannelStatus, ConversationRef, DeliveryReceipt,
    EdgeReply, Ingress, IngressAuth, IngressRequest, Outbound, Side, StaffInbound, outbound_text,
};
use super::core::{
    CoreError, StaffIdentity, StaffStateReq, StaffStateStatus, WorkerItem, WorkerTarget,
};
use super::edge::{EdgeState, MAX_WEBHOOK_BYTES, confirm_staff, staff_req};
use super::workers;
use crate::netguard::{self, Policy};

/// Frappe's webhook signature header (`frappe.integrations.doctype.webhook.webhook.WEBHOOK_SECRET_HEADER`).
pub const SIGNATURE_HEADER: &str = "x-frappe-webhook-signature";
/// One Frappe REST call (ticket insert, comment).
pub const CALL_TIMEOUT: Duration = Duration::from_secs(15);
/// The largest Frappe answer read.
const MAX_ANSWER_BYTES: usize = 256 * 1024;
/// HD Ticket statuses that close the handover.
pub const CLOSING_STATUSES: [&str; 2] = ["Resolved", "Closed"];
/// The REST path of an HD Ticket insert (the doctype name has a space).
pub const TICKET_PATH: &str = "/api/resource/HD%20Ticket";
/// The REST path of an HD Ticket Comment insert (an internal note: no email, no webhook loop).
pub const COMMENT_PATH: &str = "/api/resource/HD%20Ticket%20Comment";
/// The longest ticket subject sent.
pub const SUBJECT_CHARS: usize = 140;

/// A helpdesk binding's settings and credential, read from the binding. `Debug` shows no secret.
#[derive(Clone, PartialEq, Eq)]
pub struct HelpdeskTarget {
    /// `http(s)://host[:port]`, no trailing slash.
    pub base_url: String,
    /// The ticket requester when the customer has no email.
    pub service_user_email: String,
    pub api_key: String,
    pub api_secret: String,
}

impl fmt::Debug for HelpdeskTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HelpdeskTarget")
            .field("base_url", &self.base_url)
            .field("service_user", &self.service_user_email)
            .finish_non_exhaustive()
    }
}

fn setting<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn cred<'a>(c: Option<&'a BTreeMap<String, String>>, k: &str) -> Option<&'a str> {
    c.and_then(|m| m.get(k))
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
}

impl HelpdeskTarget {
    /// The target of a binding: `settings.base_url` + `settings.service_user` (an email), credential `api_key` +
    /// `api_secret`. `Invalid` names what is missing (never a value).
    pub fn of(
        settings: &Value,
        credential: Option<&BTreeMap<String, String>>,
    ) -> Result<Self, ChannelError> {
        let base_url = setting(settings, "base_url")
            .ok_or(ChannelError::Invalid("base_url_missing"))?
            .trim_end_matches('/')
            .to_string();
        if !(base_url.starts_with("https://") || base_url.starts_with("http://")) {
            return Err(ChannelError::Invalid("base_url_invalid"));
        }
        let service_user_email = setting(settings, "service_user")
            .and_then(workers::email_like)
            .ok_or(ChannelError::Invalid("service_user_missing"))?;
        let api_key =
            cred(credential, "api_key").ok_or(ChannelError::Invalid("api_key_missing"))?;
        let api_secret =
            cred(credential, "api_secret").ok_or(ChannelError::Invalid("api_secret_missing"))?;
        Ok(Self {
            base_url,
            service_user_email,
            api_key: api_key.to_string(),
            api_secret: api_secret.to_string(),
        })
    }

    /// `Authorization: token <api_key>:<api_secret>` (Frappe token auth).
    pub fn authorization(&self) -> String {
        format!("token {}:{}", self.api_key, self.api_secret)
    }
}

// ---- inbound: signature, payloads -----------------------------------------------------------------------------------

/// `X-Frappe-Webhook-Signature` = base64(HMAC-SHA256(secret, raw body)), compared in constant time (`hmac::verify`).
pub fn verify_signature(
    secret: &str,
    body: &[u8],
    header: Option<&str>,
) -> Result<(), ChannelError> {
    let bad = ChannelError::Unauthenticated("bad_signature");
    let Some(sig) = header
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok())
    else {
        return Err(bad);
    };
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
    hmac::verify(&key, body, &sig).map_err(|_| bad)
}

/// The signature Frappe would send for `body` (tests, the runbook's curl check).
pub fn sign(secret: &str, body: &[u8]) -> String {
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(hmac::sign(&key, body).as_ref())
}

/// One Frappe webhook call, parsed (the `webhook_json` templates of the runbook).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A Helpdesk agent's reply on a ticket (Communication after_insert, `Sent`).
    Reply {
        ticket: String,
        communication: String,
        sender: String,
        sender_name: Option<String>,
        /// The reply as plain text (HTML stripped).
        text: String,
    },
    /// The ticket moved to Resolved or Closed (HD Ticket on_update).
    Closed {
        ticket: String,
        status: String,
        /// The Frappe user who changed it (a user's name is their email).
        modified_by: String,
        /// Frappe's `modified` timestamp as sent (dedupe key part).
        modified: String,
        /// `custom_agent_key` of the ticket: the handover item id (informational; the thread resolves the item).
        agent_key: Option<String>,
    },
    /// Acked and dropped: a received mail, another doctype, a status that does not close, an empty reply.
    Ignored(&'static str),
}

/// A JSON string or number as a string (HD Ticket names are integers; `tojson` renders them as numbers).
fn str_or_num(v: &Value, k: &str) -> Option<String> {
    match v.get(k)? {
        Value::String(s) => Some(s.trim().to_string()).filter(|s| !s.is_empty()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Parse a webhook body. `Invalid` for a body that is not one of the two templates.
pub fn parse_event(body: &[u8]) -> Result<Event, ChannelError> {
    let v: Value =
        serde_json::from_slice(body).map_err(|_| ChannelError::Invalid("invalid_body"))?;
    let event = v
        .get("event")
        .and_then(Value::as_str)
        .ok_or(ChannelError::Invalid("event_missing"))?;
    match event {
        "reply" => {
            // a misconfigured webhook condition must not turn customer mail or other doctypes into staff replies
            if v.get("reference_doctype")
                .and_then(Value::as_str)
                .is_some_and(|d| d != "HD Ticket")
            {
                return Ok(Event::Ignored("doctype"));
            }
            if v.get("sent_or_received")
                .and_then(Value::as_str)
                .is_some_and(|d| d != "Sent")
            {
                return Ok(Event::Ignored("received"));
            }
            let ticket = str_or_num(&v, "ticket").ok_or(ChannelError::Invalid("ticket_missing"))?;
            let communication = str_or_num(&v, "communication")
                .ok_or(ChannelError::Invalid("communication_missing"))?;
            let sender = v
                .get("sender")
                .and_then(Value::as_str)
                .and_then(workers::email_like)
                .ok_or(ChannelError::Invalid("sender_missing"))?;
            let text = strip_html(v.get("content").and_then(Value::as_str).unwrap_or_default());
            if text.is_empty() {
                return Ok(Event::Ignored("empty"));
            }
            Ok(Event::Reply {
                ticket,
                communication,
                sender,
                sender_name: setting(&v, "sender_full_name").map(str::to_string),
                text,
            })
        }
        "status" => {
            let ticket = str_or_num(&v, "ticket").ok_or(ChannelError::Invalid("ticket_missing"))?;
            let status = setting(&v, "status").unwrap_or_default().to_string();
            if !CLOSING_STATUSES.contains(&status.as_str()) {
                return Ok(Event::Ignored("status"));
            }
            Ok(Event::Closed {
                ticket,
                status,
                modified_by: setting(&v, "modified_by").unwrap_or_default().to_string(),
                modified: str_or_num(&v, "modified").unwrap_or_default(),
                agent_key: str_or_num(&v, "agent_key"),
            })
        }
        _ => Ok(Event::Ignored("event")),
    }
}

/// The thread of a ticket (the worker `channel_conversation_refs` row agent-api stored at notify time).
pub fn thread(ticket: &str) -> ConversationRef {
    ConversationRef {
        channel: ChannelKind::Helpdesk.as_str().into(),
        id: ticket.to_string(),
        route: Default::default(),
    }
}

/// A reply event as the staff message the edge sends to agent-api's `staff` route (`target` = the binding's
/// external id: the binding is the one the signed webhook path named).
pub fn staff_inbound(binding: &Binding, ev: &Event) -> Option<StaffInbound> {
    let Event::Reply {
        ticket,
        communication,
        sender,
        sender_name,
        text,
    } = ev
    else {
        return None;
    };
    Some(StaffInbound {
        external_id: format!("hd:{communication}"),
        target: binding.external_id.clone(),
        workspace: None,
        thread: thread(ticket),
        reply_to: thread(ticket),
        staff_id: sender.clone(),
        staff_external_id: sender.clone(),
        staff_name: sender_name.clone(),
        staff_email: Some(sender.clone()),
        text: text.clone(),
        in_thread: true,
    })
}

/// HTML → plain text: tags dropped (`script` / `style` with their content, comments), block tags and `<br>` as line
/// breaks, the common entities decoded, whitespace collapsed, at most one blank line in a row.
pub fn strip_html(html: &str) -> String {
    const BLOCKS: [&str; 17] = [
        "br",
        "p",
        "div",
        "li",
        "ul",
        "ol",
        "tr",
        "table",
        "blockquote",
        "pre",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "hr",
    ];
    let lower = html.to_ascii_lowercase(); // same byte offsets as `html`
    let mut raw = String::with_capacity(html.len());
    let mut i = 0;
    while i < html.len() {
        if html[i..].starts_with("<!--") {
            i = html[i..].find("-->").map_or(html.len(), |p| i + p + 3);
            continue;
        }
        if html.as_bytes()[i] == b'<' {
            let (tag, next) = match html[i..].find('>') {
                Some(e) => (&lower[i + 1..i + e], i + e + 1),
                None => (&lower[i + 1..], html.len()),
            };
            let closing = tag.starts_with('/');
            let name: &str = tag
                .trim_start_matches('/')
                .split(|c: char| c.is_whitespace() || c == '/')
                .next()
                .unwrap_or_default();
            if !closing && (name == "script" || name == "style") {
                let end = format!("</{name}");
                i = match lower[next..].find(&end) {
                    Some(p) => lower[next + p..]
                        .find('>')
                        .map_or(html.len(), |q| next + p + q + 1),
                    None => html.len(),
                };
                continue;
            }
            if BLOCKS.contains(&name) {
                raw.push('\n');
            }
            i = next;
            continue;
        }
        let end = html[i..].find('<').map_or(html.len(), |p| i + p);
        let text = decode_entities(&html[i..end]);
        raw.extend(text.chars().map(|c| {
            if matches!(c, '\n' | '\r' | '\t') {
                ' '
            } else {
                c
            }
        }));
        i = end;
    }
    let mut out: Vec<String> = Vec::new();
    for line in raw.split('\n') {
        let l = line
            .split(|c: char| c.is_whitespace() || c == '\u{a0}')
            .filter(|w| !w.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if l.is_empty() && out.last().is_none_or(|p| p.is_empty()) {
            continue;
        }
        out.push(l);
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out.join("\n")
}

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        rest = &rest[p..];
        let semi = rest.find(';').filter(|e| *e <= 12);
        let decoded = semi.and_then(|e| {
            let ent = &rest[1..e];
            let c = match ent {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" | "#39" => Some('\''),
                "nbsp" => Some('\u{a0}'),
                _ => ent
                    .strip_prefix("#x")
                    .or_else(|| ent.strip_prefix("#X"))
                    .and_then(|h| u32::from_str_radix(h, 16).ok())
                    .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse::<u32>().ok()))
                    .and_then(char::from_u32),
            }?;
            Some((c, e + 1))
        });
        match decoded {
            Some((c, len)) => {
                out.push(c);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Text → HTML-safe text (the ticket description and comments are HTML fields).
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

// ---- outbound: the ticket -------------------------------------------------------------------------------------------

/// The HD Ticket insert body for a handover item: the card text as the description, the customer's email as the
/// requester (else the binding's service user), the item id as `custom_agent_key`.
pub fn ticket_body(item: &WorkerItem, service_user_email: &str) -> Value {
    let raised_by = item
        .customer_email
        .as_deref()
        .and_then(workers::email_like)
        .unwrap_or_else(|| service_user_email.to_string());
    let mut lines: Vec<String> = workers::facts(item)
        .into_iter()
        .map(|(k, v)| format!("<b>{k}:</b> {}", escape_html(&v)))
        .collect();
    let excerpt = workers::excerpt(item);
    if !excerpt.is_empty() {
        lines.push("<b>Last messages:</b>".into());
        for (role, text) in excerpt {
            lines.push(format!("{}: {}", escape_html(&role), escape_html(&text)));
        }
    }
    lines
        .push("Reply on this ticket to answer the customer; resolve or close it when done.".into());
    json!({
        "subject": workers::clip(&workers::title(item), SUBJECT_CHARS),
        "description": format!("<p>{}</p>", lines.join("<br>")),
        "raised_by": raised_by,
        "custom_agent_key": item.id,
    })
}

/// The ticket name in Frappe's insert answer (`{"data": {"name": 12, ...}}`).
pub fn ticket_name(answer: &Value) -> Option<String> {
    str_or_num(answer.get("data")?, "name")
}

// ---- the connector --------------------------------------------------------------------------------------------------

/// The worker-side Frappe Helpdesk connector. Unbound it serves `notify_worker` (the binding is looked up by the
/// target's id) and `status`; [`ChannelConnector::for_binding`] binds it to one binding for the webhook path (comments
/// back on the ticket).
#[derive(Clone)]
pub struct Helpdesk {
    policy: Policy,
    bindings: Arc<BindingStore>,
    bound: Option<Binding>,
}

impl Helpdesk {
    /// `policy` = the edge's `CONNECTOR_PRIVATE_TARGETS`.
    pub fn new(policy: Policy, bindings: Arc<BindingStore>) -> Self {
        Self {
            policy,
            bindings,
            bound: None,
        }
    }

    fn binding_of(&self, id: Uuid) -> Option<Binding> {
        self.bindings
            .by_id(id)
            .filter(|b| b.kind == ChannelKind::Helpdesk && b.side == Side::Worker)
    }

    /// One JSON POST to Frappe through netguard (the binding's tenant, the edge's private targets).
    async fn post(
        &self,
        tenant: Uuid,
        target: &HelpdeskTarget,
        path: &str,
        body: &Value,
    ) -> Result<Value, ChannelError> {
        let (client, url) = self.client_for(tenant, target, path).await?;
        let req = client
            .post(url)
            .header(reqwest::header::AUTHORIZATION, target.authorization())
            .header(reqwest::header::ACCEPT, "application/json")
            .json(body);
        Self::answer(req).await
    }

    /// One GET of a Frappe resource list with `query` through netguard (as [`Self::post`]).
    async fn get(
        &self,
        tenant: Uuid,
        target: &HelpdeskTarget,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<Value, ChannelError> {
        let (client, mut url) = self.client_for(tenant, target, path).await?;
        url.query_pairs_mut()
            .extend_pairs(query.iter().map(|(k, v)| (*k, v.as_str())));
        let req = client
            .get(url)
            .header(reqwest::header::AUTHORIZATION, target.authorization())
            .header(reqwest::header::ACCEPT, "application/json");
        Self::answer(req).await
    }

    /// The pinned client and checked URL of `path` on the binding's Frappe.
    async fn client_for(
        &self,
        tenant: Uuid,
        target: &HelpdeskTarget,
        path: &str,
    ) -> Result<(reqwest::Client, reqwest::Url), ChannelError> {
        let raw = format!("{}{path}", target.base_url);
        let url = netguard::check_url(&raw, &self.policy, tenant).map_err(|e| {
            tracing::warn!(
                channel = "helpdesk",
                code = e.code,
                "helpdesk_target_refused"
            );
            ChannelError::Forbidden("target_not_allowed")
        })?;
        let client = netguard::pinned_client(&url, &self.policy, tenant, CALL_TIMEOUT)
            .await
            .map_err(|e| {
                tracing::warn!(
                    channel = "helpdesk",
                    code = e.code,
                    "helpdesk_target_refused"
                );
                ChannelError::Forbidden("target_not_allowed")
            })?;
        Ok((client, url))
    }

    /// Send `req`; its JSON answer, or the error a Frappe call maps to.
    async fn answer(req: reqwest::RequestBuilder) -> Result<Value, ChannelError> {
        let resp = req.send().await.map_err(|e| {
            ChannelError::Unavailable(if e.is_timeout() { "timeout" } else { "connect" }.into())
        })?;
        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .map_err(|_| ChannelError::Unavailable("read".into()))?;
        if !status.is_success() {
            return Err(ChannelError::Upstream(format!(
                "status {}",
                status.as_u16()
            )));
        }
        if bytes.len() > MAX_ANSWER_BYTES {
            return Err(ChannelError::Upstream("answer_too_large".into()));
        }
        serde_json::from_slice(&bytes).map_err(|_| ChannelError::Upstream("invalid_answer".into()))
    }

    /// Create the HD Ticket of `item` for `binding`; its name.
    pub async fn create_ticket(
        &self,
        binding: &Binding,
        item: &WorkerItem,
    ) -> Result<String, ChannelError> {
        let target = HelpdeskTarget::of(&binding.settings, binding.credential.as_ref())?;
        let answer = self
            .post(
                binding.scope.tenant,
                &target,
                TICKET_PATH,
                &ticket_body(item, &target.service_user_email),
            )
            .await?;
        ticket_name(&answer).ok_or_else(|| ChannelError::Upstream("ticket_name_missing".into()))
    }

    /// An internal comment on a ticket (the staff member's confirmation: not delivered, not staff).
    pub async fn comment(
        &self,
        binding: &Binding,
        ticket: &str,
        text: &str,
    ) -> Result<(), ChannelError> {
        let target = HelpdeskTarget::of(&binding.settings, binding.credential.as_ref())?;
        self.post(
            binding.scope.tenant,
            &target,
            COMMENT_PATH,
            &json!({"reference_ticket": ticket, "content": comment_content(text)}),
        )
        .await
        .map(|_| ())
    }

    /// Whether ticket `ticket` holds an internal comment that is exactly what [`Self::comment`] posts for `text`.
    pub async fn comment_exists(
        &self,
        binding: &Binding,
        ticket: &str,
        text: &str,
    ) -> Result<bool, ChannelError> {
        let target = HelpdeskTarget::of(&binding.settings, binding.credential.as_ref())?;
        let filters = json!([
            ["reference_ticket", "=", ticket],
            ["content", "=", comment_content(text)]
        ]);
        let answer = self
            .get(
                binding.scope.tenant,
                &target,
                COMMENT_PATH,
                &[
                    ("filters", filters.to_string()),
                    ("fields", r#"["name"]"#.to_string()),
                    ("limit_page_length", "1".to_string()),
                ],
            )
            .await?;
        match answer.get("data").and_then(Value::as_array) {
            Some(rows) => Ok(!rows.is_empty()),
            None => Err(ChannelError::Upstream("invalid_answer".into())),
        }
    }
}

/// The HTML body of an internal comment of `text`.
fn comment_content(text: &str) -> String {
    format!("<p>{}</p>", escape_html(text))
}

impl ChannelConnector for Helpdesk {
    fn kind(&self) -> ChannelKind {
        ChannelKind::Helpdesk
    }

    fn side(&self) -> Side {
        Side::Worker
    }

    /// The webhook path names the binding; its credential's `webhook_secret` signs the body.
    fn authenticate<'a>(
        &'a self,
        req: &'a IngressRequest,
    ) -> BoxFuture<'a, Result<IngressAuth, ChannelError>> {
        Box::pin(async move {
            let b = req
                .binding
                .as_ref()
                .filter(|b| b.kind == ChannelKind::Helpdesk)
                .ok_or(ChannelError::Unauthenticated("binding_required"))?;
            let secret = cred(b.credential.as_ref(), "webhook_secret")
                .ok_or(ChannelError::Unauthenticated("webhook_secret_missing"))?;
            verify_signature(secret, &req.body, req.header(SIGNATURE_HEADER))?;
            Ok(IngressAuth::default())
        })
    }

    /// A reply is one staff message; anything else is acked here (the status event is handled by [`webhook`]).
    fn ingest<'a>(
        &'a self,
        req: &'a IngressRequest,
        _auth: &'a IngressAuth,
    ) -> BoxFuture<'a, Result<Ingress, ChannelError>> {
        Box::pin(async move {
            let b = req
                .binding
                .as_ref()
                .ok_or(ChannelError::Unauthenticated("binding_required"))?;
            let ev = parse_event(&req.body)?;
            Ok(match staff_inbound(b, &ev) {
                Some(m) => Ingress::Staff {
                    ack: EdgeReply::ok(),
                    staff: vec![m],
                },
                None => Ingress::Reply(EdgeReply::ok()),
            })
        })
    }

    /// The staff confirmation as an internal comment on the ticket. "Sent to the customer." is not posted (every
    /// reply would carry one); without a bound binding nothing is posted.
    fn deliver<'a>(
        &'a self,
        conv: &'a ConversationRef,
        msg: Outbound,
    ) -> BoxFuture<'a, Result<DeliveryReceipt, ChannelError>> {
        Box::pin(async move {
            let (Some(b), Some(text)) = (self.bound.as_ref(), outbound_text(&msg)) else {
                return Ok(DeliveryReceipt::default());
            };
            if text == workers::SENT {
                return Ok(DeliveryReceipt::default());
            }
            self.comment(b, &conv.id, &text).await?;
            Ok(DeliveryReceipt::default())
        })
    }

    /// An internal staff note as a private comment on the ticket `thread.id`, whatever its text (unlike [`deliver`],
    /// which skips the "Sent to the customer." confirmation). Without a bound binding: an error, nothing is posted.
    fn worker_note<'a>(
        &'a self,
        thread: &'a ConversationRef,
        text: &'a str,
    ) -> BoxFuture<'a, Result<(), ChannelError>> {
        Box::pin(async move {
            let b = self
                .bound
                .as_ref()
                .ok_or(ChannelError::Invalid("binding_not_loaded"))?;
            self.comment(b, &thread.id, text).await
        })
    }

    /// Whether the ticket `thread.id` already holds the note (SW-11, a retry after an unknown outcome).
    fn worker_note_exists<'a>(
        &'a self,
        thread: &'a ConversationRef,
        text: &'a str,
    ) -> BoxFuture<'a, Result<bool, ChannelError>> {
        Box::pin(async move {
            let b = self
                .bound
                .as_ref()
                .ok_or(ChannelError::Invalid("binding_not_loaded"))?;
            self.comment_exists(b, &thread.id, text).await
        })
    }

    fn for_binding(&self, binding: &Binding) -> Option<Arc<dyn ChannelConnector>> {
        (binding.kind == ChannelKind::Helpdesk && binding.side == Side::Worker).then(|| {
            Arc::new(Self {
                bound: Some(binding.clone()),
                ..self.clone()
            }) as Arc<dyn ChannelConnector>
        })
    }

    fn status(&self) -> ChannelStatus {
        ChannelStatus {
            kind: ChannelKind::Helpdesk,
            side: Side::Worker,
            bindings: self
                .bindings
                .of_kind(ChannelKind::Helpdesk, Side::Worker)
                .len(),
        }
    }

    /// Open an HD Ticket for the item; the thread is the ticket.
    fn notify_worker<'a>(
        &'a self,
        target: &'a WorkerTarget,
        item: &'a WorkerItem,
    ) -> BoxFuture<'a, Result<Option<ConversationRef>, ChannelError>> {
        Box::pin(async move {
            let b = self
                .binding_of(target.binding_id)
                .ok_or(ChannelError::Invalid("binding_not_loaded"))?;
            let ticket = self.create_ticket(&b, item).await?;
            Ok(Some(thread(&ticket)))
        })
    }
}

// ---- the edge route -------------------------------------------------------------------------------------------------

fn refuse(status: StatusCode, code: &'static str) -> Response {
    (status, axum::Json(json!({"error": {"code": code}}))).into_response()
}

/// `POST /api/channels/helpdesk/webhook/{binding_id}` (mounted through the edge's BYO webhook route): the binding
/// must be an enabled helpdesk worker binding; the body is signed with its `webhook_secret`. B7 R6 persist-before-200:
/// a reply goes to agent-api's `staff` route, a close to `staff-state`, awaited before the answer (both store first and
/// are short); core unreachable or 5xx → 503 (Frappe's Webhook Request Log shows the failure, and a resend is not
/// dropped as seen). A refusal (`not_staff`, `thread_not_found`) and a stored reply whose customer delivery failed
/// are 200. The comment confirming a refusal goes to Frappe in the background.
pub async fn webhook(
    st: EdgeState,
    binding_id: &str,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let kind = ChannelKind::Helpdesk;
    let binding = Uuid::parse_str(binding_id)
        .ok()
        .and_then(|id| st.bindings.by_id(id))
        .filter(|b| b.kind == kind && b.side == Side::Worker);
    let (Some(binding), Some(worker)) = (binding, st.registry.get_side(kind, Side::Worker)) else {
        return refuse(StatusCode::NOT_FOUND, "channel_not_configured");
    };
    let Some(binding_id) = binding.id else {
        return refuse(StatusCode::NOT_FOUND, "channel_not_configured");
    };
    if method != Method::POST {
        return refuse(StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed");
    }
    if body.len() > MAX_WEBHOOK_BYTES {
        return refuse(StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large");
    }
    let worker = st.registry.for_binding(worker, Some(&binding));
    let req = IngressRequest {
        method,
        headers,
        query: None,
        body,
        binding: Some(binding.clone()),
    };
    let ev = match worker.authenticate(&req).await {
        Ok(_) => parse_event(&req.body),
        Err(e) => Err(e),
    };
    let ev = match ev {
        Ok(ev) => ev,
        Err(e) => {
            tracing::info!(channel = kind.as_str(), binding_id = %binding_id, error = %e, "channel_webhook_refused");
            metrics::counter!("agent_edge_webhook_refused_total", "channel" => kind.as_str())
                .increment(1);
            return e.into_response();
        }
    };
    let Some(core) = st.core.clone() else {
        return refuse(StatusCode::SERVICE_UNAVAILABLE, "edge_not_configured");
    };
    match &ev {
        Event::Ignored(why) => {
            tracing::debug!(
                channel = kind.as_str(),
                why = *why,
                "helpdesk_event_ignored"
            );
        }
        Event::Reply { .. } => {
            if let Some(m) = staff_inbound(&binding, &ev)
                && st.dedupe.first_time(kind, &m.external_id)
            {
                metrics::counter!("agent_edge_staff_messages_total", "channel" => kind.as_str())
                    .increment(1);
                let r = match staff_req(worker.as_ref(), &m, &binding, binding_id).await {
                    None => None,
                    Some(req) => match core.staff_checked(&req).await {
                        Err(e) if e.retryable() => {
                            st.dedupe.forget(kind, &m.external_id);
                            return core_unavailable(binding_id, &e);
                        }
                        r => Some(r.map_err(CoreError::from)),
                    },
                };
                tokio::spawn(async move { confirm_staff(worker, &m, r).await });
            }
        }
        Event::Closed {
            ticket,
            status,
            modified_by,
            modified,
            ..
        } => {
            let key = format!("hd-state:{ticket}:{status}:{modified}");
            if st.dedupe.first_time(kind, &key) {
                let req = StaffStateReq {
                    tenant: binding.scope.tenant,
                    job_id: binding.scope.job_id,
                    binding_id,
                    kind: kind.as_str().into(),
                    thread: thread(ticket),
                    staff: StaffIdentity {
                        email: workers::email_like(modified_by),
                        external_id: modified_by.clone(),
                        name: None,
                    },
                    state: "resolved".into(),
                    message_id: key.clone(),
                };
                let r = core.staff_state_checked(&req).await;
                if let Err(e) = &r
                    && e.retryable()
                {
                    st.dedupe.forget(kind, &key);
                    return core_unavailable(binding_id, e);
                }
                let r = r.map(|r| r.status).map_err(CoreError::from);
                tokio::spawn(async move { confirm_state(worker, &req, r).await });
            }
        }
    }
    EdgeReply::ok().into_response()
}

/// 503 for a webhook core could not take (logged by code, never the text).
fn core_unavailable(binding_id: Uuid, e: &super::core::SeamError) -> Response {
    tracing::warn!(channel = "helpdesk", binding_id = %binding_id, error = %e, "channel_core_unavailable");
    metrics::counter!("agent_edge_accept_unavailable_total", "channel" => "helpdesk").increment(1);
    refuse(StatusCode::SERVICE_UNAVAILABLE, "unavailable")
}

/// The comment a refused close leaves on the ticket (`None` = nothing to say).
pub fn state_confirmation(r: &Result<StaffStateStatus, CoreError>) -> Option<&'static str> {
    match r {
        Ok(_) => None,
        Err(CoreError::Code(c)) if c == "thread_not_found" => None,
        Err(CoreError::Code(c)) if c == "not_staff" => Some(NOT_STAFF_CLOSE),
        Err(CoreError::Code(c)) if c == "owner_changed" || c == "not_owner" => {
            Some(OWNED_ELSEWHERE_CLOSE)
        }
        Err(_) => Some(CLOSE_FAILED),
    }
}

/// Fixed comment texts for a close agent-api refused.
pub const NOT_STAFF_CLOSE: &str = "The agent conversation was not closed: your Helpdesk user's email does not match an \
     agent account holding the Support agent role for this job. Close it from the console.";
/// A colleague owns the conversation in the workspace (agent-api 409 `owner_changed`, SW-5).
pub const OWNED_ELSEWHERE_CLOSE: &str = "The agent conversation was not closed: a colleague is answering it in the \
     workspace. Ask them, or close it from the workspace.";
pub const CLOSE_FAILED: &str =
    "The agent conversation was not closed: something went wrong. Close it from the console.";

/// Log agent-api's answer to a close (`staff-state`; codes only) and note a refusal on the ticket.
pub async fn confirm_state(
    worker: Arc<dyn ChannelConnector>,
    req: &StaffStateReq,
    r: Result<StaffStateStatus, CoreError>,
) {
    match &r {
        Ok(s) => {
            tracing::info!(channel = "helpdesk", binding_id = %req.binding_id, status = ?s, "channel_staff_state")
        }
        Err(e) => {
            tracing::warn!(channel = "helpdesk", binding_id = %req.binding_id, error = %e, "channel_staff_state_refused")
        }
    }
    let Some(text) = state_confirmation(&r) else {
        return;
    };
    if let Err(e) = worker
        .deliver(&req.thread, Outbound::Text(text.into()))
        .await
    {
        tracing::warn!(channel = "helpdesk", error = %e, "channel_staff_confirm_failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::core::ExcerptLine;
    use crate::interaction::model::Scope;

    const SECRET: &str = "test-webhook-secret-not-real";

    fn binding() -> Binding {
        let mut cred = BTreeMap::new();
        cred.insert("api_key".into(), "k-test".into());
        cred.insert("api_secret".into(), "s-test".into());
        cred.insert("webhook_secret".into(), SECRET.into());
        Binding {
            id: Some(Uuid::from_u128(7)),
            kind: ChannelKind::Helpdesk,
            side: Side::Worker,
            external_id: "helpdesk.example#support".into(),
            scope: Scope {
                tenant: Uuid::from_u128(1),
                job_id: Uuid::from_u128(2),
                job_key: "support".into(),
            },
            env: "prod".into(),
            settings: json!({"base_url": "http://frappe.example.com:8000/",
                             "service_user": "agent-bot@helpdesk.example"}),
            credential: Some(cred),
        }
    }

    fn item(email: Option<&str>) -> WorkerItem {
        WorkerItem {
            id: "HO-3".into(),
            subject: "Refund <order> 42".into(),
            customer: "Ada".into(),
            channel: "web_chat".into(),
            reason: "customer_asked".into(),
            priority: "normal".into(),
            excerpt: vec![ExcerptLine {
                role: "customer".into(),
                text: "Where is my refund & why?".into(),
            }],
            customer_email: email.map(str::to_string),
        }
    }

    fn req(body: &Value, sig: Option<String>) -> IngressRequest {
        let mut headers = HeaderMap::new();
        if let Some(s) = sig {
            headers.insert(SIGNATURE_HEADER, s.parse().unwrap());
        }
        IngressRequest {
            method: Method::POST,
            headers,
            query: None,
            body: Bytes::from(body.to_string()),
            binding: Some(binding()),
        }
    }

    fn reply_body() -> Value {
        json!({"event": "reply", "ticket": 12, "communication": "c0a1b2", "reference_doctype": "HD Ticket",
               "sent_or_received": "Sent", "sender": "anna@shop.example", "sender_full_name": "Anna",
               "content": "<p>Hello &amp; welcome,</p><p>your refund is <b>on its way</b>.<br>Anna</p>"})
    }

    #[test]
    fn kind_round_trips() {
        assert_eq!(ChannelKind::parse("helpdesk"), Some(ChannelKind::Helpdesk));
        assert_eq!(ChannelKind::Helpdesk.as_str(), "helpdesk");
    }

    #[test]
    fn signature_ok_bad_and_missing() {
        let body = br#"{"event":"status"}"#;
        let good = sign(SECRET, body);
        assert!(verify_signature(SECRET, body, Some(&good)).is_ok());
        assert!(verify_signature(SECRET, body, Some(&format!(" {good} "))).is_ok());
        let bad =
            || -> Result<(), ChannelError> { Err(ChannelError::Unauthenticated("bad_signature")) };
        assert_eq!(verify_signature("other-secret", body, Some(&good)), bad());
        assert_eq!(
            verify_signature(SECRET, b"{\"event\":\"x\"}", Some(&good)),
            bad()
        );
        assert_eq!(verify_signature(SECRET, body, None), bad());
        assert_eq!(verify_signature(SECRET, body, Some("not base64 !!")), bad());
    }

    #[tokio::test]
    async fn authenticate_uses_the_bindings_webhook_secret() {
        let h = Helpdesk::new(Policy::default(), Arc::new(BindingStore::default()));
        let body = reply_body();
        let signed = req(&body, Some(sign(SECRET, body.to_string().as_bytes())));
        assert!(h.authenticate(&signed).await.is_ok());
        let wrong = req(&body, Some(sign("nope", body.to_string().as_bytes())));
        assert_eq!(
            h.authenticate(&wrong).await,
            Err(ChannelError::Unauthenticated("bad_signature"))
        );
        let mut unbound = signed.clone();
        unbound.binding = None;
        assert_eq!(
            h.authenticate(&unbound).await,
            Err(ChannelError::Unauthenticated("binding_required"))
        );
        let mut no_secret = signed;
        no_secret
            .binding
            .as_mut()
            .unwrap()
            .credential
            .as_mut()
            .unwrap()
            .remove("webhook_secret");
        assert_eq!(
            h.authenticate(&no_secret).await,
            Err(ChannelError::Unauthenticated("webhook_secret_missing"))
        );
    }

    #[tokio::test]
    async fn a_sent_reply_is_one_staff_message_on_the_ticket_thread() {
        let h = Helpdesk::new(Policy::default(), Arc::new(BindingStore::default()));
        let r = req(&reply_body(), None);
        let got = h.ingest(&r, &IngressAuth::default()).await.unwrap();
        let Ingress::Staff { ack, staff } = got else {
            panic!("not staff: {got:?}");
        };
        assert_eq!(ack, EdgeReply::ok());
        assert_eq!(staff.len(), 1);
        let m = &staff[0];
        assert_eq!(m.external_id, "hd:c0a1b2");
        assert_eq!(m.target, "helpdesk.example#support");
        assert_eq!(
            (m.thread.channel.as_str(), m.thread.id.as_str()),
            ("helpdesk", "12")
        );
        assert_eq!(m.reply_to, m.thread);
        assert_eq!(m.staff_email.as_deref(), Some("anna@shop.example"));
        assert_eq!(m.staff_name.as_deref(), Some("Anna"));
        assert_eq!(
            m.text,
            "Hello & welcome,\n\nyour refund is on its way.\nAnna"
        );
        assert!(m.in_thread);
    }

    #[test]
    fn received_mail_other_doctypes_and_open_statuses_are_ignored() {
        let mut b = reply_body();
        b["sent_or_received"] = json!("Received");
        assert_eq!(
            parse_event(b.to_string().as_bytes()),
            Ok(Event::Ignored("received"))
        );
        let mut b = reply_body();
        b["reference_doctype"] = json!("Issue");
        assert_eq!(
            parse_event(b.to_string().as_bytes()),
            Ok(Event::Ignored("doctype"))
        );
        let mut b = reply_body();
        b["content"] = json!("<p>&nbsp;</p>");
        assert_eq!(
            parse_event(b.to_string().as_bytes()),
            Ok(Event::Ignored("empty"))
        );
        let open = json!({"event": "status", "ticket": "12", "status": "Replied", "modified_by": "a@b.example"});
        assert_eq!(
            parse_event(open.to_string().as_bytes()),
            Ok(Event::Ignored("status"))
        );
        assert_eq!(
            parse_event(b"not json"),
            Err(ChannelError::Invalid("invalid_body"))
        );
        assert_eq!(
            parse_event(b"{}"),
            Err(ChannelError::Invalid("event_missing"))
        );
        let no_sender =
            json!({"event": "reply", "ticket": 1, "communication": "c", "content": "x"});
        assert_eq!(
            parse_event(no_sender.to_string().as_bytes()),
            Err(ChannelError::Invalid("sender_missing"))
        );
    }

    #[test]
    fn resolved_and_closed_are_close_events() {
        for status in CLOSING_STATUSES {
            let b = json!({"event": "status", "ticket": 12, "status": status, "modified_by": "anna@shop.example",
                           "modified": "2026-10-21 10:00:00.123456", "agent_key": "HO-3"});
            assert_eq!(
                parse_event(b.to_string().as_bytes()),
                Ok(Event::Closed {
                    ticket: "12".into(),
                    status: status.into(),
                    modified_by: "anna@shop.example".into(),
                    modified: "2026-10-21 10:00:00.123456".into(),
                    agent_key: Some("HO-3".into()),
                })
            );
        }
    }

    #[test]
    fn html_is_stripped_to_text() {
        assert_eq!(strip_html("plain text"), "plain text");
        assert_eq!(strip_html("<div>a</div><div>b</div>"), "a\n\nb");
        assert_eq!(
            strip_html("<p>one<br/>two</p>\n\n<p>three</p>"),
            "one\ntwo\n\nthree"
        );
        assert_eq!(
            strip_html(
                "<style>p{x:1}</style><script>alert(1)</script><!-- c > d -->Hi&nbsp;&lt;you&gt; &#233;&#x41;"
            ),
            "Hi <you> éA"
        );
        assert_eq!(strip_html("a &unknown; & b"), "a &unknown; & b");
        assert_eq!(
            strip_html("<p>  spaced \n  out  </p><ul><li>x</li><li>y</li></ul>"),
            "spaced out\n\nx\n\ny"
        );
        assert_eq!(strip_html("<b>unclosed"), "unclosed");
        assert_eq!(strip_html("broken <tag"), "broken");
        assert_eq!(strip_html("Ünïcödé <i>ok</i>"), "Ünïcödé ok");
    }

    #[test]
    fn ticket_body_names_the_item_and_the_requester() {
        let b = ticket_body(&item(Some("ada@customer.example")), "bot@helpdesk.example");
        assert_eq!(b["raised_by"], "ada@customer.example");
        assert_eq!(b["custom_agent_key"], "HO-3");
        assert_eq!(b["subject"], "Handover HO-3: Refund <order> 42");
        let d = b["description"].as_str().unwrap();
        assert!(d.contains("HO-3") && d.contains("Ada"), "{d}");
        assert!(d.contains("Where is my refund &amp; why?"), "{d}");
        assert!(!d.contains("<order>"), "{d}");
        // no customer email (or not an email): the binding's service user
        for e in [None, Some("not-an-email")] {
            let b = ticket_body(&item(e), "bot@helpdesk.example");
            assert_eq!(b["raised_by"], "bot@helpdesk.example");
        }
    }

    #[test]
    fn ticket_names_are_strings_or_numbers() {
        assert_eq!(
            ticket_name(&json!({"data": {"name": 12}})).as_deref(),
            Some("12")
        );
        assert_eq!(
            ticket_name(&json!({"data": {"name": "HD-0012"}})).as_deref(),
            Some("HD-0012")
        );
        assert_eq!(ticket_name(&json!({"data": {}})), None);
        assert_eq!(ticket_name(&json!({"message": "x"})), None);
    }

    #[test]
    fn the_target_needs_settings_and_credential_and_hides_secrets() {
        let b = binding();
        let t = HelpdeskTarget::of(&b.settings, b.credential.as_ref()).unwrap();
        assert_eq!(t.base_url, "http://frappe.example.com:8000");
        assert_eq!(t.authorization(), "token k-test:s-test");
        assert!(!format!("{t:?}").contains("s-test"));
        assert_eq!(
            HelpdeskTarget::of(&json!({}), b.credential.as_ref()),
            Err(ChannelError::Invalid("base_url_missing"))
        );
        assert_eq!(
            HelpdeskTarget::of(&b.settings, None),
            Err(ChannelError::Invalid("api_key_missing"))
        );
        assert_eq!(
            HelpdeskTarget::of(
                &json!({"base_url": "ftp://x", "service_user": "a@b.example"}),
                None
            ),
            Err(ChannelError::Invalid("base_url_invalid"))
        );
    }

    #[test]
    fn refused_closes_leave_a_note() {
        assert_eq!(state_confirmation(&Ok(StaffStateStatus::Resolved)), None);
        assert_eq!(
            state_confirmation(&Err(CoreError::Code("thread_not_found".into()))),
            None
        );
        assert_eq!(
            state_confirmation(&Err(CoreError::Code("not_staff".into()))),
            Some(NOT_STAFF_CLOSE)
        );
        assert_eq!(
            state_confirmation(&Err(CoreError::Code("owner_changed".into()))),
            Some(OWNED_ELSEWHERE_CLOSE)
        );
        assert_eq!(
            state_confirmation(&Err(CoreError::Unavailable("timeout".into()))),
            Some(CLOSE_FAILED)
        );
    }
}
