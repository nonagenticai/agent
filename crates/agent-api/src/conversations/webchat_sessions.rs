//! Web chat anonymous sessions, durable rate limits and abuse blocks (B13 §1 rows 2–3, §3; migration 0124). No Redis:
//! sessions live in `web_chat_session` (tenant rows), fixed-window counters in `channel_rate_counter` and refusals in
//! `channel_abuse_event` (both reached only through definer functions), so every limit survives an edge or agent-api
//! restart. Fail closed: any database error while admitting a session or a message is 503 `unavailable`.
//!
//! - `POST /api/internal/channel/webchat/sessions` (EdgeKey) `{binding_id, widget_key, origin, ip_hash,
//!   turnstile_token?, client_ip?}` → 201 `{session_id, session_token, expires_at, display, csat_enabled}`; 401
//!   `widget_unknown` (unknown, disabled, unverified or rotated key: one answer), 403 `origin_not_allowed`, 429
//!   `rate_limited {retry_after_s}`, 503 `unavailable`. The token (`<session_id>.<base64url 32 random bytes>`) is
//!   shown once; only its sha256 is stored.
//! - Turnstile (B14 §2), checked before the session counts toward any limit: a binding's `settings.turnstile.mode`
//!   `off` (default) | `platform` (`WEBCHAT_TURNSTILE_SITE_KEY` + `_SECRET`) | `own` (`settings.turnstile.site_key` +
//!   credential `turnstile_secret`). No token → 428 `turnstile_required {site_key}`; siteverify (remoteip =
//!   `client_ip`, action `webchat`, hostname = the request origin's host) refuses → 403 `turnstile_failed`;
//!   unreachable → 503 `turnstile_unavailable` (fail closed); keys missing for the mode → 503
//!   `turnstile_unconfigured`. `client_ip` is used for nothing else. Each token presented counts toward 20 attempts per
//!   ip_hash per 10 min (`ts:<ip_hash>`), checked before any credential is opened or siteverify called → 429
//!   `rate_limited {retry_after_s}`.
//! - Known customer (B7, `settings.identity.mode` `hs256` | `es256`): a `customer_assertion` is verified
//!   ([`super::webchat_identity::verify`], HS256 keys from the binding's sealed credential `identity_secret` and, while
//!   `prev_valid_until` is in the future, `identity_secret_prev`, each `<kid>:<secret>`) after the origin check and
//!   before anything is consumed. A refusal → 401 `assertion_invalid` and counts toward 20 per ip_hash per 10 min
//!   (`assert:<ip_hash>`) → 429. A verified assertion skips Turnstile (the host already knows the customer); the
//!   session limit applies; then ONE transaction claims the assertion's `jti` (`agent_webchat_jti_claim`; already
//!   claimed → 401 `assertion_replayed`) and inserts the session with `customer_ref` / `customer_email` /
//!   `identified_at`, so a 428 / 429 / 503 burns no jti. Mode `off` ignores the assertion (anonymous). The assertion,
//!   `sub` and email are never logged.
//! - [`admit`]: a web chat message on a core binding carries `session_token` + `ip_hash` and party `wc:<session_id>`
//!   (401 `session_invalid` when unknown, expired or of another binding; 403 `session_blocked`); the session slides 24 h
//!   (capped at created + 7 d), then the limits apply.
//!
//! Limits (fixed windows, defaults; a binding's `settings.limits` overrides each, 1..1000): new sessions per ip_hash
//! per hour (10), messages per ip_hash per 10 min (30), per session per hour (60), per binding per minute (300). Over →
//! 429 with `retry_after_s` (the end of the window). Abuse: 3 refusals of one session by its own window within 10 min
//! (the shared per-IP / per-binding windows refuse without counting), or 5 identical texts within a 2-minute window →
//! the session is blocked for 1 h. A 10-minute tick ([`spawn_sweep`]) deletes counter windows older than 2 h and
//! sessions expired for over a day. Customer text is data: only its hash is kept, never logged.
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::post;
use base64::Engine as _;
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{Acquire, PgConnection, PgPool, Row};
use uuid::Uuid;

use super::internal::EdgeKey;
use super::webchat_identity::{self, HsKey, IdentityMode, IdentitySettings};
use super::{iso, store};
use crate::audit::{self, Actor, Event};
use crate::auth::TenantStatus;
use crate::channels::core::{
    InboundReq, WebChatSessionCheckReq, WebChatSessionReq, WebChatSessionResp,
};
use crate::error::{ApiError, ApiJson, ApiResult};
use crate::state::AppState;

/// The `settings.limits` keys of a web chat binding (each 1..1000).
pub const LIMIT_KEYS: [&str; 4] = [
    "sessions_per_ip_hour",
    "msgs_per_ip_10m",
    "msgs_per_session_hour",
    "msgs_per_minute",
];
/// The range every `settings.limits` value must fall in.
pub const LIMIT_RANGE: std::ops::RangeInclusive<i64> = 1..=1000;
/// Refusals of one session within [`ABUSE_REFUSAL_WINDOW_S`] that block it.
pub const ABUSE_REFUSALS: i32 = 3;
pub const ABUSE_REFUSAL_WINDOW_S: i32 = 600;
/// Identical texts of one session within one [`REPEAT_WINDOW_S`] window that block it.
pub const REPEAT_TEXTS: i32 = 5;
pub const REPEAT_WINDOW_S: i32 = 120;
/// How often the counter sweep runs.
pub const SWEEP_EVERY: Duration = Duration::from_secs(600);

/// The fixed-window limits of one binding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub sessions_per_ip_hour: i32,
    pub msgs_per_ip_10m: i32,
    pub msgs_per_session_hour: i32,
    pub msgs_per_minute: i32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            sessions_per_ip_hour: 10,
            msgs_per_ip_10m: 30,
            msgs_per_session_hour: 60,
            msgs_per_minute: 300,
        }
    }
}

impl Limits {
    /// The binding's `settings.limits`, each value in [`LIMIT_RANGE`] or the default.
    pub fn of(settings: &Value) -> Self {
        let d = Self::default();
        let get = |k: &str, dflt: i32| {
            settings
                .get("limits")
                .and_then(|l| l.get(k))
                .and_then(Value::as_i64)
                .filter(|n| LIMIT_RANGE.contains(n))
                .and_then(|n| i32::try_from(n).ok())
                .unwrap_or(dflt)
        };
        Self {
            sessions_per_ip_hour: get("sessions_per_ip_hour", d.sessions_per_ip_hour),
            msgs_per_ip_10m: get("msgs_per_ip_10m", d.msgs_per_ip_10m),
            msgs_per_session_hour: get("msgs_per_session_hour", d.msgs_per_session_hour),
            msgs_per_minute: get("msgs_per_minute", d.msgs_per_minute),
        }
    }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/internal/channel/webchat/sessions",
            post(create_web_chat_session),
        )
        .route(
            "/api/internal/channel/webchat/session-check",
            post(check_web_chat_session),
        )
}

fn unavailable() -> ApiError {
    ApiError::Unavailable(
        "unavailable",
        "Web chat is unavailable; try again shortly.".into(),
        None,
    )
}

/// Fail closed: a database error while admitting is 503 `unavailable` (logged without data).
fn down(e: sqlx::Error) -> ApiError {
    tracing::warn!(error = %e, "webchat_admission_db_error");
    unavailable()
}

/// [`down`] for a helper that already mapped its error: an internal or unavailable answer becomes `unavailable`.
fn closed(e: ApiError) -> ApiError {
    match e {
        ApiError::Internal(err) => {
            tracing::warn!(error = %format!("{err:#}"), "webchat_admission_error");
            unavailable()
        }
        ApiError::Unavailable(..) => unavailable(),
        other => other,
    }
}

fn widget_unknown() -> ApiError {
    ApiError::detailed(
        StatusCode::UNAUTHORIZED,
        "widget_unknown",
        "This chat widget is not available.",
    )
}

fn session_invalid() -> ApiError {
    ApiError::detailed(
        StatusCode::UNAUTHORIZED,
        "session_invalid",
        "The chat session is not valid; start a new one.",
    )
}

fn session_blocked() -> ApiError {
    ApiError::Forbidden("session_blocked", "This chat session is blocked.".into())
}

fn too_many(retry_after_s: u64) -> ApiError {
    ApiError::detailed(
        StatusCode::TOO_MANY_REQUESTS,
        "rate_limited",
        "Too many messages; try again later.",
    )
    .with("retry_after_s", retry_after_s)
}

/// `ip_hash`: hex sha256 (64 characters) → its lowercase hex and bytes; else 422 `invalid_ip_hash`.
fn ip_hash(raw: &str) -> ApiResult<(String, Vec<u8>)> {
    let s = raw.trim().to_ascii_lowercase();
    match hex::decode(&s) {
        Ok(b) if b.len() == 32 => Ok((s, b)),
        _ => Err(ApiError::Unprocessable(
            "invalid_ip_hash",
            "ip_hash is a hex sha256".into(),
        )),
    }
}

/// The `action` the widget renders its Turnstile challenge with; a token of any other action is refused.
pub const TURNSTILE_ACTION: &str = "webchat";
/// The longest Turnstile token accepted.
const MAX_TURNSTILE_TOKEN: usize = 4096;

fn turnstile_unconfigured() -> ApiError {
    ApiError::Unavailable(
        "turnstile_unconfigured",
        "Web chat verification is not configured.".into(),
        None,
    )
}

/// The binding's Turnstile widget as `(site_key, secret)`, or `None` when its `settings.turnstile.mode` is `off` or
/// absent. 503 `turnstile_unconfigured` when the mode needs keys that are not there (or the mode is unknown).
async fn turnstile_keys(
    st: &AppState,
    settings: &Value,
    tenant: Uuid,
    binding: Uuid,
    sealed: (Option<Vec<u8>>, Option<Vec<u8>>, Option<i32>),
) -> ApiResult<Option<(String, String)>> {
    let t = settings.get("turnstile");
    match t.and_then(|t| t.get("mode")).and_then(Value::as_str) {
        None | Some("off") => Ok(None),
        Some("platform") => st
            .cfg
            .edge
            .webchat_turnstile
            .platform
            .as_ref()
            .map(|k| Some((k.site_key.clone(), k.secret.clone())))
            .ok_or_else(turnstile_unconfigured),
        Some("own") => {
            let site_key = t
                .and_then(|t| t.get("site_key"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|k| !k.is_empty())
                .ok_or_else(turnstile_unconfigured)?;
            let mut tx = crate::tenancy::begin_scoped(&st.db, tenant)
                .await
                .map_err(closed)?;
            let cred = super::bindings::open_credential(&mut tx, st, tenant, binding, sealed).await;
            tx.commit().await.map_err(down)?;
            let secret = cred
                .as_ref()
                .and_then(|c| c.get("turnstile_secret"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(turnstile_unconfigured)?;
            Ok(Some((site_key.to_string(), secret.to_string())))
        }
        Some(_) => Err(turnstile_unconfigured()),
    }
}

/// The Turnstile gate of session create: nothing with mode `off`; else the visitor's token must pass siteverify.
async fn turnstile_gate(
    st: &AppState,
    keys: Option<(String, String)>,
    b: &WebChatSessionReq,
    origin: &str,
) -> ApiResult<()> {
    let Some((site_key, secret)) = keys else {
        return Ok(());
    };
    let Some(token) = b
        .turnstile_token
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    else {
        return Err(ApiError::detailed(
            StatusCode::PRECONDITION_REQUIRED,
            "turnstile_required",
            "Complete the verification challenge to start a chat.",
        )
        .with("site_key", site_key));
    };
    let failed = || {
        ApiError::Forbidden(
            "turnstile_failed",
            "The verification challenge was not accepted.".into(),
        )
    };
    if token.len() > MAX_TURNSTILE_TOKEN {
        return Err(failed());
    }
    let cfg = &st.cfg.edge.webchat_turnstile;
    let check = crate::turnstile::Check {
        url: cfg.verify_url.as_str(),
        timeout: Duration::from_millis(cfg.timeout_ms),
        secret: &secret,
        token,
        remote_ip: b.client_ip.as_deref().map(str::trim),
        action: Some(TURNSTILE_ACTION),
        // the token must have been solved on the page this (already allowed) origin names: one platform site key
        // serves every tenant, so a token solved on another tenant's page must not open a chat here
        hostname: Some(origin_host(origin)),
    };
    match crate::turnstile::verify(&st.http, &check).await {
        Ok(()) => Ok(()),
        Err(crate::turnstile::Failure::Rejected) => Err(failed()),
        Err(crate::turnstile::Failure::Unavailable) => Err(ApiError::Unavailable(
            "turnstile_unavailable",
            "Web chat verification is unavailable; try again shortly.".into(),
            None,
        )),
    }
}

/// The sha256 a session token is stored under.
pub fn token_hash(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

/// A fresh session token: `<session_id>.<base64url of 32 random bytes>`.
fn new_token(id: Uuid) -> ApiResult<String> {
    let mut b = [0u8; 32];
    SystemRandom::new()
        .fill(&mut b)
        .map_err(|_| ApiError::Internal(anyhow::anyhow!("no randomness for a session token")))?;
    Ok(format!(
        "{id}.{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
    ))
}

/// The host of a canonical origin (`https://chat.example:8443` → `chat.example`).
fn origin_host(origin: &str) -> &str {
    let rest = origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"))
        .unwrap_or(origin);
    let rest = rest.split('/').next().unwrap_or(rest);
    rest.rsplit_once(':').map_or(rest, |(h, _)| h)
}

/// Turnstile attempts (a token presented to a binding with Turnstile on) per ip_hash per 10 minutes, counted before
/// any credential is opened or siteverify is called, so a flood of bogus tokens cannot drive siteverify unthrottled.
const TURNSTILE_ATTEMPTS: (i32, i32) = (20, 600);

/// Whether the binding's `settings.turnstile.mode` asks for a challenge (anything but absent / `off`).
fn turnstile_on(settings: &Value) -> bool {
    !matches!(
        settings
            .get("turnstile")
            .and_then(|t| t.get("mode"))
            .and_then(Value::as_str),
        None | Some("off")
    )
}

/// One hit on `key`'s fixed window of `window_s` seconds: `Some(retry_after_s)` when it is now over `limit`.
pub(super) async fn hit(
    db: &PgPool,
    key: &str,
    window_s: i32,
    limit: i32,
) -> ApiResult<Option<u64>> {
    let r = sqlx::query("SELECT hits, retry_after_s FROM agent_channel_rate_hit($1, $2)")
        .bind(key)
        .bind(window_s)
        .fetch_one(db)
        .await
        .map_err(down)?;
    let hits: i32 = r.get("hits");
    let retry: i32 = r.get("retry_after_s");
    Ok((hits > limit).then(|| u64::try_from(retry.max(1)).unwrap_or(1)))
}

/// The origins a binding's sessions may come from: its own `settings.allowed_origins` (canonical form) and the edge's
/// public origin (its demo page). Compared exactly: `https://A.example/` is not `https://a.example`.
fn origin_allowed(st: &AppState, settings: &Value, origin: &str) -> bool {
    let public = crate::channels::webchat::normalize_origin(&super::bindings::public_base(st));
    public.as_deref() == Some(origin)
        || settings
            .get("allowed_origins")
            .and_then(Value::as_array)
            .is_some_and(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .filter_map(crate::channels::webchat::normalize_origin)
                    .any(|o| o == origin)
            })
}

/// The binding's own `settings.allowed_origins`, as stored.
fn settings_origins(settings: &Value) -> Vec<String> {
    settings
        .get("allowed_origins")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Refused assertions per ip_hash per 10 minutes before the answer turns 429.
const ASSERTION_REFUSALS: (i32, i32) = (20, 600);

fn assertion_invalid() -> ApiError {
    ApiError::detailed(
        StatusCode::UNAUTHORIZED,
        "assertion_invalid",
        "The customer assertion was not accepted.",
    )
}

fn assertion_replayed() -> ApiError {
    ApiError::detailed(
        StatusCode::UNAUTHORIZED,
        "assertion_replayed",
        "The customer assertion was already used.",
    )
}

/// The binding's `settings.identity` (absent or malformed: mode `off`, i.e. anonymous).
fn identity_of(settings: &Value) -> IdentitySettings {
    settings
        .get("identity")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default()
}

/// `<kid>:<secret>` → the key (both parts non-empty).
pub(super) fn parse_hs_key(v: &str) -> Option<HsKey> {
    let (kid, secret) = v.trim().split_once(':')?;
    let (kid, secret) = (kid.trim(), secret.trim());
    (!kid.is_empty() && !secret.is_empty()).then(|| HsKey {
        kid: kid.to_string(),
        secret: secret.to_string(),
    })
}

/// The HS256 keys of a binding: credential `identity_secret`, plus `identity_secret_prev` while
/// `identity.prev_valid_until` is in the future. Empty for ES256 (public keys are in the settings) or when the
/// credential does not open (every assertion is then refused).
async fn identity_keys(
    st: &AppState,
    identity: &IdentitySettings,
    tenant: Uuid,
    binding: Uuid,
    sealed: (Option<Vec<u8>>, Option<Vec<u8>>, Option<i32>),
) -> ApiResult<Vec<HsKey>> {
    if identity.mode != IdentityMode::Hs256 {
        return Ok(Vec::new());
    }
    let mut tx = crate::tenancy::begin_scoped(&st.db, tenant)
        .await
        .map_err(closed)?;
    let cred = super::bindings::open_credential(&mut tx, st, tenant, binding, sealed).await;
    tx.commit().await.map_err(down)?;
    let field = |k: &str| {
        cred.as_ref()
            .and_then(|c| c.get(k))
            .and_then(Value::as_str)
            .and_then(parse_hs_key)
    };
    let mut keys: Vec<HsKey> = field("identity_secret").into_iter().collect();
    if identity
        .prev_valid_until
        .is_some_and(|t| t > chrono::Utc::now())
    {
        keys.extend(field("identity_secret_prev"));
    }
    Ok(keys)
}

/// A refused assertion: counted toward [`ASSERTION_REFUSALS`] per ip_hash; 401 `assertion_invalid`, or 429 over it.
async fn refuse_assertion(st: &AppState, ip_hex: &str) -> ApiError {
    let (limit, window_s) = ASSERTION_REFUSALS;
    match hit(&st.db, &format!("assert:{ip_hex}"), window_s, limit).await {
        Ok(Some(retry)) => too_many(retry),
        Ok(None) => assertion_invalid(),
        Err(e) => e,
    }
}

async fn create_web_chat_session(
    _k: EdgeKey,
    State(st): State<AppState>,
    ApiJson(b): ApiJson<WebChatSessionReq>,
) -> ApiResult<(StatusCode, Json<WebChatSessionResp>)> {
    store::require_clean([b.widget_key.as_str(), b.origin.as_str(), b.ip_hash.as_str()])?;
    let (ip_hex, ip_bytes) = ip_hash(&b.ip_hash)?;
    let origin = b.origin.trim();
    if origin.is_empty() || origin.chars().count() > 300 {
        return Err(ApiError::Forbidden(
            "origin_not_allowed",
            "This origin may not open a chat.".into(),
        ));
    }
    // unknown, disabled, unverified or rotated (the key no longer matches): one answer
    let row = sqlx::query(
        "SELECT tenant_id, job_id, settings, secret_enc, secret_nonce, secret_dek_version \
           FROM agent_connector_bindings() \
          WHERE id = $1 AND kind = 'web_chat' AND side = 'customer' AND external_id = $2",
    )
    .bind(b.binding_id)
    .bind(b.widget_key.trim())
    .fetch_optional(&st.db)
    .await
    .map_err(down)?
    .ok_or_else(widget_unknown)?;
    let tenant: Uuid = row.get("tenant_id");
    let job_id: Uuid = row.get("job_id");
    let settings: Value = row.get("settings");
    let status = crate::tenancy::tenant_status(&st.db, tenant)
        .await
        .map_err(closed)?;
    if !matches!(status, Some(TenantStatus::Active)) {
        return Err(widget_unknown());
    }
    if !origin_allowed(&st, &settings, origin) {
        return Err(ApiError::Forbidden(
            "origin_not_allowed",
            "This origin may not open a chat.".into(),
        ));
    }
    // Turnstile before the limits: a visitor who has not solved the challenge yet spends no session
    let sealed = (
        row.get::<Option<Vec<u8>>, _>("secret_enc"),
        row.get::<Option<Vec<u8>>, _>("secret_nonce"),
        row.get::<Option<i32>, _>("secret_dek_version"),
    );
    // a known customer: verify the host's assertion (nothing consumed yet); mode off ignores it
    let identity = identity_of(&settings);
    let mut verified = None;
    if let Some(tok) = b
        .customer_assertion
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .filter(|_| identity.mode != IdentityMode::Off)
    {
        let keys = identity_keys(&st, &identity, tenant, b.binding_id, sealed.clone()).await?;
        match webchat_identity::verify(
            &identity,
            b.binding_id,
            &keys,
            tok,
            chrono::Utc::now().timestamp(),
        ) {
            Ok(a) => verified = Some(a),
            Err(webchat_identity::Refusal::Invalid(why)) => {
                tracing::info!(binding = %b.binding_id, reason = why, "webchat_assertion_refused");
                return Err(refuse_assertion(&st, &ip_hex).await);
            }
        }
    }
    let has_token = verified.is_none()
        && b.turnstile_token
            .as_deref()
            .is_some_and(|t| !t.trim().is_empty());
    if has_token && turnstile_on(&settings) {
        let (limit, window_s) = TURNSTILE_ATTEMPTS;
        if let Some(retry) = hit(&st.db, &format!("ts:{ip_hex}"), window_s, limit).await? {
            return Err(too_many(retry));
        }
    }
    if verified.is_none() {
        let keys = turnstile_keys(&st, &settings, tenant, b.binding_id, sealed).await?;
        turnstile_gate(&st, keys, &b, origin).await?;
    }
    let limits = Limits::of(&settings);
    if let Some(retry) = hit(
        &st.db,
        &format!("wcs:ip:{ip_hex}"),
        3600,
        limits.sessions_per_ip_hour,
    )
    .await?
    {
        return Err(too_many(retry));
    }
    let id = Uuid::new_v4();
    let token = new_token(id)?;
    let mut tx = store::begin_job(&st.db, tenant, job_id)
        .await
        .map_err(closed)?;
    // the assertion is single-use: claimed in the transaction that inserts the session, so any refusal above (428,
    // 429, 503) burned nothing, and a failure here rolls the claim back
    if let Some(a) = &verified {
        let fresh: bool =
            sqlx::query_scalar("SELECT agent_webchat_jti_claim($1, $2, to_timestamp($3))")
                .bind(b.binding_id)
                .bind(&a.jti)
                .bind(a.exp as f64)
                .fetch_one(&mut *tx)
                .await
                .map_err(down)?;
        if !fresh {
            return Err(assertion_replayed());
        }
    }
    let expires: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
        "INSERT INTO web_chat_session \
            (id, tenant_id, job_id, binding_id, token_hash, origin, ip_hash, created_at, last_seen_at, expires_at, \
             customer_ref, customer_email, identified_at, customer_name) \
         VALUES ($1, app_current_tenant(), $2, $3, $4, $5, $6, now(), now(), now() + interval '24 hours', \
                 $7, $8, CASE WHEN $7::text IS NULL THEN NULL ELSE now() END, $9) \
         RETURNING expires_at",
    )
    .bind(id)
    .bind(job_id)
    .bind(b.binding_id)
    .bind(token_hash(&token))
    .bind(origin)
    .bind(&ip_bytes)
    .bind(verified.as_ref().map(|a| a.sub.as_str()))
    .bind(verified.as_ref().and_then(|a| a.email.as_deref()))
    .bind(display_name(verified.as_ref().and_then(|a| a.name.as_deref())))
    .fetch_one(&mut *tx)
    .await
    .map_err(down)?;
    if let Some(a) = &verified {
        // no PII: the tenant, binding and session, and the first 12 hex of sha256(sub). Best effort: an audit
        // refusal must not turn a verified customer away. It runs in a SAVEPOINT (nested transaction): a failed
        // statement aborts a Postgres transaction, so without it the outer commit below would fail with it
        let sub_sha = hex::encode(&Sha256::digest(a.sub.as_bytes())[..6]);
        let mut sp = tx.begin().await.map_err(down)?;
        match audit::record(
            &mut sp,
            &Actor::system("service:agent-edge"),
            Event::new(
                "web_chat_session.identified",
                "web_chat_session",
                id.to_string(),
            )
            .detail("binding", b.binding_id.to_string())
            .detail("sub_sha", sub_sha),
        )
        .await
        {
            Ok(_) => sp.commit().await.map_err(down)?,
            Err(e) => {
                tracing::warn!(code = e.code(), "webchat_identified_audit_failed");
                sp.rollback().await.map_err(down)?;
            }
        }
    }
    tx.commit().await.map_err(down)?;
    let display = settings
        .get("display")
        .filter(|d| d.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));
    Ok((
        StatusCode::CREATED,
        Json(WebChatSessionResp {
            session_id: id.to_string(),
            session_token: token,
            expires_at: Value::String(iso(expires)),
            display,
            csat_enabled: settings
                .get("csat_enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            attachments_enabled: super::attachment_settings::Switches::of("web_chat", &settings)
                .customer,
            identified: verified.is_some(),
            greeting_name: verified.as_ref().and_then(|a| a.name.clone()),
            sign_in_url: webchat_identity::checked_sign_in_url(
                &identity,
                &settings_origins(&settings),
            ),
        }),
    ))
}

/// `POST /api/internal/channel/webchat/session-check` (EdgeKey): the liveness of the session behind a conversation's
/// stream or history read: the same rules as [`admit`] (known token of this binding and of that conversation, unexpired, not blocked, an
/// identified session inside `session_hours`), without counting a message or sliding the session. 200 `{ok: true}`;
/// 401 `session_invalid`; 403 `session_blocked`.
async fn check_web_chat_session(
    _k: EdgeKey,
    State(st): State<AppState>,
    ApiJson(b): ApiJson<WebChatSessionCheckReq>,
) -> ApiResult<Json<Value>> {
    let mut tx = store::begin_job(&st.db, b.tenant, b.job_id)
        .await
        .map_err(closed)?;
    verify_session(&mut tx, b.binding_id, &b.conversation_id, &b.session_token).await?;
    tx.commit().await.map_err(down)?;
    Ok(Json(json!({"ok": true})))
}

/// The session behind `session_token` is admitted for `conversation_id` on binding `binding_id`: known, of that
/// binding and that conversation's own, unexpired, not blocked, an identified one inside `session_hours`. Nothing is
/// counted and the session does not slide. `conn` is a [`store::begin_job`] transaction of the binding's tenant (RLS
/// confines the lookup to it). 401 `session_invalid`; 403 `session_blocked`. Shared by the session check and the
/// web chat attachment upload (SW-9).
pub(super) async fn verify_session(
    conn: &mut PgConnection,
    binding_id: Uuid,
    conversation_id: &str,
    session_token: &str,
) -> ApiResult<()> {
    let token = session_token.trim();
    if token.is_empty() || token.len() > 200 {
        return Err(session_invalid());
    }
    let row = sqlx::query(
        "SELECT binding_id, conversation_id, expires_at > now() AS live, identified_at, \
                (blocked_at IS NOT NULL \
                 AND (blocked_reason IN ('rotated', 'identity_rotated') OR blocked_at > now() - interval '1 hour')) AS blocked \
           FROM web_chat_session WHERE token_hash = $1",
    )
    .bind(token_hash(token))
    .fetch_optional(&mut *conn)
    .await
    .map_err(down)?
    .ok_or_else(session_invalid)?;
    if row.get::<Uuid, _>("binding_id") != binding_id
        || row.get::<Option<String>, _>("conversation_id").as_deref() != Some(conversation_id)
        || !row.get::<bool, _>("live")
    {
        return Err(session_invalid());
    }
    if row.get::<bool, _>("blocked") {
        return Err(session_blocked());
    }
    let identified_at: Option<chrono::DateTime<chrono::Utc>> = row.get("identified_at");
    if let Some(at) = identified_at {
        let settings: Value =
            sqlx::query_scalar("SELECT settings FROM connector_bindings WHERE id = $1")
                .bind(binding_id)
                .fetch_optional(&mut *conn)
                .await
                .map_err(down)?
                .unwrap_or_else(|| json!({}));
        let hours = i64::from(identity_of(&settings).session_hours());
        if at + chrono::Duration::hours(hours) <= chrono::Utc::now() {
            return Err(session_invalid());
        }
    }
    Ok(())
}

/// The verified customer a session was opened for (B7): read from the session row, never from a request.
#[derive(Clone, PartialEq, Eq)]
pub struct KnownCustomer {
    pub customer_ref: String,
    pub email: Option<String>,
    /// The assertion's display name (trimmed, at most 200 chars): shown to staff, never logged or audited.
    pub name: Option<String>,
}

/// The assertion's `name` as stored: trimmed, blank → `None`, cut to 200 chars (the column's CHECK).
fn display_name(name: Option<&str>) -> Option<String> {
    let n = name?.trim();
    (!n.is_empty()).then(|| {
        n.chars()
            .take(200)
            .collect::<String>()
            .trim_end()
            .to_string()
    })
}

impl std::fmt::Debug for KnownCustomer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KnownCustomer")
            .field("email", &self.email.is_some())
            .field("name", &self.name.is_some())
            .finish_non_exhaustive()
    }
}

/// A web chat message's session, admitted ([`admit`]).
#[derive(Clone, Debug)]
pub struct Admitted {
    pub session_id: Uuid,
    /// The session's one conversation, once its first message arrived.
    pub conversation_id: Option<String>,
    /// The known customer of an identified session (from the row); `None` for an anonymous one.
    pub customer: Option<KnownCustomer>,
}

/// Block session `sid` (`reason` `rate_limited` | `repeat_text`, for 1 h; a rotated or identity-rotated session stays blocked for good), audited.
async fn block(
    st: &AppState,
    b: &InboundReq,
    job_key: &str,
    sid: Uuid,
    reason: &'static str,
) -> ApiResult<()> {
    let mut tx = store::begin_job(&st.db, b.tenant, b.job_id)
        .await
        .map_err(closed)?;
    let n = sqlx::query(
        "UPDATE web_chat_session SET blocked_at = now(), blocked_reason = $2 \
          WHERE id = $1 AND (blocked_reason IS NULL OR blocked_reason NOT IN ('rotated', 'identity_rotated'))",
    )
    .bind(sid)
    .bind(reason)
    .execute(&mut *tx)
    .await
    .map_err(down)?
    .rows_affected();
    if n > 0 {
        audit::record(
            &mut tx,
            &Actor::system("service:agent-edge"),
            Event::new(
                "web_chat_session.block",
                "web_chat_session",
                sid.to_string(),
            )
            .job(job_key)
            .detail("reason", reason),
        )
        .await
        .map_err(closed)?;
    }
    tx.commit().await.map_err(down)?;
    Ok(())
}

/// Admit one web chat message on a core binding (called by `inbound` after its body checks and `checked_binding`):
/// the session (`session_token`, party `wc:<session_id>`), its slide, the limits and the abuse rules. `text` is the
/// checked message text (only its hash is kept).
pub async fn admit(
    st: &AppState,
    b: &InboundReq,
    job_key: &str,
    text: &str,
) -> ApiResult<Admitted> {
    let token = b
        .session_token
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty() && t.len() <= 200)
        .ok_or_else(session_invalid)?;
    let (ip_hex, _) = ip_hash(b.ip_hash.as_deref().ok_or_else(session_invalid)?)?;
    let mut tx = store::begin_job(&st.db, b.tenant, b.job_id)
        .await
        .map_err(closed)?;
    let row = sqlx::query(
        "SELECT id, binding_id, conversation_id, expires_at > now() AS live, \
                customer_ref, customer_email, identified_at, customer_name, \
                (blocked_at IS NOT NULL \
                 AND (blocked_reason IN ('rotated', 'identity_rotated') OR blocked_at > now() - interval '1 hour')) AS blocked \
           FROM web_chat_session WHERE token_hash = $1",
    )
    .bind(token_hash(token))
    .fetch_optional(&mut *tx)
    .await
    .map_err(down)?;
    let Some(row) = row else {
        return Err(session_invalid());
    };
    let sid: Uuid = row.get("id");
    let binding: Uuid = row.get("binding_id");
    if binding != b.binding_id || !row.get::<bool, _>("live") || b.party.id != format!("wc:{sid}") {
        return Err(session_invalid());
    }
    if row.get::<bool, _>("blocked") {
        return Err(session_blocked());
    }
    let conversation_id: Option<String> = row.get("conversation_id");
    sqlx::query(
        "UPDATE web_chat_session SET last_seen_at = now(), \
                expires_at = least(now() + interval '24 hours', created_at + interval '7 days') \
          WHERE id = $1",
    )
    .bind(sid)
    .execute(&mut *tx)
    .await
    .map_err(down)?;
    let settings: Value =
        sqlx::query_scalar("SELECT settings FROM connector_bindings WHERE id = $1")
            .bind(b.binding_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(down)?
            .unwrap_or_else(|| json!({}));
    // an identified session also ends `session_hours` after the assertion was verified
    let identified_at: Option<chrono::DateTime<chrono::Utc>> = row.get("identified_at");
    if let Some(at) = identified_at {
        let hours = i64::from(identity_of(&settings).session_hours());
        if at + chrono::Duration::hours(hours) <= chrono::Utc::now() {
            return Err(session_invalid());
        }
    }
    let customer = row
        .get::<Option<String>, _>("customer_ref")
        .map(|customer_ref| KnownCustomer {
            customer_ref,
            email: row.get("customer_email"),
            name: row.get("customer_name"),
        });
    tx.commit().await.map_err(down)?;
    // the limits: counted outside any transaction, so a refusal still counts. Only the session's own window is abuse
    // by this session: the shared per-IP and per-binding windows refuse (429) without counting toward its block, so
    // a busy network or widget cannot get a quiet visitor blocked
    let limits = Limits::of(&settings);
    let windows = [
        (
            format!("wcm:s:{sid}"),
            3600,
            limits.msgs_per_session_hour,
            true,
        ),
        (
            format!("wcm:ip:{ip_hex}"),
            600,
            limits.msgs_per_ip_10m,
            false,
        ),
        (
            format!("wcm:b:{}", b.binding_id),
            60,
            limits.msgs_per_minute,
            false,
        ),
    ];
    for (key, window_s, limit, own) in windows {
        if let Some(retry) = hit(&st.db, &key, window_s, limit).await? {
            if !own {
                return Err(too_many(retry));
            }
            let n: i32 =
                sqlx::query_scalar("SELECT agent_channel_abuse($1, 'rate_limited', NULL, $2)")
                    .bind(sid)
                    .bind(ABUSE_REFUSAL_WINDOW_S)
                    .fetch_one(&st.db)
                    .await
                    .map_err(down)?;
            if n >= ABUSE_REFUSALS {
                block(st, b, job_key, sid, "rate_limited").await?;
            }
            return Err(too_many(retry));
        }
    }
    // the same text again and again (case and surrounding space ignored)
    let digest = Sha256::digest(text.trim().to_lowercase().as_bytes());
    let key = format!("wct:{sid}:{}", hex::encode(&digest[..8]));
    if hit(&st.db, &key, REPEAT_WINDOW_S, REPEAT_TEXTS - 1)
        .await?
        .is_some()
    {
        let _: i32 = sqlx::query_scalar("SELECT agent_channel_abuse($1, 'repeat_text', $2, $3)")
            .bind(sid)
            .bind(digest.to_vec())
            .bind(REPEAT_WINDOW_S)
            .fetch_one(&st.db)
            .await
            .map_err(down)?;
        block(st, b, job_key, sid, "repeat_text").await?;
        return Err(session_blocked());
    }
    Ok(Admitted {
        session_id: sid,
        conversation_id,
        customer,
    })
}

/// One session is one conversation: the first message ties the session to `conversation_id`. `inbound` already swaps
/// the ref of a tied session for its own, so a later message in another conversation is only a lost race (two first
/// messages under different refs): 401 `session_invalid`, and the caller's transaction rolls back.
pub async fn attach(conn: &mut PgConnection, s: &Admitted, conversation_id: &str) -> ApiResult<()> {
    match s.conversation_id.as_deref() {
        Some(c) if c == conversation_id => Ok(()),
        Some(_) => Err(session_invalid()),
        None => {
            let tied: Option<String> = sqlx::query_scalar(
                "UPDATE web_chat_session SET conversation_id = COALESCE(conversation_id, $2) \
                  WHERE id = $1 RETURNING conversation_id",
            )
            .bind(s.session_id)
            .bind(conversation_id)
            .fetch_optional(&mut *conn)
            .await?;
            if tied.as_deref() == Some(conversation_id) {
                Ok(())
            } else {
                Err(session_invalid())
            }
        }
    }
}

/// Delete counter windows older than 2 h (and abuse events older than 7 days, sessions expired for over a day): the
/// number of counter rows deleted.
pub async fn sweep(db: &PgPool) -> Result<i32, sqlx::Error> {
    sqlx::query_scalar("SELECT agent_channel_counters_sweep()")
        .fetch_one(db)
        .await
}

/// The 10-minute counter sweep.
pub fn spawn_sweep(db: PgPool) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut t = tokio::time::interval(SWEEP_EVERY);
        t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            t.tick().await;
            match sweep(&db).await {
                Ok(n) => tracing::info!(deleted = n, "channel_rate_counter_sweep"),
                Err(e) => tracing::warn!(error = %e, "channel_rate_counter_sweep_failed"),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_default_and_override_in_range() {
        assert_eq!(Limits::of(&json!({})), Limits::default());
        let l = Limits::of(
            &json!({"limits": {"msgs_per_minute": 5, "msgs_per_ip_10m": 0, "sessions_per_ip_hour": 2000}}),
        );
        assert_eq!(l.msgs_per_minute, 5);
        assert_eq!(l.msgs_per_ip_10m, 30);
        assert_eq!(l.sessions_per_ip_hour, 10);
    }

    #[test]
    fn tokens_name_their_session_and_hash_to_32_bytes() {
        let id = Uuid::new_v4();
        let t = new_token(id).unwrap();
        let (head, secret) = t.split_once('.').unwrap();
        assert_eq!(head, id.to_string());
        assert_eq!(secret.len(), 43);
        assert_eq!(token_hash(&t).len(), 32);
        assert_ne!(t, new_token(id).unwrap());
    }

    #[test]
    fn origin_host_drops_scheme_and_port() {
        assert_eq!(origin_host("https://chat.example"), "chat.example");
        assert_eq!(origin_host("https://chat.example:8443"), "chat.example");
        assert_eq!(origin_host("http://localhost:5173"), "localhost");
    }

    #[test]
    fn turnstile_on_only_for_a_mode_other_than_off() {
        assert!(!turnstile_on(&json!({})));
        assert!(!turnstile_on(&json!({"turnstile": {"mode": "off"}})));
        assert!(turnstile_on(&json!({"turnstile": {"mode": "platform"}})));
        assert!(turnstile_on(&json!({"turnstile": {"mode": "own"}})));
    }

    #[test]
    fn ip_hash_is_hex_sha256() {
        assert!(ip_hash(&"ab".repeat(32)).is_ok());
        assert_eq!(ip_hash("1.2.3.4").unwrap_err().code(), "invalid_ip_hash");
        assert_eq!(
            ip_hash(&"ab".repeat(16)).unwrap_err().code(),
            "invalid_ip_hash"
        );
    }
}
