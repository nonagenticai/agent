//! strata_rag and strata_ingest data-plane clients (B3 T2; plan §1 "Upstream auth", §7, G1/G7).
//!
//! Every call goes through a [`Plane`]: the service's in-cluster base URL from config (`st.cfg.strata`), the
//! tenant's token and, for strata_rag only, `X-Tenant-Id`, `X-App-Id: agent` and `X-Actor-Id` (the caller's `sub` when
//! it is a non-nil uuid, else agent's service account `SERVICE_ACTOR_ID`, else none).
//! strata_ingest takes the token alone: its token decides the tenant. The token is, in order: the tenant's static
//! pair from config (`STRATA_RAG_TOKENS` / `STRATA_INGEST_TOKENS`: the operator override), else the tenant's own
//! licence for the app (`strata-rag` / `strata-ingest`, [`crate::tenants::app_licence::token`]); no any-tenant token
//! exists. A [`Plane`] is only built by [`gate`] (or [`plane`], which skips the provisioning check), which answers
//! before any call:
//! - 503 `unavailable` when the service is not configured in this deployment (no URL);
//! - 409 `provision_pending` when the tenant has neither a static pair nor a licence (G1);
//! - `provision::require_provisioned`: 409 `provision_pending` while the target's latest outbox row is pending, 503
//!   `provision_failed` when it failed or died.
//!
//! Calls are [`crate::upstream::Call`]s (timed by `observability::upstream_http`, errors mapped there). strata URLs
//! are config, never tenant input, so nothing here goes through `crate::netguard`.
pub mod ingest;
pub mod rag;

use std::fmt::Write as _;

use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::config::StrataConfig;
use crate::error::{ApiError, ApiResult};
use crate::provision::{self, Target};
use crate::state::AppState;
use crate::tenants::app_licence::{self, App};
use crate::upstream::{self, Auth, Call};

/// A store id in strata_rag (`stores.id`); strata_ingest jobs and store providers name the same ids.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StrataStoreId(pub Uuid);

impl std::fmt::Display for StrataStoreId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

/// Which strata service a call is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Service {
    Rag,
    Ingest,
}

impl Service {
    /// The provisioning target whose outbox state gates this service.
    pub fn target(self) -> Target {
        match self {
            Service::Rag => Target::StrataRag,
            Service::Ingest => Target::StrataIngest,
        }
    }

    /// The upstream name (metrics label, error text): `strata_rag` / `strata_ingest`.
    pub fn name(self) -> &'static str {
        self.target().as_str()
    }
}

/// The bearer of a [`Plane`]: the tenant's static pair from config, or its own licence token (owned). `Debug` never
/// prints it.
#[derive(Clone)]
enum PlaneToken<'a> {
    Static(&'a str),
    Tenant(std::sync::Arc<str>),
}

impl PlaneToken<'_> {
    fn expose(&self) -> &str {
        match self {
            PlaneToken::Static(t) => t,
            PlaneToken::Tenant(t) => t,
        }
    }
}

impl std::fmt::Debug for PlaneToken<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[redacted]")
    }
}

/// The data plane of one service for one tenant: where to call and as whom. Built by [`gate`] only. `Debug` never
/// prints the token.
#[derive(Clone)]
pub struct Plane<'a> {
    service: Service,
    base: &'a str,
    token: PlaneToken<'a>,
    tenant: Uuid,
    actor: Option<Uuid>,
    /// `SERVICE_ACTOR_ID`: the actor when no person's `sub` is in scope.
    service_actor: Option<Uuid>,
    /// Set only for a TENANT licence token (never a static pair): a 401 from strata drops the cached token, re-mints
    /// and resends once ([`crate::upstream::Call::renewal`]). A plane built without state ([`static_plane`]) has none.
    renew: Option<app_licence::Renewal>,
}

impl std::fmt::Debug for Plane<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Plane")
            .field("service", &self.service)
            .field("base", &self.base)
            .field("token", &self.token)
            .field("tenant", &self.tenant)
            .field("actor", &self.actor)
            .finish()
    }
}

impl<'a> Plane<'a> {
    pub fn service(&self) -> Service {
        self.service
    }

    pub fn tenant(&self) -> Uuid {
        self.tenant
    }

    /// The acting person, sent as `X-Actor-Id` (strata_rag only) when `sub` is a non-nil uuid; any other `sub`
    /// (a service account name, an empty string) leaves agent's service account (`SERVICE_ACTOR_ID`), else
    /// nothing. Never invented.
    pub fn with_actor(mut self, sub: &str) -> Self {
        self.actor = crate::providers::llm::caller_actor(Some(sub), self.service_actor);
        self
    }

    /// The headers this plane sends: rag = token + tenant (+ actor); ingest = token only.
    /// The tenant-token renewal of this plane (`None` for a static operator pair or a plane built without state).
    pub fn renewal(&self) -> Option<&app_licence::Renewal> {
        self.renew.as_ref()
    }

    pub fn auth(&self) -> Auth<'_> {
        match self.service {
            Service::Rag => Auth {
                token: Some(self.token.expose()),
                tenant: Some(self.tenant),
                actor: self.actor,
                app: Some(crate::providers::llm::APP_ID),
            },
            Service::Ingest => Auth {
                token: Some(self.token.expose()),
                tenant: None,
                actor: None,
                app: None,
            },
        }
    }

    /// One call to `path` (path and query, starting with `/`) on this plane; `op` labels the metric.
    pub fn call<'b>(&'b self, op: &'static str, method: Method, path: &'b str) -> Call<'b> {
        Call::new(self.service.name(), op, self.base, method, path)
            .auth(self.auth())
            .renewal(self.renew.clone())
    }

    /// A 2xx body as `T`; a body of another shape is 503 `unavailable` (logged without the body).
    pub fn decode<T: serde::de::DeserializeOwned>(
        &self,
        op: &'static str,
        body: Value,
    ) -> ApiResult<T> {
        serde_json::from_value(body).map_err(|e| {
            tracing::warn!(
                upstream = self.service.name(),
                op,
                category = ?e.classify(),
                "unexpected upstream response shape"
            );
            upstream::unavailable(self.service.name())
        })
    }
}

/// The URL and static token pair of `service` for `tenant` in config: 503 `unavailable` without a URL; the pair is
/// `None` when the tenant has none.
fn configured(
    cfg: &StrataConfig,
    tenant: Uuid,
    service: Service,
) -> ApiResult<(&str, Option<&str>)> {
    let (url, tokens) = match service {
        Service::Rag => (cfg.rag_url.as_deref(), &cfg.rag_tokens),
        Service::Ingest => (cfg.ingest_url.as_deref(), &cfg.ingest_tokens),
    };
    let Some(base) = url else {
        return Err(upstream::unavailable(service.name()));
    };
    Ok((base, tokens.get(tenant)))
}

fn pending() -> ApiError {
    ApiError::Conflict(
        "provision_pending",
        "Knowledge is still being set up for this tenant.".to_string(),
    )
}

fn built<'a>(
    cfg: &StrataConfig,
    base: &'a str,
    service: Service,
    tenant: Uuid,
    token: PlaneToken<'a>,
) -> Plane<'a> {
    Plane {
        service,
        base,
        token,
        tenant,
        actor: cfg.service_actor_id,
        service_actor: cfg.service_actor_id,
        renew: None,
    }
}

/// The plane of `service` for `tenant` from config alone (no database), static pair only: 503 `unavailable` without
/// a URL, 409 `provision_pending` without the tenant's pair. Handlers use [`gate`]; code with state uses [`plane`].
pub fn static_plane(cfg: &StrataConfig, tenant: Uuid, service: Service) -> ApiResult<Plane<'_>> {
    let (base, token) = configured(cfg, tenant, service)?;
    let token = token.ok_or_else(pending)?;
    Ok(built(cfg, base, service, tenant, PlaneToken::Static(token)))
}

/// The licence app of `service`.
fn app_of(service: Service) -> App {
    match service {
        Service::Rag => App::StrataRag,
        Service::Ingest => App::StrataIngest,
    }
}

/// The plane of `service` for `tenant`, token resolved on `conn` (a transaction scoped to `tenant`): the tenant's
/// static pair (operator override) first, else its own licence ([`app_licence::token`]), else 409 `provision_pending`
/// (and 503 `unavailable` without a URL). It does not check the provisioning outbox ([`gate`] does).
pub async fn plane_in<'a>(
    st: &'a AppState,
    conn: &mut PgConnection,
    tenant: Uuid,
    service: Service,
) -> ApiResult<Plane<'a>> {
    let cfg = &st.cfg.strata;
    let (base, token) = configured(cfg, tenant, service)?;
    let token = match token {
        Some(t) => PlaneToken::Static(t),
        None => match app_licence::token(st, conn, tenant, app_of(service)).await? {
            Some(t) => PlaneToken::Tenant(std::sync::Arc::from(t.expose())),
            None => return Err(pending()),
        },
    };
    let renewable = matches!(token, PlaneToken::Tenant(_));
    let mut p = built(cfg, base, service, tenant, token);
    if renewable {
        p.renew = Some(app_licence::Renewal::new(st, tenant, app_of(service)));
    }
    Ok(p)
}

/// [`plane_in`] in its own short transaction scoped to `tenant` (dropped before any strata call), for callers that
/// hold no transaction. A static pair needs no database read at all.
pub async fn plane<'a>(st: &'a AppState, tenant: Uuid, service: Service) -> ApiResult<Plane<'a>> {
    let (_, token) = configured(&st.cfg.strata, tenant, service)?;
    if token.is_some() {
        return static_plane(&st.cfg.strata, tenant, service);
    }
    let mut tx = crate::tenancy::begin_scoped(&st.db, tenant).await?;
    let p = plane_in(st, &mut tx, tenant, service).await;
    drop(tx);
    p
}

/// The data-plane gate (plan §1, G1), run in the request's tenant-scoped transaction before any strata call:
/// [`plane_in`], then `provision::require_provisioned(conn, service.target())`. It takes no row lock; never hold the
/// transaction's locks across the call that follows.
pub async fn gate<'a>(
    st: &'a AppState,
    conn: &mut PgConnection,
    tenant: Uuid,
    service: Service,
) -> ApiResult<Plane<'a>> {
    let p = plane_in(st, &mut *conn, tenant, service).await?;
    provision::require_provisioned(conn, service.target()).await?;
    Ok(p)
}

/// `s` percent-encoded for a path segment or a query value (RFC 3986 unreserved characters kept).
pub(crate) fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// `?k=v&…` of the pairs that are set (values encoded), or `""`.
pub(crate) fn query_string(pairs: &[(&str, Option<String>)]) -> String {
    let mut out = String::new();
    for (k, v) in pairs {
        if let Some(v) = v {
            out.push(if out.is_empty() { '?' } else { '&' });
            out.push_str(k);
            out.push('=');
            out.push_str(&enc(v));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// agent's service account (`SERVICE_ACTOR_ID`) in these tests.
    const SERVICE_ACTOR: Uuid = Uuid::from_u128(0x0195_f48b_d000_7000_8000_0000_0000_00aa);

    fn cfg(tenant: Uuid, service_actor: Option<Uuid>) -> StrataConfig {
        let pairs = format!("{tenant}:tok-rag");
        StrataConfig {
            rag_url: Some("http://strata-rag.test".into()),
            ingest_url: Some("http://strata-ingest.test".into()),
            rag_tokens: crate::config::TenantTokens::parse("STRATA_RAG_TOKENS", &pairs).unwrap(),
            ingest_tokens: crate::config::TenantTokens::parse("STRATA_INGEST_TOKENS", &pairs)
                .unwrap(),
            service_actor_id: service_actor,
        }
    }

    /// The headers `p.auth()` puts on a request (built, not sent).
    fn sent(p: &Plane<'_>) -> reqwest::header::HeaderMap {
        p.auth()
            .apply(reqwest::Client::new().get("http://strata.test/x"))
            .build()
            .unwrap()
            .headers()
            .clone()
    }

    fn header(h: &reqwest::header::HeaderMap, k: &str) -> Option<String> {
        h.get(k).and_then(|v| v.to_str().ok()).map(String::from)
    }

    #[test]
    fn strata_rag_is_told_the_app_and_the_actor_falls_back_to_the_service_account() {
        let tenant = Uuid::new_v4();
        let person = Uuid::new_v4();
        let c = cfg(tenant, Some(SERVICE_ACTOR));
        let rag = static_plane(&c, tenant, Service::Rag).unwrap();
        // no sub in scope: the service account
        let h = sent(&rag);
        assert_eq!(header(&h, "x-app-id").as_deref(), Some("agent"));
        assert_eq!(header(&h, "x-actor-id"), Some(SERVICE_ACTOR.to_string()));
        assert_eq!(header(&h, "x-tenant-id"), Some(tenant.to_string()));
        // a person's sub wins; a sub that is no uuid (or nil) leaves the service account
        let h = sent(&rag.clone().with_actor(&person.to_string()));
        assert_eq!(header(&h, "x-actor-id"), Some(person.to_string()));
        for sub in [
            "service-account-agent",
            "",
            "00000000-0000-0000-0000-000000000000",
        ] {
            let h = sent(&rag.clone().with_actor(&person.to_string()).with_actor(sub));
            assert_eq!(
                header(&h, "x-actor-id"),
                Some(SERVICE_ACTOR.to_string()),
                "{sub:?}"
            );
        }
        // no SERVICE_ACTOR_ID: no actor, never an invented one; the app is still named
        let c = cfg(tenant, None);
        let h = sent(
            &static_plane(&c, tenant, Service::Rag)
                .unwrap()
                .with_actor("svc"),
        );
        assert_eq!(header(&h, "x-actor-id"), None);
        assert_eq!(header(&h, "x-app-id").as_deref(), Some("agent"));
    }

    /// A tenant-licence token is sent as the bearer, and no plane prints its token.
    #[test]
    fn a_tenant_token_is_the_bearer_and_debug_never_prints_a_token() {
        let tenant = Uuid::new_v4();
        let c = cfg(tenant, None);
        let (base, _) = configured(&c, tenant, Service::Rag).unwrap();
        let p = built(
            &c,
            base,
            Service::Rag,
            tenant,
            PlaneToken::Tenant(std::sync::Arc::from("licence-secret")),
        );
        let h = sent(&p);
        assert_eq!(
            header(&h, "authorization").as_deref(),
            Some("Bearer licence-secret")
        );
        assert_eq!(header(&h, "x-tenant-id"), Some(tenant.to_string()));
        let shown = format!("{p:?}");
        assert!(
            !shown.contains("licence-secret") && !shown.contains("tok-rag"),
            "{shown}"
        );
        let st = static_plane(&c, tenant, Service::Rag).unwrap();
        assert!(!format!("{st:?}").contains("tok-rag"));
        // another tenant has no static pair: pending, never another tenant's token
        let other = static_plane(&c, Uuid::new_v4(), Service::Rag).unwrap_err();
        assert_eq!(other.code(), "provision_pending");
    }

    #[test]
    fn strata_ingest_gets_the_token_alone() {
        let tenant = Uuid::new_v4();
        let c = cfg(tenant, Some(SERVICE_ACTOR));
        let h = sent(&static_plane(&c, tenant, Service::Ingest).unwrap());
        assert_eq!(
            header(&h, "authorization").as_deref(),
            Some("Bearer tok-rag")
        );
        for k in ["x-app-id", "x-actor-id", "x-tenant-id"] {
            assert_eq!(header(&h, k), None, "{k}");
        }
    }
}
