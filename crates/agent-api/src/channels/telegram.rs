//! Telegram customer-side connector (B7 T15b, owner rulings R6/R7). A Telegram channel is a B12 job channel binding
//! of kind `telegram` with the tenant's own bot (BYO; there is no platform bot): the binding's credential carries
//! `bot_token` and `webhook_secret`, its settings `allowed_user_ids` (and optionally `test_chat_id` /
//! `test_thread_id`). The edge stays DB-less: the binding comes from core's binding list, and every message goes
//! through core's internal seam (`POST /api/internal/channel/inbound`), which stores it and dedupes on the message id.
//!
//! - Ingress: `POST /api/channels/telegram/webhook/{binding_id}` only (the BYO path; the bare kind path has no bot
//!   behind it and is 401). [`ChannelConnector::authenticate`] compares the `X-Telegram-Bot-Api-Secret-Token` header
//!   with the binding's `webhook_secret` in constant time (`ring::hmac::verify` over a fixed label), 401
//!   `bad_secret_token` on a mismatch.
//! - Allow-list (D75): an update whose `from.id` is not in `settings.allowed_user_ids` is answered 200 and dropped
//!   before anything reaches core (log line + `agent_edge_updates_dropped_total{reason="not_allowed"}`, no reply).
//!   An empty or absent list allows nobody.
//! - Dedupe: the Inbound's `external_id` is the update's `update_id` (unique per bot); the edge's in-process dedupe
//!   and core's per-conversation `message_id` dedupe both key on it, so a Telegram retry is answered once.
//! - Persist-before-200 (R6, [`ChannelConnector::persist_before_ack`]): the edge awaits core's `inbound` with
//!   `accept_only` (store + dedupe, no LLM) before it answers Telegram 200; core down → 503 and Telegram resends the
//!   update. The answer runs after the 200 (a second `inbound` call, which answers the stored message).
//! - Chats: private chats, groups and supergroups (forum topics too); channel posts are ignored. A private chat and a
//!   group's General topic are keyed by `chat.id`; a forum topic message (`is_topic_message` + `message_thread_id`)
//!   by `{chat.id}:{message_thread_id}`, with the thread id in the route so the answer lands in that topic. The
//!   allow-list applies to `from.id` in every chat type.
//! - Outbound: `sendMessage {chat_id, [message_thread_id], text, parse_mode: "HTML"}`: the agent's markdown rendered
//!   to Telegram HTML and split into well-formed parts of at most [`MAX_TEXT_CHARS`] ([`super::telegram_html`]), with
//!   the binding's own bot token; `Typing` is `sendChatAction`. The token is part of the Bot API URL, so no transport
//!   error is ever formatted.
//!
//! - CSAT (SW-6): a rating request (`Outbound::CsatRequest`) is sent with an inline keyboard of five buttons
//!   ([`rating_keyboard`], callback data `csat:1` … `csat:5`). A press is a `callback_query` read as the customer typing
//!   that digit (same allow-list, same chat or topic, `external_id` the update id); the edge answers the query and
//!   removes the keyboard from that message, so a second press cannot rate again.
//!
//! - Files (SW-9): an inbound `photo` (the largest size of at most 10 MB) or `document` (at most 10 MB) becomes
//!   [`Ingress::Media`]: the edge downloads it through [`TelegramConnector::download_media`] (`getFile`, then
//!   `{api_base}/file/bot<token>/<file_path>` on the same host, no redirects, capped at 10 MB while streaming) and core
//!   validates it by its bytes, never by Telegram's mime type. The caption is the message text; media without a caption
//!   is "(photo)" / "(file)". Outbound, [`Outbound::Attachment`] is `sendPhoto` (images) / `sendDocument` (PDF), multipart,
//!   never an URL; the reply text is the caption of the file when it fits in 1024 characters, else its own message first.
//!
//! Only `message` updates with text or media and rating-button presses are read; edits, channel posts, other callback
//! queries, other bots and other media (stickers, voice, video) are acknowledged and ignored. Registering the webhook is an operator step (docs/runbooks/telegram-test-bot.md): this
//! crate never calls the Bot API's update-registration methods (the signup crate-wide scan forbids their names).
//! Webhook health (B7 T15b) reads the registration through [`TelegramConnector::webhook_info`], read-only.
//!
//! Message text is customer data: never in a log line, a metric label or an error. The bot token is part of every Bot
//! API and file URL, so no transport error is ever formatted and no URL is ever logged.
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::http::Method;
use futures_util::future::BoxFuture;
use ring::hmac;
use serde_json::{Value, json};

use super::bindings::Binding;
use super::connector::{
    ChannelConnector, ChannelError, ChannelKind, ChannelStatus, ConversationRef, DeliveryReceipt,
    EdgeReply, ExternalIdentity, Inbound, InboundMedia, Ingress, IngressAuth, IngressRequest,
    MediaFile, MediaKind, Outbound, Side, outbound_text,
};
use crate::attachments::validate::MAX_BYTES as MAX_MEDIA_BYTES;

/// The Bot API base when `TELEGRAM_CHANNEL_API_BASE` is unset.
pub const DEFAULT_API_BASE: &str = "https://api.telegram.org";
/// The header Telegram sends with the `secret_token` the webhook was registered with.
pub const SECRET_HEADER: &str = "x-telegram-bot-api-secret-token";
/// Telegram's text limit is 4096 characters after entity parsing; stay below it.
pub const MAX_TEXT_CHARS: usize = 4000;
/// The bound on one Bot API call.
pub const API_TIMEOUT: Duration = Duration::from_secs(10);
/// The most user ids one binding's allow-list holds.
pub const MAX_ALLOWED_USERS: usize = 50;
/// A caption's limit in characters after entity parsing (Telegram); longer text goes as its own message first.
pub const MAX_CAPTION_CHARS: usize = 1024;
/// The bound on one file upload (`sendPhoto` / `sendDocument`, at most 10 MB).
pub const UPLOAD_TIMEOUT: Duration = Duration::from_secs(60);
/// The bound on one file download.
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30);
/// The route key of a forum topic's thread id (Telegram's `message_thread_id`).
pub const THREAD_KEY: &str = "message_thread_id";
/// The fixed label the webhook secret is MACed under, so a candidate is compared in constant time.
const SECRET_LABEL: &[u8] = b"agent-edge:telegram:webhook-secret:v1";

/// One parsed Telegram update: the message, the rating button it pressed (if any) and its media (if any).
type ParsedUpdate = (Inbound, Option<Pressed>, Option<InboundMedia>);

/// `TELEGRAM_CHANNEL_API_BASE` (tests point it at a mock). Not the signup bot's `TELEGRAM_API_BASE`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TelegramConfig {
    pub api_base: String,
}

impl Default for TelegramConfig {
    fn default() -> Self {
        Self {
            api_base: DEFAULT_API_BASE.into(),
        }
    }
}

/// The connector's config: always on (Telegram bindings come from core only); a malformed
/// `TELEGRAM_CHANNEL_API_BASE` fails the boot by name.
pub fn config_from(trimmed: &impl Fn(&str) -> Option<String>) -> Result<TelegramConfig> {
    let api_base = match trimmed("TELEGRAM_CHANNEL_API_BASE") {
        None => DEFAULT_API_BASE.to_string(),
        Some(u) => {
            let p = reqwest::Url::parse(&u)
                .ok()
                .filter(|p| matches!(p.scheme(), "https" | "http") && p.host_str().is_some())
                .context("TELEGRAM_CHANNEL_API_BASE must be an http(s) URL")?;
            if p.query().is_some() || p.fragment().is_some() {
                anyhow::bail!("TELEGRAM_CHANNEL_API_BASE must not carry a query or a fragment");
            }
            u.trim_end_matches('/').to_string()
        }
    };
    Ok(TelegramConfig { api_base })
}

/// The bot id of a Bot API token (`<digits>:<secret part>`), `None` when the token is not token-shaped. The binding's
/// `external_id` is this id: the token itself proves ownership of the bot.
pub fn bot_id_of(token: &str) -> Option<&str> {
    let (id, rest) = token.trim().split_once(':')?;
    let id_ok = (1..=20).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit());
    let rest_ok = (20..=200).contains(&rest.len())
        && rest
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    (id_ok && rest_ok).then_some(id)
}

/// A `secret_token` Telegram accepts: 1 to 256 of `A-Z a-z 0-9 _ -`. The edge requires at least 16.
pub fn valid_webhook_secret(s: &str) -> bool {
    (16..=256).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The binding's `bot_token` and `webhook_secret` (trimmed) when its credential names its own bot: a token-shaped
/// `bot_token` whose bot id is the binding's `external_id`, and a valid `webhook_secret`. `None` otherwise.
fn bot_credential(binding: &Binding) -> Option<(String, String)> {
    let cred = binding.credential.as_ref()?;
    let token = cred.get("bot_token")?.trim().to_string();
    let secret = cred.get("webhook_secret")?.trim().to_string();
    if bot_id_of(&token)? != binding.external_id || !valid_webhook_secret(&secret) {
        return None;
    }
    Some((token, secret))
}

/// Whether [`TelegramConnector::with_binding`] builds a bot for `binding` (its credential is complete), i.e. whether
/// crate::channels::edge::Registry::for_binding resolves it to its own bot rather than falling back to the
/// registered connector, which has none. Checks the credential without building anything or logging.
pub fn binding_has_bot(binding: &Binding) -> bool {
    binding.kind == ChannelKind::Telegram && bot_credential(binding).is_some()
}

/// A binding's `settings.allowed_user_ids`, read leniently (integers, or digit strings); anything else is skipped.
pub fn allowed_user_ids(settings: &Value) -> BTreeSet<i64> {
    settings
        .get("allowed_user_ids")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_i64().or_else(|| v.as_str()?.trim().parse().ok()))
                .filter(|id| *id > 0)
                .take(MAX_ALLOWED_USERS)
                .collect()
        })
        .unwrap_or_default()
}

/// One bot's credential and allow-list (a binding's). `Debug` shows no secret.
#[derive(Clone)]
struct Bot {
    /// The binding's `external_id` (the bot id).
    bot_id: String,
    token: String,
    secret_key: hmac::Key,
    secret_tag: Vec<u8>,
    allowed: BTreeSet<i64>,
}

impl fmt::Debug for Bot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Bot")
            .field("bot_id", &self.bot_id)
            .field("allowed", &self.allowed.len())
            .finish_non_exhaustive()
    }
}

/// The Telegram connector. The registered one has no bot (it refuses every call); [`ChannelConnector::for_binding`]
/// builds one per binding with that binding's bot.
pub struct TelegramConnector {
    cfg: TelegramConfig,
    bot: Option<Bot>,
    http: reqwest::Client,
}

impl TelegramConnector {
    pub fn new(cfg: TelegramConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(API_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("telegram: building the Bot API client")?;
        Ok(Self {
            cfg,
            bot: None,
            http,
        })
    }

    /// This connector with one binding's bot: `None` unless the credential has a token-shaped `bot_token` whose bot
    /// id is the binding's `external_id`, and a valid `webhook_secret`.
    pub fn with_binding(&self, binding: &Binding) -> Option<Self> {
        let (token, secret) = bot_credential(binding)?;
        let secret_key = hmac::Key::new(hmac::HMAC_SHA256, SECRET_LABEL);
        let secret_tag = hmac::sign(&secret_key, secret.as_bytes()).as_ref().to_vec();
        Some(Self {
            cfg: self.cfg.clone(),
            bot: Some(Bot {
                bot_id: binding.external_id.clone(),
                token,
                secret_key,
                secret_tag,
                allowed: allowed_user_ids(&binding.settings),
            }),
            http: self.http.clone(),
        })
    }

    fn bot(&self) -> Result<&Bot, ChannelError> {
        self.bot
            .as_ref()
            .ok_or(ChannelError::Unauthenticated("binding_not_configured"))
    }

    /// `POST` with the binding's secret header (constant time).
    pub fn check(&self, req: &IngressRequest) -> Result<(), ChannelError> {
        if req.method != Method::POST {
            return Err(ChannelError::Invalid("method_not_allowed"));
        }
        let bot = self.bot()?;
        let got = req
            .header(SECRET_HEADER)
            .ok_or(ChannelError::Unauthenticated("bad_secret_token"))?;
        hmac::verify(&bot.secret_key, got.as_bytes(), &bot.secret_tag)
            .map_err(|_| ChannelError::Unauthenticated("bad_secret_token"))
    }

    /// One (authenticated) update: `None` when it carries nothing for the seam (not a text or media message nor a
    /// rating button, a bot, a sender outside the allow-list). A media message comes back as its [`Inbound`] (text =
    /// caption or "(photo)" / "(file)"); [`TelegramConnector::parse_media`] also returns the media.
    pub fn parse(&self, body: &[u8]) -> Result<Option<Inbound>, ChannelError> {
        Ok(self.parse_update(body)?.map(|(m, _, _)| m))
    }

    /// [`TelegramConnector::parse`] with the photo or document the message carries, if any.
    pub fn parse_media(
        &self,
        body: &[u8],
    ) -> Result<Option<(Inbound, Option<InboundMedia>)>, ChannelError> {
        Ok(self.parse_update(body)?.map(|(m, _, media)| (m, media)))
    }

    /// [`Self::parse_media`], and the rating-button press it came from (to answer and to clear its keyboard).
    fn parse_update(&self, body: &[u8]) -> Result<Option<ParsedUpdate>, ChannelError> {
        let bot = self.bot()?;
        let v: Value =
            serde_json::from_slice(body).map_err(|_| ChannelError::Invalid("bad_payload"))?;
        let update_id = v
            .get("update_id")
            .and_then(Value::as_i64)
            .ok_or(ChannelError::Invalid("bad_payload"))?;
        // a CSAT rating button is the customer typing its digit, in the chat of the message that carried it; no other
        // callback query is read
        let (m, from, text, pressed) = if let Some(q) = v.get("callback_query") {
            let (Some(rating), Some(m), Some(from), Some(query_id)) = (
                q.get("data").and_then(Value::as_str).and_then(rating_of),
                q.get("message"),
                q.get("from"),
                q.get("id").and_then(Value::as_str),
            ) else {
                return Ok(None);
            };
            let pressed = Pressed {
                query_id: query_id.to_string(),
                chat_id: None,
                message_id: m.get("message_id").and_then(Value::as_i64),
            };
            (m, from, rating.to_string(), Some(pressed))
        } else {
            let Some(m) = v.get("message") else {
                return Ok(None);
            };
            let text = m
                .get("text")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| media_of(m).map(|md| md.caption));
            let (Some(from), Some(text)) = (m.get("from"), text) else {
                // service messages (forum_topic_created, new_chat_members, …) carry no text or media
                return Ok(None);
            };
            (m, from, text, None)
        };
        let media = media_of(m);
        let Some(chat_id) = m.pointer("/chat/id").and_then(Value::as_i64) else {
            return Ok(None);
        };
        let chat_type = m.pointer("/chat/type").and_then(Value::as_str);
        let in_group = match chat_type {
            Some("private") => false,
            Some("group" | "supergroup") => true,
            // channel posts, and anything Telegram adds later
            _ => return Ok(None),
        };
        if from.get("is_bot").and_then(Value::as_bool) == Some(true)
            || (text.trim().is_empty() && media.is_none())
        {
            return Ok(None);
        }
        let Some(user_id) = from.get("id").and_then(Value::as_i64) else {
            return Ok(None);
        };
        if !bot.allowed.contains(&user_id) {
            tracing::info!(channel = "telegram", bot_id = %bot.bot_id, "channel_update_not_allowed");
            metrics::counter!("agent_edge_updates_dropped_total", "channel" => "telegram", "reason" => "not_allowed")
                .increment(1);
            return Ok(None);
        }
        let name = [
            from.get("first_name").and_then(Value::as_str),
            from.get("last_name").and_then(Value::as_str),
        ]
        .into_iter()
        .flatten()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
        let display_name = Some(name).filter(|n| !n.is_empty()).or_else(|| {
            from.get("username")
                .and_then(Value::as_str)
                .filter(|u| !u.trim().is_empty())
                .map(str::to_string)
        });
        let chat = chat_id.to_string();
        // A forum topic is its own conversation (`{chat}:{thread}`), answered in that topic; the General topic and a
        // plain group are the chat; a private chat keeps its bare chat id.
        let thread = m
            .get("message_thread_id")
            .and_then(Value::as_i64)
            .filter(|t| {
                in_group
                    && *t > 0
                    && m.get("is_topic_message").and_then(Value::as_bool) == Some(true)
            });
        let mut route = BTreeMap::from([("chat_id".to_string(), chat.clone())]);
        let id = match thread {
            Some(t) => {
                route.insert(THREAD_KEY.to_string(), t.to_string());
                format!("{chat}:{t}")
            }
            None => chat,
        };
        let pressed = pressed.map(|p| Pressed {
            chat_id: Some(chat_id.to_string()),
            ..p
        });
        let text = match &media {
            Some(md) if text.trim().is_empty() => md.kind.placeholder().to_string(),
            _ => text,
        };
        let inbound = Inbound {
            external_id: update_id.to_string(),
            workspace: Some(bot.bot_id.clone()),
            text,
            identity: ExternalIdentity {
                channel: ChannelKind::Telegram,
                subject: user_id.to_string(),
                display_name,
                // Telegram vouches for `from.id` on a webhook that carried the bot's secret.
                verified: true,
            },
            scope: None,
            conversation: ConversationRef {
                channel: ChannelKind::Telegram.as_str().to_string(),
                id,
                route,
            },
        };
        Ok(Some((inbound, pressed, media)))
    }

    /// Answer a rating-button press and clear the keyboard of the message it was on. Failures are logged by code
    /// only: the rating itself goes through either way.
    async fn acknowledge(&self, p: &Pressed) {
        if let Err(e) = self
            .call(
                "answerCallbackQuery",
                &json!({"callback_query_id": p.query_id}),
            )
            .await
        {
            tracing::warn!(channel = "telegram", error = %e, "telegram_callback_answer_failed");
        }
        if let (Some(chat), Some(message_id)) = (&p.chat_id, p.message_id) {
            let body = json!({"chat_id": chat_value(chat), "message_id": message_id,
                              "reply_markup": {"inline_keyboard": []}});
            if let Err(e) = self.call("editMessageReplyMarkup", &body).await {
                tracing::warn!(channel = "telegram", error = %e, "telegram_keyboard_clear_failed");
            }
        }
    }

    /// The `sendMessage` body of one part already rendered to Telegram HTML ([`super::telegram_html::html_parts`]);
    /// `message_thread_id` when the reply goes to a forum topic.
    pub fn send_message_body(chat_id: &str, thread_id: Option<i64>, html: &str) -> Value {
        let mut b = json!({"chat_id": chat_value(chat_id), "text": html, "parse_mode": "HTML"});
        if let Some(t) = thread_id {
            b["message_thread_id"] = Value::from(t);
        }
        b
    }

    /// One Bot API call; the `result` on success. Never formats a transport error (the URL carries the token).
    async fn call(&self, method: &str, payload: &Value) -> Result<Value, ChannelError> {
        let bot = self.bot()?;
        let url = format!("{}/bot{}/{method}", self.cfg.api_base, bot.token);
        let resp = self
            .http
            .post(url)
            .json(payload)
            .send()
            .await
            .map_err(|_| ChannelError::Upstream("telegram_unreachable".into()))?;
        Self::finish(resp).await
    }

    /// A Bot API answer as the `result`, or the error mapped to a code (429 with its `retry_after`; 403 bot blocked and
    /// the rest as `telegram_<status>_<error_code>`).
    async fn finish(resp: reqwest::Response) -> Result<Value, ChannelError> {
        let status = resp.status();
        let v: Value = resp.json().await.unwrap_or(Value::Null);
        if status.as_u16() == 429 {
            let retry_after_s = v
                .pointer("/parameters/retry_after")
                .and_then(Value::as_u64)
                .unwrap_or(1);
            return Err(ChannelError::RateLimited { retry_after_s });
        }
        if !status.is_success() || v.get("ok").and_then(Value::as_bool) != Some(true) {
            // The status and the numeric error code only, never the description.
            let code = v.get("error_code").and_then(Value::as_i64);
            return Err(ChannelError::Upstream(match code {
                Some(c) => format!("telegram_{}_{c}", status.as_u16()),
                None => format!("telegram_{}", status.as_u16()),
            }));
        }
        Ok(v.get("result").cloned().unwrap_or(Value::Null))
    }

    /// The bot's webhook registration as Telegram sees it (B7 T15b, owner ruling 2026-10-23): `getWebhookInfo`, the
    /// one Bot API webhook method this crate calls. It is read-only and claims nothing, so C1 (the crate never claims
    /// the bot's updates) holds; the claiming methods stay forbidden by the signup crate-wide scan, which exempts this
    /// file (and only this file) for this one name. The description Telegram returns is reduced to a code here.
    pub async fn webhook_info(&self) -> Result<WebhookInfo, ChannelError> {
        let r = self.call("getWebhookInfo", &json!({})).await?;
        parse_webhook_info(&r)
    }

    async fn send(
        &self,
        conv: &ConversationRef,
        msg: Outbound,
    ) -> Result<DeliveryReceipt, ChannelError> {
        let (chat_id, thread_id) =
            route_target(conv).ok_or(ChannelError::Invalid("missing_route"))?;
        if msg == Outbound::Typing {
            let mut b = json!({"chat_id": chat_value(&chat_id), "action": "typing"});
            if let Some(t) = thread_id {
                b["message_thread_id"] = Value::from(t);
            }
            self.call("sendChatAction", &b).await?;
            return Ok(DeliveryReceipt::default());
        }
        if let Outbound::Attachment {
            media_type,
            display_name,
            bytes,
            caption,
        } = msg
        {
            return self
                .send_attachment(
                    &chat_id,
                    thread_id,
                    &media_type,
                    &display_name,
                    bytes.0,
                    caption.as_deref(),
                )
                .await;
        }
        let rating = matches!(msg, Outbound::CsatRequest { .. });
        let Some(text) = outbound_text(&msg) else {
            return Ok(DeliveryReceipt::default());
        };
        let mut receipt = DeliveryReceipt::default();
        let parts = super::telegram_html::html_parts(&text, MAX_TEXT_CHARS);
        let last = parts.len().saturating_sub(1);
        for (i, part) in parts.iter().enumerate() {
            let mut body = Self::send_message_body(&chat_id, thread_id, part);
            // a rating request carries the 1–5 buttons on its (last) part
            if rating && i == last {
                body["reply_markup"] = rating_keyboard();
            }
            let r = self.call("sendMessage", &body).await?;
            if receipt.external_id.is_none() {
                receipt.external_id = r
                    .get("message_id")
                    .and_then(Value::as_i64)
                    .map(|i| i.to_string());
            }
        }
        Ok(receipt)
    }

    /// `sendPhoto` (images, field `photo`) / `sendDocument` (PDF, field `document`, filename = `name`) as multipart, the
    /// bytes themselves, never an URL. `caption` (markdown) is the file's caption when it renders to at most
    /// [`MAX_CAPTION_CHARS`]; otherwise it goes first as its own message(s) and the file has none.
    async fn send_attachment(
        &self,
        chat_id: &str,
        thread_id: Option<i64>,
        media_type: &str,
        name: &str,
        bytes: Vec<u8>,
        caption: Option<&str>,
    ) -> Result<DeliveryReceipt, ChannelError> {
        let (method, field) = match media_type {
            "image/jpeg" | "image/png" | "image/webp" => ("sendPhoto", "photo"),
            "application/pdf" => ("sendDocument", "document"),
            _ => return Err(ChannelError::Invalid("unsupported_attachment")),
        };
        let bot = self.bot()?;
        let mut receipt = DeliveryReceipt::default();
        let mut caption_html = None;
        if let Some(text) = caption.filter(|t| !t.trim().is_empty()) {
            let mut parts = super::telegram_html::html_parts(text, MAX_CAPTION_CHARS);
            if parts.len() == 1 {
                caption_html = parts.pop();
            } else {
                for part in super::telegram_html::html_parts(text, MAX_TEXT_CHARS) {
                    let r = self
                        .call(
                            "sendMessage",
                            &Self::send_message_body(chat_id, thread_id, &part),
                        )
                        .await?;
                    if receipt.external_id.is_none() {
                        receipt.external_id = message_id_of(&r);
                    }
                }
            }
        }
        let file = reqwest::multipart::Part::bytes(bytes)
            .file_name(name.to_string())
            .mime_str(media_type)
            .map_err(|_| ChannelError::Invalid("unsupported_attachment"))?;
        let mut form = reqwest::multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .part(field, file);
        if let Some(t) = thread_id {
            form = form.text("message_thread_id", t.to_string());
        }
        if let Some(c) = caption_html {
            form = form.text("caption", c).text("parse_mode", "HTML");
        }
        let url = format!("{}/bot{}/{method}", self.cfg.api_base, bot.token);
        let resp = self
            .http
            .post(url)
            .timeout(UPLOAD_TIMEOUT)
            .multipart(form)
            .send()
            .await
            .map_err(|_| ChannelError::Upstream("telegram_unreachable".into()))?;
        let r = Self::finish(resp).await?;
        if receipt.external_id.is_none() {
            receipt.external_id = message_id_of(&r);
        }
        Ok(receipt)
    }

    /// `getFile`, then the file from `{api_base}/file/bot<token>/<file_path>`: the same host as the Bot API, redirects
    /// refused, `file_path` checked before it is put in an URL, the body cut off at 10 MB while it streams. Every error
    /// is a reason code; the URL (it carries the token) is never formatted.
    pub async fn download(&self, file_id: &str) -> Result<Vec<u8>, &'static str> {
        let bot = self.bot().map_err(|_| "download_failed")?;
        let r = self
            .call("getFile", &json!({"file_id": file_id}))
            .await
            .map_err(|_| "download_failed")?;
        if r.get("file_size")
            .and_then(Value::as_u64)
            .is_some_and(|n| n > MAX_MEDIA_BYTES as u64)
        {
            return Err("too_large");
        }
        let path = r
            .get("file_path")
            .and_then(Value::as_str)
            .filter(|p| valid_file_path(p))
            .ok_or("download_failed")?;
        let url = format!("{}/file/bot{}/{path}", self.cfg.api_base, bot.token);
        let mut resp = self
            .http
            .get(url)
            .timeout(DOWNLOAD_TIMEOUT)
            .send()
            .await
            .map_err(|_| "download_failed")?;
        if !resp.status().is_success() {
            return Err("download_failed");
        }
        if resp
            .content_length()
            .is_some_and(|n| n > MAX_MEDIA_BYTES as u64)
        {
            return Err("too_large");
        }
        let mut out = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|_| "download_failed")? {
            if out.len() + chunk.len() > MAX_MEDIA_BYTES {
                return Err("too_large");
            }
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }
}

/// What the webhook-info result says about a bot's webhook, reduced to what health needs. The URL stays in memory on
/// the edge (it carries the binding id and may carry more): only whether it is set and whether it points at this
/// binding is reported. Never logged.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WebhookInfo {
    /// The registered webhook URL; `""` when none is registered.
    pub url: String,
    /// Updates Telegram holds that it has not delivered yet.
    pub pending_update_count: u32,
    /// Telegram's last delivery error as a code ([`webhook_error_code`]), never its message text.
    pub last_error_code: Option<String>,
    /// When that error happened (epoch ms; Telegram reports unix seconds).
    pub last_error_at_ms: Option<i64>,
}

/// The webhook-info `result`. An error message without its date cannot be ordered against updates and is dropped; a
/// date without a message is `webhook_error`.
pub fn parse_webhook_info(v: &Value) -> Result<WebhookInfo, ChannelError> {
    let o = v
        .as_object()
        .ok_or_else(|| ChannelError::Upstream("telegram_bad_webhook_info".into()))?;
    let url = o
        .get("url")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    let pending_update_count = o
        .get("pending_update_count")
        .and_then(Value::as_u64)
        .map_or(0, |n| u32::try_from(n).unwrap_or(u32::MAX));
    let last_error_at_ms = o
        .get("last_error_date")
        .and_then(Value::as_i64)
        .filter(|s| *s > 0)
        .and_then(|s| s.checked_mul(1000));
    let last_error_code = last_error_at_ms.map(|_| {
        webhook_error_code(
            o.get("last_error_message")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        )
    });
    Ok(WebhookInfo {
        url,
        pending_update_count,
        last_error_code,
        last_error_at_ms,
    })
}

/// Telegram's webhook delivery error message as a short code (the store keeps codes, never message text):
/// `Wrong response from the webhook: 502 Bad Gateway` → `webhook_http_502`; a timeout → `webhook_timeout`; a refused
/// connection → `webhook_refused`; an SSL / certificate problem → `webhook_tls`; anything else → `webhook_error`.
pub fn webhook_error_code(message: &str) -> String {
    let m = message.trim().to_ascii_lowercase();
    if let Some(rest) = m.strip_prefix("wrong response from the webhook:") {
        let digits: String = rest
            .trim_start()
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if digits.len() == 3 {
            return format!("webhook_http_{digits}");
        }
    }
    let code = if m.contains("timed out") || m.contains("timeout") {
        "webhook_timeout"
    } else if m.contains("connection refused") {
        "webhook_refused"
    } else if m.contains("ssl") || m.contains("certificate") {
        "webhook_tls"
    } else {
        "webhook_error"
    };
    code.to_string()
}

/// The path this edge serves a binding's webhook on (the BYO route; docs/runbooks/telegram-test-bot.md).
pub fn webhook_path(binding_id: uuid::Uuid) -> String {
    format!("/api/channels/telegram/webhook/{binding_id}")
}

/// Whether a registered webhook URL points at this edge's route for `binding_id`. Only the path is compared: the edge
/// does not know the public origin Telegram reaches it on (a proxy may also prefix the path), but a URL on another
/// path (another binding's, the bare kind path, which is 401) never reaches this binding. Query and fragment are
/// ignored; the id compares case-insensitively.
pub fn url_points_at(url: &str, binding_id: uuid::Uuid) -> bool {
    reqwest::Url::parse(url).is_ok_and(|u| {
        u.path()
            .to_ascii_lowercase()
            .ends_with(&webhook_path(binding_id))
    })
}

/// A rating-button press: the callback query to answer, and the message whose keyboard to clear.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Pressed {
    query_id: String,
    chat_id: Option<String>,
    message_id: Option<i64>,
}

/// The callback data prefix of a CSAT rating button.
pub const RATING_PREFIX: &str = "csat:";

/// The inline keyboard of a rating request: one row, 1 to 5.
pub fn rating_keyboard() -> Value {
    let row: Vec<Value> = (1..=5)
        .map(|n| json!({"text": n.to_string(), "callback_data": format!("{RATING_PREFIX}{n}")}))
        .collect();
    json!({"inline_keyboard": [row]})
}

/// The rating a button's callback data carries (`csat:1` … `csat:5`).
fn rating_of(data: &str) -> Option<u8> {
    data.strip_prefix(RATING_PREFIX)
        .and_then(|n| n.parse::<u8>().ok())
        .filter(|n| (1..=5).contains(n))
}

/// Where a reply goes: the route's `chat_id` (else the conversation id, `{chat}` or `{chat}:{thread}`) and its forum
/// topic (`message_thread_id`), if any. `None` when the chat id or the thread id is malformed.
fn route_target(conv: &ConversationRef) -> Option<(String, Option<i64>)> {
    let (id_chat, id_thread) = match conv.id.split_once(':') {
        Some((c, t)) => (c, Some(t)),
        None => (conv.id.as_str(), None),
    };
    let (chat, thread) = match conv.route.get("chat_id") {
        Some(c) => (c.as_str(), conv.route.get(THREAD_KEY).map(String::as_str)),
        None => (id_chat, id_thread),
    };
    if !valid_chat_id(chat) {
        return None;
    }
    let thread = match thread {
        None => None,
        Some(t) => Some(t.parse::<i64>().ok().filter(|t| *t > 0)?),
    };
    Some((chat.to_string(), thread))
}

/// The `message_id` of a sent message, as the receipt's external id.
fn message_id_of(result: &Value) -> Option<String> {
    result
        .get("message_id")
        .and_then(Value::as_i64)
        .map(|i| i.to_string())
}

/// A `file_path` Telegram returned, safe to put in an URL: relative, `[A-Za-z0-9_./-]`, no `..`, no empty segment.
fn valid_file_path(p: &str) -> bool {
    !p.is_empty()
        && p.len() <= 256
        && !p.starts_with('/')
        && p.split('/').all(|seg| !seg.is_empty() && seg != "..")
        && p.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'/' | b'-'))
}

/// The photo or document of a `message`, or `None` (text only, or other media: stickers, voice, video). A photo is
/// the largest size (by pixels) of at most 10 MB; a document is judged by its size only: its type is decided later,
/// by the file's bytes, never by Telegram's mime type. Over 10 MB is `skip: too_large` (nothing is downloaded).
fn media_of(m: &Value) -> Option<InboundMedia> {
    let caption = m
        .get("caption")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let cap = MAX_MEDIA_BYTES as u64;
    if let Some(sizes) = m.get("photo").and_then(Value::as_array) {
        let ok = |s: &&Value| {
            s.get("file_size")
                .and_then(Value::as_u64)
                .is_none_or(|n| n <= cap)
                && s.get("file_id").and_then(Value::as_str).is_some()
        };
        let best = sizes.iter().filter(ok).max_by_key(|s| {
            let dim = |k| s.get(k).and_then(Value::as_u64).unwrap_or(0);
            (dim("width") * dim("height"), dim("file_size"))
        });
        return Some(match best {
            Some(s) => InboundMedia {
                kind: MediaKind::Photo,
                file_id: s["file_id"].as_str().unwrap_or_default().to_string(),
                declared_name: None,
                caption,
                skip: None,
            },
            None if !sizes.is_empty() => InboundMedia {
                kind: MediaKind::Photo,
                file_id: String::new(),
                declared_name: None,
                caption,
                skip: Some("too_large"),
            },
            None => return None,
        });
    }
    let d = m.get("document")?;
    let file_id = d.get("file_id").and_then(Value::as_str)?.to_string();
    let too_large = d
        .get("file_size")
        .and_then(Value::as_u64)
        .is_some_and(|n| n > cap);
    Some(InboundMedia {
        kind: MediaKind::File,
        file_id,
        declared_name: d
            .get("file_name")
            .and_then(Value::as_str)
            .map(str::to_string),
        caption,
        skip: too_large.then_some("too_large"),
    })
}

/// A Telegram chat id: an optional `-` and 1 to 20 digits.
fn valid_chat_id(c: &str) -> bool {
    let d = c.strip_prefix('-').unwrap_or(c);
    (1..=20).contains(&d.len()) && d.bytes().all(|b| b.is_ascii_digit())
}

/// A chat id as JSON: a number when it fits (always, for a valid id), else the string.
fn chat_value(c: &str) -> Value {
    c.parse::<i64>()
        .map(Value::from)
        .unwrap_or_else(|_| Value::from(c))
}

impl ChannelConnector for TelegramConnector {
    fn kind(&self) -> ChannelKind {
        ChannelKind::Telegram
    }

    fn side(&self) -> Side {
        Side::Customer
    }

    fn authenticate<'a>(
        &'a self,
        req: &'a IngressRequest,
    ) -> BoxFuture<'a, Result<IngressAuth, ChannelError>> {
        Box::pin(async move { self.check(req).map(|()| IngressAuth::default()) })
    }

    fn ingest<'a>(
        &'a self,
        req: &'a IngressRequest,
        _auth: &'a IngressAuth,
    ) -> BoxFuture<'a, Result<Ingress, ChannelError>> {
        Box::pin(async move {
            Ok(match self.parse_update(&req.body)? {
                None => Ingress::Reply(EdgeReply::ok()),
                Some((m, pressed, media)) => {
                    if let Some(p) = &pressed {
                        self.acknowledge(p).await;
                    }
                    match media {
                        None => Ingress::Messages {
                            ack: EdgeReply::ok(),
                            inbound: vec![m],
                        },
                        Some(media) => Ingress::Media {
                            ack: EdgeReply::ok(),
                            inbound: Box::new(m),
                            media,
                        },
                    }
                }
            })
        })
    }

    fn deliver<'a>(
        &'a self,
        conv: &'a ConversationRef,
        msg: Outbound,
    ) -> BoxFuture<'a, Result<DeliveryReceipt, ChannelError>> {
        Box::pin(self.send(conv, msg))
    }

    fn download_media<'a>(
        &'a self,
        media: &'a InboundMedia,
    ) -> BoxFuture<'a, Result<MediaFile, &'static str>> {
        Box::pin(async move {
            if let Some(reason) = media.skip {
                return Err(reason);
            }
            let bytes = self.download(&media.file_id).await?;
            Ok(MediaFile {
                bytes,
                declared_name: media.declared_name.clone().unwrap_or_default(),
            })
        })
    }

    /// R6: Telegram resends an update until it gets a 2xx, so the update is stored in core before the 200.
    fn persist_before_ack(&self) -> bool {
        true
    }

    /// The binding's bot (`bot_token` + `webhook_secret`) and allow-list; `None` when the credential is incomplete,
    /// so the registered connector (no bot) refuses the binding's webhook.
    fn for_binding(&self, binding: &Binding) -> Option<Arc<dyn ChannelConnector>> {
        if binding.kind != ChannelKind::Telegram {
            return None;
        }
        match self.with_binding(binding) {
            Some(c) => Some(Arc::new(c)),
            None => {
                tracing::warn!(binding_id = ?binding.id, "telegram_binding_credential_incomplete");
                None
            }
        }
    }

    fn read_webhook_info(&self) -> BoxFuture<'_, Result<WebhookInfo, ChannelError>> {
        Box::pin(self.webhook_info())
    }

    fn status(&self) -> ChannelStatus {
        ChannelStatus {
            kind: ChannelKind::Telegram,
            side: Side::Customer,
            bindings: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interaction::model::Scope;
    use axum::body::Bytes;
    use axum::http::HeaderMap;
    use uuid::Uuid;

    /// Token-shaped, not a real token.
    const TOKEN: &str = "123456789:test-not-a-real-bot-token-000000";
    const SECRET: &str = "test-webhook-secret-0001";

    fn binding(settings: Value) -> Binding {
        Binding {
            id: Some(Uuid::nil()),
            kind: ChannelKind::Telegram,
            side: Side::Customer,
            external_id: "123456789".into(),
            scope: Scope {
                tenant: Uuid::nil(),
                job_id: Uuid::nil(),
                job_key: "support".into(),
            },
            env: "prod".into(),
            settings,
            credential: Some(BTreeMap::from([
                ("bot_token".to_string(), TOKEN.to_string()),
                ("webhook_secret".to_string(), SECRET.to_string()),
            ])),
        }
    }

    fn connector(settings: Value) -> TelegramConnector {
        TelegramConnector::new(TelegramConfig::default())
            .unwrap()
            .with_binding(&binding(settings))
            .unwrap()
    }

    fn req(secret: Option<&str>, body: Value) -> IngressRequest {
        let mut headers = HeaderMap::new();
        if let Some(s) = secret {
            headers.insert(SECRET_HEADER, s.parse().unwrap());
        }
        IngressRequest {
            method: Method::POST,
            headers,
            query: None,
            body: Bytes::from(body.to_string()),
            binding: None,
        }
    }

    fn update(update_id: i64, from: i64, text: &str) -> Value {
        json!({"update_id": update_id, "message": {"message_id": 7, "date": 1,
            "from": {"id": from, "is_bot": false, "first_name": "Ada", "last_name": "L"},
            "chat": {"id": from, "type": "private"}, "text": text}})
    }

    #[test]
    fn secret_header_is_checked_in_constant_time() {
        let c = connector(json!({"allowed_user_ids": [42]}));
        let b = update(1, 42, "hi");
        assert_eq!(c.check(&req(Some(SECRET), b.clone())), Ok(()));
        for bad in [None, Some("wrong"), Some("test-webhook-secret-0002")] {
            assert_eq!(
                c.check(&req(bad, b.clone())),
                Err(ChannelError::Unauthenticated("bad_secret_token")),
                "{bad:?}"
            );
        }
        let mut get = req(Some(SECRET), b);
        get.method = Method::GET;
        assert_eq!(
            c.check(&get),
            Err(ChannelError::Invalid("method_not_allowed"))
        );
    }

    #[test]
    fn the_registered_connector_without_a_bot_refuses() {
        let c = TelegramConnector::new(TelegramConfig::default()).unwrap();
        assert_eq!(
            c.check(&req(Some(SECRET), update(1, 42, "hi"))),
            Err(ChannelError::Unauthenticated("binding_not_configured"))
        );
    }

    #[test]
    fn a_binding_needs_a_matching_token_and_a_valid_secret() {
        let base = TelegramConnector::new(TelegramConfig::default()).unwrap();
        let mut b = binding(json!({}));
        b.external_id = "999".into();
        assert!(
            base.with_binding(&b).is_none(),
            "bot id must match external_id"
        );
        let mut b = binding(json!({}));
        b.credential
            .as_mut()
            .unwrap()
            .insert("webhook_secret".into(), "short".into());
        assert!(base.with_binding(&b).is_none(), "secret too short");
        let mut b = binding(json!({}));
        b.credential.as_mut().unwrap().remove("bot_token");
        assert!(base.with_binding(&b).is_none());
        assert!(base.with_binding(&binding(json!({}))).is_some());
        assert_eq!(bot_id_of(TOKEN), Some("123456789"));
        for bad in ["", "abc:def", "123:short", "123456789", ":x"] {
            assert_eq!(bot_id_of(bad), None, "{bad}");
        }
    }

    #[test]
    fn update_id_is_the_external_id_and_chat_the_conversation() {
        let c = connector(json!({"allowed_user_ids": [42]}));
        let m = c
            .parse(update(9001, 42, "hello").to_string().as_bytes())
            .unwrap()
            .unwrap();
        assert_eq!(m.external_id, "9001");
        assert_eq!(m.workspace.as_deref(), Some("123456789"));
        assert_eq!(m.text, "hello");
        assert_eq!(m.identity.subject, "42");
        assert_eq!(m.identity.display_name.as_deref(), Some("Ada L"));
        assert_eq!(m.conversation.channel, "telegram");
        assert_eq!(m.conversation.id, "42");
        assert_eq!(m.conversation.route["chat_id"], "42");
        assert!(m.scope.is_none(), "the edge scopes it from the binding");
    }

    #[test]
    fn senders_outside_the_allow_list_are_dropped() {
        let c = connector(json!({"allowed_user_ids": [42, "43"]}));
        assert!(
            c.parse(update(1, 7, "hi").to_string().as_bytes())
                .unwrap()
                .is_none()
        );
        assert!(
            c.parse(update(2, 43, "hi").to_string().as_bytes())
                .unwrap()
                .is_some()
        );
        // empty or absent list: nobody
        for s in [json!({}), json!({"allowed_user_ids": []})] {
            let c = connector(s);
            assert!(
                c.parse(update(3, 42, "hi").to_string().as_bytes())
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn bots_edits_and_other_media_are_ignored_and_garbage_is_invalid() {
        let c = connector(json!({"allowed_user_ids": [42]}));
        let mut bot = update(1, 42, "hi");
        bot["message"]["from"]["is_bot"] = json!(true);
        let edit = json!({"update_id": 2, "edited_message": update(2, 42, "x")["message"]});
        let mut no_text = update(3, 42, "x");
        no_text["message"].as_object_mut().unwrap().remove("text");
        let cb =
            json!({"update_id": 4, "callback_query": {"id": "q", "from": {"id": 42}, "data": "5"}});
        // media the connector does not take: a sticker, a voice note (no photo / document)
        let mut sticker = no_text.clone();
        sticker["message"]["sticker"] = json!({"file_id": "s", "width": 1, "height": 1});
        let mut voice = no_text.clone();
        voice["message"]["voice"] = json!({"file_id": "v", "duration": 3});
        // an empty photo array is nothing
        let mut empty = no_text.clone();
        empty["message"]["photo"] = json!([]);
        for v in [bot, edit, no_text, cb, sticker, voice, empty] {
            assert!(c.parse(v.to_string().as_bytes()).unwrap().is_none(), "{v}");
        }
        assert_eq!(c.parse(b"nope"), Err(ChannelError::Invalid("bad_payload")));
        assert_eq!(
            c.parse(json!({"message": {}}).to_string().as_bytes()),
            Err(ChannelError::Invalid("bad_payload"))
        );
    }

    /// A private-chat update from user 42 with `extra` merged into the message (and no text).
    fn media_update(update_id: i64, extra: Value) -> Value {
        let mut v = update(update_id, 42, "x");
        let m = v["message"].as_object_mut().unwrap();
        m.remove("text");
        for (k, val) in extra.as_object().unwrap() {
            m.insert(k.clone(), val.clone());
        }
        v
    }

    fn parsed_media(c: &TelegramConnector, v: &Value) -> (Inbound, Option<InboundMedia>) {
        c.parse_media(v.to_string().as_bytes()).unwrap().unwrap()
    }

    const MB: u64 = 1024 * 1024;

    #[test]
    fn a_photo_is_its_largest_size_within_ten_mb() {
        let c = connector(json!({"allowed_user_ids": [42]}));
        // Telegram sends several sizes, smallest first; the biggest is over the cap, so the next one is taken
        let v = media_update(
            1,
            json!({"photo": [
                {"file_id": "s", "file_unique_id": "a", "width": 90, "height": 60, "file_size": 1_000},
                {"file_id": "m", "file_unique_id": "b", "width": 320, "height": 240, "file_size": 20_000},
                {"file_id": "l", "file_unique_id": "c", "width": 1280, "height": 960, "file_size": 300_000},
                {"file_id": "xl", "file_unique_id": "d", "width": 4000, "height": 3000, "file_size": 11 * MB}]}),
        );
        let (m, media) = parsed_media(&c, &v);
        let media = media.unwrap();
        assert_eq!(
            (media.kind, media.file_id.as_str()),
            (MediaKind::Photo, "l")
        );
        assert_eq!(media.skip, None);
        assert_eq!(m.text, "(photo)", "no caption: the placeholder");
        assert_eq!(m.external_id, "1");
        assert_eq!(m.conversation.id, "42");
        // the plain `parse` still yields the message
        assert_eq!(
            c.parse(v.to_string().as_bytes()).unwrap().unwrap().text,
            "(photo)"
        );
        // a size without file_size is allowed (the download is capped while it streams)
        let v = media_update(
            2,
            json!({"photo": [{"file_id": "only", "width": 10, "height": 10}]}),
        );
        assert_eq!(parsed_media(&c, &v).1.unwrap().file_id, "only");
    }

    #[test]
    fn a_photo_whose_every_size_is_over_ten_mb_is_skipped_as_too_large() {
        let c = connector(json!({"allowed_user_ids": [42]}));
        let v = media_update(
            1,
            json!({"photo": [{"file_id": "xl", "width": 4000, "height": 3000, "file_size": 11 * MB}]}),
        );
        let (m, media) = parsed_media(&c, &v);
        let media = media.unwrap();
        assert_eq!(media.skip, Some("too_large"));
        assert_eq!(m.text, "(photo)");
        assert_eq!(
            media.refused_text("too_large"),
            "(photo — not accepted: it is over 10 MB)"
        );
    }

    #[test]
    fn a_document_keeps_its_name_and_caption_and_the_type_is_not_telegrams() {
        let c = connector(json!({"allowed_user_ids": [42]}));
        // Telegram says image/png; we do not look: core sniffs the bytes
        let v = media_update(
            1,
            json!({"caption": "my invoice", "document": {"file_id": "d1", "file_name": "inv.pdf",
                "mime_type": "image/png", "file_size": 5 * MB}}),
        );
        let (m, media) = parsed_media(&c, &v);
        let media = media.unwrap();
        assert_eq!(m.text, "my invoice", "the caption is the message text");
        assert_eq!(
            (
                media.kind,
                media.file_id.as_str(),
                media.declared_name.as_deref()
            ),
            (MediaKind::File, "d1", Some("inv.pdf"))
        );
        assert_eq!((media.caption.as_str(), media.skip), ("my invoice", None));
        assert_eq!(
            media.refused_text("active_pdf"),
            "my invoice\n(file — not accepted: the PDF contains active content)"
        );
        // no caption: "(file)"
        let v = media_update(2, json!({"document": {"file_id": "d2"}}));
        let (m, media) = parsed_media(&c, &v);
        assert_eq!(m.text, "(file)");
        assert_eq!(media.unwrap().skip, None);
    }

    #[test]
    fn an_oversize_document_is_skipped_and_a_stranger_is_still_dropped() {
        let c = connector(json!({"allowed_user_ids": [42]}));
        let v = media_update(
            1,
            json!({"document": {"file_id": "big", "file_size": 10 * MB + 1}}),
        );
        assert_eq!(parsed_media(&c, &v).1.unwrap().skip, Some("too_large"));
        let ok = media_update(
            2,
            json!({"document": {"file_id": "edge", "file_size": 10 * MB}}),
        );
        assert_eq!(
            parsed_media(&c, &ok).1.unwrap().skip,
            None,
            "exactly 10 MB is allowed"
        );
        let mut stranger = media_update(3, json!({"document": {"file_id": "d"}}));
        stranger["message"]["from"]["id"] = json!(7);
        stranger["message"]["chat"]["id"] = json!(7);
        assert!(
            c.parse_media(stranger.to_string().as_bytes())
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn ingest_turns_media_into_an_ingress_media_and_text_into_messages() {
        let c = connector(json!({"allowed_user_ids": [42]}));
        let v = media_update(
            5,
            json!({"caption": "look", "photo": [{"file_id": "p", "width": 1, "height": 1}]}),
        );
        match c
            .ingest(&req(Some(SECRET), v), &IngressAuth::default())
            .await
            .unwrap()
        {
            Ingress::Media { inbound, media, .. } => {
                assert_eq!(
                    (inbound.text.as_str(), media.file_id.as_str()),
                    ("look", "p")
                );
            }
            other => panic!("{other:?}"),
        }
        let t = update(6, 42, "hi");
        assert!(matches!(
            c.ingest(&req(Some(SECRET), t), &IngressAuth::default())
                .await
                .unwrap(),
            Ingress::Messages { .. }
        ));
    }

    /// The owner's forum supergroup (live: -1004368404922), one message in a topic and one in General.
    fn group_update(update_id: i64, from: i64, thread: Option<i64>, text: &str) -> Value {
        let mut v = json!({"update_id": update_id, "message": {"message_id": 7, "date": 1,
            "from": {"id": from, "is_bot": false, "first_name": "Ada"},
            "chat": {"id": -1004368404922_i64, "type": "supergroup", "is_forum": true,
                "title": "Support"}, "text": text}});
        if let Some(t) = thread {
            v["message"]["message_thread_id"] = json!(t);
            v["message"]["is_topic_message"] = json!(true);
        }
        v
    }

    #[test]
    fn a_forum_topic_is_its_own_conversation_routed_to_its_thread() {
        let c = connector(json!({"allowed_user_ids": [42]}));
        let m = c
            .parse(group_update(1, 42, Some(17), "hi").to_string().as_bytes())
            .unwrap()
            .unwrap();
        assert_eq!(m.conversation.id, "-1004368404922:17");
        assert_eq!(m.conversation.route["chat_id"], "-1004368404922");
        assert_eq!(m.conversation.route[THREAD_KEY], "17");
        assert_eq!(m.identity.subject, "42", "the customer is the sender");
        assert_eq!(
            route_target(&m.conversation),
            Some(("-1004368404922".to_string(), Some(17)))
        );
        // a thread id without is_topic_message (a reply thread in a plain group) is not a topic
        let mut reply = group_update(2, 42, Some(17), "hi");
        reply["message"]["is_topic_message"] = json!(false);
        let m = c.parse(reply.to_string().as_bytes()).unwrap().unwrap();
        assert_eq!(m.conversation.id, "-1004368404922");
        assert!(!m.conversation.route.contains_key(THREAD_KEY));
    }

    #[test]
    fn the_general_topic_and_a_plain_group_are_the_chat() {
        let c = connector(json!({"allowed_user_ids": [42]}));
        let m = c
            .parse(group_update(1, 42, None, "hi").to_string().as_bytes())
            .unwrap()
            .unwrap();
        assert_eq!(m.conversation.id, "-1004368404922");
        assert_eq!(
            m.conversation.route,
            BTreeMap::from([("chat_id".to_string(), "-1004368404922".to_string())])
        );
        let mut plain = group_update(2, 42, None, "hi");
        plain["message"]["chat"] = json!({"id": -555, "type": "group", "title": "g"});
        let m = c.parse(plain.to_string().as_bytes()).unwrap().unwrap();
        assert_eq!(m.conversation.id, "-555");
        // the allow-list holds in a group too
        assert!(
            c.parse(group_update(3, 7, Some(17), "hi").to_string().as_bytes())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn channel_posts_bots_and_service_messages_in_groups_are_ignored() {
        let c = connector(json!({"allowed_user_ids": [42]}));
        let mut bot = group_update(1, 42, Some(17), "hi");
        bot["message"]["from"]["is_bot"] = json!(true);
        let mut created = group_update(2, 42, Some(17), "x");
        let msg = created["message"].as_object_mut().unwrap();
        msg.remove("text");
        msg.insert(
            "forum_topic_created".into(),
            json!({"name": "Orders", "icon_color": 1}),
        );
        let mut joined = group_update(3, 42, None, "x");
        let msg = joined["message"].as_object_mut().unwrap();
        msg.remove("text");
        msg.insert(
            "new_chat_members".into(),
            json!([{"id": 9, "is_bot": true}]),
        );
        let mut channel = update(4, 42, "x");
        channel["message"]["chat"] = json!({"id": -1009, "type": "channel"});
        let post = json!({"update_id": 5, "channel_post": {"message_id": 1, "date": 1,
            "chat": {"id": -1009, "type": "channel"}, "text": "x"}});
        let mut untyped = update(6, 42, "x");
        untyped["message"]["chat"]
            .as_object_mut()
            .unwrap()
            .remove("type");
        for v in [bot, created, joined, channel, post, untyped] {
            assert!(c.parse(v.to_string().as_bytes()).unwrap().is_none(), "{v}");
        }
    }

    #[test]
    fn route_target_reads_the_route_then_the_conversation_id() {
        let r = |id: &str, route: &[(&str, &str)]| ConversationRef {
            channel: "telegram".into(),
            id: id.into(),
            route: route
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        };
        assert_eq!(
            route_target(&r("42", &[("chat_id", "42")])),
            Some(("42".into(), None))
        );
        assert_eq!(route_target(&r("42", &[])), Some(("42".into(), None)));
        assert_eq!(
            route_target(&r("-100:5", &[])),
            Some(("-100".into(), Some(5)))
        );
        assert_eq!(
            route_target(&r("x", &[("chat_id", "-100"), (THREAD_KEY, "5")])),
            Some(("-100".into(), Some(5)))
        );
        for bad in [
            r("x", &[("chat_id", "-100"), (THREAD_KEY, "0")]),
            r("x", &[("chat_id", "-100"), (THREAD_KEY, "a")]),
            r("-100:", &[]),
            r("@chan", &[]),
        ] {
            assert_eq!(route_target(&bad), None, "{bad:?}");
        }
    }

    #[test]
    fn send_message_body_is_chat_thread_and_html_text() {
        assert_eq!(
            TelegramConnector::send_message_body("42", None, "Hi"),
            json!({"chat_id": 42, "text": "Hi", "parse_mode": "HTML"})
        );
        assert_eq!(
            TelegramConnector::send_message_body("-100123", None, "x")["chat_id"],
            json!(-100123)
        );
        assert_eq!(
            TelegramConnector::send_message_body("-1004368404922", Some(17), "x"),
            json!({"chat_id": -1004368404922_i64, "message_thread_id": 17, "text": "x",
                "parse_mode": "HTML"})
        );
        // What `send` passes: the markdown rendered and escaped, one part per body.
        let parts = crate::channels::telegram_html::html_parts("**Hi** a < b", MAX_TEXT_CHARS);
        assert_eq!(
            TelegramConnector::send_message_body("42", None, &parts[0])["text"],
            json!("<b>Hi</b> a &lt; b")
        );
        assert!(valid_chat_id("-100123") && valid_chat_id("42"));
        assert!(!valid_chat_id("@chan") && !valid_chat_id("") && !valid_chat_id("4a"));
    }

    #[test]
    fn secrets_and_settings_are_validated() {
        assert!(valid_webhook_secret(SECRET));
        assert!(!valid_webhook_secret("short"));
        assert!(!valid_webhook_secret(&"a".repeat(257)));
        assert!(!valid_webhook_secret("has space in it 0123"));
        assert_eq!(
            allowed_user_ids(&json!({"allowed_user_ids": [1, "2", -3, "x", 1.5]})),
            BTreeSet::from([1, 2])
        );
        let dbg = format!("{:?}", connector(json!({})).bot);
        assert!(
            !dbg.contains("test-not-a-real") && !dbg.contains(SECRET),
            "{dbg}"
        );
    }

    #[test]
    fn api_base_is_checked_by_name() {
        let none = |_: &str| -> Option<String> { None };
        assert_eq!(config_from(&none).unwrap().api_base, DEFAULT_API_BASE);
        let set =
            |k: &str| (k == "TELEGRAM_CHANNEL_API_BASE").then(|| "http://127.0.0.1:9/".to_string());
        assert_eq!(config_from(&set).unwrap().api_base, "http://127.0.0.1:9");
        let bad = |k: &str| (k == "TELEGRAM_CHANNEL_API_BASE").then(|| "ftp://x".to_string());
        assert!(
            config_from(&bad)
                .unwrap_err()
                .to_string()
                .contains("TELEGRAM_CHANNEL_API_BASE")
        );
    }

    #[test]
    fn webhook_errors_are_codes() {
        for (m, code) in [
            (
                "Wrong response from the webhook: 502 Bad Gateway",
                "webhook_http_502",
            ),
            (
                "wrong response from the webhook: 404 Not Found",
                "webhook_http_404",
            ),
            (
                "Wrong response from the webhook: Bad Gateway",
                "webhook_error",
            ),
            ("Read timeout expired", "webhook_timeout"),
            ("Connection timed out", "webhook_timeout"),
            ("Connection refused", "webhook_refused"),
            ("SSL error {error:14094418:SSL routines}", "webhook_tls"),
            ("Bad webhook: certificate verify failed", "webhook_tls"),
            (
                "Failed to resolve host: Name or service not known",
                "webhook_error",
            ),
            ("", "webhook_error"),
        ] {
            let c = webhook_error_code(m);
            assert_eq!(c, code, "{m}");
            assert!(crate::channels::webhook_health::is_error_code(&c));
        }
    }

    #[test]
    fn webhook_info_is_parsed_without_its_text() {
        let w = parse_webhook_info(&json!({
            "url": "https://chat.example/api/channels/telegram/webhook/x", "has_custom_certificate": false,
            "pending_update_count": 150, "last_error_date": 1_700_000_000,
            "last_error_message": "Wrong response from the webhook: 503 Service Unavailable", "max_connections": 40
        }))
        .unwrap();
        assert_eq!(
            w,
            WebhookInfo {
                url: "https://chat.example/api/channels/telegram/webhook/x".into(),
                pending_update_count: 150,
                last_error_code: Some("webhook_http_503".into()),
                last_error_at_ms: Some(1_700_000_000_000),
            }
        );
        // no webhook registered, no error
        assert_eq!(
            parse_webhook_info(&json!({"url": "", "pending_update_count": 0})).unwrap(),
            WebhookInfo::default()
        );
        // a message without a date cannot be ordered: dropped; a date without a message is an error
        let w = parse_webhook_info(&json!({"url": "", "last_error_message": "Connection refused"}))
            .unwrap();
        assert_eq!((w.last_error_code, w.last_error_at_ms), (None, None));
        let w = parse_webhook_info(&json!({"url": "", "last_error_date": 5})).unwrap();
        assert_eq!(
            (w.last_error_code.as_deref(), w.last_error_at_ms),
            (Some("webhook_error"), Some(5_000))
        );
        assert!(parse_webhook_info(&Value::Null).is_err());
    }

    #[test]
    fn a_url_points_at_its_bindings_path() {
        let id = Uuid::parse_str("6f1c2a3b-4d5e-4f60-8a7b-9c0d1e2f3a4b").unwrap();
        let other = Uuid::parse_str("00000000-0000-4000-8000-000000000001").unwrap();
        let at = |u: &str| url_points_at(u, id);
        assert!(at(
            "https://chat.nonagentic.ai/api/channels/telegram/webhook/6f1c2a3b-4d5e-4f60-8a7b-9c0d1e2f3a4b"
        ));
        assert!(at(
            "https://chat.example/edge/api/channels/telegram/webhook/6F1C2A3B-4D5E-4F60-8A7B-9C0D1E2F3A4B?x=1"
        ));
        assert!(!at(&format!(
            "https://chat.nonagentic.ai{}",
            webhook_path(other)
        )));
        assert!(!at(
            "https://chat.nonagentic.ai/api/channels/telegram/webhook"
        ));
        assert!(!at(
            "https://chat.nonagentic.ai/api/channels/telegram/webhook/6f1c2a3b-4d5e-4f60-8a7b-9c0d1e2f3a4b/"
        ));
        assert!(!at("not a url"));
        assert!(!at(""));
    }

    /// A rating-button press (`callback_query`, `csat:<n>` on the message that carried the keyboard).
    fn press(update_id: i64, from: i64, data: &str) -> Value {
        json!({"update_id": update_id, "callback_query": {
            "id": "cbq-1", "from": {"id": from, "first_name": "Ada", "is_bot": false}, "data": data,
            "message": {"message_id": 99, "date": 1, "chat": {"id": from, "type": "private"},
                        "from": {"id": 123456789, "is_bot": true}, "text": "How did we do? Reply 1–5"}}})
    }

    #[test]
    fn a_rating_button_is_the_digit_typed_in_its_chat() {
        let c = connector(json!({"allowed_user_ids": [42]}));
        let m = c
            .parse(press(77, 42, "csat:4").to_string().as_bytes())
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                m.text.as_str(),
                m.external_id.as_str(),
                m.conversation.id.as_str()
            ),
            ("4", "77", "42")
        );
        assert_eq!(m.identity.subject, "42");
        let (_, pressed, _) = c
            .parse_update(press(77, 42, "csat:4").to_string().as_bytes())
            .unwrap()
            .unwrap();
        assert_eq!(
            pressed,
            Some(Pressed {
                query_id: "cbq-1".into(),
                chat_id: Some("42".into()),
                message_id: Some(99),
            })
        );
        // a typed message is no press
        let (_, typed, _) = c
            .parse_update(update(78, 42, "4").to_string().as_bytes())
            .unwrap()
            .unwrap();
        assert_eq!(typed, None);
    }

    #[test]
    fn other_buttons_and_strangers_pressing_are_ignored() {
        let c = connector(json!({"allowed_user_ids": [42]}));
        for v in [
            press(1, 42, "csat:0"),
            press(2, 42, "csat:6"),
            press(3, 42, "menu:4"),
            press(4, 42, "4"),
            press(5, 7, "csat:4"),
        ] {
            assert!(c.parse(v.to_string().as_bytes()).unwrap().is_none(), "{v}");
        }
    }

    #[test]
    fn the_rating_keyboard_is_one_row_of_five() {
        let k = rating_keyboard();
        let row = k["inline_keyboard"][0].as_array().unwrap();
        let datas: Vec<&str> = row
            .iter()
            .map(|b| b["callback_data"].as_str().unwrap())
            .collect();
        assert_eq!(datas, ["csat:1", "csat:2", "csat:3", "csat:4", "csat:5"]);
        assert_eq!(row[2]["text"], "3");
        assert_eq!(k["inline_keyboard"].as_array().unwrap().len(), 1);
    }

    /// The Bot API calls a fake bot saw: `(method, body)`.
    async fn json_bot() -> (String, Arc<std::sync::Mutex<Vec<(String, Value)>>>) {
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = calls.clone();
        let app = axum::Router::new().route(
            "/{bot}/{method}",
            axum::routing::post(
                move |axum::extract::Path((_bot, method)): axum::extract::Path<(
                    String,
                    String,
                )>,
                      axum::Json(body): axum::Json<Value>| {
                    let seen = seen.clone();
                    async move {
                        seen.lock().unwrap().push((method, body));
                        axum::Json(json!({"ok": true, "result": {"message_id": 5}}))
                    }
                },
            ),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        (format!("http://{addr}"), calls)
    }

    #[tokio::test]
    async fn a_rating_request_carries_the_buttons_and_a_reply_does_not() {
        let (base, calls) = json_bot().await;
        let c = TelegramConnector::new(TelegramConfig { api_base: base })
            .unwrap()
            .with_binding(&binding(json!({"allowed_user_ids": [42]})))
            .unwrap();
        let conv = ConversationRef {
            channel: "telegram".into(),
            id: "42".into(),
            route: BTreeMap::from([("chat_id".to_string(), "42".to_string())]),
        };
        c.deliver(
            &conv,
            Outbound::CsatRequest {
                text: "How did we do? Reply 1–5".into(),
            },
        )
        .await
        .unwrap();
        c.deliver(&conv, Outbound::Text("Your refund is on its way.".into()))
            .await
            .unwrap();
        let calls = calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert_eq!(calls[0].0, "sendMessage");
        assert_eq!(calls[0].1["reply_markup"], rating_keyboard());
        assert!(calls[1].1.get("reply_markup").is_none(), "{calls:?}");
    }

    #[tokio::test]
    async fn a_press_is_answered_and_its_keyboard_cleared() {
        let (base, calls) = json_bot().await;
        let c = TelegramConnector::new(TelegramConfig { api_base: base })
            .unwrap()
            .with_binding(&binding(json!({"allowed_user_ids": [42]})))
            .unwrap();
        let r = req(Some(SECRET), press(80, 42, "csat:5"));
        let auth = c.authenticate(&r).await.unwrap();
        match c.ingest(&r, &auth).await.unwrap() {
            Ingress::Messages { inbound, .. } => assert_eq!(inbound[0].text, "5"),
            _ => panic!("a press is a message"),
        }
        let calls = calls.lock().unwrap().clone();
        let methods: Vec<&str> = calls.iter().map(|(m, _)| m.as_str()).collect();
        assert_eq!(methods, ["answerCallbackQuery", "editMessageReplyMarkup"]);
        assert_eq!(calls[0].1["callback_query_id"], "cbq-1");
        assert_eq!(
            (&calls[1].1["chat_id"], &calls[1].1["message_id"]),
            (&json!(42), &json!(99))
        );
        assert_eq!(calls[1].1["reply_markup"]["inline_keyboard"], json!([]));
    }

    /// The call goes to the binding's bot and comes back parsed; the registered connector (no bot) refuses.
    #[tokio::test]
    async fn webhook_info_reads_the_registration() {
        let app = axum::Router::new().route(
            "/{bot}/getWebhookInfo",
            axum::routing::post(
                |axum::extract::Path(bot): axum::extract::Path<String>| async move {
                    let url = if bot == format!("bot{TOKEN}") {
                        "https://chat.example/x"
                    } else {
                        ""
                    };
                    axum::Json(
                        json!({"ok": true, "result": {"url": url, "pending_update_count": 3}}),
                    )
                },
            ),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let base = TelegramConnector::new(TelegramConfig {
            api_base: format!("http://{addr}"),
        })
        .unwrap();
        assert_eq!(
            base.read_webhook_info().await,
            Err(ChannelError::Unauthenticated("binding_not_configured"))
        );
        let c = base.with_binding(&binding(json!({}))).unwrap();
        let w = c.read_webhook_info().await.unwrap();
        assert_eq!(
            (w.url.as_str(), w.pending_update_count),
            ("https://chat.example/x", 3)
        );
    }

    // ---- fake Bot API ------------------------------------------------------------------------------------------

    /// One request the fake Bot API saw.
    #[derive(Clone, Debug)]
    struct Seen {
        method: String,
        path: String,
        content_type: String,
        body: Vec<u8>,
    }

    impl Seen {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.body).into_owned()
        }
    }

    struct Fake {
        seen: std::sync::Mutex<Vec<Seen>>,
        /// The status the send methods answer with.
        send_status: u16,
    }

    async fn fake_handler(
        axum::extract::State(f): axum::extract::State<Arc<Fake>>,
        req: axum::http::Request<axum::body::Body>,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        let method = req.method().to_string();
        let path = req.uri().path().to_string();
        let content_type = req
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let body = axum::body::to_bytes(req.into_body(), 64 * 1024 * 1024)
            .await
            .unwrap()
            .to_vec();
        f.seen.lock().unwrap().push(Seen {
            method,
            path: path.clone(),
            content_type,
            body: body.clone(),
        });
        let prefix = format!("/bot{TOKEN}/");
        if let Some(rest) = path.strip_prefix(&prefix) {
            return match rest {
                "sendMessage" | "sendPhoto" | "sendDocument" => match f.send_status {
                    200 => axum::Json(json!({"ok": true, "result": {"message_id": 501}}))
                        .into_response(),
                    429 => (
                        axum::http::StatusCode::TOO_MANY_REQUESTS,
                        axum::Json(json!({"ok": false, "error_code": 429,
                            "description": "Too Many Requests: retry after 7", "parameters": {"retry_after": 7}})),
                    )
                        .into_response(),
                    s => (
                        axum::http::StatusCode::from_u16(s).unwrap(),
                        axum::Json(json!({"ok": false, "error_code": s,
                            "description": "Forbidden: bot was blocked by the user"})),
                    )
                        .into_response(),
                },
                "getFile" => {
                    let v: Value = serde_json::from_slice(&body).unwrap();
                    let (path, size) = match v["file_id"].as_str().unwrap_or_default() {
                        "photo1" => ("photos/file_1.jpg", Some(5)),
                        "big" => ("photos/big.jpg", Some(11 * MB)),
                        "huge_len" => ("photos/huge_len.jpg", None),
                        "huge_stream" => ("photos/huge_stream.jpg", None),
                        "evil" => ("../../etc/passwd", Some(5)),
                        "gone" => ("photos/gone.jpg", Some(5)),
                        _ => return axum::Json(json!({"ok": false, "error_code": 400})).into_response(),
                    };
                    axum::Json(json!({"ok": true, "result": {"file_id": "x", "file_path": path, "file_size": size}}))
                        .into_response()
                }
                _ => axum::http::StatusCode::NOT_FOUND.into_response(),
            };
        }
        match path.strip_prefix(&format!("/file/bot{TOKEN}/")) {
            Some("photos/file_1.jpg") => b"JPEGBYTES".to_vec().into_response(),
            Some("photos/huge_len.jpg") => vec![0u8; MAX_MEDIA_BYTES + 1].into_response(),
            Some("photos/huge_stream.jpg") => {
                // no Content-Length: the cap must hold while it streams
                let chunk = vec![0u8; 1024 * 1024];
                let chunks = (0..11).map(move |_| Ok::<_, std::convert::Infallible>(chunk.clone()));
                axum::body::Body::from_stream(futures_util::stream::iter(chunks)).into_response()
            }
            _ => axum::http::StatusCode::NOT_FOUND.into_response(),
        }
    }

    async fn fake_bot(send_status: u16) -> (TelegramConnector, Arc<Fake>) {
        let fake = Arc::new(Fake {
            seen: Default::default(),
            send_status,
        });
        let app = axum::Router::new()
            .fallback(fake_handler)
            .with_state(fake.clone());
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let c = TelegramConnector::new(TelegramConfig {
            api_base: format!("http://{addr}"),
        })
        .unwrap()
        .with_binding(&binding(json!({"allowed_user_ids": [42]})))
        .unwrap();
        (c, fake)
    }

    fn conv(id: &str) -> ConversationRef {
        let mut route = BTreeMap::new();
        let (chat, thread) = id.split_once(':').map_or((id, None), |(c, t)| (c, Some(t)));
        route.insert("chat_id".to_string(), chat.to_string());
        if let Some(t) = thread {
            route.insert(THREAD_KEY.to_string(), t.to_string());
        }
        ConversationRef {
            channel: "telegram".into(),
            id: id.into(),
            route,
        }
    }

    fn attachment(media_type: &str, name: &str, caption: Option<&str>) -> Outbound {
        Outbound::Attachment {
            media_type: media_type.into(),
            display_name: name.into(),
            bytes: super::super::connector::Blob(b"FILEBYTES".to_vec()),
            caption: caption.map(str::to_string),
        }
    }

    fn calls(f: &Fake) -> Vec<Seen> {
        f.seen.lock().unwrap().clone()
    }

    #[tokio::test]
    async fn an_image_goes_out_as_a_multipart_send_photo_with_the_reply_as_caption() {
        let (c, f) = fake_bot(200).await;
        let r = c
            .deliver(
                &conv("42"),
                attachment("image/png", "photo.png", Some("Here is **it**")),
            )
            .await
            .unwrap();
        assert_eq!(r.external_id.as_deref(), Some("501"));
        let seen = calls(&f);
        assert_eq!(seen.len(), 1, "text fits: no separate sendMessage");
        let s = &seen[0];
        assert_eq!(
            (s.method.as_str(), s.path.as_str()),
            ("POST", format!("/bot{TOKEN}/sendPhoto").as_str())
        );
        assert!(
            s.content_type.starts_with("multipart/form-data; boundary="),
            "{}",
            s.content_type
        );
        let t = s.text();
        for want in [
            "name=\"chat_id\"\r\n\r\n42",
            "name=\"photo\"; filename=\"photo.png\"",
            "Content-Type: image/png",
            "FILEBYTES",
            "name=\"caption\"\r\n\r\nHere is <b>it</b>",
            "name=\"parse_mode\"\r\n\r\nHTML",
        ] {
            assert!(t.contains(want), "missing {want:?} in {t}");
        }
        assert!(!t.contains("message_thread_id"));
        assert!(!t.contains("name=\"document\""));
    }

    #[tokio::test]
    async fn a_pdf_goes_out_as_send_document_with_its_display_name_and_the_topic() {
        let (c, f) = fake_bot(200).await;
        c.deliver(
            &conv("-100123:17"),
            attachment("application/pdf", "invoice.pdf", None),
        )
        .await
        .unwrap();
        let seen = calls(&f);
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].path, format!("/bot{TOKEN}/sendDocument"));
        let t = seen[0].text();
        for want in [
            "name=\"chat_id\"\r\n\r\n-100123",
            "name=\"message_thread_id\"\r\n\r\n17",
            "name=\"document\"; filename=\"invoice.pdf\"",
            "Content-Type: application/pdf",
        ] {
            assert!(t.contains(want), "missing {want:?} in {t}");
        }
        assert!(!t.contains("name=\"caption\""), "no text: no caption");
        assert!(!t.contains("name=\"photo\""));
    }

    #[tokio::test]
    async fn a_reply_over_1024_characters_goes_first_as_its_own_message() {
        let (c, f) = fake_bot(200).await;
        let long = "word ".repeat(300); // 1500 characters
        c.deliver(
            &conv("42"),
            attachment("image/jpeg", "photo.jpg", Some(&long)),
        )
        .await
        .unwrap();
        let seen = calls(&f);
        let paths: Vec<_> = seen
            .iter()
            .map(|s| s.path.rsplit('/').next().unwrap())
            .collect();
        assert_eq!(
            paths,
            ["sendMessage", "sendPhoto"],
            "text first, then the file"
        );
        assert!(seen[0].text().contains("word word"));
        assert!(
            !seen[1].text().contains("name=\"caption\""),
            "the file has no caption"
        );
        // exactly at the limit still fits as a caption
        let (c, f) = fake_bot(200).await;
        let fit = "a".repeat(MAX_CAPTION_CHARS);
        c.deliver(
            &conv("42"),
            attachment("image/jpeg", "photo.jpg", Some(&fit)),
        )
        .await
        .unwrap();
        let seen = calls(&f);
        assert_eq!(seen.len(), 1);
        assert!(
            seen[0]
                .text()
                .contains(&format!("name=\"caption\"\r\n\r\n{fit}"))
        );
    }

    #[tokio::test]
    async fn send_errors_keep_their_codes_and_never_the_token() {
        for (status, want) in [
            (403, ChannelError::Upstream("telegram_403_403".into())),
            (429, ChannelError::RateLimited { retry_after_s: 7 }),
        ] {
            let (c, _) = fake_bot(status).await;
            let e = c
                .deliver(&conv("42"), attachment("image/png", "p.png", Some("hi")))
                .await
                .unwrap_err();
            assert_eq!(e, want);
            assert!(!format!("{e} {e:?}").contains("test-not-a-real"));
        }
        // unsupported type: refused by name, nothing sent
        let (c, f) = fake_bot(200).await;
        assert_eq!(
            c.deliver(&conv("42"), attachment("image/gif", "a.gif", None))
                .await,
            Err(ChannelError::Invalid("unsupported_attachment"))
        );
        assert!(calls(&f).is_empty());
        // unreachable Bot API: a code, not the URL (which carries the token)
        let c = TelegramConnector::new(TelegramConfig {
            api_base: "http://127.0.0.1:9".into(),
        })
        .unwrap()
        .with_binding(&binding(json!({})))
        .unwrap();
        let e = c
            .deliver(&conv("42"), attachment("image/png", "p.png", None))
            .await
            .unwrap_err();
        assert_eq!(e, ChannelError::Upstream("telegram_unreachable".into()));
        // a logged Outbound never dumps the file
        let dbg = format!("{:?}", attachment("image/png", "p.png", None));
        assert!(
            !dbg.contains("FILEBYTES") && dbg.contains("9 bytes"),
            "{dbg}"
        );
    }

    fn media(file_id: &str) -> InboundMedia {
        InboundMedia {
            kind: MediaKind::Photo,
            file_id: file_id.into(),
            declared_name: Some("pic.jpg".into()),
            caption: String::new(),
            skip: None,
        }
    }

    #[tokio::test]
    async fn an_inbound_file_is_fetched_with_get_file_then_the_file_url() {
        let (c, f) = fake_bot(200).await;
        let got = c.download_media(&media("photo1")).await.unwrap();
        assert_eq!(got.bytes, b"JPEGBYTES");
        assert_eq!(got.declared_name, "pic.jpg");
        let seen = calls(&f);
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].path, format!("/bot{TOKEN}/getFile"));
        assert_eq!(
            serde_json::from_slice::<Value>(&seen[0].body).unwrap(),
            json!({"file_id": "photo1"})
        );
        assert_eq!(
            (seen[1].method.as_str(), seen[1].path.as_str()),
            (
                "GET",
                format!("/file/bot{TOKEN}/photos/file_1.jpg").as_str()
            )
        );
    }

    #[tokio::test]
    async fn downloads_are_capped_checked_and_never_name_the_token() {
        let (c, f) = fake_bot(200).await;
        // the size getFile reports, the Content-Length, and a stream with neither: all stop at 10 MB
        for id in ["big", "huge_len", "huge_stream"] {
            assert_eq!(c.download_media(&media(id)).await, Err("too_large"), "{id}");
        }
        // a file_path that is not a plain relative path is never put in an URL
        let before = calls(&f).len();
        assert_eq!(
            c.download_media(&media("evil")).await,
            Err("download_failed")
        );
        let after = calls(&f);
        assert_eq!(after.len(), before + 1, "getFile only, no download");
        assert!(after.iter().all(|s| !s.path.contains("passwd")));
        // an unknown file, a missing file
        for id in ["unknown", "gone"] {
            assert_eq!(
                c.download_media(&media(id)).await,
                Err("download_failed"),
                "{id}"
            );
        }
        // already known too large: nothing is called
        let n = calls(&f).len();
        let mut skipped = media("photo1");
        skipped.skip = Some("too_large");
        assert_eq!(c.download_media(&skipped).await, Err("too_large"));
        assert_eq!(calls(&f).len(), n);
        // an unreachable server: a code
        let dead = TelegramConnector::new(TelegramConfig {
            api_base: "http://127.0.0.1:9".into(),
        })
        .unwrap()
        .with_binding(&binding(json!({})))
        .unwrap();
        assert_eq!(
            dead.download_media(&media("photo1")).await,
            Err("download_failed")
        );
        // the registered connector (no bot) cannot download
        let none = TelegramConnector::new(TelegramConfig::default()).unwrap();
        assert_eq!(
            none.download_media(&media("photo1")).await,
            Err("download_failed")
        );
    }

    #[test]
    fn file_paths_are_checked_before_they_are_urls() {
        for ok in ["photos/file_1.jpg", "documents/file_12.pdf", "a-b/c.d"] {
            assert!(valid_file_path(ok), "{ok}");
        }
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../b",
            "a//b",
            "a b",
            "a?b=c",
            "a#b",
            "a\\b",
            "é",
        ] {
            assert!(!valid_file_path(bad), "{bad:?}");
        }
    }
}
