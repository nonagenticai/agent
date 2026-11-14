//! Keycloak JWT verification and the resolved caller. After care-bus-agents@45ee128
//! `crates/carebus-api/src/auth.rs` (D53 renames), without its static service tokens and release-writer.
//!
//! Issuers (realm per app, A1; commonpractice #480): a token's UNVERIFIED `iss` selects one entry of
//! `KEYCLOAK_TRUSTED_ISSUERS` (default `[KEYCLOAK_ISSUER]`); a token whose `iss` is not listed is 401 before any key
//! lookup. Each issuer has its own JWKS cache (`KEYCLOAK_JWKS_URL` for `KEYCLOAK_ISSUER`, else
//! `<iss>/protocol/openid-connect/certs`) and the token is then validated with `iss` pinned to exactly that issuer, so
//! one realm's key can never vouch for another realm's `iss`. A realm in `KEYCLOAK_SERVICE_ONLY_REALMS` (secure-apps)
//! verifies only as a service account; each signup door pins its own issuer (crate::signup).
//!
//! Two kinds of bearer reach this API (today both from the portal realm, `KEYCLOAK_ISSUER`):
//! - a PERSON (the web app, client `agent`): `aud` must contain `KEYCLOAK_AUDIENCE` AND `azp` must equal
//!   `KEYCLOAK_CLIENT_ID` (default: the audience). The `azp` pin is what stops cross-app replay: portal-realm
//!   clients have `fullScopeAllowed`, so a person token issued to ANOTHER app (care-bus, strata-configurator,
//!   w2v, ...) also carries `aud: agent` and `resource_access.agent.roles` — only `azp` says who it was issued
//!   to. The one role read is the client role `super_admin` of that client
//!   (`resource_access[<audience>].roles`), `realm_access` is never read for people. Tenant and tenant role
//!   come from agent's own `users` table, through `agent_auth_user`.
//! - a SERVICE ACCOUNT (the early-access intake handler, chat-bridge's signup decider): verified for
//!   signature, issuer and expiry here, then checked by the route's own guard for `azp`, `aud` containing that
//!   `azp`, its narrow role, and NO `super_admin` (crate::signup::guard).
use std::collections::HashMap;
use std::time::{Duration, Instant};

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

/// The JWKS is cached this long.
const JWKS_TTL: Duration = Duration::from_secs(600);
/// An unknown `kid` re-fetches the JWKS (key rotation) at most this often.
const JWKS_MIN_REFRESH: Duration = Duration::from_secs(30);

/// Client role (on the `agent` client): acts in any tenant as its admin, and decides signups.
pub const SUPER_ADMIN: &str = "super_admin";

#[derive(Clone, Debug)]
pub struct Claims {
    pub sub: String,
    pub preferred_username: Option<String>,
    /// Keycloak's session id (`sid`), when the token carries one.
    pub sid: Option<String>,
    /// Issued-at (seconds since the epoch).
    pub iat: Option<i64>,
    /// `resource_access[<our audience>].roles` only.
    pub client_roles: Vec<String>,
    /// The token's `email` claim, when it carries one.
    pub email: Option<String>,
    /// `email_verified`: Keycloak vouches the person controls `email` (absent = false).
    pub email_verified: bool,
}
impl Claims {
    pub fn actor(&self) -> String {
        self.preferred_username
            .clone()
            .unwrap_or_else(|| self.sub.clone())
    }
    pub fn has(&self, role: &str) -> bool {
        self.client_roles.iter().any(|r| r == role)
    }
}

/// A service-account token after signature, issuer and expiry checks; the route guard decides the rest.
#[derive(Clone, Debug)]
pub struct ServiceClaims {
    /// The VERIFIED issuer (one of `KEYCLOAK_TRUSTED_ISSUERS`): each door pins the one it accepts.
    pub iss: String,
    pub sub: String,
    pub azp: Option<String>,
    pub aud: Vec<String>,
    /// REALM roles only (`realm_access.roles`): the narrow door roles (C3 `agent-signup-decide`, the intake
    /// role) are realm roles, and a client role of the same name must never open a door.
    pub roles: Vec<String>,
    /// Every client's roles (`resource_access.*.roles`). Read ONLY to refuse an operator role anywhere in the
    /// token ([`ServiceClaims::has_anywhere`]); never to grant.
    pub client_roles: Vec<String>,
}
impl ServiceClaims {
    /// The realm role `role`.
    pub fn has(&self, role: &str) -> bool {
        self.roles.iter().any(|r| r == role)
    }

    /// `role` as a realm role or as any client's role: for refusals only.
    pub fn has_anywhere(&self, role: &str) -> bool {
        self.has(role) || self.client_roles.iter().any(|r| r == role)
    }
}

/// `aud` is a string or an array of strings.
#[derive(Deserialize)]
#[serde(untagged)]
enum Aud {
    One(String),
    Many(Vec<String>),
}
impl Aud {
    fn into_vec(self) -> Vec<String> {
        match self {
            Aud::One(s) => vec![s],
            Aud::Many(v) => v,
        }
    }
}

#[derive(Deserialize)]
struct RawClaims {
    sub: String,
    preferred_username: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    email_verified: Option<bool>,
    #[serde(default)]
    sid: Option<String>,
    #[serde(default)]
    iat: Option<i64>,
    azp: Option<String>,
    aud: Option<Aud>,
    #[serde(default)]
    resource_access: HashMap<String, Access>,
    #[serde(default)]
    realm_access: Access,
}
#[derive(Deserialize, Default)]
struct Access {
    #[serde(default)]
    roles: Vec<String>,
}

/// A tenant role (D88, plan §R.1): each one holds the right to one family of tenant writes. Only `tenant_admin`
/// implies others: the job configuration roles ([`TENANT_ADMIN_IMPLIES`], D127). A member with no role is an observer
/// (every tenant read, test chat, dry runs).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    TenantAdmin,
    JobDesigner,
    ReleaseManager,
    KnowledgeManager,
    IntegrationsManager,
    SupportManager,
    SupportAgent,
    Operator,
}

/// The roles that work a conversation: a `support_manager` can do everything a `support_agent` can (owner decision
/// 2026-10-26). The one definition: the route guards ([`crate::tenancy::TenantCtx::require_conversation_worker`]),
/// the assignee queries ([`CONVERSATION_WORKERS_SQL`]) and the web (`CONVERSATION_WORKERS` in `lib/auth/roles.ts`).
pub const CONVERSATION_WORKERS: [Role; 2] = [Role::SupportAgent, Role::SupportManager];

/// SQL predicate over `users.roles`: the member holds one of [`CONVERSATION_WORKERS`] (keep in step with it).
pub const CONVERSATION_WORKERS_SQL: &str =
    "roles && ARRAY['support_agent', 'support_manager']::text[]";

impl Role {
    /// The vocabulary in §R.1 order (= migration 0039 `agent_role_keys()`).
    pub const ALL: [Role; 8] = [
        Role::TenantAdmin,
        Role::JobDesigner,
        Role::ReleaseManager,
        Role::KnowledgeManager,
        Role::IntegrationsManager,
        Role::SupportManager,
        Role::SupportAgent,
        Role::Operator,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Role::TenantAdmin => "tenant_admin",
            Role::JobDesigner => "job_designer",
            Role::ReleaseManager => "release_manager",
            Role::KnowledgeManager => "knowledge_manager",
            Role::IntegrationsManager => "integrations_manager",
            Role::SupportManager => "support_manager",
            Role::SupportAgent => "support_agent",
            Role::Operator => "operator",
        }
    }

    pub fn parse(s: &str) -> Option<Role> {
        Role::ALL.into_iter().find(|r| r.key() == s)
    }

    pub fn label(self) -> &'static str {
        match self {
            Role::TenantAdmin => "Tenant admin",
            Role::JobDesigner => "Job designer",
            Role::ReleaseManager => "Release manager",
            Role::KnowledgeManager => "Knowledge manager",
            Role::IntegrationsManager => "Integrations manager",
            Role::SupportManager => "Support manager",
            Role::SupportAgent => "Support agent",
            Role::Operator => "Operator",
        }
    }

    /// What the role holds the right to (the role catalog, `GET /api/roles`; mirrored by the web fixture).
    pub fn description(self) -> &'static str {
        match self {
            Role::TenantAdmin => {
                "Members and their roles, template upgrade requests, provisioning retry and cancel."
            }
            Role::JobDesigner => {
                "Every draft write of job design: settings, rules, ontology, procedures, gates, toolsets, journeys, test cases and discovery."
            }
            Role::ReleaseManager => {
                "Release targets, approving and applying releases, publishing procedure versions, validation cuts and verdicts."
            }
            Role::KnowledgeManager => {
                "Knowledge stores, documents, uploads, ingest jobs, sources, golden questions, eval runs and model changes."
            }
            Role::IntegrationsManager => {
                "LLM providers and bindings, provider slots, connectors, credentials, mappings, operation routes and channel tests."
            }
            Role::SupportManager => "Handover assignment and the handover and SLA policy.",
            Role::SupportAgent => {
                "Handing over and claiming cases, replies, taking over and handing back conversations."
            }
            Role::Operator => {
                "Run management: unknown outcomes, stuck stream entries and dead letters."
            }
        }
    }

    fn bit(self) -> u16 {
        1 << (self as u16)
    }
}

/// D127 (owner, 2026-10-27: the tenant_admin designs, connects and publishes the tenant's jobs): `tenant_admin` holds
/// the configuration roles. Never the conversation roles (`support_agent`, `support_manager`) or `operator`, so an
/// admin is no conversation worker or round-robin candidate unless granted one; SQL over `users.roles` is unaffected.
pub const TENANT_ADMIN_IMPLIES: [Role; 4] = [
    Role::JobDesigner,
    Role::ReleaseManager,
    Role::KnowledgeManager,
    Role::IntegrationsManager,
];

/// A set of [`Role`]s (a member's held roles, a view-as, the effective set).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct RoleSet(u16);

impl RoleSet {
    pub const NONE: RoleSet = RoleSet(0);
    pub const ALL: RoleSet = RoleSet((1 << Role::ALL.len()) - 1);

    pub fn of(roles: &[Role]) -> RoleSet {
        RoleSet(roles.iter().fold(0, |a, r| a | r.bit()))
    }

    pub fn has(self, r: Role) -> bool {
        self.0 & r.bit() != 0
    }

    /// The set may work conversations (Take, reply, Hand back, Resolve, Hand over, Link customer, be assigned):
    /// it holds one of [`CONVERSATION_WORKERS`].
    pub fn works_conversations(self) -> bool {
        CONVERSATION_WORKERS.iter().any(|r| self.has(*r))
    }

    pub fn is_subset(self, o: RoleSet) -> bool {
        self.0 & !o.0 == 0
    }

    pub fn intersect(self, o: RoleSet) -> RoleSet {
        RoleSet(self.0 & o.0)
    }

    /// The effective set of a member holding `self`: the held roles narrowed by a super_admin's view-as (D79) when
    /// one is active, plus what a remaining `tenant_admin` implies (D127) — so viewing as tenant_admin shows what a
    /// real tenant admin can do.
    pub fn effective(self, view_as: Option<RoleSet>) -> RoleSet {
        let set = view_as.map_or(self, |v| self.intersect(v));
        if set.has(Role::TenantAdmin) {
            RoleSet(set.0 | RoleSet::of(&TENANT_ADMIN_IMPLIES).0)
        } else {
            set
        }
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The roles in §R.1 order.
    pub fn roles(self) -> Vec<Role> {
        Role::ALL.into_iter().filter(|r| self.has(*r)).collect()
    }

    /// The keys in §R.1 order.
    pub fn keys(self) -> Vec<&'static str> {
        self.roles().into_iter().map(Role::key).collect()
    }

    /// A role set from request keys: 422 `invalid_role` for a key outside §R.1, 422 `duplicate_role` for one listed
    /// twice.
    pub fn from_keys<S: AsRef<str>>(keys: &[S]) -> Result<RoleSet, ApiError> {
        let mut set = RoleSet::NONE;
        for k in keys {
            let k = k.as_ref();
            let r = Role::parse(k).ok_or_else(|| {
                ApiError::Unprocessable("invalid_role", format!("{k:?} is not a tenant role"))
            })?;
            if set.has(r) {
                return Err(ApiError::Unprocessable(
                    "duplicate_role",
                    format!("{k:?} is listed twice"),
                ));
            }
            set.0 |= r.bit();
        }
        Ok(set)
    }

    /// A role set read from the database (`users.roles`, guarded by CHECKs there): an unknown key is an internal
    /// error, never a silent drop.
    pub fn from_db(keys: &[String]) -> Result<RoleSet, ApiError> {
        RoleSet::from_keys(keys).map_err(|_| {
            ApiError::Internal(anyhow::anyhow!(
                "a stored role set holds an unknown or repeated role"
            ))
        })
    }
}

/// A live narrow-only view (D79): until `expires_at` the effective roles are `held ∩ roles`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ViewAs {
    pub roles: RoleSet,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

/// A provisioned (and not disabled) person's home tenant.
#[derive(Clone, Debug)]
pub struct Home {
    pub tenant: Uuid,
    /// The roles the member holds (`users.roles`).
    pub roles: RoleSet,
    pub status: TenantStatus,
    /// A live member view-as (`member_view_as`, expired rows already dropped by `agent_auth_user`).
    pub view_as: Option<ViewAs>,
}

/// `tenants.status`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TenantStatus {
    Active,
    Suspended,
    Archived,
}
impl TenantStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TenantStatus::Active => "active",
            TenantStatus::Suspended => "suspended",
            TenantStatus::Archived => "archived",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "active" => Some(TenantStatus::Active),
            "suspended" => Some(TenantStatus::Suspended),
            "archived" => Some(TenantStatus::Archived),
            _ => None,
        }
    }
}

/// Who is calling (a person; crate::tenancy picks the tenant).
#[derive(Clone, Debug)]
pub struct Caller {
    pub sub: String,
    /// `preferred_username`, else `sub`.
    pub actor: String,
    /// `None` when agent has no (enabled) `users` row for the subject.
    pub home: Option<Home>,
    pub super_admin: bool,
    /// The session this token belongs to: Keycloak's `sid`, else an hour bucket of `iat`, else `unknown`
    /// (member sessions, crate::members::seen).
    pub session: String,
    /// The token's `email`, only when Keycloak vouches for it (`email_verified`); else `None`. The one source of a
    /// staff conversation's party ([`crate::runtime::customer_context::staff_party`]).
    pub verified_email: Option<String>,
}
impl Caller {
    pub fn is_super_admin(&self) -> bool {
        self.super_admin
    }
}

/// 403 `super_admin_required` unless the caller holds the super_admin client role (`SUPER_ADMIN_ROLE`).
pub fn require_super_admin(c: &Caller) -> Result<(), ApiError> {
    if c.super_admin {
        Ok(())
    } else {
        Err(ApiError::Forbidden(
            "super_admin_required",
            "this needs the super_admin role".into(),
        ))
    }
}

/// Parsed keys by `kid`, plus when we last fetched successfully and when we last tried at all.
#[derive(Default)]
struct JwksCache {
    keys: HashMap<String, DecodingKey>,
    /// Last successful fetch; the cache is fresh for `JWKS_TTL` after it.
    fetched: Option<Instant>,
    /// Last fetch attempt, successful or not. The 30 s floor applies to this, so a Keycloak outage is
    /// hit once per 30 s in total instead of once per request.
    last_attempt: Option<Instant>,
    last_failed: bool,
}

enum Keys {
    Jwks {
        url: String,
        http: reqwest::Client,
        cache: Mutex<JwksCache>,
    },
    Static(DecodingKey),
}

/// One trusted issuer: its exact `iss`, its own key source, and whether its realm is service-only.
struct Trusted {
    issuer: String,
    /// A `KEYCLOAK_SERVICE_ONLY_REALMS` realm: verifies on the service-account doors only.
    service_only: bool,
    keys: Keys,
}

pub struct Verifier {
    audience: String,
    /// The `azp` a person token must carry (`KEYCLOAK_CLIENT_ID`).
    client_id: String,
    /// `KEYCLOAK_TRUSTED_ISSUERS`, each with its own JWKS cache. Only listed issuers ever get a cache, so a caller
    /// cannot make this service fetch from an `iss` of its choosing.
    issuers: Vec<Trusted>,
}

/// The `iss` of a token, read WITHOUT verifying it: only to pick which trusted issuer validates it.
fn unverified_iss(token: &str) -> Option<String> {
    use base64::Engine;
    #[derive(Deserialize)]
    struct Iss {
        iss: Option<String>,
    }
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    serde_json::from_slice::<Iss>(&bytes).ok()?.iss
}

impl Verifier {
    /// One issuer, `jwks_url` its key source, not service-only.
    pub fn jwks(
        issuer: &str,
        audience: &str,
        client_id: &str,
        jwks_url: &str,
        http: reqwest::Client,
    ) -> Self {
        Self::trusted(
            audience,
            client_id,
            &[],
            vec![(issuer.to_string(), jwks_keys(jwks_url, http))],
        )
    }

    /// `KEYCLOAK_TRUSTED_ISSUERS`: `default_jwks_url` (`KEYCLOAK_JWKS_URL`) for `default_issuer`
    /// (`KEYCLOAK_ISSUER`), `<iss>/protocol/openid-connect/certs` for every other issuer.
    pub fn jwks_many(
        trusted: &[String],
        service_only_realms: &[String],
        default_issuer: &str,
        default_jwks_url: &str,
        audience: &str,
        client_id: &str,
        http: reqwest::Client,
    ) -> Self {
        let entries = trusted
            .iter()
            .map(|iss| {
                let url = if iss == default_issuer {
                    default_jwks_url.to_string()
                } else {
                    format!("{iss}/protocol/openid-connect/certs")
                };
                (iss.clone(), jwks_keys(&url, http.clone()))
            })
            .collect();
        Self::trusted(audience, client_id, service_only_realms, entries)
    }

    /// HS256 with a fixed secret. Reachable only through `state::build_for_tests`.
    pub fn test_key(issuer: &str, audience: &str, client_id: &str, secret: &[u8]) -> Self {
        Self::test_key_many(&[issuer.to_string()], &[], audience, client_id, secret)
    }

    /// [`Verifier::test_key`] for several issuers (one shared HS256 secret).
    pub fn test_key_many(
        trusted: &[String],
        service_only_realms: &[String],
        audience: &str,
        client_id: &str,
        secret: &[u8],
    ) -> Self {
        let entries = trusted
            .iter()
            // jsonwebtoken 12 makes `from_secret` fallible: an empty HMAC secret is now a
            // hard error (InvalidKeyFormat). This helper is test-only and the secret is a
            // literal, so panicking with the reason is the honest failure mode.
            .map(|iss| {
                (
                    iss.clone(),
                    Keys::Static(
                        DecodingKey::from_secret(secret)
                            .expect("test HMAC secret must not be empty"),
                    ),
                )
            })
            .collect();
        Self::trusted(audience, client_id, service_only_realms, entries)
    }

    fn trusted(
        audience: &str,
        client_id: &str,
        service_only_realms: &[String],
        entries: Vec<(String, Keys)>,
    ) -> Self {
        let issuers = entries
            .into_iter()
            .map(|(issuer, keys)| Trusted {
                service_only: crate::config::realm_of_issuer(&issuer)
                    .is_some_and(|r| service_only_realms.iter().any(|s| s == r)),
                issuer,
                keys,
            })
            .collect();
        Self {
            audience: audience.into(),
            client_id: client_id.into(),
            issuers,
        }
    }

    /// The token's claims, validated against EXACTLY the trusted issuer its `iss` names, and that issuer. A
    /// non-listed `iss` is 401 before any key lookup; a service-only realm is 401 unless `allow_service_only`.
    async fn decode_raw(
        &self,
        token: &str,
        check_audience: bool,
        allow_service_only: bool,
    ) -> Result<(RawClaims, &str), ApiError> {
        let header = decode_header(token).map_err(|_| ApiError::Unauthenticated)?;
        let iss = unverified_iss(token).ok_or(ApiError::Unauthenticated)?;
        let t = self
            .issuers
            .iter()
            .find(|t| t.issuer == iss)
            .ok_or(ApiError::Unauthenticated)?;
        if t.service_only && !allow_service_only {
            return Err(ApiError::Unauthenticated);
        }
        let (key, alg) = match &t.keys {
            Keys::Static(k) => (k.clone(), Algorithm::HS256),
            Keys::Jwks { url, http, cache } => {
                let kid = header.kid.as_deref().ok_or(ApiError::Unauthenticated)?;
                (Self::jwk(url, http, cache, kid).await?, Algorithm::RS256)
            }
        };
        let mut v = Validation::new(alg);
        // jsonwebtoken checks `iss`/`aud` only when present: make them (and `sub`) mandatory, so a
        // portal-realm token minted for another client without an `aud` cannot pass.
        v.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        v.set_issuer(&[&t.issuer]);
        if check_audience {
            v.set_audience(&[&self.audience]);
        } else {
            v.validate_aud = false;
        }
        v.leeway = 5;
        let raw = decode::<RawClaims>(token, &key, &v)
            .map_err(|_| ApiError::Unauthenticated)?
            .claims;
        if raw.sub.trim().is_empty() {
            return Err(ApiError::Unauthenticated);
        }
        Ok((raw, t.issuer.as_str()))
    }

    /// A person's token: `aud` must contain our audience and `azp` must be our client. A token issued to
    /// another portal-realm client (or with no `azp`) is 401, however it is scoped, and so is a token from a
    /// `KEYCLOAK_SERVICE_ONLY_REALMS` realm (secure-apps holds human accounts too).
    pub async fn verify(&self, token: &str) -> Result<Claims, ApiError> {
        let (mut raw, _) = self.decode_raw(token, true, false).await?;
        if raw.azp.as_deref() != Some(self.client_id.as_str()) {
            return Err(ApiError::Unauthenticated);
        }
        let client_roles = raw
            .resource_access
            .remove(&self.audience)
            .map(|a| a.roles)
            .unwrap_or_default();
        Ok(Claims {
            sub: raw.sub,
            preferred_username: raw.preferred_username,
            sid: raw.sid.filter(|s| !s.trim().is_empty()),
            iat: raw.iat,
            client_roles,
            email: raw.email.filter(|e| !e.trim().is_empty()),
            email_verified: raw.email_verified == Some(true),
        })
    }

    /// A service-account token: signature, a trusted issuer (service-only realms included), expiry and a present
    /// `aud`; the door's guard checks the rest, including that `iss` is the one issuer that door accepts.
    pub async fn verify_service(&self, token: &str) -> Result<ServiceClaims, ApiError> {
        let (raw, iss) = self.decode_raw(token, false, true).await?;
        let iss = iss.to_string();
        let aud = raw.aud.map(Aud::into_vec).unwrap_or_default();
        if aud.is_empty() {
            return Err(ApiError::Unauthenticated);
        }
        let client_roles = raw
            .resource_access
            .into_values()
            .flat_map(|a| a.roles)
            .collect();
        Ok(ServiceClaims {
            iss,
            sub: raw.sub,
            azp: raw.azp.filter(|a| !a.is_empty()),
            aud,
            roles: raw.realm_access.roles,
            client_roles,
        })
    }

    /// JWKS cached 10 minutes. A stale cache or an unknown `kid` triggers a fetch (key rotation), but
    /// never more than one attempt per 30 s (failures count), so neither garbage `kid`s nor a Keycloak
    /// outage can turn this service into a hammer. If a fetch fails, keys already held keep verifying
    /// (stale beats down); 503 only when no usable key exists.
    async fn jwk(
        url: &str,
        http: &reqwest::Client,
        cache: &Mutex<JwksCache>,
        kid: &str,
    ) -> Result<DecodingKey, ApiError> {
        // The lock is held across the fetch on purpose: concurrent callers wait for one attempt and
        // then reuse its outcome (the floor below stops them from fetching again).
        let mut c = cache.lock().await;
        let fresh = c.fetched.is_some_and(|t| t.elapsed() < JWKS_TTL);
        let known = c.keys.contains_key(kid);
        if !(fresh && known)
            && c.last_attempt
                .is_none_or(|t| t.elapsed() >= JWKS_MIN_REFRESH)
        {
            c.last_attempt = Some(Instant::now());
            match fetch(url, http).await {
                Ok(keys) => {
                    c.keys = keys;
                    c.fetched = Some(Instant::now());
                    c.last_failed = false;
                }
                Err(_) => c.last_failed = true,
            }
        }
        if let Some(k) = c.keys.get(kid) {
            return Ok(k.clone());
        }
        // Unknown kid and the last fetch failed: we cannot tell "rotated" from "forged", so say 503.
        if c.last_failed {
            return Err(keycloak_unavailable());
        }
        Err(ApiError::Unauthenticated)
    }
}

/// A JWKS key source with its own cache.
fn jwks_keys(url: &str, http: reqwest::Client) -> Keys {
    Keys::Jwks {
        url: url.into(),
        http,
        cache: Mutex::new(JwksCache::default()),
    }
}

/// The JWKS could not be fetched and no held key fits: 503 `unavailable`.
fn keycloak_unavailable() -> ApiError {
    ApiError::Unavailable(
        "unavailable",
        "keycloak is unavailable; try again shortly".into(),
        None,
    )
}

async fn fetch(
    url: &str,
    http: &reqwest::Client,
) -> Result<HashMap<String, DecodingKey>, ApiError> {
    let resp = crate::observability::upstream_http("keycloak", "jwks".into(), http.get(url).send())
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|_| keycloak_unavailable())?;
    let set: JwkSet = resp.json().await.map_err(|_| keycloak_unavailable())?;
    Ok(set
        .keys
        .iter()
        .filter_map(|j| {
            let kid = j.common.key_id.clone()?;
            Some((kid, DecodingKey::from_jwk(j).ok()?))
        })
        .collect())
}

/// The bearer token of the request, or 401.
pub fn bearer(parts: &Parts) -> Result<&str, ApiError> {
    parts
        .headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .filter(|t| !t.is_empty())
        .ok_or(ApiError::Unauthenticated)
}

/// The member-session key of a token: `sid`, else `iat-<hour>`, else `unknown` (at most 128 characters).
pub fn session_of(sid: Option<&str>, iat: Option<i64>) -> String {
    match (sid, iat) {
        (Some(s), _) => s.chars().take(128).collect(),
        (None, Some(i)) => format!("iat-{}", i - i.rem_euclid(3600)),
        (None, None) => "unknown".into(),
    }
}

type AuthRow = (
    Uuid,
    Vec<String>,
    String,
    Option<Vec<String>>,
    Option<chrono::DateTime<chrono::Utc>>,
);

/// `sub`'s home (`agent_auth_user`).
async fn auth_row(st: &AppState, sub: &str) -> Result<Option<AuthRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT tenant_id, roles, tenant_status, view_as, view_as_expires_at FROM agent_auth_user($1)",
    )
    .bind(sub)
    .fetch_optional(&st.db)
    .await
}

/// Token → caller. A person's home tenant is read through `agent_auth_user` (SECURITY DEFINER), the one read
/// of `users` that needs no tenant GUC.
async fn resolve(parts: &Parts, st: &AppState) -> Result<Caller, ApiError> {
    let c = st.auth.verify(bearer(parts)?).await?;
    let actor = c.actor();
    let mut row = auth_row(st, &c.sub).await?;
    // Not a member yet: a pending, unexpired invite for the token's VERIFIED email is accepted now (CP
    // `acceptInvite`; crate::members::accept). An unverified or absent email accepts nothing.
    if row.is_none()
        && c.email_verified
        && let Some(email) = c.email.as_deref()
    {
        match crate::members::accept::accept_invite(st, &c.sub, email, &actor).await {
            // Re-read either way: a concurrent sign-in of the same person may have just accepted it.
            Ok(_) => row = auth_row(st, &c.sub).await?,
            Err(e) => tracing::warn!(error = %e, "invite_accept_failed"),
        }
    }
    let home = match row {
        None => None,
        Some((tenant, roles, status, view, until)) => Some(Home {
            tenant,
            roles: RoleSet::from_db(&roles)?,
            status: TenantStatus::parse(&status).ok_or_else(|| {
                ApiError::Internal(anyhow::anyhow!("tenants.status has an unknown value"))
            })?,
            view_as: match (view, until) {
                (Some(v), Some(expires_at)) => Some(ViewAs {
                    roles: RoleSet::from_db(&v)?,
                    expires_at,
                }),
                _ => None,
            },
        }),
    };
    Ok(Caller {
        super_admin: c.has(&st.cfg.super_admin_role),
        session: session_of(c.sid.as_deref(), c.iat),
        verified_email: c.email.filter(|_| c.email_verified),
        sub: c.sub,
        actor,
        home,
    })
}

/// Any verified person. Resolved once per request (cached in the request extensions).
impl FromRequestParts<AppState> for Caller {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, ApiError> {
        if let Some(c) = parts.extensions.get::<Caller>() {
            return Ok(c.clone());
        }
        let c = resolve(parts, st).await?;
        parts.extensions.insert(c.clone());
        Ok(c)
    }
}

/// Any token from a trusted issuer, read as a service account (the route guard decides, issuer pin included).
impl FromRequestParts<AppState> for ServiceClaims {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, ApiError> {
        st.auth.verify_service(bearer(parts)?).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;
    use std::sync::Arc;
    use std::sync::LazyLock;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// Test keypair generated once per test binary (RSA keygen is slow); nothing secret is committed.
    struct TestKey {
        pem: String,
        /// base64url (no padding) modulus, as a JWKS `n`.
        n: String,
    }

    fn b64url(bytes: &[u8]) -> String {
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for c in bytes.chunks(3) {
            let v = (u32::from(c[0]) << 16)
                | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
                | u32::from(*c.get(2).unwrap_or(&0));
            for i in 0..=c.len() {
                out.push(T[((v >> (18 - 6 * i)) & 63) as usize] as char);
            }
        }
        out
    }

    fn gen_key() -> TestKey {
        use rsa::pkcs1::EncodeRsaPrivateKey;
        use rsa::traits::PublicKeyParts;
        let k = rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).unwrap();
        TestKey {
            pem: k
                .to_pkcs1_pem(rsa::pkcs1::LineEnding::LF)
                .unwrap()
                .to_string(),
            n: b64url(&k.n().to_bytes_be()),
        }
    }

    static KEY_A_GEN: LazyLock<TestKey> = LazyLock::new(gen_key);
    static KEY_B_GEN: LazyLock<TestKey> = LazyLock::new(gen_key);

    struct Stub {
        issuer: String,
        hits: Arc<AtomicUsize>,
        fail: Arc<AtomicBool>,
    }

    /// A JWKS endpoint serving `jwks`; counts hits; answers 500 while `fail` is set.
    async fn stub_jwks(jwks: serde_json::Value) -> Stub {
        stub_jwks_realm(jwks, "portal").await
    }

    /// [`stub_jwks`] for the realm `realm` (issuer `http://<addr>/realms/<realm>`).
    async fn stub_jwks_realm(jwks: serde_json::Value, realm: &str) -> Stub {
        let hits = Arc::new(AtomicUsize::new(0));
        let fail = Arc::new(AtomicBool::new(false));
        let (h, f) = (hits.clone(), fail.clone());
        let app = axum::Router::new().route(
            &format!("/realms/{realm}/protocol/openid-connect/certs"),
            axum::routing::get(move || {
                let (h, f, jwks) = (h.clone(), f.clone(), jwks.clone());
                async move {
                    h.fetch_add(1, Ordering::SeqCst);
                    if f.load(Ordering::SeqCst) {
                        return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "boom")
                            .into_response();
                    }
                    axum::Json(jwks).into_response()
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = format!("http://{}/realms/{realm}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        Stub { issuer, hits, fail }
    }

    fn certs(issuer: &str) -> String {
        format!("{issuer}/protocol/openid-connect/certs")
    }

    fn empty() -> serde_json::Value {
        serde_json::json!({"keys": []})
    }
    fn jwks_a() -> serde_json::Value {
        serde_json::json!({"keys": [{"kty":"RSA","kid":"k1","use":"sig","alg":"RS256","n":KEY_A_GEN.n.as_str(),"e":"AQAB"}]})
    }

    fn jwks_b() -> serde_json::Value {
        serde_json::json!({"keys": [{"kty":"RSA","kid":"k1","use":"sig","alg":"RS256","n":KEY_B_GEN.n.as_str(),"e":"AQAB"}]})
    }

    /// Realm per app A1: `portal` (key A, the default issuer), `agent` (key B) and `secure-apps` (key B) trusted,
    /// secure-apps service-only. Each realm is its own JWKS endpoint.
    async fn three_realms() -> (Stub, Stub, Stub, Verifier) {
        let portal = stub_jwks_realm(jwks_a(), "portal").await;
        let agent = stub_jwks_realm(jwks_b(), "agent").await;
        let secure = stub_jwks_realm(jwks_b(), "secure-apps").await;
        let v = Verifier::jwks_many(
            &[
                portal.issuer.clone(),
                agent.issuer.clone(),
                secure.issuer.clone(),
            ],
            &["secure-apps".to_string()],
            &portal.issuer,
            &certs(&portal.issuer),
            "agent",
            "agent",
            reqwest::Client::new(),
        );
        (portal, agent, secure, v)
    }

    #[tokio::test]
    async fn a_second_trusted_issuer_verifies_against_its_own_jwks() {
        let (portal, agent, _secure, v) = three_realms().await;
        let c = v
            .verify(&rs256(&KEY_B_GEN.pem, "k1", &agent.issuer, "agent"))
            .await
            .unwrap();
        assert_eq!(c.sub, "u-9");
        assert!(c.has("super_admin"));
        assert_eq!(agent.hits.load(Ordering::SeqCst), 1);
        assert_eq!(
            portal.hits.load(Ordering::SeqCst),
            0,
            "only the token's own realm is fetched"
        );
        // the default issuer still verifies, through its own (KEYCLOAK_JWKS_URL) cache
        assert!(
            v.verify(&rs256(&KEY_A_GEN.pem, "k1", &portal.issuer, "agent"))
                .await
                .is_ok()
        );
        assert_eq!(portal.hits.load(Ordering::SeqCst), 1);
        // cross-realm forgery: portal's key cannot vouch for an agent-realm `iss`, nor the reverse
        for t in [
            rs256(&KEY_A_GEN.pem, "k1", &agent.issuer, "agent"),
            rs256(&KEY_B_GEN.pem, "k1", &portal.issuer, "agent"),
        ] {
            assert!(matches!(v.verify(&t).await, Err(ApiError::Unauthenticated)));
            assert!(matches!(
                v.verify_service(&t).await,
                Err(ApiError::Unauthenticated)
            ));
        }
        // the azp pin still applies to every trusted issuer
        let t = rs256_azp(
            &KEY_B_GEN.pem,
            "k1",
            &agent.issuer,
            "agent",
            Some("care-bus"),
        );
        assert!(matches!(v.verify(&t).await, Err(ApiError::Unauthenticated)));
    }

    #[tokio::test]
    async fn a_service_only_realm_is_refused_as_a_person_and_verified_as_a_service() {
        let (_portal, _agent, secure, v) = three_realms().await;
        let t = rs256(&KEY_B_GEN.pem, "k1", &secure.issuer, "agent");
        assert!(matches!(v.verify(&t).await, Err(ApiError::Unauthenticated)));
        assert_eq!(
            secure.hits.load(Ordering::SeqCst),
            0,
            "refused before any key lookup"
        );
        let sc = v.verify_service(&t).await.unwrap();
        assert_eq!(sc.iss, secure.issuer);
        assert_eq!(secure.hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_non_trusted_issuer_is_401_without_a_jwks_fetch() {
        let (portal, agent, _secure, v) = three_realms().await;
        // a realm on the same host that is not in the list, served the same key
        let other = stub_jwks_realm(jwks_a(), "commonpractice").await;
        let host = portal.issuer.trim_end_matches("/realms/portal").to_string();
        for iss in [
            other.issuer.clone(),
            format!("{host}/realms/commonpractice"),
            format!("{}/", portal.issuer),
            String::new(),
        ] {
            let t = rs256(&KEY_A_GEN.pem, "k1", &iss, "agent");
            assert!(
                matches!(v.verify(&t).await, Err(ApiError::Unauthenticated)),
                "{iss}"
            );
            assert!(
                matches!(v.verify_service(&t).await, Err(ApiError::Unauthenticated)),
                "{iss}"
            );
        }
        for s in [&other, &portal, &agent] {
            assert_eq!(s.hits.load(Ordering::SeqCst), 0);
        }
        // a token with no `iss` at all
        let mut h = jsonwebtoken::Header::new(Algorithm::RS256);
        h.kid = Some("k1".into());
        let exp = (chrono::Utc::now() + chrono::Duration::minutes(5)).timestamp();
        let t = jsonwebtoken::encode(
            &h,
            &serde_json::json!({"sub":"u","exp":exp,"aud":"agent","azp":"agent"}),
            &jsonwebtoken::EncodingKey::from_rsa_pem(KEY_A_GEN.pem.as_bytes()).unwrap(),
        )
        .unwrap();
        assert!(matches!(v.verify(&t).await, Err(ApiError::Unauthenticated)));
    }

    #[tokio::test]
    async fn the_single_issuer_verifier_reports_its_issuer_to_the_doors() {
        let s = stub_jwks(jwks_a()).await;
        let v = Verifier::jwks(
            &s.issuer,
            "agent",
            "agent",
            &certs(&s.issuer),
            reqwest::Client::new(),
        );
        let sc = v
            .verify_service(&rs256(&KEY_A_GEN.pem, "k1", &s.issuer, "agent"))
            .await
            .unwrap();
        assert_eq!(sc.iss, s.issuer);
    }

    fn rs256(pem: &str, kid: &str, iss: &str, aud: &str) -> String {
        rs256_azp(pem, kid, iss, aud, Some(aud))
    }

    /// `aud` (and its client roles) with an explicit `azp` (`None` = claim absent).
    fn rs256_azp(pem: &str, kid: &str, iss: &str, aud: &str, azp: Option<&str>) -> String {
        let mut h = jsonwebtoken::Header::new(Algorithm::RS256);
        h.kid = Some(kid.into());
        let exp = (chrono::Utc::now() + chrono::Duration::minutes(5)).timestamp();
        jsonwebtoken::encode(
            &h,
            &serde_json::json!({"sub":"u-9","preferred_username":"j.devries","exp":exp,"iss":iss,"aud":aud,
                "azp":azp,
                "realm_access":{"roles":["care-admin"]},
                "resource_access":{aud:{"roles":["super_admin"]},"other-client":{"roles":["release-writer"]}}}),
            &jsonwebtoken::EncodingKey::from_rsa_pem(pem.as_bytes()).unwrap(),
        )
        .unwrap()
    }

    /// A token naming `iss` (it must be a trusted issuer, or it is refused before any key lookup) and `kid`.
    fn token_with_kid(iss: &str, kid: &str) -> String {
        // The key lookup fails before the signature matters, so any signing key will do.
        let mut h = jsonwebtoken::Header::new(Algorithm::HS256);
        h.kid = Some(kid.into());
        jsonwebtoken::encode(
            &h,
            &serde_json::json!({"sub": "x", "iss": iss}),
            &jsonwebtoken::EncodingKey::from_secret(b"k").expect("literal test secret"),
        )
        .unwrap()
    }

    /// Pretend the last fetch (and attempt) happened `secs` ago.
    async fn age(v: &Verifier, secs: u64) {
        if let Keys::Jwks { cache, .. } = &v.issuers[0].keys {
            let mut c = cache.lock().await;
            let t = Instant::now()
                .checked_sub(Duration::from_secs(secs))
                .unwrap();
            c.fetched = Some(t);
            c.last_attempt = Some(t);
        }
    }

    #[tokio::test]
    async fn rs256_token_signed_by_the_jwks_key_verifies() {
        let s = stub_jwks(jwks_a()).await;
        let v = Verifier::jwks(
            &s.issuer,
            "aud",
            "aud",
            &certs(&s.issuer),
            reqwest::Client::new(),
        );
        let c = v
            .verify(&rs256(&KEY_A_GEN.pem, "k1", &s.issuer, "aud"))
            .await
            .unwrap();
        assert_eq!(c.sub, "u-9");
        assert_eq!(c.actor(), "j.devries");
        // only our audience's client roles; realm roles and other clients' roles are never read
        assert_eq!(c.client_roles, vec!["super_admin".to_string()]);
        // the parsed key is cached: a second token does not re-fetch
        v.verify(&rs256(&KEY_A_GEN.pem, "k1", &s.issuer, "aud"))
            .await
            .unwrap();
        assert_eq!(s.hits.load(Ordering::SeqCst), 1);
        // wrong audience still refused on the production path
        let r = v
            .verify(&rs256(&KEY_A_GEN.pem, "k1", &s.issuer, "other"))
            .await;
        assert!(matches!(r, Err(ApiError::Unauthenticated)));
        // ...but a service-account read of it passes: realm roles grant, client roles only refuse
        let s2 = v
            .verify_service(&rs256(&KEY_A_GEN.pem, "k1", &s.issuer, "other"))
            .await
            .unwrap();
        assert_eq!(s2.aud, vec!["other".to_string()]);
        assert_eq!(s2.roles, vec!["care-admin".to_string()]);
        assert!(s2.has("care-admin"));
        assert!(
            !s2.has("super_admin") && !s2.has("release-writer"),
            "client roles never grant"
        );
        assert!(s2.has_anywhere("super_admin") && s2.has_anywhere("release-writer"));
    }

    /// Cross-app replay: portal clients have fullScopeAllowed, so a person token issued to another app
    /// carries our `aud` and our client's `super_admin`. Only `azp` tells them apart.
    #[tokio::test]
    async fn person_token_issued_to_another_client_is_401() {
        let s = stub_jwks(jwks_a()).await;
        let v = Verifier::jwks(
            &s.issuer,
            "agent",
            "agent",
            &certs(&s.issuer),
            reqwest::Client::new(),
        );
        for azp in [
            Some("care-bus"),
            Some("strata-configurator"),
            Some(""),
            None,
        ] {
            let t = rs256_azp(&KEY_A_GEN.pem, "k1", &s.issuer, "agent", azp);
            assert!(
                matches!(v.verify(&t).await, Err(ApiError::Unauthenticated)),
                "azp {azp:?} must be refused"
            );
        }
        // control: the same token issued to our own client verifies, super_admin intact
        let c = v
            .verify(&rs256_azp(
                &KEY_A_GEN.pem,
                "k1",
                &s.issuer,
                "agent",
                Some("agent"),
            ))
            .await
            .unwrap();
        assert!(c.has("super_admin"));
        // KEYCLOAK_CLIENT_ID distinct from the audience pins that client instead
        let v2 = Verifier::jwks(
            &s.issuer,
            "agent",
            "agent-web",
            &certs(&s.issuer),
            reqwest::Client::new(),
        );
        let ok = rs256_azp(&KEY_A_GEN.pem, "k1", &s.issuer, "agent", Some("agent-web"));
        assert!(v2.verify(&ok).await.is_ok());
        let bad = rs256_azp(&KEY_A_GEN.pem, "k1", &s.issuer, "agent", Some("agent"));
        assert!(matches!(
            v2.verify(&bad).await,
            Err(ApiError::Unauthenticated)
        ));
    }

    #[tokio::test]
    async fn rs256_unknown_kid_or_foreign_signature_is_401() {
        let s = stub_jwks(jwks_a()).await;
        let v = Verifier::jwks(
            &s.issuer,
            "aud",
            "aud",
            &certs(&s.issuer),
            reqwest::Client::new(),
        );
        let r = v
            .verify(&rs256(&KEY_A_GEN.pem, "nope", &s.issuer, "aud"))
            .await;
        assert!(matches!(r, Err(ApiError::Unauthenticated)));
        // right kid, signed by a different keypair
        let r = v
            .verify(&rs256(&KEY_B_GEN.pem, "k1", &s.issuer, "aud"))
            .await;
        assert!(matches!(r, Err(ApiError::Unauthenticated)));
        let r = v
            .verify_service(&rs256(&KEY_B_GEN.pem, "k1", &s.issuer, "aud"))
            .await;
        assert!(matches!(r, Err(ApiError::Unauthenticated)));
        // control: the real key is accepted by the same verifier
        assert!(
            v.verify(&rs256(&KEY_A_GEN.pem, "k1", &s.issuer, "aud"))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn unknown_kid_refreshes_jwks_at_most_once_per_30s() {
        let s = stub_jwks(empty()).await;
        let v = Verifier::jwks(
            &s.issuer,
            "aud",
            "aud",
            &certs(&s.issuer),
            reqwest::Client::new(),
        );
        for i in 0..5 {
            let r = v
                .verify(&token_with_kid(&s.issuer, &format!("rotated-{i}")))
                .await;
            assert!(matches!(r, Err(ApiError::Unauthenticated)));
        }
        // 1 initial fetch; the unknown kids right after must not re-fetch
        assert_eq!(s.hits.load(Ordering::SeqCst), 1);
        // 30 s later an unknown kid is allowed to refresh once
        age(&v, 31).await;
        let _ = v.verify(&token_with_kid(&s.issuer, "rotated-late")).await;
        assert_eq!(s.hits.load(Ordering::SeqCst), 2);
        let _ = v.verify(&token_with_kid(&s.issuer, "rotated-late-2")).await;
        assert_eq!(s.hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn keycloak_outage_is_hit_once_not_once_per_request() {
        let s = stub_jwks(empty()).await;
        let v = Arc::new(Verifier::jwks(
            &s.issuer,
            "aud",
            "aud",
            &certs(&s.issuer),
            reqwest::Client::new(),
        ));
        v.verify(&token_with_kid(&s.issuer, "warm"))
            .await
            .unwrap_err();
        assert_eq!(s.hits.load(Ordering::SeqCst), 1);
        s.fail.store(true, Ordering::SeqCst);
        age(&v, 31).await;
        let tasks: Vec<_> = (0..5)
            .map(|i| {
                let (v, iss) = (v.clone(), s.issuer.clone());
                tokio::spawn(async move { v.verify(&token_with_kid(&iss, &format!("n{i}"))).await })
            })
            .collect();
        for t in tasks {
            // the cache holds no such kid and the last attempt failed: 503, not 401
            assert!(matches!(
                t.await.unwrap(),
                Err(ApiError::Unavailable("unavailable", _, None))
            ));
        }
        // 1 (warm) + exactly 1 failed attempt shared by all 5 waiters
        assert_eq!(s.hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn stale_keys_keep_verifying_through_an_outage() {
        let s = stub_jwks(jwks_a()).await;
        let v = Verifier::jwks(
            &s.issuer,
            "aud",
            "aud",
            &certs(&s.issuer),
            reqwest::Client::new(),
        );
        v.verify(&rs256(&KEY_A_GEN.pem, "k1", &s.issuer, "aud"))
            .await
            .unwrap();
        s.fail.store(true, Ordering::SeqCst);
        age(&v, 601).await; // past the TTL
        for _ in 0..3 {
            let c = v
                .verify(&rs256(&KEY_A_GEN.pem, "k1", &s.issuer, "aud"))
                .await
                .unwrap();
            assert_eq!(c.sub, "u-9");
        }
        // 1 warm-up + 1 failed refresh; the next two requests reuse the failure
        assert_eq!(s.hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn keycloak_down_is_503_not_401() {
        let v = Verifier::jwks(
            "http://127.0.0.1:1/realms/portal",
            "aud",
            "aud",
            &certs("http://127.0.0.1:1/realms/portal"),
            reqwest::Client::new(),
        );
        let r = v
            .verify(&token_with_kid("http://127.0.0.1:1/realms/portal", "k"))
            .await;
        assert!(matches!(
            r,
            Err(ApiError::Unavailable("unavailable", _, None))
        ));
    }
}
