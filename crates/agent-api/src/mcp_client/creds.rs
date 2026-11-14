//! Per-tenant MCP credentials for the agent platform (care-bus `mcp_credentials.rs`, agent flavour).
//!
//! Every MCP call for tenant T carries T's own credentials: its `mcp` LICENCE (a bundle token minted on demand by
//! [`BundleMinter`], never stored) and its BINDING token (issued once by the MCP server for (T, product `agent`),
//! stored SEALED in `tenant_mcp_binding` under T's data key, AAD `tenant ‖ "mcp-binding" ‖ binding_id`). A tenant with
//! no stored binding is ensured lazily: `POST {mcp}/mcp/internal/tenants/{T}/bindings/ensure` as the Keycloak client
//! `agent-bundle-minter`. `200 token_already_issued` with no local row (the token was lost) is rotated.
//!
//! When the server REFUSED the previous credentials (`fresh`), the licence is minted anew; a binding refused twice in
//! a row is rotated. Nothing here logs or prints a token.
use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use sqlx::PgPool;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::config::BundleMinterConfig;
use crate::connectors::custody::{open_fields, seal_fields};
use crate::crypto::keyring::Keyring;
use crate::http_base::{BaseUrl, BaseUrlError};
use crate::mcp_client::client::{CredentialSource, McpCredentials};
use crate::service_token::form;
use crate::tenancy::begin_scoped;
use crate::tenants::app_licence::{App, BundleMinter, SKEW};

/// The product the MCP server registers an agent-platform binding under.
pub const PRODUCT: &str = "agent";
/// Keycloak audience of the MCP server's internal API. The `agent-bundle-minter` client's token carries it through
/// the `nonagenticai-mcp-internal-audience` mapper (gitops keycloak realm patch); nothing is requested per call.
pub const INTERNAL_AUDIENCE: &str = "nonagenticai-mcp-internal";
/// Two refusals of the same binding within this window mean the stored token is bad.
const REFUSAL_WINDOW: Duration = Duration::from_secs(600);
/// Refusals of the binding token that rotate it.
const ROTATE_AFTER_REFUSALS: u32 = 2;
/// A stored-token binding is re-ensured at most once per this window, per process, so a toolkit published after the
/// binding was made still reaches the tenant without rotating its token.
const ENSURE_TTL: Duration = Duration::from_secs(300);
/// Lifetime assumed for a licence whose expiry cannot be read (the minter's cache hides it): short, so a stale guess
/// costs one refusal and a fresh mint, never a long outage.
const FALLBACK_LIFE: Duration = Duration::from_secs(300);
const MAX_BODY: usize = 64 * 1024;

/// The AAD of the sealed binding token: `tenant ‖ "mcp-binding" ‖ binding_id`.
pub fn binding_aad(tenant: Uuid, binding_id: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + 11 + binding_id.len());
    out.extend_from_slice(tenant.as_bytes());
    out.extend_from_slice(b"mcp-binding");
    out.extend_from_slice(binding_id.as_bytes());
    out
}

/// Time until the `exp` claim of a JWT, when `token` is one (no signature check: this only sizes a cache).
fn jwt_life(token: &str) -> Option<Duration> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    let exp = serde_json::from_slice::<Value>(&bytes)
        .ok()?
        .get("exp")?
        .as_i64()?;
    let left = exp - chrono::Utc::now().timestamp();
    Some(Duration::from_secs(left.max(0) as u64))
}

/// What the server answered to `bindings/ensure`.
#[derive(Debug, PartialEq, Eq)]
enum Ensured {
    /// A new token (201).
    Issued { binding_id: String, token: String },
    /// A binding exists and its token was issued before (200): not retrievable, rotate it.
    AlreadyIssued,
}

impl Ensured {
    fn parse(status: u16, body: &Value) -> Result<Self> {
        let s = |k: &str| {
            body.get(k)
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
        };
        if body.get("token_already_issued").and_then(Value::as_bool) == Some(true)
            && s("token").is_none()
        {
            return Ok(Ensured::AlreadyIssued);
        }
        match status {
            200 | 201 => {
                let token = s("token")
                    .or_else(|| s("binding_token"))
                    .context("the MCP server's ensure answer carries no token")?;
                let binding_id = s("binding_id")
                    .or_else(|| s("id"))
                    .context("the MCP server's ensure answer carries no binding_id")?;
                Ok(Ensured::Issued {
                    binding_id: binding_id.to_string(),
                    token: token.to_string(),
                })
            }
            other => bail!("the MCP server refused the binding ensure (HTTP {other})"),
        }
    }
}

/// [`CredentialSource`] of the agent platform (API and runtime processes alike).
pub struct AgentMcpSource {
    db: PgPool,
    keyring: Option<Arc<Keyring>>,
    minter: Arc<BundleMinter>,
    /// The minter's Keycloak client (`agent-bundle-minter`); `None` = unconfigured, every fetch fails.
    client: Option<BundleMinterConfig>,
    /// [`Self::client`]'s token endpoint, parsed once; `Err` fails every fetch closed with the reason.
    client_token_url: Option<Result<BaseUrl, BaseUrlError>>,
    http: reqwest::Client,
    /// The MCP server's base URL (deployment/tenant-configured), parsed once.
    ///
    /// TRUST DECISION: fetching the MCP endpoint a tenant's configuration names is this source's purpose — it makes
    /// the call on that tenant's behalf, so following the configured endpoint is not a request forgery. What the parse
    /// restricts is its SHAPE: `http`/`https` only (never `file:`, `ftp:` or another scheme), a host, no query or
    /// fragment, and every appended path segment percent-encoded under the base. `Err` (an unusable URL) fails every
    /// fetch closed rather than sending a raw string.
    mcp_base: Result<BaseUrl, BaseUrlError>,
    client_token: Mutex<Option<(String, Instant)>>,
    refusals: StdMutex<HashMap<Uuid, (u32, Instant)>>,
    /// When each tenant's stored-token binding was last re-ensured (see [`ENSURE_TTL`]).
    ensured: StdMutex<HashMap<Uuid, Instant>>,
}

impl std::fmt::Debug for AgentMcpSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentMcpSource")
            .field("mcp_base", &self.mcp_base)
            .field("sealing", &self.keyring.is_some())
            .finish_non_exhaustive()
    }
}

impl AgentMcpSource {
    pub fn new(
        db: PgPool,
        keyring: Option<Arc<Keyring>>,
        minter: Arc<BundleMinter>,
        client: Option<BundleMinterConfig>,
        http: reqwest::Client,
        mcp_base: &str,
    ) -> Self {
        let client_token_url = client.as_ref().map(|c| BaseUrl::parse(&c.token_url));
        Self {
            db,
            keyring,
            minter,
            client,
            client_token_url,
            http,
            mcp_base: BaseUrl::parse(mcp_base),
            client_token: Mutex::new(None),
            refusals: StdMutex::new(HashMap::new()),
            ensured: StdMutex::new(HashMap::new()),
        }
    }

    /// Counts a refusal of `tenant`'s credentials; true when the binding token itself should be rotated.
    fn refused(&self, tenant: Uuid) -> bool {
        let mut m = self.refusals.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let e = m.entry(tenant).or_insert((0, now));
        if now.duration_since(e.1) > REFUSAL_WINDOW {
            *e = (0, now);
        }
        e.0 += 1;
        e.1 = now;
        e.0 >= ROTATE_AFTER_REFUSALS
    }

    fn forget_refusals(&self, tenant: Uuid) {
        self.refusals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&tenant);
    }

    /// `tenant`'s `mcp` licence token and when it stops being usable.
    async fn licence(&self, tenant: Uuid, fresh: bool) -> Result<(String, String, Instant)> {
        if fresh {
            self.minter.invalidate(tenant, App::Mcp);
        }
        let m = self
            .minter
            .mint(tenant, App::Mcp)
            .await
            .map_err(|e| anyhow!("the mcp licence cannot be minted: {e:?}"))?;
        let token = m.token.expose().to_string();
        let life = jwt_life(&token)
            .unwrap_or(FALLBACK_LIFE)
            .saturating_sub(SKEW);
        if life.is_zero() {
            bail!("the minted mcp licence is already expired");
        }
        Ok((token, m.licence_id, Instant::now() + life))
    }

    /// The stored binding token: `(binding_id, token)`. `None` = no row, or one that no longer opens.
    async fn stored(&self, tenant: Uuid) -> Result<Option<(String, String)>> {
        let mut tx = begin_scoped(&self.db, tenant)
            .await
            .map_err(|e| anyhow!("{e}"))?;
        let row: Option<(String, Vec<u8>, Vec<u8>, i32)> = sqlx::query_as(
            "SELECT binding_id, sealed_token, nonce, dek_version FROM tenant_mcp_binding WHERE tenant_id = $1",
        )
        .bind(tenant)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((binding_id, ct, nonce, dek_version)) = row else {
            return Ok(None);
        };
        let opened = open_fields(
            &mut tx,
            self.keyring.as_deref(),
            tenant,
            &binding_aad(tenant, &binding_id),
            &ct,
            &nonce,
            dek_version,
        )
        .await;
        tx.commit().await?;
        match opened {
            Ok(f) => Ok(f
                .get("token")
                .and_then(Value::as_str)
                .map(|t| (binding_id, t.to_string()))),
            Err(e) => {
                tracing::warn!(%tenant, error = %e, "mcp binding token unreadable; it will be re-issued");
                Ok(None)
            }
        }
    }

    async fn store(
        &self,
        tenant: Uuid,
        binding_id: &str,
        token: &str,
        licence_id: &str,
        rotated: bool,
    ) -> Result<()> {
        let mut tx = begin_scoped(&self.db, tenant)
            .await
            .map_err(|e| anyhow!("{e}"))?;
        let mut fields = Map::new();
        fields.insert("token".into(), Value::String(token.to_string()));
        let blob = seal_fields(
            &mut tx,
            self.keyring.as_deref(),
            tenant,
            &binding_aad(tenant, binding_id),
            &fields,
        )
        .await
        .map_err(|e| anyhow!("the mcp binding token cannot be sealed: {e}"))?;
        sqlx::query(
            "INSERT INTO tenant_mcp_binding
                 (tenant_id, binding_id, sealed_token, nonce, dek_version, fingerprint, licence_id, product, rotated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, CASE WHEN $9 THEN now() END)
             ON CONFLICT (tenant_id) DO UPDATE
                SET binding_id = EXCLUDED.binding_id, sealed_token = EXCLUDED.sealed_token, nonce = EXCLUDED.nonce,
                    dek_version = EXCLUDED.dek_version, fingerprint = EXCLUDED.fingerprint,
                    licence_id = EXCLUDED.licence_id, ensured_at = now(),
                    rotated_at = CASE WHEN $9 THEN now() ELSE tenant_mcp_binding.rotated_at END",
        )
        .bind(tenant)
        .bind(binding_id)
        .bind(&blob.ciphertext)
        .bind(&blob.nonce)
        .bind(blob.dek_version)
        .bind(&blob.fingerprint)
        .bind(licence_id)
        .bind(PRODUCT)
        .bind(rotated)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// The `agent-bundle-minter` client's access token for the MCP internal API (cached until near expiry).
    async fn client_access_token(&self, drop_cached: bool) -> Result<String> {
        let c = self
            .client
            .as_ref()
            .context("the MCP binding client (BUNDLE_MINTER_*) is not configured")?;
        let token_url = match self.client_token_url.as_ref() {
            Some(Ok(base)) => base,
            Some(Err(e)) => {
                bail!("the MCP binding client token endpoint is unusable: {e}")
            }
            None => bail!("the MCP binding client (BUNDLE_MINTER_*) is not configured"),
        };
        let mut cached = self.client_token.lock().await;
        if drop_cached {
            *cached = None;
        }
        if let Some((t, until)) = cached.as_ref()
            && Instant::now() < *until
        {
            return Ok(t.clone());
        }
        let body = form(&[
            ("grant_type", "client_credentials"),
            ("client_id", c.client_id.as_str()),
            ("client_secret", c.client_secret.as_str()),
        ]);
        let res = self
            .http
            .post(token_url.endpoint(&[]))
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(body)
            .send()
            .await
            .map_err(|e| {
                anyhow!(
                    "MCP internal client authentication unavailable: {}",
                    e.without_url()
                )
            })?;
        if res.status().as_u16() != 200 {
            bail!(
                "MCP internal client authentication failed (HTTP {})",
                res.status().as_u16()
            );
        }
        let bytes = res
            .bytes()
            .await
            .context("MCP internal client token unreadable")?;
        if bytes.len() > MAX_BODY {
            bail!("MCP internal client token response rejected");
        }
        let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        let token = v
            .get("access_token")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty() && t.len() <= 8192)
            .context("MCP internal client token rejected")?;
        let life = v.get("expires_in").and_then(Value::as_u64).unwrap_or(0);
        let until = Instant::now() + Duration::from_secs(life).saturating_sub(SKEW);
        if until > Instant::now() {
            *cached = Some((token.to_string(), until));
        }
        Ok(token.to_string())
    }

    /// `POST bindings/ensure` (one retry on 401 with a fresh client token).
    async fn ensure(
        &self,
        tenant: Uuid,
        licence_id: &str,
        licence_token: &str,
        rotate: bool,
    ) -> Result<Ensured> {
        let url = self
            .mcp_base
            .as_ref()
            .map_err(|e| anyhow!("the MCP server base URL is unusable: {e}"))?
            .endpoint(&[
                "mcp",
                "internal",
                "tenants",
                &tenant.to_string(),
                "bindings",
                "ensure",
            ]);
        let mut body =
            json!({"product": PRODUCT, "licence_id": licence_id, "licence_token": licence_token});
        if rotate {
            body["rotate"] = Value::Bool(true);
        }
        let mut retried = false;
        loop {
            let bearer = self.client_access_token(retried).await?;
            let res = self
                .http
                .post(url.as_str())
                .bearer_auth(bearer)
                .json(&body)
                .send()
                .await
                .map_err(|e| {
                    anyhow!(
                        "the MCP server is unreachable for the binding ensure: {}",
                        e.without_url()
                    )
                })?;
            let status = res.status().as_u16();
            if status == 401 && !retried {
                retried = true;
                continue;
            }
            let bytes = res
                .bytes()
                .await
                .context("the MCP ensure answer is unreadable")?;
            if bytes.len() > MAX_BODY {
                bail!("the MCP ensure answer was rejected");
            }
            let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            return Ensured::parse(status, &v);
        }
    }

    /// Re-ensures the tenant's binding so it follows the latest published toolkit versions. Always
    /// `rotate: false`: the stored token is kept. Best effort — a failure leaves the token in use.
    async fn refresh_binding(
        &self,
        tenant: Uuid,
        licence_id: &str,
        licence_token: &str,
    ) -> Option<Ensured> {
        {
            let seen = self.ensured.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(at) = seen.get(&tenant)
                && Instant::now().duration_since(*at) < ENSURE_TTL
            {
                return None;
            }
        }
        let answer = self.ensure(tenant, licence_id, licence_token, false).await;
        // Recorded whatever the outcome: a failing server must not be hit on every request within the window.
        self.ensured
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(tenant, Instant::now());
        match answer {
            Ok(Ensured::Issued { binding_id, token }) => {
                Some(Ensured::Issued { binding_id, token })
            }
            Ok(Ensured::AlreadyIssued) => None,
            Err(e) => {
                tracing::warn!(
                    %tenant,
                    error = %e,
                    "mcp binding re-ensure failed; the stored token stays in use"
                );
                None
            }
        }
    }

    async fn credentials(&self, tenant: Uuid, fresh: bool) -> Result<McpCredentials> {
        let rotate_bad = fresh && self.refused(tenant);
        let (licence_token, licence_id, expires) = self.licence(tenant, fresh).await?;
        let stored = if rotate_bad {
            None
        } else {
            self.stored(tenant).await?
        };
        let (binding_token, rotated) = match stored {
            Some((_, stored_token)) => {
                // Follow the latest published toolkit versions without rotating the stored token.
                let token = match self
                    .refresh_binding(tenant, &licence_id, &licence_token)
                    .await
                {
                    Some(Ensured::Issued { binding_id, token }) => {
                        self.store(tenant, &binding_id, &token, &licence_id, false)
                            .await?;
                        token
                    }
                    _ => stored_token,
                };
                (token, false)
            }
            None => {
                // No usable token here: ask for one; when the server says it issued one before, rotate it.
                let first = self
                    .ensure(tenant, &licence_id, &licence_token, rotate_bad)
                    .await?;
                let (issued, rotated) = match first {
                    Ensured::Issued { binding_id, token } => ((binding_id, token), rotate_bad),
                    Ensured::AlreadyIssued => match self
                        .ensure(tenant, &licence_id, &licence_token, true)
                        .await?
                    {
                        Ensured::Issued { binding_id, token } => ((binding_id, token), true),
                        Ensured::AlreadyIssued => {
                            bail!("the MCP server would not rotate the binding token")
                        }
                    },
                };
                self.store(tenant, &issued.0, &issued.1, &licence_id, rotated)
                    .await?;
                if rotated {
                    self.forget_refusals(tenant);
                }
                (issued.1, rotated)
            }
        };
        let _ = rotated;
        McpCredentials::new(&licence_token, &binding_token, expires)
    }
}

impl CredentialSource for AgentMcpSource {
    fn fetch(&self, tenant: Uuid, fresh: bool) -> BoxFuture<'_, Result<McpCredentials>> {
        Box::pin(self.credentials(tenant, fresh))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_aad_binds_tenant_purpose_and_binding() {
        let t = Uuid::from_u128(7);
        let a = binding_aad(t, "b1");
        assert_eq!(&a[..16], t.as_bytes());
        assert_eq!(&a[16..27], b"mcp-binding");
        assert_eq!(&a[27..], b"b1");
        assert_ne!(a, binding_aad(t, "b2"));
    }

    #[test]
    fn ensure_answers_are_read_strictly() {
        let issued = json!({"binding_id": "b1", "token": "tok"});
        assert_eq!(
            Ensured::parse(201, &issued).unwrap(),
            Ensured::Issued {
                binding_id: "b1".into(),
                token: "tok".into()
            }
        );
        let again = json!({"binding_id": "b1", "token_already_issued": true});
        assert_eq!(Ensured::parse(200, &again).unwrap(), Ensured::AlreadyIssued);
        assert!(Ensured::parse(201, &json!({"binding_id": "b1"})).is_err());
        assert!(Ensured::parse(403, &json!({})).is_err());
    }

    #[test]
    fn jwt_life_reads_exp_and_ignores_opaque_tokens() {
        let exp = chrono::Utc::now().timestamp() + 600;
        let payload =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!("{{\"exp\":{exp}}}"));
        let life = jwt_life(&format!("h.{payload}.s")).unwrap();
        assert!(life > Duration::from_secs(590) && life <= Duration::from_secs(600));
        assert!(jwt_life("opaque-token").is_none());
    }

    #[tokio::test]
    async fn two_refusals_rotate_and_a_quiet_spell_resets() {
        let src = AgentMcpSource::new(
            PgPool::connect_lazy("postgres://x@localhost/x").unwrap(),
            None,
            Arc::new(BundleMinter::new(None, reqwest::Client::new())),
            None,
            reqwest::Client::new(),
            "http://mcp.test/",
        );
        let t = Uuid::from_u128(1);
        assert!(!src.refused(t));
        assert!(src.refused(t));
        src.forget_refusals(t);
        assert!(!src.refused(t));
        assert_eq!(src.mcp_base.as_ref().unwrap().as_str(), "http://mcp.test/");
    }

    use axum::response::IntoResponse as _;

    const TENANT: Uuid = Uuid::from_u128(0x7e11);
    const OTHER_TENANT: Uuid = Uuid::from_u128(0x7e12);

    #[derive(Clone, Copy)]
    enum Reply {
        Issued,
        Already,
        Fail,
    }

    #[derive(Clone)]
    struct Fake {
        reply: Reply,
        /// Every `bindings/ensure` request body, in arrival order.
        ensures: Arc<StdMutex<Vec<Value>>>,
    }

    async fn token_route() -> axum::Json<Value> {
        axum::Json(json!({"access_token": "svc", "expires_in": 600}))
    }

    async fn ensure_route(
        axum::extract::State(fake): axum::extract::State<Fake>,
        axum::extract::Path(_tenant): axum::extract::Path<String>,
        body: axum::body::Bytes,
    ) -> axum::response::Response {
        let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        fake.ensures
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(v);
        match fake.reply {
            Reply::Issued => (
                axum::http::StatusCode::CREATED,
                axum::Json(json!({"binding_id": "b1", "token": "tok-new"})),
            )
                .into_response(),
            Reply::Already => (
                axum::http::StatusCode::OK,
                axum::Json(json!({"binding_id": "b1", "token_already_issued": true})),
            )
                .into_response(),
            Reply::Fail => (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({"error": "boom"})),
            )
                .into_response(),
        }
    }

    /// The MCP server's view: `/token` for the client credentials, `bindings/ensure` for the binding.
    async fn fake_mcp(reply: Reply) -> (String, Fake) {
        let fake = Fake {
            reply,
            ensures: Arc::new(StdMutex::new(Vec::new())),
        };
        let app = axum::Router::new()
            .route("/token", axum::routing::post(token_route))
            .route(
                "/mcp/internal/tenants/{t}/bindings/ensure",
                axum::routing::post(ensure_route),
            )
            .with_state(fake.clone());
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        (format!("http://{addr}"), fake)
    }

    fn source_at(base: &str) -> AgentMcpSource {
        AgentMcpSource::new(
            PgPool::connect_lazy("postgres://x@localhost/x").unwrap(),
            None,
            Arc::new(BundleMinter::new(None, reqwest::Client::new())),
            Some(BundleMinterConfig {
                licenses_url: format!("{base}/licenses"),
                token_url: format!("{base}/token"),
                client_id: "agent-bundle-minter".into(),
                client_secret: "secret".into(),
                bundle: PRODUCT.into(),
                ttl_minutes: 15,
            }),
            reqwest::Client::new(),
            base,
        )
    }

    fn seen(fake: &Fake) -> Vec<Value> {
        fake.ensures
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    #[tokio::test]
    async fn a_stored_token_ensure_re_pins_without_rotating() {
        let (base, fake) = fake_mcp(Reply::Already).await;
        let src = source_at(&base);
        let got = src.refresh_binding(TENANT, "lic", "tok").await;
        assert!(got.is_none());
        let bodies = seen(&fake);
        assert_eq!(bodies.len(), 1, "exactly one ensure request");
        assert_eq!(
            bodies[0].get("product").and_then(Value::as_str),
            Some(PRODUCT)
        );
        // Never rotates: the body omits `rotate` (it is only sent when true) and never carries `true`.
        assert_ne!(bodies[0].get("rotate").and_then(Value::as_bool), Some(true));
    }

    #[tokio::test]
    async fn an_issued_token_from_a_re_ensure_is_returned_to_be_stored() {
        let (base, fake) = fake_mcp(Reply::Issued).await;
        let src = source_at(&base);
        let got = src.refresh_binding(TENANT, "lic", "tok").await;
        match got {
            Some(Ensured::Issued { binding_id, token }) => {
                assert_eq!(binding_id, "b1");
                assert_eq!(token, "tok-new");
            }
            other => panic!("expected an issued binding, got {other:?}"),
        }
        assert_eq!(seen(&fake).len(), 1);
    }

    #[tokio::test]
    async fn a_failed_re_ensure_is_swallowed() {
        let (base, fake) = fake_mcp(Reply::Fail).await;
        let src = source_at(&base);
        let got = src.refresh_binding(TENANT, "lic", "tok").await;
        assert!(got.is_none());
        assert_eq!(seen(&fake).len(), 1);
    }

    #[tokio::test]
    async fn the_re_ensure_is_not_repeated_inside_the_ttl() {
        let (base, fake) = fake_mcp(Reply::Already).await;
        let src = source_at(&base);
        let first = src.refresh_binding(TENANT, "lic", "tok").await;
        assert!(first.is_none());
        let again = src.refresh_binding(TENANT, "lic", "tok").await;
        assert!(again.is_none());
        assert_eq!(seen(&fake).len(), 1, "one request per tenant per TTL");
        let other = src.refresh_binding(OTHER_TENANT, "lic", "tok").await;
        assert!(other.is_none());
        assert_eq!(seen(&fake).len(), 2, "another tenant has its own window");
    }
}
