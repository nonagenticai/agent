//! Per-tenant platform-app licences, MINTED ON DEMAND (commonpractice `tenant-inference.ts` pattern, mechanism
//! mirroring nonagenticai_mcp_server `src/credentials/minter.py`): every call a tenant's traffic makes to an upstream
//! (inference-router, strata-rag, strata-ingest) carries a short-lived BUNDLE TOKEN minted for that tenant, so it is
//! attributable and budgetable per tenant. NOTHING is stored: [`token`] returns a cached or freshly minted token.
//!
//! [`BundleMinter`] (held in `AppState`, configured from `BUNDLE_MINTER_*`, [`crate::config::BundleMinterConfig`]):
//! as the Keycloak client `agent-bundle-minter` (client credentials; the access token is cached until its expiry minus
//! the skew) it calls licences `POST /internal/tenants/{tenant}/bundle-token?app=&bundle=&ttl_minutes=` (201 `{token,
//! expires_at, tenant_id, bundle, app, license_id, token_id}`), verifies the echo (tenant, app, bundle), and caches the
//! token per (tenant, app, bundle) until `expires_at` minus [`SKEW`]. A 401 from the minter drops the client token and
//! retries ONCE; a 401 from the upstream with a cached token is [`invalidate`]d by the caller. Every refusal, outage or
//! malformed answer fails closed; only successful mints are cached. Unconfigured: [`token`] is `Ok(None)` and the
//! consumer refuses the call (503 / 409), never falling back to a shared credential.
//!
//! Provisioning ([`provision_tenant`], signup step `app_licences` and the backfill) ENSURES the bundle through the
//! licences-admin client (the minter's client may not), then per app mints, SMOKES as the tenant (a bogus token must be
//! refused first, then one cheap authenticated call must succeed) and records the STATUS row (ids only, migration 0216).
//! The bundle licence is shared, so nothing is ever revoked here; tokens are short-lived.
//!
//! ⛔ The token is never logged or stored; [`AppToken`] has no `Display` and no `Serialize`.
use std::collections::HashMap;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;
use uuid::Uuid;

use crate::audit::{self, Actor, Event};
use crate::config::BundleMinterConfig;
use crate::error::{ApiError, ApiResult};
use crate::http_base::{BaseUrl, BaseUrlError};
use crate::service_token::form;
use crate::state::AppState;
use crate::tenancy::begin_scoped;

/// Tokens (cached or the minter's own) are treated as expired this long before they are.
pub const SKEW: Duration = Duration::from_secs(30);
/// Cap on any minter / Keycloak response body.
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
/// Cached tokens kept at most (oldest evicted).
const CACHE_ENTRIES: usize = 1024;

/// A platform app a tenant holds its own licence for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum App {
    InferenceRouter,
    StrataRag,
    StrataIngest,
    /// The MCP server (nonagenticai_mcp_server): minted on demand by `crate::mcp_client::creds`; NOT in [`ALL`]
    /// (no provisioning, no `tenant_app_licence` row).
    Mcp,
    /// The conveyor: a tenant's licence bearer for the conveyor's own `/mcp` (S06b). Minted on demand by the
    /// connector executor for an `AuthKind::LicenceBearer` template; NOT in [`ALL`], like [`App::Mcp`], because
    /// nothing is provisioned for it up front.
    Conveyor,
}

/// Every app, in provisioning order (the three the provisioning smoke covers).
pub const ALL: [App; 3] = [App::InferenceRouter, App::StrataRag, App::StrataIngest];

impl App {
    /// The licences app key, and `tenant_app_licence.app_key`.
    pub fn key(self) -> &'static str {
        match self {
            App::InferenceRouter => "inference-router",
            App::StrataRag => "strata-rag",
            App::StrataIngest => "strata-ingest",
            App::Mcp => "mcp",
            App::Conveyor => "conveyor",
        }
    }
}

/// A tenant's token for one app. Opaque: no `Display`, no `Serialize`, `Debug` is redacted.
#[derive(Clone)]
pub struct AppToken(String);

impl AppToken {
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    /// The bearer value. Hand it to the upstream request and nothing else.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for AppToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AppToken(<redacted>)")
    }
}

/// A minted token and the (non-secret) ids licences returned with it.
#[derive(Clone, Debug)]
pub struct Minted {
    pub token: AppToken,
    pub licence_id: String,
    pub token_id: String,
}

/// Why a mint did not produce a token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MintError {
    /// No `BUNDLE_MINTER_*` configuration in this process.
    NotConfigured,
    /// Licences refused (4xx): `code` is its `detail.error` (`no_bundle`, `app_not_in_bundle`, `no_app_licence`,
    /// `dependency_grant_scope`, `unknown_api_client`, `ambiguous_*`, ...). The tenant has no usable licence.
    Refused { status: u16, code: String },
    /// Licences or Keycloak unreachable, 5xx, or an answer that is not what a mint must be (retryable).
    Unavailable(String),
}

#[derive(Clone)]
struct Cached {
    minted: Minted,
    until: Instant,
}

/// The bundle-token minter (see the module doc). One per process, in `AppState`.
pub struct BundleMinter {
    cfg: Option<BundleMinterConfig>,
    /// The configured URLs, parsed once: `None` = no `BUNDLE_MINTER_*` configuration; `Some(Err)` = a URL this
    /// process cannot use, and every mint then fails closed with that reason instead of sending a raw string.
    urls: Option<Result<MinterUrls, BaseUrlError>>,
    http: reqwest::Client,
    /// The minter client's access token and when it must be refreshed. The lock is held across a fetch.
    client_token: Mutex<Option<(String, Instant)>>,
    cache: StdMutex<HashMap<(Uuid, &'static str, String), Cached>>,
}

/// `BUNDLE_MINTER_LICENSES_URL` and `BUNDLE_MINTER_TOKEN_URL`, parsed and checked once.
struct MinterUrls {
    licences: BaseUrl,
    token: BaseUrl,
}

impl BundleMinter {
    pub fn new(cfg: Option<BundleMinterConfig>, http: reqwest::Client) -> Self {
        let urls = cfg.as_ref().map(|c| {
            Ok::<_, BaseUrlError>(MinterUrls {
                licences: BaseUrl::parse(&c.licenses_url)?,
                token: BaseUrl::parse(&c.token_url)?,
            })
        });
        Self {
            cfg,
            urls,
            http,
            client_token: Mutex::new(None),
            cache: StdMutex::new(HashMap::new()),
        }
    }

    /// The configured URLs, parsed: `Err` when this process has no usable minter configuration.
    fn urls(&self) -> Result<&MinterUrls, MintError> {
        match &self.urls {
            Some(Ok(urls)) => Ok(urls),
            Some(Err(e)) => Err(MintError::Unavailable(format!(
                "BUNDLE_MINTER_* URLs are unusable: {e}"
            ))),
            None => Err(MintError::NotConfigured),
        }
    }

    pub fn configured(&self) -> bool {
        self.cfg.is_some()
    }

    /// The bundle tokens are minted in (`LICENSE_BUNDLE_KEY`), when configured.
    pub fn bundle(&self) -> Option<&str> {
        self.cfg.as_ref().map(|c| c.bundle.as_str())
    }

    /// Forget the cached token of (`tenant`, `app`): the upstream refused it (401), so the next [`Self::mint`]
    /// asks licences for a new one.
    pub fn invalidate(&self, tenant: Uuid, app: App) {
        if let Some(c) = &self.cfg {
            self.cache
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&(tenant, app.key(), c.bundle.clone()));
        }
    }

    async fn read_body(res: reqwest::Response) -> Result<serde_json::Value, MintError> {
        let bytes = res
            .bytes()
            .await
            .map_err(|_| MintError::Unavailable("licence authority response unreadable".into()))?;
        if bytes.len() > MAX_RESPONSE_BYTES {
            return Err(MintError::Unavailable(
                "licence authority response rejected".into(),
            ));
        }
        Ok(serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
    }

    /// The minter client's access token (client credentials), cached until its expiry minus [`SKEW`].
    async fn access_token(&self, c: &BundleMinterConfig) -> Result<String, MintError> {
        let mut cached = self.client_token.lock().await;
        if let Some((t, until)) = cached.as_ref()
            && Instant::now() < *until
        {
            return Ok(t.clone());
        }
        *cached = None;
        let body = form(&[
            ("grant_type", "client_credentials"),
            ("client_id", c.client_id.as_str()),
            ("client_secret", c.client_secret.as_str()),
        ]);
        let res = self
            .http
            .post(self.urls()?.token.endpoint(&[]))
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(body)
            .send()
            .await
            .map_err(|_| {
                MintError::Unavailable("minter client authentication unavailable".into())
            })?;
        if res.status().as_u16() != 200 {
            return Err(MintError::Unavailable(
                "minter client authentication failed".into(),
            ));
        }
        let payload = Self::read_body(res).await?;
        let token = payload
            .get("access_token")
            .and_then(|v| v.as_str())
            .filter(|t| printable(t, 8192));
        let lifetime = payload.get("expires_in").and_then(|v| v.as_u64());
        let (Some(token), Some(lifetime)) = (token, lifetime.filter(|l| *l > 0)) else {
            return Err(MintError::Unavailable(
                "minter client token rejected".into(),
            ));
        };
        let until = Instant::now() + Duration::from_secs(lifetime).saturating_sub(SKEW);
        if until > Instant::now() {
            *cached = Some((token.to_string(), until));
        }
        Ok(token.to_string())
    }

    async fn drop_client_token(&self) {
        *self.client_token.lock().await = None;
    }

    /// A short-lived token on `tenant`'s licence for `app` in the configured bundle: cached, else minted. Fails
    /// closed (see [`MintError`]).
    pub async fn mint(&self, tenant: Uuid, app: App) -> Result<Minted, MintError> {
        let Some(c) = &self.cfg else {
            return Err(MintError::NotConfigured);
        };
        let key = (tenant, app.key(), c.bundle.clone());
        {
            let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            let hit = cache.get(&key).map(|h| (h.until, h.minted.clone()));
            if let Some((until, minted)) = hit {
                if Instant::now() < until {
                    return Ok(minted);
                }
                cache.remove(&key);
            }
        }
        let ttl_minutes = c.ttl_minutes.to_string();
        let url = self.urls()?.licences.endpoint_with_query(
            &["internal", "tenants", &tenant.to_string(), "bundle-token"],
            &[
                ("app", app.key()),
                ("bundle", &c.bundle),
                ("ttl_minutes", &ttl_minutes),
            ],
        );
        let mut retried = false;
        let (status, payload) = loop {
            let bearer = self.access_token(c).await?;
            let res = self
                .http
                .post(url.clone())
                .bearer_auth(bearer)
                .send()
                .await
                .map_err(|_| MintError::Unavailable("licence authority unavailable".into()))?;
            let status = res.status().as_u16();
            let payload = Self::read_body(res).await?;
            if status == 401 {
                // the cached client token was rejected (rotated realm key, revoked client): drop it, retry once
                self.drop_client_token().await;
                if !retried {
                    retried = true;
                    continue;
                }
                return Err(MintError::Unavailable(
                    "minter client token rejected".into(),
                ));
            }
            break (status, payload);
        };
        match status {
            201 => {}
            400..=499 => {
                let code = payload
                    .get("detail")
                    .and_then(|d| d.get("error").or(Some(d)))
                    .and_then(|e| e.as_str())
                    .filter(|e| {
                        e.len() <= 64 && e.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                    })
                    .unwrap_or("refused")
                    .to_string();
                return Err(MintError::Refused { status, code });
            }
            _ => {
                return Err(MintError::Unavailable(
                    "licence authority unavailable".into(),
                ));
            }
        }
        let bad = || MintError::Unavailable("licence authority response rejected".into());
        let field = |k: &str| payload.get(k).and_then(|v| v.as_str());
        let tenant_ok = field("tenant_id")
            .and_then(|t| Uuid::parse_str(t).ok())
            .is_some_and(|t| t == tenant);
        if !tenant_ok
            || field("app") != Some(app.key())
            || field("bundle") != Some(c.bundle.as_str())
        {
            return Err(bad());
        }
        let token = field("token")
            .filter(|t| printable(t, 4096))
            .ok_or_else(bad)?;
        let licence_id = field("license_id")
            .filter(|t| !t.is_empty())
            .ok_or_else(bad)?;
        let token_id = field("token_id").map(str::to_string).unwrap_or_default();
        let remaining = field("expires_at").and_then(parse_expiry).ok_or_else(bad)?;
        let until = Instant::now() + remaining.saturating_sub(SKEW);
        if remaining <= SKEW {
            return Err(MintError::Unavailable(
                "minted credential already expired".into(),
            ));
        }
        let minted = Minted {
            token: AppToken::new(token),
            licence_id: licence_id.to_string(),
            token_id,
        };
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if cache.len() >= CACHE_ENTRIES {
            let now = Instant::now();
            cache.retain(|_, v| v.until > now);
            if cache.len() >= CACHE_ENTRIES
                && let Some(oldest) = cache
                    .iter()
                    .min_by_key(|(_, v)| v.until)
                    .map(|(k, _)| k.clone())
            {
                cache.remove(&oldest);
            }
        }
        cache.insert(
            key,
            Cached {
                minted: minted.clone(),
                until,
            },
        );
        Ok(minted)
    }
}

/// Printable ASCII, 1..=`max` characters (credential material).
fn printable(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && s.bytes().all(|b| (33..=126).contains(&b))
}

/// Time from now until the ISO timestamp `s` (RFC 3339, or naive = UTC); zero when already past.
fn parse_expiry(s: &str) -> Option<Duration> {
    let at = chrono::DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&chrono::Utc))
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f").map(|n| n.and_utc())
        })
        .ok()?;
    Some((at - chrono::Utc::now()).to_std().unwrap_or(Duration::ZERO))
}

/// `tenant`'s token for `app`, cached or freshly minted; `Ok(None)` when it has none (minter unset, or licences
/// refused: the caller fails closed). `conn` is unused (nothing is stored) and kept for the callers' signature. A
/// licences / Keycloak outage is 503 `app_licence_unavailable`, with no secret in the message or the log.
pub async fn token(
    st: &AppState,
    _conn: &mut sqlx::PgConnection,
    tenant: Uuid,
    app: App,
) -> ApiResult<Option<AppToken>> {
    match st.bundle_minter.mint(tenant, app).await {
        Ok(m) => Ok(Some(m.token)),
        Err(MintError::NotConfigured) => Ok(None),
        Err(MintError::Refused { status, code }) => {
            tracing::warn!(%tenant, app = app.key(), status, code, "app_licence_mint_refused");
            Ok(None)
        }
        Err(MintError::Unavailable(m)) => {
            tracing::error!(%tenant, app = app.key(), error = %m, "app_licence_mint_unavailable");
            Err(ApiError::Unavailable(
                "app_licence_unavailable",
                format!("the tenant's {} credential cannot be minted", app.key()),
                None,
            ))
        }
    }
}

/// The upstream answered 401 to a cached token of (`tenant`, `app`): drop it so the next [`token`] mints anew.
pub fn invalidate(st: &AppState, tenant: Uuid, app: App) {
    st.bundle_minter.invalidate(tenant, app);
}

/// How an upstream's refusal of a tenant bundle token (401) is answered: drop the cached token of (`tenant`, `app`),
/// mint a fresh one. Carried by a call that was made with a TENANT token (never a static pair or the platform key),
/// so it may be resent once; the caller owns the "once". `Debug` prints no token (there is none in it).
#[derive(Clone)]
pub struct Renewal {
    minter: std::sync::Arc<BundleMinter>,
    tenant: Uuid,
    app: App,
}

impl std::fmt::Debug for Renewal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Renewal")
            .field("tenant", &self.tenant)
            .field("app", &self.app.key())
            .finish()
    }
}

impl Renewal {
    pub fn new(st: &AppState, tenant: Uuid, app: App) -> Self {
        Self {
            minter: st.bundle_minter.clone(),
            tenant,
            app,
        }
    }

    pub fn tenant(&self) -> Uuid {
        self.tenant
    }

    /// Drop the cached token without minting (an upstream refused it and the request cannot be resent): the next call
    /// mints a fresh one.
    pub fn invalidate(&self) {
        self.minter.invalidate(self.tenant, self.app);
    }

    /// [`invalidate`] the cached token, then mint anew. `None` when licences cannot answer (the caller keeps the
    /// upstream's 401 as its error); the failure is logged by kind, never with a token.
    pub async fn fresh(&self) -> Option<String> {
        self.minter.invalidate(self.tenant, self.app);
        match self.minter.mint(self.tenant, self.app).await {
            Ok(m) => Some(m.token.expose().to_string()),
            Err(_) => {
                tracing::warn!(tenant = %self.tenant, app = self.app.key(), "app_licence_remint_failed");
                None
            }
        }
    }
}

/// Why a provision did not land. `retry`: transient (the step retries with backoff); else it will fail identically.
#[derive(Clone, Debug)]
pub struct ProvisionError {
    pub retry: bool,
    pub code: &'static str,
    pub message: String,
}

impl ProvisionError {
    fn retry(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            retry: true,
            code,
            message: message.into(),
        }
    }
    fn fail(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            retry: false,
            code,
            message: message.into(),
        }
    }
}

/// A mint failure as the visible, retryable provisioning error.
fn mint_failure(app: App, e: MintError) -> ProvisionError {
    match e {
        MintError::NotConfigured => ProvisionError::retry(
            "bundle_minter_not_configured",
            format!("{}: BUNDLE_MINTER_* is not configured", app.key()),
        ),
        MintError::Refused { status, code } => {
            let known = match code.as_str() {
                "app_not_in_bundle" => "app_not_in_bundle",
                "no_bundle" | "no_bundle_held" | "no_app_licence" => "bundle_not_held",
                "dependency_grant_scope" => "dependency_grant_scope",
                "unknown_api_client" => "unknown_api_client",
                c if c.starts_with("ambiguous_") => "ambiguous_licence",
                _ => "bundle_mint_refused",
            };
            ProvisionError::retry(
                known,
                format!(
                    "{}: licences refused the bundle token (HTTP {status}, {code})",
                    app.key()
                ),
            )
        }
        MintError::Unavailable(m) => {
            ProvisionError::retry("bundle_mint_unavailable", format!("{}: {m}", app.key()))
        }
    }
}

/// What one app's provisioning did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provisioned {
    /// Minted and smoke-tested as the tenant; status recorded.
    Stored,
    /// Already provisioned (this or a racing runner's): nothing minted.
    AlreadyStored,
    /// The app's upstream is not configured here (no strata URL): nothing minted, nothing recorded.
    NotConfigured,
}

impl Provisioned {
    pub fn label(self) -> &'static str {
        match self {
            Provisioned::Stored => "stored",
            Provisioned::AlreadyStored => "already_stored",
            Provisioned::NotConfigured => "not_configured",
        }
    }
}

/// The URL the smoke authenticates against, or `None` when the app's upstream is not configured (or its URL is not
/// a usable http(s) base). The router has a default URL, so it is always configured; strata is configured by
/// `STRATA_*_URL`.
fn smoke_url(st: &AppState, app: App) -> Option<reqwest::Url> {
    let endpoint =
        |raw: &str, segments: &[&str]| BaseUrl::parse(raw).ok().map(|base| base.endpoint(segments));
    match app {
        App::InferenceRouter => {
            endpoint(&st.cfg.internal_providers.llm_url, &["chat", "completions"])
        }
        // strata_rag src/api/mod.rs:39-40 `GET /api/stores` (list the tenant's stores).
        App::StrataRag => st
            .cfg
            .strata
            .rag_url
            .as_deref()
            .and_then(|u| endpoint(u, &["api", "stores"])),
        // strata_ingest src/api/mod.rs:140-142 `GET /api/ingest/jobs` (list the tenant's jobs).
        App::StrataIngest => st
            .cfg
            .strata
            .ingest_url
            .as_deref()
            .and_then(|u| endpoint(u, &["api", "ingest", "jobs"])),
        // Never provisioned or smoked (not in ALL).
        App::Mcp | App::Conveyor => None,
    }
}

/// One smoke request with `bearer`: the HTTP status, or a retryable transport error.
async fn smoke_request(
    st: &AppState,
    app: App,
    url: &reqwest::Url,
    bearer: &str,
    tenant: Uuid,
) -> Result<u16, ProvisionError> {
    let req = match app {
        App::InferenceRouter => st.http.post(url.clone()).json(&serde_json::json!({
            "model": st.cfg.internal_providers.llm_model,
            "messages": [{"role": "user", "content": "ping"}],
            "max_tokens": 1,
        })),
        App::StrataRag | App::StrataIngest | App::Mcp | App::Conveyor => st.http.get(url.clone()),
    };
    let res = req
        .bearer_auth(bearer)
        .header("x-tenant-id", tenant.to_string())
        .send()
        .await
        .map_err(|e| {
            ProvisionError::retry(
                "app_smoke_unreachable",
                format!("{}: smoke call failed: {}", app.key(), e.without_url()),
            )
        })?;
    Ok(res.status().as_u16())
}

/// The smoke, with a NEGATIVE CONTROL first (commonpractice `smokeCall`): a bogus token must be REFUSED, else the
/// upstream does not enforce licences and a 2xx for the tenant's token would prove nothing. Then one authenticated
/// call as the tenant must succeed. The bogus token is random per call (no cache can vouch for it).
async fn smoke(
    st: &AppState,
    tenant: Uuid,
    app: App,
    url: &reqwest::Url,
    token: &AppToken,
) -> Result<(), ProvisionError> {
    let bogus = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let control = smoke_request(st, app, url, &bogus, tenant).await?;
    let refused = match app {
        App::InferenceRouter => matches!(control, 401 | 403),
        App::StrataRag | App::StrataIngest | App::Mcp | App::Conveyor => control == 401,
    };
    if !refused {
        return Err(ProvisionError::retry(
            "app_not_enforcing",
            format!(
                "{} answered a bogus licence token with HTTP {control}; it is not enforcing licences, so a smoke pass would prove nothing",
                app.key()
            ),
        ));
    }
    let status = smoke_request(st, app, url, token.expose(), tenant).await?;
    if !(200..300).contains(&status) {
        return Err(ProvisionError::retry(
            "app_smoke_failed",
            format!("{} refused the tenant's token: HTTP {status}", app.key()),
        ));
    }
    Ok(())
}

/// Best-effort: record `err` as the app's `last_error` (a status row without `provisioned_at`).
async fn record_failure(st: &AppState, tenant: Uuid, app: App, err: &ProvisionError) {
    let text: String = format!("{}: {}", err.code, err.message)
        .chars()
        .take(400)
        .collect();
    let r: ApiResult<()> = async {
        let mut tx = begin_scoped(&st.db, tenant).await?;
        sqlx::query(
            "INSERT INTO tenant_app_licence (tenant_id, app_key, last_error) \
             SELECT id, $2, $3 FROM tenants WHERE id = $1 AND status = 'active' \
             ON CONFLICT (tenant_id, app_key) DO UPDATE SET last_error = EXCLUDED.last_error",
        )
        .bind(tenant)
        .bind(app.key())
        .bind(text)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }
    .await;
    if let Err(e) = r {
        tracing::warn!(%tenant, app = app.key(), error = %e, "app_licence_status_failed");
    }
}

/// Provision `app` for `tenant` unless it is provisioned already: mint (nothing stored), smoke as the tenant, record
/// the status row. `Ok(Err(_))` is a provisioning failure the caller journals; `Err(_)` is an internal (database)
/// error. A failed smoke discards only the cached token; the bundle licence is shared and never revoked here.
pub async fn provision_app(
    st: &AppState,
    tenant: Uuid,
    app: App,
    actor: &Actor,
) -> ApiResult<Result<Provisioned, ProvisionError>> {
    let mut tx = begin_scoped(&st.db, tenant).await?;
    let held: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM tenant_app_licence \
          WHERE tenant_id = $1 AND app_key = $2 AND provisioned_at IS NOT NULL)",
    )
    .bind(tenant)
    .bind(app.key())
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    if held {
        return Ok(Ok(Provisioned::AlreadyStored));
    }
    let Some(url) = smoke_url(st, app) else {
        return Ok(Ok(Provisioned::NotConfigured));
    };
    let got = match st.bundle_minter.mint(tenant, app).await {
        Ok(g) => g,
        Err(e) => {
            let e = mint_failure(app, e);
            tracing::warn!(%tenant, app = app.key(), code = e.code, "app_licence_mint_failed");
            record_failure(st, tenant, app, &e).await;
            return Ok(Err(e));
        }
    };
    if let Err(e) = smoke(st, tenant, app, &url, &got.token).await {
        tracing::warn!(%tenant, app = app.key(), code = e.code, "app_licence_smoke_failed");
        st.bundle_minter.invalidate(tenant, app);
        record_failure(st, tenant, app, &e).await;
        return Ok(Err(e));
    }
    let mut tx = begin_scoped(&st.db, tenant).await?;
    let written = sqlx::query(
        "INSERT INTO tenant_app_licence \
             (tenant_id, app_key, licence_id, token_id, provisioned_at, last_smoke_at, last_error) \
         SELECT id, $2, $3, NULLIF($4, ''), now(), now(), NULL FROM tenants WHERE id = $1 AND status = 'active' \
         ON CONFLICT (tenant_id, app_key) DO UPDATE SET licence_id = EXCLUDED.licence_id, \
             token_id = EXCLUDED.token_id, provisioned_at = COALESCE(tenant_app_licence.provisioned_at, now()), \
             last_smoke_at = now(), last_error = NULL",
    )
    .bind(tenant)
    .bind(app.key())
    .bind(&got.licence_id)
    .bind(&got.token_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if written == 0 {
        tx.commit().await?;
        // No row and no write: the tenant is not active (offboarded while we minted).
        return Ok(Err(ProvisionError::fail(
            "tenant_not_active",
            format!("{}: the tenant is no longer active", app.key()),
        )));
    }
    audit::record(
        &mut tx,
        actor,
        Event::new(
            "app_licence.provisioned",
            "tenant_app_licence",
            app.key().to_string(),
        )
        .detail("license_id", got.licence_id.clone())
        .detail("app", app.key()),
    )
    .await?;
    tx.commit().await?;
    Ok(Ok(Provisioned::Stored))
}

/// Ensure the bundle for `tenant` through the licences-admin client (the minter's own client may not call ensure):
/// creates the closure apps' dependency licences, idempotent. A 403 (role missing) is `bundle_ensure_forbidden`; an
/// app licences refuses to cover (`refused`) is `bundle_ensure_refused`, naming app and reason.
async fn ensure_bundle(st: &AppState, tenant: Uuid, needed: &[App]) -> Result<(), ProvisionError> {
    let Some(bundle) = st.bundle_minter.bundle() else {
        return Err(ProvisionError::retry(
            "bundle_minter_not_configured",
            "BUNDLE_MINTER_* is not configured",
        ));
    };
    match st
        .identity
        .licences
        .ensure_bundle(tenant, bundle, &crate::licenses_admin::ENSURE_PLANS)
        .await
    {
        Ok(done) => {
            let hits: Vec<String> = done
                .refused
                .iter()
                .filter(|(app, _)| needed.iter().any(|n| n.key() == app))
                .map(|(app, reason)| format!("{app}: {reason}"))
                .collect();
            if hits.is_empty() {
                Ok(())
            } else {
                Err(ProvisionError::retry(
                    "bundle_ensure_refused",
                    format!("licences refused to cover: {}", hits.join(", ")),
                ))
            }
        }
        Err(e) if e.status == Some(403) => {
            Err(ProvisionError::retry("bundle_ensure_forbidden", e.message))
        }
        Err(e) => Err(ProvisionError::retry("bundle_ensure_failed", e.message)),
    }
}

/// Every app of `tenant`, in order, stopping at the first that does not land: the bundle is ensured first (when any
/// configured app is still to provision). `Ok(apps)` = each app's outcome.
pub async fn provision_tenant(
    st: &AppState,
    tenant: Uuid,
    actor: &Actor,
) -> ApiResult<Result<Vec<(App, Provisioned)>, ProvisionError>> {
    let mut tx = begin_scoped(&st.db, tenant).await?;
    let done_apps: Vec<String> = sqlx::query_scalar(
        "SELECT app_key FROM tenant_app_licence WHERE tenant_id = $1 AND provisioned_at IS NOT NULL",
    )
    .bind(tenant)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let pending: Vec<App> = ALL
        .into_iter()
        .filter(|a| smoke_url(st, *a).is_some() && !done_apps.iter().any(|d| d == a.key()))
        .collect();
    if !pending.is_empty()
        && let Err(e) = ensure_bundle(st, tenant, &pending).await
    {
        tracing::warn!(%tenant, code = e.code, message = %e.message, "app_licence_bundle_ensure_failed");
        for app in &pending {
            record_failure(st, tenant, *app, &e).await;
        }
        return Ok(Err(e));
    }
    let mut done = Vec::with_capacity(ALL.len());
    for app in ALL {
        match provision_app(st, tenant, app, actor).await? {
            Ok(p) => done.push((app, p)),
            Err(e) => return Ok(Err(e)),
        }
    }
    Ok(Ok(done))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_the_licences_app_keys() {
        let keys: Vec<_> = ALL.iter().map(|a| a.key()).collect();
        assert_eq!(keys, ["inference-router", "strata-rag", "strata-ingest"]);
    }

    #[test]
    fn the_conveyor_app_is_minted_on_demand_and_never_provisioned() {
        assert_eq!(App::Conveyor.key(), "conveyor");
        assert!(
            !ALL.contains(&App::Conveyor),
            "like App::Mcp: minted when a connector asks, never provisioned up front"
        );
    }

    #[test]
    fn debug_never_shows_the_token() {
        let t = AppToken::new("super-secret-token");
        assert_eq!(format!("{t:?}"), "AppToken(<redacted>)");
        assert_eq!(t.expose(), "super-secret-token");
    }
}
