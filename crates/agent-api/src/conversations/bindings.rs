//! Channel bindings (B12 §2/§4, role `integrations_manager`): which provider account (customer side) or worker target
//! (worker side) belongs to a job. A BYO credential is sealed under the tenant data key (crate::connectors::custody,
//! AAD `tenant ‖ "binding_cred" ‖ binding_id`) and never
//! returned: a binding shows `has_credential` and `credential_fpr` only. A web chat binding's `external_id` is generated
//! (`wk_` + 24 base32 characters).
//!
//! Frappe Helpdesk (B7 R9, worker side only): `settings {base_url (http(s), no path), service_user (an email; a
//! settings key may not contain "email")}`, credential `{api_key, api_secret, webhook_secret}` (all three required). Born verified like web chat: the binding only works
//! with the tenant's own Frappe API key, and agent-edge reaches the site only through its netguard policy.
//!
//! Web chat (B13 §4): `settings.display {title ≤60, greeting ≤280, colour #rrggbb}` and `settings.limits` (the
//! crate::conversations::webchat_sessions windows, each 1..1000) are checked; the binding view carries `snippet`
//! (`<script src="{WEBCHAT_PUBLIC_BASE}/webchat/widget.js" data-widget="{key}" async></script>`);
//! `rotateWebChatKey` gives it a new `wk_` key and blocks every open session of it (reason `rotated`).
//!
//! Telegram (B7 T15b, R6): customer side only. The credential is the tenant's own bot — `bot_token` (its bot id is
//! the binding's `external_id`, so the token itself proves ownership and the binding is verified at creation) and
//! `webhook_secret` (the `secret_token` the bot's webhook is registered with); `settings.allowed_user_ids` (at most
//! 50 positive Telegram user ids; empty = nobody) and `settings.test_chat_id`. `sendChannelTestMessage`
//! (`POST …/channel-bindings/{id}/test-message`) sends one fixed text through the edge's `/internal/deliver`.
//! A web chat's `settings.turnstile` is `{mode: off|platform|own, site_key?}`; `own` needs
//! a Cloudflare site key (`0x` + 10..100 of `[A-Za-z0-9_-]`) and the credential key `turnstile_secret`. The
//! credential checks run against the credential the binding ends up with (the new one, or the stored one opened).
//! A web chat's `settings.identity` (known-customer chat, [`webchat_identity::IdentitySettings`]) is checked with
//! `validate_settings` against the binding's `allowed_origins`; no other kind may carry it, and `prev_valid_until` is
//! the server's alone. Its HS256 secrets live only in the sealed credential (`identity_secret`,
//! `identity_secret_prev`, each `<kid>:<secret>`): never in a view, never accepted from a request —
//! `POST …/identity-secret[?revoke_sessions=true]` makes them (200 `{kid, secret}`, the secret shown once). A rotation
//! keeps the old secret as `identity_secret_prev` for 24 h; `revoke_sessions` blocks the identified sessions
//! (reason `identity_rotated`). A credential replaced through update keeps both identity keys.
//! `POST …/test-session {customer_ref}` (TEST web chat in HS256 mode only) signs a 120 s assertion server-side and
//! answers `{url}` of the hosted test page carrying it in the fragment: the admin tests as a known customer.
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::{get, post};
use ring::rand::{SecureRandom, SystemRandom};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{PgConnection, Row};
use uuid::Uuid;

use super::webchat_identity::{self, IdentityMode, IdentitySettings};
use super::{BINDING_KINDS, iso, store, webchat_sessions};
use crate::audit::{self, Event};
use crate::auth::Role;
use crate::connectors::custody;
use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::state::AppState;
use crate::tenancy::{JobCtx, JobMember, JobRequires, missing_role, role};

/// The largest `settings` object accepted (serialized bytes).
pub const MAX_SETTINGS_BYTES: usize = 8 * 1024;
/// The largest credential accepted (serialized bytes).
pub const MAX_CREDENTIAL_BYTES: usize = 16 * 1024;
/// The edge's public base when `WEBCHAT_PUBLIC_BASE` is unset.
pub const DEFAULT_WEBCHAT_PUBLIC_BASE: &str = "https://chat.nonagentic.ai";

/// The edge's public base (`WEBCHAT_PUBLIC_BASE`, no trailing slash).
pub fn public_base(st: &AppState) -> String {
    st.cfg
        .edge
        .webchat_public_base
        .clone()
        .unwrap_or_else(|| DEFAULT_WEBCHAT_PUBLIC_BASE.to_string())
}

/// The embed snippet of web chat key `key`.
pub fn snippet(base: &str, key: &str) -> String {
    format!(r#"<script src="{base}/webchat/widget.js" data-widget="{key}" async></script>"#)
}

/// D126: every channel-binding route (reads included, they expose settings) takes `JobMember` plus this check:
/// `tenant_admin` or `integrations_manager` (tests/roles.rs ANY_OF). 403 `missing_role` names `integrations_manager`.
pub fn require_channel_admin(j: &JobCtx) -> ApiResult<()> {
    if j.t.effective.has(Role::TenantAdmin) || j.t.effective.has(Role::IntegrationsManager) {
        Ok(())
    } else {
        Err(missing_role(Role::IntegrationsManager))
    }
}

fn body_of<T: serde::de::DeserializeOwned>(raw: Value) -> ApiResult<T> {
    serde_json::from_value(raw).map_err(|e| {
        ApiError::BadRequest(
            "invalid_body",
            format!("The request body is not valid: {e}"),
        )
    })
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/jobs/{job}/channel-bindings",
            get(channel_bindings).post(create_channel_binding),
        )
        .route(
            "/api/jobs/{job}/channel-bindings/{id}",
            axum::routing::put(update_channel_binding).delete(delete_channel_binding),
        )
        .route(
            "/api/jobs/{job}/channel-bindings/{id}/rotate-key",
            post(rotate_web_chat_key),
        )
        .route(
            "/api/jobs/{job}/channel-bindings/{id}/identity-secret",
            post(rotate_identity_secret),
        )
        .route(
            "/api/jobs/{job}/channel-bindings/{id}/test-message",
            post(send_test_message),
        )
        .route(
            "/api/jobs/{job}/channel-bindings/{id}/test-session",
            post(create_test_session),
        )
        .route(
            "/api/jobs/{job}/channel-bindings/{id}/publish",
            post(publish_channel_binding),
        )
        .route(
            "/api/jobs/{job}/channel-bindings/{id}/unpublish",
            post(unpublish_channel_binding),
        )
}

const B32: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

/// A fresh web chat key: `wk_` + 24 lowercase base32 characters (120 random bits).
pub fn web_chat_key() -> ApiResult<String> {
    let mut b = [0u8; 15];
    SystemRandom::new()
        .fill(&mut b)
        .map_err(|_| ApiError::Internal(anyhow::anyhow!("no randomness for a web chat key")))?;
    let mut bits: u64 = 0;
    let mut n = 0;
    let mut out = String::from("wk_");
    for byte in b {
        bits = (bits << 8) | u64::from(byte);
        n += 8;
        while n >= 5 {
            n -= 5;
            out.push(B32[((bits >> n) & 31) as usize] as char);
        }
    }
    Ok(out)
}

fn invalid(code: &'static str, msg: impl Into<String>) -> ApiError {
    ApiError::Unprocessable(code, msg.into())
}

fn str_of<'a>(settings: &'a Value, k: &str) -> Option<&'a str> {
    settings
        .get(k)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// The non-secret settings of a binding, checked for its kind and side.
pub fn check_settings(kind: &str, side: &str, settings: &Value) -> ApiResult<()> {
    if !settings.is_object() {
        return Err(invalid("invalid_settings", "settings must be an object"));
    }
    if serde_json::to_vec(settings).map_or(usize::MAX, |v| v.len()) > MAX_SETTINGS_BYTES {
        return Err(invalid("invalid_settings", "settings are too large"));
    }
    if let Some(field) = secret_field(settings) {
        return Err(invalid(
            "secret_in_settings",
            format!("settings.{field} looks like a secret; send it as the credential"),
        ));
    }
    if kind != "web_chat" && settings.get("identity").is_some() {
        return Err(invalid(
            "invalid_settings",
            "only a web chat binding has identity settings",
        ));
    }
    super::attachment_settings::check(kind, settings)?;
    match (kind, side) {
        ("web_chat", _) => {
            if let Some(t) = settings.get("turnstile") {
                check_turnstile(t)?;
            }
            if let Some(o) = settings.get("allowed_origins") {
                let ok = o.as_array().is_some_and(|a| {
                    a.len() <= 20
                        && a.iter().all(|x| {
                            x.as_str().is_some_and(|s| {
                                s.starts_with("https://") || s.starts_with("http://localhost")
                            })
                        })
                });
                if !ok {
                    return Err(invalid(
                        "invalid_settings",
                        "allowed_origins is a list of at most 20 https origins",
                    ));
                }
            }
            if let Some(d) = settings.get("display") {
                check_display(d)?;
            }
            if let Some(l) = settings.get("limits") {
                check_limits(l)?;
            }
            if let Some(i) = settings.get("identity") {
                check_identity(i, settings)?;
            }
        }
        ("telegram", _) => check_telegram_settings(settings)?,
        ("helpdesk", "worker") => {
            let url = str_of(settings, "base_url")
                .ok_or_else(|| invalid("invalid_settings", "a Helpdesk target needs base_url"))?;
            check_helpdesk_base_url(url)?;
            str_of(settings, "service_user")
                .and_then(crate::channels::workers::email_like)
                .ok_or_else(|| {
                    invalid(
                        "invalid_settings",
                        "a Helpdesk target needs service_user: an email, the requester when the customer has none",
                    )
                })?;
        }
        _ => {}
    }
    Ok(())
}

/// A web chat's `settings.turnstile.mode` (`None` when absent = off).
pub fn turnstile_mode(settings: &Value) -> Option<&str> {
    settings
        .get("turnstile")
        .and_then(|t| t.get("mode"))
        .and_then(Value::as_str)
}

/// A web chat's `settings.identity`: an [`IdentitySettings`] that `validate_settings` accepts against the binding's
/// `allowed_origins` (422 `invalid_settings`, with its message).
fn check_identity(i: &Value, settings: &Value) -> ApiResult<()> {
    let id: IdentitySettings = serde_json::from_value(i.clone()).map_err(|_| {
        invalid(
            "invalid_settings",
            "identity: mode is off, hs256 or es256, with issuer, sign_in_url, session_hours and jwks",
        )
    })?;
    let origins: Vec<String> = settings
        .get("allowed_origins")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    webchat_identity::validate_settings(&id, &origins)
        .map_err(|m| invalid("invalid_settings", format!("identity: {m}")))
}

/// A web chat `settings.turnstile`: `mode` (`off` | `platform` | `own`) and `site_key`, nothing else; `own` needs a
/// Cloudflare site key, the other modes ignore it.
fn check_turnstile(t: &Value) -> ApiResult<()> {
    let m = t
        .as_object()
        .ok_or_else(|| invalid("invalid_settings", "turnstile must be an object"))?;
    if let Some(k) = m
        .keys()
        .find(|k| !matches!(k.as_str(), "mode" | "site_key"))
    {
        return Err(invalid(
            "invalid_settings",
            format!("turnstile.{k}: turnstile takes mode and site_key"),
        ));
    }
    match m.get("mode").and_then(Value::as_str) {
        Some("off" | "platform") => Ok(()),
        Some("own")
            if m.get("site_key")
                .and_then(Value::as_str)
                .is_some_and(is_site_key) =>
        {
            Ok(())
        }
        Some("own") => Err(invalid(
            "invalid_settings",
            "turnstile mode own needs site_key: 0x followed by 10 to 100 letters, digits, _ or -",
        )),
        _ => Err(invalid(
            "invalid_settings",
            "turnstile.mode is off, platform or own",
        )),
    }
}

/// A Cloudflare Turnstile site key: `^0x[A-Za-z0-9_-]{10,100}$`.
pub fn is_site_key(s: &str) -> bool {
    s.strip_prefix("0x").is_some_and(|r| {
        (10..=100).contains(&r.len())
            && r.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    })
}

/// Whether a binding with these settings needs credential fields ([`check_required_credential`]).
fn needs_credential(kind: &str, settings: &Value) -> bool {
    (kind == "web_chat" && turnstile_mode(settings) == Some("own")) || kind == "helpdesk"
}

/// The credential fields a binding's settings require, checked against `cred` (the credential the binding will
/// hold): a web chat with Turnstile mode `own` its `turnstile_secret`, a Helpdesk target its Frappe API key
/// and webhook secret ([`check_helpdesk_credential`]). 422 `invalid_credential`, naming the fields, never their values.
pub fn check_required_credential(
    kind: &str,
    _side: &str,
    settings: &Value,
    cred: Option<&serde_json::Map<String, Value>>,
) -> ApiResult<()> {
    let field = |k: &str| {
        cred.and_then(|m| m.get(k))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    if kind == "web_chat"
        && turnstile_mode(settings) == Some("own")
        && field("turnstile_secret").is_none()
    {
        return Err(invalid(
            "invalid_credential",
            "turnstile mode own needs credential turnstile_secret",
        ));
    }
    if kind == "helpdesk" {
        check_helpdesk_credential(cred.cloned().map(Value::Object).as_ref())?;
    }
    Ok(())
}

/// A web chat `settings.display`: `title` (≤60), `greeting` (≤280), `colour` (`#rrggbb`), nothing else.
fn check_display(d: &Value) -> ApiResult<()> {
    let m = d
        .as_object()
        .ok_or_else(|| invalid("invalid_settings", "display must be an object"))?;
    for (k, v) in m {
        let ok = match k.as_str() {
            "title" => v.as_str().is_some_and(|s| s.chars().count() <= 60),
            "greeting" => v.as_str().is_some_and(|s| s.chars().count() <= 280),
            "colour" => v.as_str().is_some_and(|s| {
                s.len() == 7 && s.starts_with('#') && s[1..].bytes().all(|b| b.is_ascii_hexdigit())
            }),
            _ => false,
        };
        if !ok {
            return Err(invalid(
                "invalid_settings",
                format!(
                    "display.{k}: display takes title (at most 60), greeting (at most 280) and colour (#rrggbb)"
                ),
            ));
        }
    }
    Ok(())
}

/// A web chat `settings.limits`: only the [`webchat_sessions::LIMIT_KEYS`], each an integer 1..1000.
fn check_limits(l: &Value) -> ApiResult<()> {
    let m = l
        .as_object()
        .ok_or_else(|| invalid("invalid_settings", "limits must be an object"))?;
    for (k, v) in m {
        let ok = webchat_sessions::LIMIT_KEYS.contains(&k.as_str())
            && v.as_i64()
                .is_some_and(|n| webchat_sessions::LIMIT_RANGE.contains(&n));
        if !ok {
            return Err(invalid(
                "invalid_settings",
                format!(
                    "limits.{k}: limits takes {:?}, each an integer from 1 to 1000",
                    webchat_sessions::LIMIT_KEYS
                ),
            ));
        }
    }
    Ok(())
}

/// A Telegram binding's settings: `allowed_user_ids` (at most 50 positive integers), `test_chat_id` (a non-zero
/// integer: a user id, or a negative group id) and `test_thread_id` (a positive forum topic id).
fn check_telegram_settings(settings: &Value) -> ApiResult<()> {
    use crate::channels::telegram::MAX_ALLOWED_USERS;
    if let Some(ids) = settings.get("allowed_user_ids") {
        let ok = ids.as_array().is_some_and(|a| {
            a.len() <= MAX_ALLOWED_USERS && a.iter().all(|x| x.as_i64().is_some_and(|n| n > 0))
        });
        if !ok {
            return Err(invalid(
                "invalid_settings",
                "allowed_user_ids is a list of at most 50 Telegram user ids (positive integers)",
            ));
        }
    }
    if let Some(c) = settings.get("test_chat_id")
        && c.as_i64().is_none_or(|n| n == 0)
    {
        return Err(invalid(
            "invalid_settings",
            "test_chat_id is a Telegram chat id (a non-zero integer)",
        ));
    }
    if let Some(t) = settings.get("test_thread_id")
        && t.as_i64().is_none_or(|n| n <= 0)
    {
        return Err(invalid(
            "invalid_settings",
            "test_thread_id is a forum topic's message_thread_id (a positive integer)",
        ));
    }
    Ok(())
}

/// A Telegram credential: a token-shaped `bot_token` and a `webhook_secret` of 16 to 256 `A-Z a-z 0-9 _ -`; the bot
/// id of the token, which must be `external_id` when one is given.
pub fn check_telegram_credential(
    cred: Option<&Value>,
    external_id: Option<&str>,
) -> ApiResult<String> {
    use crate::channels::telegram::{bot_id_of, valid_webhook_secret};
    let field = |k: &str| {
        cred.and_then(|c| c.get(k))
            .and_then(Value::as_str)
            .map(str::trim)
    };
    let bot_id = field("bot_token").and_then(bot_id_of).ok_or_else(|| {
        invalid(
            "invalid_credential",
            "a Telegram binding needs credential.bot_token (the token @BotFather issued)",
        )
    })?;
    if field("webhook_secret").is_none_or(|s| !valid_webhook_secret(s)) {
        return Err(invalid(
            "invalid_credential",
            "a Telegram binding needs credential.webhook_secret: 16 to 256 of A-Z, a-z, 0-9, _ and -",
        ));
    }
    if external_id.is_some_and(|e| !e.trim().is_empty() && e.trim() != bot_id) {
        return Err(invalid(
            "invalid_credential",
            "the bot token belongs to another bot than external_id",
        ));
    }
    Ok(bot_id.to_string())
}

/// The first settings key (dotted path) that names a secret (`token`, `password`, `client_secret`, ...).
fn secret_field(v: &Value) -> Option<String> {
    fn walk(path: &str, v: &Value) -> Option<String> {
        match v {
            Value::Object(m) => m.iter().find_map(|(k, x)| {
                let p = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                };
                if crate::audit::scrub::is_secret_key(k) && !x.is_null() {
                    Some(p)
                } else {
                    walk(&p, x)
                }
            }),
            Value::Array(a) => a.iter().find_map(|x| walk(path, x)),
            _ => None,
        }
    }
    walk("", v)
}

/// A Frappe site's base URL: `http(s)://host[:port]`, no credentials, path, query or fragment. Where it may point is
/// agent-edge's netguard policy (`CONNECTOR_PRIVATE_TARGETS` admits an in-cluster site per tenant).
pub fn check_helpdesk_base_url(url: &str) -> ApiResult<()> {
    let bad = || {
        invalid(
            "invalid_settings",
            "base_url is http(s)://host[:port] with nothing after it",
        )
    };
    let u = reqwest::Url::parse(url.trim()).map_err(|_| bad())?;
    let ok = matches!(u.scheme(), "http" | "https")
        && u.host_str().is_some_and(|h| !h.is_empty())
        && u.username().is_empty()
        && u.password().is_none()
        && matches!(u.path(), "" | "/")
        && u.query().is_none()
        && u.fragment().is_none();
    if ok { Ok(()) } else { Err(bad()) }
}

/// Shortest accepted Helpdesk webhook secret (the Frappe Webhook's HMAC key).
const HELPDESK_MIN_WEBHOOK_SECRET: usize = 16;

/// The credential a Helpdesk binding needs: `api_key`, `api_secret` non-empty, and a `webhook_secret` of at least
/// [`HELPDESK_MIN_WEBHOOK_SECRET`] characters (it alone authenticates Frappe's inbound webhooks).
fn check_helpdesk_credential(cred: Option<&Value>) -> ApiResult<()> {
    let ok = cred.and_then(Value::as_object).is_some_and(|m| {
        let s = |k: &str| m.get(k).and_then(Value::as_str).map(str::trim);
        ["api_key", "api_secret"]
            .iter()
            .all(|k| s(k).is_some_and(|v| !v.is_empty()))
            && s("webhook_secret").is_some_and(|v| v.chars().count() >= HELPDESK_MIN_WEBHOOK_SECRET)
    });
    if ok {
        Ok(())
    } else {
        Err(invalid(
            "invalid_credential",
            "a Helpdesk binding needs credential {api_key, api_secret, webhook_secret (at least 16 characters)}",
        ))
    }
}

/// The sealed credential keys of a web chat's identity secrets (`<kid>:<secret>`), set only by [`rotate_identity_secret`].
pub const IDENTITY_KEYS: [&str; 2] = ["identity_secret", "identity_secret_prev"];

/// The credential object of a request, checked (a non-empty object of at most [`MAX_CREDENTIAL_BYTES`]).
fn credential_fields(cred: &Value) -> ApiResult<&serde_json::Map<String, Value>> {
    let m = cred.as_object().filter(|o| !o.is_empty()).ok_or_else(|| {
        invalid(
            "invalid_credential",
            "credential must be a non-empty object",
        )
    })?;
    if serde_json::to_vec(cred).map_or(usize::MAX, |b| b.len()) > MAX_CREDENTIAL_BYTES {
        return Err(invalid("invalid_credential", "credential is too large"));
    }
    if m.keys().any(|k| IDENTITY_KEYS.contains(&k.as_str())) {
        return Err(invalid(
            "invalid_credential",
            "identity secrets are made by the identity-secret route, never sent",
        ));
    }
    Ok(m)
}

/// Seal `cred` for binding `id` of `tenant` (`conn` scoped to the tenant).
async fn seal_credential(
    conn: &mut PgConnection,
    st: &AppState,
    tenant: Uuid,
    id: Uuid,
    cred: &Value,
) -> ApiResult<custody::SealedBlob> {
    seal_map(conn, st, tenant, id, credential_fields(cred)?).await
}

/// Seal the credential `fields` (already checked) for binding `id` of `tenant`.
async fn seal_map(
    conn: &mut PgConnection,
    st: &AppState,
    tenant: Uuid,
    id: Uuid,
    fields: &serde_json::Map<String, Value>,
) -> ApiResult<custody::SealedBlob> {
    custody::seal_fields(
        conn,
        st.tenant_keyring.as_deref(),
        tenant,
        &custody::binding_aad(tenant, id),
        fields,
    )
    .await
}

/// Open the sealed credential of binding `id` (`conn` scoped to `tenant`); `None` when it has none or it does not
/// open (logged by id).
pub async fn open_credential(
    conn: &mut PgConnection,
    st: &AppState,
    tenant: Uuid,
    id: Uuid,
    sealed: (Option<Vec<u8>>, Option<Vec<u8>>, Option<i32>),
) -> Option<Value> {
    let (Some(enc), Some(nonce), Some(dek_version)) = sealed else {
        return None;
    };
    match custody::open_fields(
        conn,
        st.tenant_keyring.as_deref(),
        tenant,
        &custody::binding_aad(tenant, id),
        &enc,
        &nonce,
        dek_version,
    )
    .await
    {
        Ok(m) => Some(Value::Object(m)),
        Err(e) => {
            tracing::warn!(binding = %id, code = e.code(), "binding_credential_does_not_open");
            None
        }
    }
}

const COLS: &str = "b.id, b.kind, b.side, b.external_id, COALESCE(b.label, '') AS label, b.settings, b.enabled, \
     b.secret_enc IS NOT NULL AS has_credential, b.secret_fpr, b.verified_at, b.created_by, \
     b.created_at, b.updated_at, b.env, w.status AS webhook_status, w.last_update_at AS webhook_last_update_at, \
     w.last_error AS webhook_last_error, w.last_error_at AS webhook_last_error_at, \
     w.observed_at AS webhook_observed_at, w.pending_update_count AS webhook_pending_update_count, \
     w.webhook_down_reason AS webhook_down_reason";

/// A binding with its webhook health (B7 T15b, `channel_webhook_status`, 0153 / 0155), if any.
const FROM: &str = "connector_bindings b LEFT JOIN channel_webhook_status w ON w.binding_id = b.id";

/// The binding view's `webhook` (B7 T15b): what agent-edge last reported for the binding, or `null` when it has
/// reported nothing (shown as "unknown", never faked). `pending_update_count` is Telegram's own count (`null` until
/// an edge has read it); `down_reason` (`url_unset` | `url_mismatch` | `webhook_error`) is set exactly when the status
/// is `down` (0155).
pub fn webhook_view(
    status: Option<String>,
    last_update_at: Option<chrono::DateTime<chrono::Utc>>,
    pending_update_count: Option<i32>,
    down_reason: Option<String>,
    last_error: Option<String>,
    last_error_at: Option<chrono::DateTime<chrono::Utc>>,
    observed_at: Option<chrono::DateTime<chrono::Utc>>,
) -> Value {
    match (status, observed_at) {
        (Some(status), Some(observed_at)) => json!({
            "status": status,
            "last_update_at": last_update_at.map(iso),
            "pending_update_count": pending_update_count,
            "down_reason": down_reason,
            "last_error": last_error,
            "last_error_at": last_error_at.map(iso),
            "observed_at": iso(observed_at),
        }),
        _ => Value::Null,
    }
}

fn view(r: &sqlx::postgres::PgRow, base: &str) -> Value {
    let kind: String = r.get("kind");
    let embed = (kind == "web_chat").then(|| snippet(base, &r.get::<String, _>("external_id")));
    json!({
        "id": r.get::<Uuid, _>("id"),
        "kind": r.get::<String, _>("kind"),
        "side": r.get::<String, _>("side"),
        "env": r.get::<String, _>("env"),
        "external_id": r.get::<String, _>("external_id"),
        "label": r.get::<String, _>("label"),
        "settings": r.get::<Value, _>("settings"),
        "enabled": r.get::<bool, _>("enabled"),
        "has_credential": r.get::<bool, _>("has_credential"),
        "credential_fpr": r.get::<Option<String>, _>("secret_fpr"),
        // ownership (B12 review #8): an unverified binding carries no traffic until `verify <code>` arrives from it
        "verified": r.get::<Option<chrono::DateTime<chrono::Utc>>, _>("verified_at").is_some(),
        "verified_at": r.get::<Option<chrono::DateTime<chrono::Utc>>, _>("verified_at").map(iso),
        "created_by": r.get::<Option<String>, _>("created_by"),
        "created_at": iso(r.get("created_at")),
        "updated_at": iso(r.get("updated_at")),
        "snippet": embed,
        "webhook": webhook_view(
            r.get("webhook_status"),
            r.get("webhook_last_update_at"),
            r.get("webhook_pending_update_count"),
            r.get("webhook_down_reason"),
            r.get("webhook_last_error"),
            r.get("webhook_last_error_at"),
            r.get("webhook_observed_at"),
        ),
    })
}

/// D130: the env a new binding is created in.
fn creation_env(side: &str) -> &'static str {
    if side == "customer" { "test" } else { "prod" }
}

fn binding_not_found() -> ApiError {
    ApiError::Missing(
        "binding_not_found",
        "No such channel binding in this job.".into(),
    )
}

async fn one(conn: &mut PgConnection, st: &AppState, j: &JobCtx, id: Uuid) -> ApiResult<Value> {
    let r = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM {FROM} WHERE b.id = $1 AND b.job_id = $2"
    )))
    .bind(id)
    .bind(j.job_id)
    .fetch_optional(conn)
    .await?
    .ok_or_else(binding_not_found)?;
    Ok(view(&r, &public_base(st)))
}

/// A unique violation on `(kind, side, external_id)` is 409 `binding_unavailable`: the same answer whether this
/// tenant or another one holds the account, so a probe learns nothing about the owner.
fn exists(e: sqlx::Error) -> ApiError {
    if e.as_database_error().and_then(|d| d.code()).as_deref() == Some("23505") {
        ApiError::Conflict(
            "binding_unavailable",
            "That account or target cannot be bound.".into(),
        )
    } else {
        e.into()
    }
}

async fn channel_bindings(
    JobMember(j): JobMember,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    require_channel_admin(&j)?;
    let mut tx = j.begin(&st.db).await?;
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM {FROM} WHERE b.job_id = $1 ORDER BY b.created_at, b.id"
    )))
    .bind(j.job_id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let base = public_base(&st);
    Ok(Json(crate::page::paged(
        rows.iter().map(|r| view(r, &base)).collect(),
        None,
    )))
}

#[derive(Deserialize)]
struct CreateBody {
    kind: String,
    side: String,
    #[serde(default)]
    external_id: Option<String>,
    #[serde(default)]
    label: String,
    #[serde(default)]
    settings: Option<Value>,
    #[serde(default)]
    credential: Option<Value>,
}

fn check_label(label: &str) -> ApiResult<String> {
    let l = label.trim();
    if l.is_empty() || l.chars().count() > 120 {
        return Err(invalid("label_required", "label is 1 to 120 characters"));
    }
    Ok(l.to_string())
}

async fn create_channel_binding(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiJson(raw): ApiJson<Value>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    require_channel_admin(&j)?;
    let b: CreateBody = body_of(raw)?;
    if !BINDING_KINDS.contains(&b.kind.as_str()) {
        return Err(invalid(
            "invalid_kind",
            format!("kind is one of {BINDING_KINDS:?}"),
        ));
    }
    if !matches!(b.side.as_str(), "customer" | "worker") {
        return Err(invalid("invalid_side", "side is customer or worker"));
    }
    if b.kind == "web_chat" && b.side == "worker" {
        return Err(invalid("invalid_side", "web chat has no worker side"));
    }
    if b.kind == "telegram" && b.side == "worker" {
        return Err(invalid("invalid_side", "Telegram has no worker side"));
    }
    if b.kind == "helpdesk" && b.side != "worker" {
        return Err(invalid("invalid_side", "Helpdesk has no customer side"));
    }
    store::require_clean([
        b.label.as_str(),
        b.external_id.as_deref().unwrap_or_default(),
    ])?;
    for v in [b.settings.as_ref(), b.credential.as_ref()]
        .into_iter()
        .flatten()
    {
        store::require_clean_json(v)?;
    }
    let label = check_label(&b.label)?;
    let settings = b.settings.unwrap_or_else(|| json!({}));
    check_settings(&b.kind, &b.side, &settings)?;
    if b.kind == "helpdesk" {
        check_helpdesk_credential(b.credential.as_ref())?;
    }
    let external_id = if b.kind == "web_chat" {
        web_chat_key()?
    } else if b.kind == "telegram" {
        // the bot id of the token: holding the token is the ownership proof (no `verify <code>` round)
        check_telegram_credential(b.credential.as_ref(), b.external_id.as_deref())?
    } else {
        let e = b.external_id.as_deref().map(str::trim).unwrap_or_default();
        if e.is_empty() || e.chars().count() > 200 || e.chars().any(char::is_control) {
            return Err(invalid(
                "external_id_required",
                "external_id is 1 to 200 characters",
            ));
        }
        e.to_string()
    };
    let cred = b.credential.as_ref().and_then(Value::as_object);
    check_required_credential(&b.kind, &b.side, &settings, cred)?;
    let id = Uuid::new_v4();
    let mut tx = j.begin(&st.db).await?;
    let sealed = match b.credential.as_ref().filter(|c| !c.is_null()) {
        Some(c) => Some(seal_credential(&mut tx, &st, j.t.tenant, id, c).await?),
        None => None,
    };
    sqlx::query(
        "INSERT INTO connector_bindings \
            (id, tenant_id, job_id, env, kind, side, external_id, label, settings, \
             secret_enc, secret_nonce, secret_fpr, secret_dek_version, enabled, created_by, created_at, updated_at, \
             verified_at) \
         VALUES ($1, app_current_tenant(), $2, $13, $3, $4, $5, $6, $7, $8, $9, $10, $11, true, $12, now(), now(), \
                 now())",
    )
    .bind(id)
    .bind(j.job_id)
    .bind(&b.kind)
    .bind(&b.side)
    .bind(&external_id)
    .bind(&label)
    .bind(&settings)
    .bind(sealed.as_ref().map(|s| s.ciphertext.clone()))
    .bind(sealed.as_ref().map(|s| s.nonce.clone()))
    .bind(sealed.as_ref().map(|s| s.fingerprint.clone()))
    .bind(sealed.as_ref().map(|s| s.dek_version))
    .bind(&j.t.caller.sub)
    // verified at creation: web chat keys are generated here, a Telegram bot is proven by its token, a helpdesk
    // binding works only with the tenant's own Frappe API key (and the edge's netguard policy)
    // D130: a customer-side channel starts in the test env (published once the job has a release); a worker-side
    // channel (staff notifications) is always live
    .bind(creation_env(&b.side))
    .execute(&mut *tx)
    .await
    .map_err(exists)?;
    audit::record(
        &mut tx,
        &j.t.actor(),
        Event::new("channel_binding.create", "channel_binding", id.to_string())
            .job(&j.job_key)
            .detail("kind", b.kind.clone())
            .detail("side", b.side.clone())
            .detail("has_credential", sealed.is_some()),
    )
    .await?;
    let v = one(&mut tx, &st, &j, id).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(v)))
}

#[derive(Deserialize)]
struct UpdateBody {
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    settings: Option<Value>,
    #[serde(default)]
    enabled: Option<bool>,
    /// An object replaces the credential; `null` removes it; absent keeps it.
    #[serde(default, deserialize_with = "some_value")]
    credential: Option<Value>,
}

/// Keep an explicit `null` as `Some(Value::Null)` (absent stays `None`).
fn some_value<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

async fn update_channel_binding(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiPath((_, id)): ApiPath<(String, String)>,
    ApiJson(raw): ApiJson<Value>,
) -> ApiResult<Json<Value>> {
    require_channel_admin(&j)?;
    let b: UpdateBody = body_of(raw)?;
    let id = Uuid::parse_str(&id).map_err(|_| binding_not_found())?;
    let mut tx = j.begin(&st.db).await?;
    let cur = sqlx::query(
        "SELECT kind, side, external_id, settings, secret_enc, secret_nonce, secret_dek_version \
           FROM connector_bindings WHERE id = $1 AND job_id = $2 FOR UPDATE",
    )
    .bind(id)
    .bind(j.job_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(binding_not_found)?;
    let kind: String = cur.get("kind");
    let side: String = cur.get("side");
    store::require_clean([b.label.as_deref().unwrap_or_default()])?;
    for v in [b.settings.as_ref(), b.credential.as_ref()]
        .into_iter()
        .flatten()
    {
        store::require_clean_json(v)?;
    }
    let label = b.label.as_deref().map(check_label).transpose()?;
    let mut new_settings = b.settings.clone();
    if let Some(s) = &new_settings {
        check_settings(&kind, &side, s)?;
    }
    // a new Telegram credential must be for the same bot (its id is the binding's external id)
    if kind == "telegram"
        && let Some(c) = b.credential.as_ref().filter(|c| !c.is_null())
    {
        let bot_id: String = cur.get("external_id");
        check_telegram_credential(Some(c), Some(bot_id.as_str()))?;
    }
    // what the resulting settings require of the resulting credential (new, removed, or the stored one); a
    // helpdesk binding always needs its whole credential, so it is replaced whole and never removed
    let cur_settings: Value = cur.get("settings");
    if kind == "web_chat"
        && let Some(s) = new_settings.as_mut()
    {
        pin_prev_valid_until(s, &cur_settings);
    }
    let touched = new_settings.is_some() || b.credential.is_some();
    let settings = new_settings.as_ref().unwrap_or(&cur_settings);
    if touched && needs_credential(&kind, settings) {
        let stored: Option<Value>;
        let cred = match &b.credential {
            Some(c) => c.as_object(),
            None => {
                let sealed = (
                    cur.get::<Option<Vec<u8>>, _>("secret_enc"),
                    cur.get::<Option<Vec<u8>>, _>("secret_nonce"),
                    cur.get::<Option<i32>, _>("secret_dek_version"),
                );
                stored = open_credential(&mut tx, &st, j.t.tenant, id, sealed).await;
                stored.as_ref().and_then(Value::as_object)
            }
        };
        check_required_credential(&kind, &side, settings, cred)?;
    }
    // a credential replaced or removed keeps the web chat's identity secrets: only their route makes or drops them
    let kept = if kind == "web_chat" && b.credential.is_some() {
        let sealed = (
            cur.get::<Option<Vec<u8>>, _>("secret_enc"),
            cur.get::<Option<Vec<u8>>, _>("secret_nonce"),
            cur.get::<Option<i32>, _>("secret_dek_version"),
        );
        match open_credential(&mut tx, &st, j.t.tenant, id, sealed).await {
            Some(Value::Object(m)) => {
                // a previous identity secret past its grace window is purged on this write (the loader already
                // ignores it)
                let prev_live = cur_settings
                    .pointer("/identity/prev_valid_until")
                    .and_then(Value::as_str)
                    .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                    .is_some_and(|t| t > chrono::Utc::now());
                m.into_iter()
                    .filter(|(k, _)| IDENTITY_KEYS.contains(&k.as_str()))
                    .filter(|(k, _)| prev_live || k != "identity_secret_prev")
                    .collect()
            }
            _ => serde_json::Map::new(),
        }
    } else {
        serde_json::Map::new()
    };
    let (set_cred, sealed) = match &b.credential {
        None => (false, None),
        Some(Value::Null) if kept.is_empty() => (true, None),
        Some(c) => {
            let mut fields = if c.is_null() {
                serde_json::Map::new()
            } else {
                credential_fields(c)?.clone()
            };
            fields.extend(kept);
            (
                true,
                Some(seal_map(&mut tx, &st, j.t.tenant, id, &fields).await?),
            )
        }
    };
    sqlx::query(
        "UPDATE connector_bindings SET \
                label = COALESCE($2, label), settings = COALESCE($3, settings), enabled = COALESCE($4, enabled), \
                secret_enc = CASE WHEN $5 THEN $6 ELSE secret_enc END, \
                secret_nonce = CASE WHEN $5 THEN $7 ELSE secret_nonce END, \
                secret_fpr = CASE WHEN $5 THEN $8 ELSE secret_fpr END, \
                secret_dek_version = CASE WHEN $5 THEN $9 ELSE secret_dek_version END, \
                updated_at = now() \
          WHERE id = $1",
    )
    .bind(id)
    .bind(label)
    .bind(&new_settings)
    .bind(b.enabled)
    .bind(set_cred)
    .bind(sealed.as_ref().map(|s| s.ciphertext.clone()))
    .bind(sealed.as_ref().map(|s| s.nonce.clone()))
    .bind(sealed.as_ref().map(|s| s.fingerprint.clone()))
    .bind(sealed.as_ref().map(|s| s.dek_version))
    .execute(&mut *tx)
    .await?;
    let mut ev = Event::new("channel_binding.update", "channel_binding", id.to_string())
        .job(&j.job_key)
        .detail("kind", kind)
        .detail("credential_changed", set_cred);
    if let Some(e) = b.enabled {
        ev = ev.detail("enabled", e);
    }
    audit::record(&mut tx, &j.t.actor(), ev).await?;
    let v = one(&mut tx, &st, &j, id).await?;
    tx.commit().await?;
    Ok(Json(v))
}

/// `prev_valid_until` is the server's alone: the new web chat settings carry the stored value (or none).
fn pin_prev_valid_until(new: &mut Value, cur: &Value) {
    let kept = cur.pointer("/identity/prev_valid_until").cloned();
    if let Some(i) = new.get_mut("identity").and_then(Value::as_object_mut) {
        match kept {
            Some(v) if !v.is_null() => i.insert("prev_valid_until".into(), v),
            _ => i.remove("prev_valid_until"),
        };
    }
}

/// How long the previous identity secret stays valid after a rotation.
const IDENTITY_PREV_GRACE_HOURS: i64 = 24;

#[derive(Deserialize)]
struct RotateIdentityQuery {
    #[serde(default)]
    revoke_sessions: bool,
}

/// The number of a `k<N>` key id (`None` for any other shape).
fn kid_number(kid: &str) -> Option<u64> {
    kid.strip_prefix('k')?.parse().ok()
}

/// `rotateIdentitySecret`: a new HS256 secret for a web chat binding in mode `hs256`, shown once (`{kid, secret}`).
/// The key id counts up (`k1`, `k2`, …). The old current secret becomes `identity_secret_prev` and stays valid for 24 h
/// (`settings.identity.prev_valid_until`); an older previous one is dropped. `?revoke_sessions=true` also blocks every
/// open identified session of the binding (reason `identity_rotated`). 422 `not_web_chat`; 409
/// `identity_mode_not_hs256`. The audit row names the actor and binding, never a secret.
async fn rotate_identity_secret(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiPath((_, id)): ApiPath<(String, String)>,
    ApiQuery(q): ApiQuery<RotateIdentityQuery>,
) -> ApiResult<Json<Value>> {
    require_channel_admin(&j)?;
    let id = Uuid::parse_str(&id).map_err(|_| binding_not_found())?;
    let mut tx = j.begin(&st.db).await?;
    let cur = sqlx::query(
        "SELECT kind, settings, secret_enc, secret_nonce, secret_dek_version \
           FROM connector_bindings WHERE id = $1 AND job_id = $2 FOR UPDATE",
    )
    .bind(id)
    .bind(j.job_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(binding_not_found)?;
    let kind: String = cur.get("kind");
    if kind != "web_chat" {
        return Err(invalid(
            "not_web_chat",
            "Only a web chat binding has an identity secret.",
        ));
    }
    let mut settings: Value = cur.get("settings");
    let hs256 = settings
        .get("identity")
        .and_then(|i| serde_json::from_value::<IdentitySettings>(i.clone()).ok())
        .is_some_and(|i| i.mode == IdentityMode::Hs256);
    if !hs256 {
        return Err(ApiError::Conflict(
            "identity_mode_not_hs256",
            "Set the identity mode to HS256 before making a secret.".into(),
        ));
    }
    let sealed = (
        cur.get::<Option<Vec<u8>>, _>("secret_enc"),
        cur.get::<Option<Vec<u8>>, _>("secret_nonce"),
        cur.get::<Option<i32>, _>("secret_dek_version"),
    );
    let has_credential = sealed.0.is_some();
    let mut fields = match open_credential(&mut tx, &st, j.t.tenant, id, sealed).await {
        Some(Value::Object(m)) => m,
        // a credential that does not open is never overwritten: the other keys in it would be lost
        _ if has_credential => {
            return Err(ApiError::Internal(anyhow::anyhow!(
                "the binding credential does not open"
            )));
        }
        _ => serde_json::Map::new(),
    };
    let current = fields
        .get("identity_secret")
        .and_then(Value::as_str)
        .map(str::to_string);
    let next = current
        .as_deref()
        .and_then(|c| c.split_once(':'))
        .and_then(|(kid, _)| kid_number(kid))
        .map_or(1, |n| n + 1);
    let kid = format!("k{next}");
    let secret = webchat_identity::generate_secret();
    fields.remove("identity_secret_prev");
    let prev_valid_until = current.map(|c| {
        fields.insert("identity_secret_prev".into(), Value::String(c));
        chrono::Utc::now() + chrono::Duration::hours(IDENTITY_PREV_GRACE_HOURS)
    });
    fields.insert(
        "identity_secret".into(),
        Value::String(format!("{kid}:{secret}")),
    );
    if let Some(i) = settings.get_mut("identity").and_then(Value::as_object_mut) {
        match prev_valid_until {
            Some(t) => i.insert("prev_valid_until".into(), json!(t)),
            None => i.remove("prev_valid_until"),
        };
    }
    let blob = seal_map(&mut tx, &st, j.t.tenant, id, &fields).await?;
    sqlx::query(
        "UPDATE connector_bindings SET settings = $2, secret_enc = $3, secret_nonce = $4, secret_fpr = $5, \
                secret_dek_version = $6, updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(&settings)
    .bind(&blob.ciphertext)
    .bind(&blob.nonce)
    .bind(&blob.fingerprint)
    .bind(blob.dek_version)
    .execute(&mut *tx)
    .await?;
    let blocked = if q.revoke_sessions {
        sqlx::query(
            "UPDATE web_chat_session SET blocked_at = now(), blocked_reason = 'identity_rotated' \
              WHERE binding_id = $1 AND identified_at IS NOT NULL AND expires_at > now() AND blocked_at IS NULL",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
    } else {
        0
    };
    audit::record(
        &mut tx,
        &j.t.actor(),
        Event::new(
            "channel_binding.identity_secret_rotate",
            "channel_binding",
            id.to_string(),
        )
        .job(&j.job_key)
        .detail("kid", kid.clone())
        .detail("sessions_blocked", blocked),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({"kid": kid, "secret": secret})))
}

/// How long a test-session assertion lives (seconds); the verifier allows at most 300.
const TEST_SESSION_TTL_S: i64 = 120;

/// `createChannelTestSession`: the tenant admin tries a TEST web chat binding as a known customer. Core signs a
/// short-lived assertion for `{customer_ref}` exactly as the tenant's own site would (HS256 with the binding's identity
/// secret, `aud` = the binding id, a fresh single-use `jti`, 120 s) and answers `{url}`: the edge's hosted test page
/// with the assertion in the URL fragment (never the query string, so it is not sent to any server or logged), which the
/// page hands to the widget's identify flow and then removes from the address bar. The secret is read the way the
/// session verifier reads it and is never returned or logged; the audit row carries ids only.
/// 404 `binding_not_found`; 422 `not_web_chat`, `invalid_customer_ref`; 409 `binding_live` (only a TEST binding),
/// `identity_not_configured` (identity off, or no secret made yet), `identity_mode_not_hs256` (core holds no private
/// ES256 key, so it cannot sign for that mode).
async fn create_test_session(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiPath((_, id)): ApiPath<(String, String)>,
    ApiJson(raw): ApiJson<Value>,
) -> ApiResult<Json<Value>> {
    require_channel_admin(&j)?;
    let id = Uuid::parse_str(&id).map_err(|_| binding_not_found())?;
    let customer_ref = raw
        .get("customer_ref")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    if !webchat_identity::sub_ok(&customer_ref) {
        return Err(invalid(
            "invalid_customer_ref",
            "The customer reference is 1 to 200 letters, digits or . _ : @ - .",
        ));
    }
    let mut tx = j.begin(&st.db).await?;
    let cur = sqlx::query(
        "SELECT kind, env, external_id, settings, secret_enc, secret_nonce, secret_dek_version \
           FROM connector_bindings WHERE id = $1 AND job_id = $2",
    )
    .bind(id)
    .bind(j.job_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(binding_not_found)?;
    if cur.get::<String, _>("kind") != "web_chat" {
        return Err(invalid(
            "not_web_chat",
            "Only a web chat binding can be tried as a customer.",
        ));
    }
    if cur.get::<String, _>("env") != "test" {
        return Err(ApiError::Conflict(
            "binding_live",
            "This channel is live. Unpublish it to test it as a customer.".into(),
        ));
    }
    let settings: Value = cur.get("settings");
    let identity: IdentitySettings = settings
        .get("identity")
        .and_then(|i| serde_json::from_value(i.clone()).ok())
        .unwrap_or_default();
    let not_configured = || {
        ApiError::Conflict(
            "identity_not_configured",
            "Set up customer identity (HS256) for this channel and make its secret first.".into(),
        )
    };
    match identity.mode {
        IdentityMode::Off => return Err(not_configured()),
        IdentityMode::Es256 => {
            return Err(ApiError::Conflict(
                "identity_mode_not_hs256",
                "Testing as a customer needs the HS256 identity mode.".into(),
            ));
        }
        IdentityMode::Hs256 => {}
    }
    let sealed = (
        cur.get::<Option<Vec<u8>>, _>("secret_enc"),
        cur.get::<Option<Vec<u8>>, _>("secret_nonce"),
        cur.get::<Option<i32>, _>("secret_dek_version"),
    );
    let key = open_credential(&mut tx, &st, j.t.tenant, id, sealed)
        .await
        .and_then(|c| {
            c.get("identity_secret")
                .and_then(Value::as_str)
                .and_then(webchat_sessions::parse_hs_key)
        })
        .ok_or_else(not_configured)?;
    let mut rnd = [0u8; 16];
    SystemRandom::new()
        .fill(&mut rnd)
        .map_err(|_| ApiError::Internal(anyhow::anyhow!("no randomness for a test session")))?;
    let assertion = webchat_identity::mint_hs256(
        &key,
        &identity.issuer,
        id,
        &customer_ref,
        None,
        None,
        chrono::Utc::now().timestamp(),
        TEST_SESSION_TTL_S,
        &hex::encode(rnd),
    );
    audit::record(
        &mut tx,
        &j.t.actor(),
        Event::new(
            "channel_binding.test_session",
            "channel_binding",
            id.to_string(),
        )
        .job(&j.job_key),
    )
    .await?;
    let external_id: String = cur.get("external_id");
    tx.commit().await?;
    Ok(Json(json!({
        "url": format!("{}/webchat/demo?key={external_id}#assertion={assertion}", public_base(&st)),
    })))
}

/// `rotateWebChatKey`: a new `wk_` key for a web chat binding; every open session of it is blocked (reason
/// `rotated`), so a widget still holding the old key must start over. 422 `not_web_chat` for any other kind.
async fn rotate_web_chat_key(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiPath((_, id)): ApiPath<(String, String)>,
) -> ApiResult<Json<Value>> {
    require_channel_admin(&j)?;
    let id = Uuid::parse_str(&id).map_err(|_| binding_not_found())?;
    let mut tx = j.begin(&st.db).await?;
    let kind: String = sqlx::query_scalar(
        "SELECT kind FROM connector_bindings WHERE id = $1 AND job_id = $2 FOR UPDATE",
    )
    .bind(id)
    .bind(j.job_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(binding_not_found)?;
    if kind != "web_chat" {
        return Err(invalid(
            "not_web_chat",
            "Only a web chat binding has a widget key to rotate.",
        ));
    }
    sqlx::query("UPDATE connector_bindings SET external_id = $2, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(web_chat_key()?)
        .execute(&mut *tx)
        .await
        .map_err(exists)?;
    let blocked = sqlx::query(
        "UPDATE web_chat_session SET blocked_at = now(), blocked_reason = 'rotated' \
          WHERE binding_id = $1 AND expires_at > now() AND blocked_reason IS DISTINCT FROM 'rotated'",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    audit::record(
        &mut tx,
        &j.t.actor(),
        Event::new(
            "channel_binding.rotate_key",
            "channel_binding",
            id.to_string(),
        )
        .job(&j.job_key)
        .detail("sessions_blocked", blocked),
    )
    .await?;
    let v = one(&mut tx, &st, &j, id).await?;
    tx.commit().await?;
    Ok(Json(v))
}

/// Where a Telegram test message goes: `settings.test_chat_id`, in the forum topic `settings.test_thread_id` when one is
/// set (the same ref shape the edge builds for a topic message); else the first allowed user's private chat, which has
/// no topics, so `test_thread_id` is ignored there. `None` when neither is configured.
fn test_conversation(settings: &Value) -> Option<crate::interaction::ConversationRef> {
    use crate::channels::telegram::THREAD_KEY;
    let (chat, thread) = match settings.get("test_chat_id").and_then(Value::as_i64) {
        Some(chat) => (
            chat,
            settings
                .get("test_thread_id")
                .and_then(Value::as_i64)
                .filter(|t| *t > 0),
        ),
        None => (
            crate::channels::telegram::allowed_user_ids(settings)
                .into_iter()
                .next()?,
            None,
        ),
    };
    let mut route = std::collections::BTreeMap::from([("chat_id".to_string(), chat.to_string())]);
    let id = match thread {
        Some(t) => {
            route.insert(THREAD_KEY.to_string(), t.to_string());
            format!("{chat}:{t}")
        }
        None => chat.to_string(),
    };
    Some(crate::interaction::ConversationRef {
        channel: "telegram".into(),
        id,
        route,
    })
}

/// `sendChannelTestMessage` (B7 T15): one fixed text to an enabled Telegram binding's test chat (`settings.test_chat_id`,
/// in topic `settings.test_thread_id` when set; else its first allowed user), sent by the edge with the binding's own
/// bot (`/internal/deliver`). 409 `channel_not_bound` for any other kind, a disabled binding or one without a
/// credential (or an edge without the Telegram connector); 409 `test_chat_not_configured`; 503 `edge_not_configured`;
/// 502 `channel_send_failed` when the edge or Telegram refused it (the edge picks a new binding up within a minute).
/// One attempt, never retried.
async fn send_test_message(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiPath((_, id)): ApiPath<(String, String)>,
) -> ApiResult<Json<Value>> {
    require_channel_admin(&j)?;
    let id = Uuid::parse_str(&id).map_err(|_| binding_not_found())?;
    let mut tx = j.begin(&st.db).await?;
    let r = sqlx::query(
        "SELECT kind, COALESCE(label, '') AS label, settings, enabled, verified_at IS NOT NULL AS verified, \
                secret_enc IS NOT NULL AS has_credential \
           FROM connector_bindings WHERE id = $1 AND job_id = $2",
    )
    .bind(id)
    .bind(j.job_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(binding_not_found)?;
    tx.commit().await?;
    let live = r.get::<String, _>("kind") == "telegram"
        && r.get::<bool, _>("enabled")
        && r.get::<bool, _>("verified")
        && r.get::<bool, _>("has_credential");
    let not_bound = || {
        ApiError::Conflict(
            "channel_not_bound",
            "Only an enabled Telegram binding with its bot credential can send a test message."
                .into(),
        )
    };
    if !live {
        return Err(not_bound());
    }
    let settings: Value = r.get("settings");
    let conversation = test_conversation(&settings).ok_or_else(|| {
        ApiError::Conflict(
            "test_chat_not_configured",
            "Set settings.test_chat_id or allowed_user_ids first.".into(),
        )
    })?;
    let Some(edge) = st.edge.clone() else {
        return Err(ApiError::Unavailable(
            "edge_not_configured",
            "The channel edge is not configured.".into(),
            None,
        ));
    };
    let label: String = r.get("label");
    let text = format!(
        "Test from Agent: {label} is connected to job {}.",
        j.job_key
    );
    let message_id = match edge
        .deliver(
            &conversation,
            super::edge_link::DeliverKind::Text,
            &text,
            None,
            Some(id),
            None,
        )
        .await
    {
        Ok(m) => m,
        Err(e) if e.code == "channel_not_configured" => return Err(not_bound()),
        Err(e) => {
            tracing::warn!(binding = %id, code = e.code, "channel_test_message_failed");
            return Err(ApiError::BadGateway(
                "channel_send_failed",
                "The test message was not delivered.".into(),
            ));
        }
    };
    let mut tx = j.begin(&st.db).await?;
    audit::record(
        &mut tx,
        &j.t.actor(),
        Event::new(
            "channel_binding.test_message",
            "channel_binding",
            id.to_string(),
        )
        .job(&j.job_key)
        .detail("kind", "telegram"),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({
        "delivered": true,
        "refused": false,
        "text": text,
        "message_id": Some(message_id).filter(|m| !m.is_empty()),
        "at": iso(chrono::Utc::now()),
    })))
}

async fn delete_channel_binding(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiPath((_, id)): ApiPath<(String, String)>,
) -> ApiResult<StatusCode> {
    require_channel_admin(&j)?;
    let id = Uuid::parse_str(&id).map_err(|_| binding_not_found())?;
    let mut tx = j.begin(&st.db).await?;
    let kind: Option<String> = sqlx::query_scalar(
        "DELETE FROM connector_bindings WHERE id = $1 AND job_id = $2 RETURNING kind",
    )
    .bind(id)
    .bind(j.job_id)
    .fetch_optional(&mut *tx)
    .await?;
    let kind = kind.ok_or_else(binding_not_found)?;
    audit::record(
        &mut tx,
        &j.t.actor(),
        Event::new("channel_binding.delete", "channel_binding", id.to_string())
            .job(&j.job_key)
            .detail("kind", kind),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// D130: move a binding to `env` (publish = `prod`, unpublish = `test`). Idempotent: a binding already there is
/// returned unchanged and not audited again.
async fn set_binding_env(
    j: JobCtx,
    st: AppState,
    id: String,
    env: &'static str,
) -> ApiResult<Json<Value>> {
    let id = Uuid::parse_str(&id).map_err(|_| binding_not_found())?;
    let mut tx = j.begin(&st.db).await?;
    let current: String = sqlx::query_scalar(
        "SELECT env FROM connector_bindings WHERE id = $1 AND job_id = $2 FOR UPDATE",
    )
    .bind(id)
    .bind(j.job_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(binding_not_found)?;
    if current != env {
        if env == "prod" {
            let released: bool =
                sqlx::query_scalar("SELECT current_release_id IS NOT NULL FROM jobs WHERE id = $1")
                    .bind(j.job_id)
                    .fetch_one(&mut *tx)
                    .await?;
            if !released {
                return Err(ApiError::Conflict(
                    "no_release",
                    "This job has no release yet — nothing is live. Apply a release, then publish the channel."
                        .into(),
                ));
            }
        }
        sqlx::query("UPDATE connector_bindings SET env = $2, updated_at = now() WHERE id = $1")
            .bind(id)
            .bind(env)
            .execute(&mut *tx)
            .await?;
        let action = if env == "prod" {
            "channel_binding.publish"
        } else {
            "channel_binding.unpublish"
        };
        audit::record(
            &mut tx,
            &j.t.actor(),
            Event::new(action, "channel_binding", id.to_string()).job(&j.job_key),
        )
        .await?;
    }
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM {FROM} WHERE b.id = $1 AND b.job_id = $2"
    )))
    .bind(id)
    .bind(j.job_id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(view(&row, &public_base(&st))))
}

async fn publish_channel_binding(
    JobRequires(j, _): JobRequires<role::ReleaseManager>,
    State(st): State<AppState>,
    ApiPath((_, id)): ApiPath<(String, String)>,
) -> ApiResult<Json<Value>> {
    set_binding_env(j, st, id, "prod").await
}

async fn unpublish_channel_binding(
    JobRequires(j, _): JobRequires<role::ReleaseManager>,
    State(st): State<AppState>,
    ApiPath((_, id)): ApiPath<(String, String)>,
) -> ApiResult<Json<Value>> {
    set_binding_env(j, st, id, "test").await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creation_env_is_test_for_customers_and_prod_for_workers() {
        assert_eq!(creation_env("customer"), "test");
        assert_eq!(creation_env("worker"), "prod");
    }

    #[test]
    fn webhook_view_is_null_without_a_row_and_the_row_with_one() {
        assert_eq!(
            webhook_view(None, None, None, None, None, None, None),
            Value::Null
        );
        let t = chrono::DateTime::from_timestamp_millis(1_760_000_000_000).unwrap();
        assert_eq!(
            webhook_view(
                Some("degraded".into()),
                Some(t),
                Some(150),
                None,
                Some("telegram_403_403".into()),
                Some(t),
                Some(t)
            ),
            json!({"status": "degraded", "last_update_at": iso(t), "pending_update_count": 150, "down_reason": null,
                   "last_error": "telegram_403_403", "last_error_at": iso(t), "observed_at": iso(t)})
        );
        assert_eq!(
            webhook_view(Some("ok".into()), None, None, None, None, None, Some(t)),
            json!({"status": "ok", "last_update_at": null, "pending_update_count": null, "down_reason": null,
                   "last_error": null, "last_error_at": null, "observed_at": iso(t)})
        );
        assert_eq!(
            webhook_view(
                Some("down".into()),
                None,
                Some(0),
                Some("url_unset".into()),
                None,
                None,
                Some(t)
            )["down_reason"],
            "url_unset"
        );
    }

    #[test]
    fn web_chat_keys_are_wk_and_24_base32() {
        let k = web_chat_key().unwrap();
        assert_eq!(k.len(), 27, "{k}");
        assert!(k.starts_with("wk_"));
        assert!(k[3..].bytes().all(|c| B32.contains(&c)));
        assert_ne!(k, web_chat_key().unwrap());
    }

    #[test]
    fn helpdesk_settings_and_credential_are_checked() {
        let ok =
            json!({"base_url": "http://frappe.example.com:8000", "service_user": "bot@hd.example"});
        assert!(check_settings("helpdesk", "worker", &ok).is_ok());
        for bad in [
            json!({"service_user": "bot@hd.example"}),
            json!({"base_url": "http://frappe.example", "service_user": "not-an-email"}),
            json!({"base_url": "ftp://frappe.example", "service_user": "bot@hd.example"}),
            json!({"base_url": "https://frappe.example/app", "service_user": "bot@hd.example"}),
            json!({"base_url": "https://u:p@frappe.example", "service_user": "bot@hd.example"}),
            json!({"base_url": "https://frappe.example?x=1", "service_user": "bot@hd.example"}),
        ] {
            assert_eq!(
                check_settings("helpdesk", "worker", &bad)
                    .unwrap_err()
                    .code(),
                "invalid_settings",
                "{bad}"
            );
        }
        // an "email" key is refused as person data: the requester setting is `service_user`
        assert_eq!(
            check_settings(
                "helpdesk",
                "worker",
                &json!({"base_url": "https://frappe.example", "service_user_email": "bot@hd.example"})
            )
            .unwrap_err()
            .code(),
            "secret_in_settings"
        );
        let full = json!({"api_key": "k", "api_secret": "s", "webhook_secret": "0123456789abcdef"});
        assert!(check_helpdesk_credential(Some(&full)).is_ok());
        for bad in [
            None,
            Some(json!({"api_key": "k", "api_secret": "s"})),
            Some(json!({"api_key": "k", "api_secret": " ", "webhook_secret": "0123456789abcdef"})),
            Some(json!({"api_key": "k", "api_secret": "s", "webhook_secret": "too-short"})),
        ] {
            assert_eq!(
                check_helpdesk_credential(bad.as_ref()).unwrap_err().code(),
                "invalid_credential"
            );
        }
    }

    #[test]
    fn settings_are_checked_per_kind_and_side() {
        assert_eq!(
            check_settings("web_chat", "customer", &json!({"token": "abc"}))
                .unwrap_err()
                .code(),
            "secret_in_settings"
        );
        assert!(
            check_settings(
                "web_chat",
                "customer",
                &json!({"allowed_origins": ["https://a.example"]})
            )
            .is_ok()
        );
    }

    #[test]
    fn telegram_settings_and_credential_are_checked() {
        let ok = json!({"allowed_user_ids": [42, 43], "test_chat_id": -100123});
        assert!(check_settings("telegram", "customer", &ok).is_ok());
        let forum = json!({"allowed_user_ids": [7619636015_i64],
            "test_chat_id": -1004368404922_i64, "test_thread_id": 17});
        assert!(check_settings("telegram", "customer", &forum).is_ok());
        assert!(check_settings("telegram", "customer", &json!({})).is_ok());
        for bad in [
            json!({"allowed_user_ids": "42"}),
            json!({"allowed_user_ids": [0]}),
            json!({"allowed_user_ids": ["42"]}),
            json!({"allowed_user_ids": (1..=51).collect::<Vec<i64>>()}),
            json!({"test_chat_id": 0}),
            json!({"test_chat_id": "42"}),
            json!({"test_thread_id": 0}),
            json!({"test_thread_id": -5}),
            json!({"test_thread_id": "17"}),
        ] {
            assert_eq!(
                check_settings("telegram", "customer", &bad)
                    .unwrap_err()
                    .code(),
                "invalid_settings",
                "{bad}"
            );
        }
        // token-shaped, not a real token
        let cred = json!({"bot_token": "123456789:test-not-a-real-bot-token-000000",
            "webhook_secret": "test-webhook-secret-0001"});
        assert_eq!(
            check_telegram_credential(Some(&cred), None).unwrap(),
            "123456789"
        );
        assert_eq!(
            check_telegram_credential(Some(&cred), Some("123456789")).unwrap(),
            "123456789"
        );
        assert_eq!(
            check_telegram_credential(Some(&cred), Some("987"))
                .unwrap_err()
                .code(),
            "invalid_credential"
        );
        let mut short = cred.clone();
        short["webhook_secret"] = json!("short");
        let mut no_token = cred.clone();
        no_token["bot_token"] = json!("not-a-token");
        for bad in [Some(&short), Some(&no_token), None] {
            assert_eq!(
                check_telegram_credential(bad, None).unwrap_err().code(),
                "invalid_credential"
            );
        }
    }

    #[test]
    fn telegram_test_message_targets_the_test_topic() {
        let c =
            test_conversation(&json!({"test_chat_id": -1004368404922_i64, "test_thread_id": 17}))
                .unwrap();
        assert_eq!(c.id, "-1004368404922:17");
        assert_eq!(c.route["chat_id"], "-1004368404922");
        assert_eq!(c.route["message_thread_id"], "17");
        let c = test_conversation(&json!({"allowed_user_ids": [42]})).unwrap();
        assert_eq!(c.id, "42");
        assert_eq!(
            c.route,
            std::collections::BTreeMap::from([("chat_id".to_string(), "42".to_string())])
        );
        // A thread id without a test chat would aim a private chat at a topic it cannot have.
        let c =
            test_conversation(&json!({"allowed_user_ids": [42], "test_thread_id": 17})).unwrap();
        assert_eq!(c.id, "42");
        assert!(!c.route.contains_key("message_thread_id"));
        assert!(test_conversation(&json!({})).is_none());
    }

    #[test]
    fn web_chat_display_and_limits_are_checked() {
        let ok = json!({
            "display": {"title": "Help", "greeting": "Hi!", "colour": "#1a2B3c"},
            "limits": {"sessions_per_ip_hour": 1, "msgs_per_ip_10m": 1000, "msgs_per_session_hour": 60,
                       "msgs_per_minute": 300},
        });
        assert!(check_settings("web_chat", "customer", &ok).is_ok());
        for bad in [
            json!({"display": {"title": "x".repeat(61)}}),
            json!({"display": {"greeting": "x".repeat(281)}}),
            json!({"display": {"colour": "red"}}),
            json!({"display": {"colour": "#12345g"}}),
            json!({"display": {"font": "serif"}}),
            json!({"display": "Help"}),
            json!({"limits": {"msgs_per_minute": 0}}),
            json!({"limits": {"msgs_per_minute": 1001}}),
            json!({"limits": {"msgs_per_minute": 1.5}}),
            json!({"limits": {"burst": 3}}),
        ] {
            assert_eq!(
                check_settings("web_chat", "customer", &bad)
                    .unwrap_err()
                    .code(),
                "invalid_settings",
                "{bad}"
            );
        }
    }

    #[test]
    fn web_chat_turnstile_is_checked() {
        let site = "0x4AAAAAAAAbcdEF_-12";
        for ok in [
            json!({"turnstile": {"mode": "off"}}),
            json!({"turnstile": {"mode": "platform"}}),
            json!({"turnstile": {"mode": "platform", "site_key": "ignored"}}),
            json!({"turnstile": {"mode": "own", "site_key": site}}),
        ] {
            assert!(check_settings("web_chat", "customer", &ok).is_ok(), "{ok}");
        }
        for bad in [
            json!({"turnstile": "own"}),
            json!({"turnstile": {}}),
            json!({"turnstile": {"mode": "captcha"}}),
            json!({"turnstile": {"mode": "off", "theme": "dark"}}),
            json!({"turnstile": {"mode": "own"}}),
            json!({"turnstile": {"mode": "own", "site_key": "1x4AAAAAAAAbcd"}}),
            json!({"turnstile": {"mode": "own", "site_key": "0xshort"}}),
            json!({"turnstile": {"mode": "own", "site_key": format!("0x{}", "a".repeat(101))}}),
            json!({"turnstile": {"mode": "own", "site_key": "0x4AAAAAAAA bcd"}}),
        ] {
            assert_eq!(
                check_settings("web_chat", "customer", &bad)
                    .unwrap_err()
                    .code(),
                "invalid_settings",
                "{bad}"
            );
        }
        assert!(is_site_key(&format!("0x{}", "a".repeat(100))));
        let own = json!({"turnstile": {"mode": "own", "site_key": site}});
        assert_eq!(turnstile_mode(&own), Some("own"));
        assert_eq!(
            check_required_credential("web_chat", "customer", &own, None)
                .unwrap_err()
                .code(),
            "invalid_credential"
        );
        let cred = json!({"turnstile_secret": "0x4AAAAAAAAsecretvalue"});
        assert!(check_required_credential("web_chat", "customer", &own, cred.as_object()).is_ok());
        let platform = json!({"turnstile": {"mode": "platform"}});
        assert!(check_required_credential("web_chat", "customer", &platform, None).is_ok());
    }

    #[test]
    fn the_snippet_names_the_widget_script_and_key() {
        assert_eq!(
            snippet("https://chat.nonagentic.ai", "wk_abc"),
            r#"<script src="https://chat.nonagentic.ai/webchat/widget.js" data-widget="wk_abc" async></script>"#
        );
    }
}
