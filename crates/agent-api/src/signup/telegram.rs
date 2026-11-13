//! Telegram notices for early-access signups (contract C1). Ported from strata_configurator `signup.rs`
//! (detached send, bounded retry) with commonpractice v2 `services/signup-telegram.ts` (names, code entities,
//! callback buttons).
//!
//! ⛔ SEND ONLY: this service calls `sendMessage` and nothing else. It never registers or deletes a webhook and
//! never polls for updates: the bot's updates belong to chat-bridge, which relays an Approve/Reject tap to
//! `POST /api/signup-approvals/{id}/telegram-decision` (C2/C3). A test greps this crate for those names.
//!
//! ⛔ FIRE-AND-FORGET: a notice is sent from a detached task with a bounded retry; every failure is logged and
//! none is returned, so a Telegram outage cannot fail an intake or a decision. The bot token is part of the API
//! URL, so no transport error is ever formatted into a log.
//!
//! PERSONAL DATA (owner 2026-10-19, v2): the new-signup and decision messages carry the requester's name,
//! email, company and a message excerpt (the topic is the owner's private forum chat). Every requester-typed
//! value is forced onto one line and rendered inside a `code` entity, so Telegram cannot auto-link it and a
//! newline cannot forge a line of the message; there is no `parse_mode`.
use std::time::Duration;

use reqwest::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::config::{SignupConfig, TelegramConfig};
use crate::signup::single_line;

/// Attempts per message, and the backoff before each retry.
const MAX_ATTEMPTS: usize = 3;
const BACKOFF: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(4)];
/// Ceiling on honouring Telegram's `retry_after` (429).
const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest message excerpt shown.
pub const MESSAGE_EXCERPT_MAX: usize = 300;
/// Longest rendering of one single-line requester-typed field.
const FIELD_MAX: usize = 200;

/// The C1 callback data of an inline decision button: `ea1:<app>:a|r:<signupId>` (≤ 64 bytes for a uuid).
pub fn callback_data(app: &str, id: Uuid, approve: bool) -> String {
    format!("ea1:{app}:{}:{id}", if approve { "a" } else { "r" })
}

/// The super_admin signups page for one signup.
pub fn deep_link(cfg: &SignupConfig, id: Uuid) -> String {
    format!("{}/platform/signups?signup={id}", cfg.public_url)
}

/// First `max` characters of `v` on one line, with `…` when cut.
pub fn excerpt(v: &str, max: usize) -> String {
    let flat = single_line(v);
    if flat.chars().count() > max {
        let mut s: String = flat.chars().take(max).collect();
        s.push('…');
        s
    } else {
        flat
    }
}

/// Plain text plus `code` entities around every requester-typed value. Offsets are UTF-16 code units.
#[derive(Default)]
struct Message {
    text: String,
    utf16: usize,
    entities: Vec<Value>,
}

impl Message {
    fn plain(&mut self, s: &str) -> &mut Self {
        self.text.push_str(s);
        self.utf16 += s.encode_utf16().count();
        self
    }

    /// One labelled line whose value is untrusted. Empty → an em dash, outside any entity.
    fn field(&mut self, label: &str, value: Option<&str>, max: usize) -> &mut Self {
        let v = excerpt(value.unwrap_or_default(), max);
        self.plain(&format!("{label}: "));
        if v.is_empty() {
            return self.plain("—\n");
        }
        let len = v.encode_utf16().count();
        self.entities
            .push(json!({"type": "code", "offset": self.utf16, "length": len}));
        self.plain(&v);
        self.plain("\n")
    }

    fn payload(&self, t: &TelegramConfig) -> Value {
        // Trailing newlines are trimmed AFTER the last entity, so no offset moves.
        json!({
            "chat_id": t.chat_id,
            "message_thread_id": t.thread_id,
            "text": self.text.trim_end_matches('\n'),
            "entities": self.entities,
            "link_preview_options": {"is_disabled": true},
        })
    }
}

/// A new pending signup, as announced.
pub struct NewSignup<'a> {
    pub id: Uuid,
    pub name: &'a str,
    pub email: &'a str,
    pub company: Option<&'a str>,
    pub message: Option<&'a str>,
    pub source: &'a str,
    pub locale: Option<&'a str>,
    pub pending: i64,
}

/// A decision, as announced.
pub struct Decided<'a> {
    pub id: Uuid,
    pub approved: bool,
    pub name: &'a str,
    pub email: &'a str,
    /// `Telegram (<user id>)` or `Agent signups page`.
    pub via: &'a str,
    pub tenant: Option<(Uuid, &'a str)>,
    /// Provisioning steps that are not ok/skipped: (step, status, code).
    pub not_ok: &'a [(String, String, Option<String>)],
    /// How the requester was told (crate::signup::provisioning::email_note); no personal data.
    pub email_note: Option<&'a str>,
}

pub const NEW_SIGNUP_HEADLINE: &str = "New Agent early-access signup";

/// The `sendMessage` payload for a new signup; `None` while Telegram is off.
pub fn new_signup_payload(cfg: &SignupConfig, n: &NewSignup<'_>) -> Option<Value> {
    let t = cfg.telegram.as_ref()?;
    let mut m = Message::default();
    m.plain(&format!("{NEW_SIGNUP_HEADLINE}\n\n"))
        .field("Name", Some(n.name), FIELD_MAX)
        .field("Email", Some(n.email), FIELD_MAX)
        .field("Company", n.company, FIELD_MAX)
        .plain(&format!("Source: {}\n", n.source))
        .plain(&format!("Language: {}\n", n.locale.unwrap_or("— (en)")))
        .plain("Trusted via: website/chat intake (service account)\n");
    if let Some(msg) = n.message.filter(|m| !single_line(m).is_empty()) {
        m.field("Message", Some(msg), MESSAGE_EXCERPT_MAX);
    }
    m.plain(&format!("Pending signups: {}\n", n.pending))
        .plain(&format!("Signup id: {}\n\n", n.id))
        .plain(
            "Approve creates the workspace now (suggested name, default plan) and emails the set-password \
             link once it is ready. Open in Agent to choose a name first.",
        );
    let mut p = m.payload(t);
    p["reply_markup"] = json!({"inline_keyboard": [
        [
            {"text": "Approve", "callback_data": callback_data(&cfg.app_key, n.id, true)},
            {"text": "Reject", "callback_data": callback_data(&cfg.app_key, n.id, false)},
        ],
        [{"text": "Open in Agent", "url": deep_link(cfg, n.id)}],
    ]});
    Some(p)
}

/// The `sendMessage` payload for a decision; `None` while Telegram is off.
pub fn decision_payload(cfg: &SignupConfig, d: &Decided<'_>) -> Option<Value> {
    let t = cfg.telegram.as_ref()?;
    let mut m = Message::default();
    m.plain(&format!(
        "Agent signup {}\n\n",
        if d.approved { "approved" } else { "rejected" }
    ))
    .field("Name", Some(d.name), FIELD_MAX)
    .field("Email", Some(d.email), FIELD_MAX)
    .plain(&format!("Decided via: {}\n", d.via));
    if d.approved {
        if let Some((id, name)) = d.tenant {
            m.field("Workspace", Some(name), FIELD_MAX)
                .plain(&format!("Tenant id: {id}\n"));
        }
        if d.not_ok.is_empty() {
            m.plain(&format!(
                "Provisioning: all steps ok; {}.\n",
                d.email_note.unwrap_or("email step done")
            ));
        } else {
            m.plain("Provisioning steps NOT ok (retried automatically):\n");
            for (step, status, code) in d.not_ok {
                let code = code
                    .as_deref()
                    .map(|c| format!(" ({c})"))
                    .unwrap_or_default();
                m.plain(&format!("  - {step}: {status}{code}\n"));
            }
            // a step after the email (`app_licences`) may still be pending: how the requester was told still shows
            if let Some(note) = d.email_note {
                m.plain(&format!("Email: {note}.\n"));
            }
        }
    }
    m.plain(&format!("Signup id: {}", d.id));
    Some(m.payload(t))
}

/// Send `payload` from a detached task ([`deliver`]). Never returns an error. No-op while Telegram is off.
pub fn send(
    cfg: &SignupConfig,
    http: &reqwest::Client,
    payload: Option<Value>,
    kind: &'static str,
) {
    let (Some(t), Some(payload)) = (cfg.telegram.as_ref(), payload) else {
        return;
    };
    let url = format!("{}/bot{}/sendMessage", t.api_base, t.bot_token);
    let http = http.clone();
    tokio::spawn(async move {
        deliver(&http, &url, &payload, kind).await;
    });
}

/// Post `payload` (awaited, for the renotify command): `false` while Telegram is off or when it was not delivered.
pub async fn send_now(
    cfg: &SignupConfig,
    http: &reqwest::Client,
    payload: Option<Value>,
    kind: &'static str,
) -> bool {
    let (Some(t), Some(payload)) = (cfg.telegram.as_ref(), payload) else {
        return false;
    };
    let url = format!("{}/bot{}/sendMessage", t.api_base, t.bot_token);
    deliver(http, &url, &payload, kind).await
}

/// At most [`MAX_ATTEMPTS`], `retry_after` honoured on 429 (capped), no retry on any other 4xx; `true` once
/// delivered. Every failure is logged without the URL (it carries the bot token).
async fn deliver(http: &reqwest::Client, url: &str, payload: &Value, kind: &'static str) -> bool {
    for attempt in 0..MAX_ATTEMPTS {
        let mut wait = None;
        match http
            .post(url)
            .json(payload)
            .timeout(SEND_TIMEOUT)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {
                tracing::info!(kind, outcome = "ok", "signup_telegram_send");
                return true;
            }
            Ok(r) if r.status() == StatusCode::TOO_MANY_REQUESTS => {
                let body: Option<Value> = r.json().await.ok();
                wait = body
                    .as_ref()
                    .and_then(|b| b["parameters"]["retry_after"].as_u64())
                    .map(|s| Duration::from_secs(s).min(MAX_RETRY_AFTER));
            }
            Ok(r) if r.status().is_client_error() => {
                tracing::warn!(
                    kind,
                    status = r.status().as_u16(),
                    outcome = "failed",
                    "signup_telegram_send refused; not retried (check chat/thread id and bot membership)"
                );
                return false;
            }
            Ok(r) => {
                tracing::warn!(
                    kind,
                    status = r.status().as_u16(),
                    attempt,
                    "signup_telegram_send failed"
                );
            }
            // The error text carries the URL, which carries the bot token: never format it.
            Err(e) => {
                tracing::warn!(
                    kind,
                    timeout = e.is_timeout(),
                    attempt,
                    "signup_telegram_send failed"
                );
            }
        }
        let Some(backoff) = BACKOFF.get(attempt) else {
            break;
        };
        tokio::time::sleep(wait.unwrap_or(*backoff)).await;
    }
    tracing::warn!(kind, outcome = "failed", "signup_telegram_send gave up");
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_data_is_c1_and_fits_telegrams_64_bytes() {
        let id = Uuid::parse_str("6f1c2a3b-4d5e-4f60-8a7b-9c0d1e2f3a4b").unwrap();
        let a = callback_data("agent", id, true);
        assert_eq!(a, "ea1:agent:a:6f1c2a3b-4d5e-4f60-8a7b-9c0d1e2f3a4b");
        assert_eq!(
            callback_data("agent", id, false),
            "ea1:agent:r:6f1c2a3b-4d5e-4f60-8a7b-9c0d1e2f3a4b"
        );
        assert!(a.len() <= 64);
    }

    #[test]
    fn entities_wrap_requester_text_with_utf16_offsets() {
        let mut m = Message::default();
        m.plain("Ü😀\n")
            .field("Name", Some("Ann\nEvil.example/x"), 200);
        // "Ü😀\n" = 1 + 2 + 1 UTF-16 units; "Name: " = 6
        let e = &m.entities[0];
        assert_eq!(e["offset"], 10);
        assert_eq!(e["length"], "Ann Evil.example/x".encode_utf16().count());
        assert!(
            m.text.contains("Name: Ann Evil.example/x\n"),
            "one line: {}",
            m.text
        );
        assert_eq!(excerpt(&"x".repeat(301), 300).chars().count(), 301);
        assert!(excerpt(&"x".repeat(301), 300).ends_with('…'));
    }

    /// Send-only (C1): this crate never CLAIMS the bot's updates. Scans the WHOLE crate (sources, tests, build
    /// script, manifest, migrations; not `target`), case-insensitively. The method names are assembled at runtime so
    /// this test does not match itself.
    ///
    /// Claiming vs reading (owner ruling 2026-10-23): registering, removing or polling for updates takes the bot's
    /// updates away from whoever else serves it, so those three names are forbidden in every file. The webhook-info
    /// call only reads the registration Telegram already holds and claims nothing; tenant channel bots need it for
    /// webhook health (B7 T15b), so it is allowed in exactly one file, the channel connector
    /// (`src/channels/telegram.rs`, an exact path, not a directory), and forbidden everywhere else, prose included.
    #[test]
    fn the_crate_never_calls_the_update_methods() {
        let claiming = [
            ["set", "webhook"].concat(),
            ["delete", "webhook"].concat(),
            ["get", "updates"].concat(),
        ];
        let reading = ["get", "webhookinfo"].concat();
        let reading_allowed_in = std::path::Path::new("src/channels/telegram.rs");
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let mut stack = vec![root.clone()];
        let mut files = Vec::new();
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let p = entry.unwrap().path();
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
                if p.is_dir() {
                    if name != "target" && !name.starts_with('.') {
                        stack.push(p);
                    }
                } else if p
                    .extension()
                    .is_some_and(|e| e == "rs" || e == "toml" || e == "sql")
                {
                    let text = std::fs::read_to_string(&p).unwrap().to_lowercase();
                    for f in &claiming {
                        assert!(!text.contains(f.as_str()), "{} names {f}", p.display());
                    }
                    if p != root.join(reading_allowed_in) {
                        assert!(
                            !text.contains(reading.as_str()),
                            "{} names {reading} (allowed only in {})",
                            p.display(),
                            reading_allowed_in.display()
                        );
                    }
                    files.push(p);
                }
            }
        }
        // the scan really covered this file, the routes, the integration tests and the exempted connector
        for must in [
            "src/signup/telegram.rs",
            "src/routes/signups.rs",
            "tests/signups.rs",
            "src/channels/telegram.rs",
        ] {
            assert!(files.contains(&root.join(must)), "the scan missed {must}");
        }
        // the exemption is used (the connector really makes the read), so it is not a stale hole
        let connector = std::fs::read_to_string(root.join(reading_allowed_in))
            .unwrap()
            .to_lowercase();
        assert!(
            connector.contains(reading.as_str()),
            "{} no longer makes the read: drop the exemption",
            reading_allowed_in.display()
        );
        // control: the matcher finds a forbidden name when one is present
        let probe = format!("POST /bot<t>/{}", ["set", "Webhook"].concat()).to_lowercase();
        assert!(claiming.iter().any(|f| probe.contains(f.as_str())));
        let probe = format!("POST /bot<t>/{}", ["get", "WebhookInfo"].concat()).to_lowercase();
        assert!(probe.contains(reading.as_str()));
    }
}
