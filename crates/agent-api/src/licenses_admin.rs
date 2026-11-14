//! Licence issuance. Ported from strata_configurator `licenses_admin.rs` (after commonpractice
//! `services/licenses-admin.ts` and `license-provisioning.ts`), split into the steps the early-access
//! approve journal records separately (crate::signup::provisioning): register the tenant in the licences
//! registry (`PUT /tenants/{id}`, idempotent), create the member licence for the holder's `sub`, and mint
//! its opaque token. Each is fatal on failure; the journal retries.
//!
//! The licences admin API sits behind the `secure-apps` realm: this client authenticates as the
//! client-credentials SA `agent-licenses-admin` (`LICENSES_ADMIN_*`).
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::config::LicensesAdminConfig;
use crate::http_base::BaseUrl;
use crate::service_token::{ServiceToken, UpstreamError};

/// A licence the registry created: its id and the licences service's own Subject id for the holder (not
/// the Keycloak `sub`).
#[derive(Clone, Debug)]
pub struct Created {
    pub license_id: String,
    pub subject_id: String,
}

#[derive(Deserialize)]
struct WireId {
    id: Option<String>,
}

#[derive(Deserialize)]
struct WirePlan {
    id: Option<String>,
    code: Option<String>,
}

#[derive(Deserialize)]
struct WireRegion {
    id: Option<String>,
    code: Option<String>,
}

#[derive(Deserialize)]
struct WireLicense {
    license_id: Option<String>,
    id: Option<String>,
    subject_id: Option<String>,
}

#[derive(Deserialize)]
struct WireToken {
    token: Option<String>,
}

#[derive(Deserialize)]
struct WireRevoke {
    revoked: Option<bool>,
}

/// The name a tenant is registered under in the licences registry, which must match
/// `^[a-z0-9][a-z0-9-]*$` and is unique per owning app: the tenant name as a slug (at most 40
/// characters) plus the last 8 hex digits of its id, so two tenants with one name do not collide.
pub fn registry_name(name: &str, id: Uuid) -> String {
    let mut slug = String::new();
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
        if slug.len() >= 40 {
            break;
        }
    }
    let slug = slug.trim_end_matches('-');
    let hex = id.simple().to_string();
    let tail = &hex[hex.len() - 8..];
    if slug.is_empty() {
        format!("t-{tail}")
    } else {
        format!("{slug}-{tail}")
    }
}

/// How long the region list is trusted before `region_id` re-reads `GET /regions` even for a hit.
const REGIONS_TTL: Duration = Duration::from_secs(5 * 60);

/// Licences' active regions as (code, id), and when they were read (`None` = never).
#[derive(Default)]
struct RegionCache {
    list: Vec<(String, String)>,
    fetched: Option<Instant>,
}

pub struct LicensesAdmin {
    /// The admin API base, parsed and checked once (http(s), no query or fragment), no trailing slash.
    base: BaseUrl,
    app_key: String,
    plan_code: String,
    /// The realm the holder lives in, stated on the licence (commonpractice `subject_realm`): without
    /// it licences would record the caller's realm, `secure-apps`.
    subject_realm: String,
    token: ServiceToken,
    http: reqwest::Client,
    /// (app id, plan id), resolved once and kept for the process (ids never change for a code).
    ids: Mutex<Option<(String, String)>>,
    /// Licences' active regions as (code, id), read from `GET /regions` (B1b T10), with when they were read;
    /// re-read once past `regions_ttl` and on a miss.
    regions: Mutex<RegionCache>,
    regions_ttl: Duration,
}

impl LicensesAdmin {
    pub fn new(cfg: &LicensesAdminConfig, http: reqwest::Client) -> Self {
        Self {
            base: cfg.url.clone(),
            app_key: cfg.app_key.clone(),
            plan_code: cfg.plan_code.clone(),
            subject_realm: cfg.subject_realm.clone(),
            token: ServiceToken::from_creds(&cfg.creds, http.clone()),
            http,
            ids: Mutex::new(None),
            regions: Mutex::new(RegionCache::default()),
            regions_ttl: REGIONS_TTL,
        }
    }

    /// Test seam: trust the region list for `ttl` instead of five minutes.
    #[doc(hidden)]
    pub fn set_regions_ttl(&mut self, ttl: Duration) {
        self.regions_ttl = ttl;
    }

    /// `req` with the SA's bearer token (and `X-Tenant-Id` when acting on a tenant), sent.
    async fn send(
        &self,
        req: reqwest::RequestBuilder,
        tenant: Option<Uuid>,
        what: &str,
    ) -> Result<reqwest::Response, UpstreamError> {
        let token = self.token.get().await?;
        let mut req = req.bearer_auth(token);
        if let Some(t) = tenant {
            req = req.header("x-tenant-id", t.to_string());
        }
        let op = crate::observability::op_label("licences ", what);
        crate::observability::upstream_http("licences", op, req.send())
            .await
            .map_err(|e| UpstreamError::transport(what, &e))
    }

    /// `resp`'s JSON body, after checking it is a 2xx.
    async fn ok_json<T: serde::de::DeserializeOwned>(
        resp: reqwest::Response,
        what: &str,
    ) -> Result<T, UpstreamError> {
        if !resp.status().is_success() {
            return Err(UpstreamError::status(what, resp.status()));
        }
        resp.json()
            .await
            .map_err(|e| UpstreamError::transport(what, &e))
    }

    /// The (app id, plan id) of `LICENSE_APP_KEY` / `LICENSE_PLAN_CODE`.
    async fn app_and_plan(&self) -> Result<(String, String), UpstreamError> {
        let mut ids = self.ids.lock().await;
        if let Some(hit) = ids.as_ref() {
            return Ok(hit.clone());
        }
        let key = self.app_key.as_str();
        let what = "licences resolve app";
        let resp = self
            .send(
                self.http.get(self.base.endpoint(&["apps", key])),
                None,
                what,
            )
            .await?;
        let app: WireId = Self::ok_json(resp, what).await?;
        let app_id = app
            .id
            .ok_or_else(|| UpstreamError::new(None, "licences resolve app: no id"))?;
        let what = "licences resolve plans";
        let resp = self
            .send(
                self.http.get(self.base.endpoint(&["apps", key, "plans"])),
                None,
                what,
            )
            .await?;
        let plans: Vec<WirePlan> = Self::ok_json(resp, what).await?;
        let plan_id = plans
            .into_iter()
            .find(|p| p.code.as_deref() == Some(self.plan_code.as_str()))
            .and_then(|p| p.id)
            .ok_or_else(|| {
                UpstreamError::new(
                    None,
                    format!(
                        "plan {:?} not found for app {:?}",
                        self.plan_code, self.app_key
                    ),
                )
            })?;
        *ids = Some((app_id.clone(), plan_id.clone()));
        Ok((app_id, plan_id))
    }

    /// The licences region id of a tenant residency (`eu` / `us`, D66): the active region whose `code` is the
    /// residency, read from licences' own `GET /regions` (never a made-up id). `None` when licences has no such
    /// region; the list is cached for five minutes and re-read on a miss, so a region registered later is picked up.
    pub async fn region_id(&self, residency: &str) -> Result<Option<String>, UpstreamError> {
        let want = residency.trim().to_ascii_lowercase();
        let mut cache = self.regions.lock().await;
        let fresh = cache
            .fetched
            .is_some_and(|fetched| fetched.elapsed() < self.regions_ttl);
        if fresh && let Some((_, id)) = cache.list.iter().find(|(c, _)| *c == want) {
            return Ok(Some(id.clone()));
        }
        let what = "licences list regions";
        let resp = self
            .send(self.http.get(self.base.endpoint(&["regions"])), None, what)
            .await?;
        let listed: Vec<WireRegion> = Self::ok_json(resp, what).await?;
        cache.list = listed
            .into_iter()
            .filter_map(|r| Some((r.code?.to_ascii_lowercase(), r.id?)))
            .collect();
        cache.fetched = Some(Instant::now());
        Ok(cache
            .list
            .iter()
            .find(|(c, _)| *c == want)
            .map(|(_, id)| id.clone()))
    }

    /// Register `tenant` in the licences registry under `registry_name` (idempotent PUT).
    pub async fn register_tenant(
        &self,
        tenant: Uuid,
        registry_name: &str,
    ) -> Result<(), UpstreamError> {
        let what = "licences register tenant";
        let resp = self
            .send(
                self.http
                    .put(self.base.endpoint(&["tenants", &tenant.to_string()]))
                    .json(&json!({"name": registry_name})),
                None,
                what,
            )
            .await?;
        if !resp.status().is_success() {
            return Err(UpstreamError::status(what, resp.status()));
        }
        Ok(())
    }

    /// Create `tenant`'s member licence for `holder_sub` on `LICENSE_APP_KEY` / `LICENSE_PLAN_CODE`, in the region of
    /// the tenant's `residency` (B1b T10: `LicenseCreate.region_id`; without it licences stores `region_id` NULL). A
    /// residency licences has no region for is issued without one, warned and counted
    /// (`agent_licence_region_unmapped_total`), never refused.
    pub async fn create_licence(
        &self,
        tenant: Uuid,
        holder_sub: &str,
        residency: &str,
    ) -> Result<Created, UpstreamError> {
        let (app_id, plan_id) = self.app_and_plan().await?;
        let region = self.region_id(residency).await?;
        if region.is_none() {
            metrics::counter!("agent_licence_region_unmapped_total").increment(1);
            tracing::warn!(%tenant, residency, "licence_region_unmapped");
        }
        let what = "licences create licence";
        let mut body = json!({
            "app_id": app_id,
            "plan_id": plan_id,
            "subject_realm": self.subject_realm,
        });
        if let Some(r) = region {
            body["region_id"] = Value::String(r);
        }
        let resp = self
            .send(
                self.http
                    .post(self.base.endpoint(&["licenses", "user", holder_sub]))
                    .json(&body),
                Some(tenant),
                what,
            )
            .await?;
        let l: WireLicense = Self::ok_json(resp, what).await?;
        let license_id = l
            .license_id
            .or(l.id)
            .ok_or_else(|| UpstreamError::new(None, "licences create licence: no license_id"))?;
        // Never substitute the Keycloak `sub`: they are different identifiers.
        let subject_id = l
            .subject_id
            .ok_or_else(|| UpstreamError::new(None, "licences create licence: no subject_id"))?;
        Ok(Created {
            license_id,
            subject_id,
        })
    }

    /// Mint `license_id`'s opaque token: a registered PERPETUAL (`long_lived`) `internal` token.
    /// Licences gives a mint without `expires_days` a bounded default (365 days), which would fail
    /// gate 2 for every tenant a year after onboarding while its licence is still active. Licences
    /// refuses a long-lived `api` token outside LONG_LIVED_API_TENANTS (422
    /// `long_lived_not_permitted`) but registers `internal` ones; the token is only used server-side
    /// for gate 2's `/license/introspect`, which never reads the token type. A long-lived mint needs
    /// the SA's `license-admin` realm role (else 403 `long_lived_requires_license_admin`); a refusal
    /// keeps licences' `detail.error` in the error, and still fails the issuance.
    pub async fn mint(&self, tenant: Uuid, license_id: &str) -> Result<String, UpstreamError> {
        let what = "licences mint token";
        let url = self.base.endpoint_with_query(
            &["licenses", license_id, "tokens"],
            &[
                ("token_type", "internal"),
                ("long_lived", "true"),
                ("long_lived_owner", MINT_OWNER),
                ("long_lived_reason", MINT_REASON),
            ],
        );
        let resp = self
            .send(self.http.post(url).json(&json!({})), Some(tenant), what)
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let refusal = resp
                .json::<Value>()
                .await
                .ok()
                .and_then(|b| refusal_code(&b));
            let message = match refusal {
                Some(code) => format!("{what} failed: HTTP {status}: {code}"),
                None => format!("{what} failed: HTTP {status}"),
            };
            return Err(UpstreamError::new(Some(status.as_u16()), message));
        }
        let t: WireToken = Self::ok_json(resp, what).await?;
        t.token
            .filter(|s| !s.is_empty())
            .ok_or_else(|| UpstreamError::new(None, "licences mint token: no token"))
    }

    /// Revoke a licence by id: `Ok(true)` only when the body says `revoked: true`. Licences answers 200
    /// `{"revoked": false}` for an unknown licence or a tenant mismatch (commonpractice `revokeConfirmed`,
    /// la.ts:1122), which is `Ok(false)`: a 200 is not a revocation.
    pub async fn revoke(&self, tenant: Uuid, license_id: &str) -> Result<bool, UpstreamError> {
        let what = "licences revoke";
        let resp = self
            .send(
                self.http
                    .post(self.base.endpoint(&["license", "revoke"]))
                    .json(&json!({"license_id": license_id})),
                Some(tenant),
                what,
            )
            .await?;
        let r: WireRevoke = Self::ok_json(resp, what).await?;
        Ok(r.revoked == Some(true))
    }

    /// Ensure `bundle_key` for `tenant` (licences `POST /internal/tenants/{id}/bundles/{key}/ensure`, role
    /// `bundle-provisioner` / `license-admin` / `license-manager`): creates the dependency licences of the bundle's
    /// closure apps (held by service subjects), insert-or-adopt, so it is idempotent. `refused` names the apps
    /// licences would not cover. A 403 is an `UpstreamError` with `status == Some(403)` (the role is missing).
    pub async fn ensure_bundle(
        &self,
        tenant: Uuid,
        bundle_key: &str,
        plans: &[&str],
    ) -> Result<Ensured, UpstreamError> {
        let what = "licences bundle ensure";
        let url = self.base.endpoint(&[
            "internal",
            "tenants",
            &tenant.to_string(),
            "bundles",
            bundle_key,
            "ensure",
        ]);
        let resp = self
            .send(
                self.http.post(url).json(&json!({ "plans": plans })),
                None,
                what,
            )
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let refusal = resp
                .json::<Value>()
                .await
                .ok()
                .and_then(|b| refusal_code(&b));
            let message = match refusal {
                Some(code) => format!("{what} failed: HTTP {status}: {code}"),
                None => format!("{what} failed: HTTP {status}"),
            };
            return Err(UpstreamError::new(Some(status.as_u16()), message));
        }
        let body: Value = Self::ok_json(resp, what).await?;
        let refused = body
            .get("refused")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|r| {
                        (
                            r.get("app")
                                .and_then(Value::as_str)
                                .unwrap_or("?")
                                .to_string(),
                            r.get("reason")
                                .and_then(Value::as_str)
                                .unwrap_or("?")
                                .to_string(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(Ensured { refused })
    }
}

/// What a bundle ensure answered: the apps licences refused to cover, as (app, reason).
#[derive(Clone, Debug, Default)]
pub struct Ensured {
    pub refused: Vec<(String, String)>,
}

/// The plan codes a bundle is ensured with.
pub const ENSURE_PLANS: [&str; 1] = ["standard"];

/// `long_lived_owner` of every tenant licence token: who answers for the perpetual token.
const MINT_OWNER: &str = "agent";
/// `long_lived_reason` of every licence token (tenant and member).
const MINT_REASON: &str = "agent licence token (tenant or member); revoked on offboarding";

/// Licences' refusal code in an error body: `detail.error` (`{"detail": {"error": ...}}`) or a plain
/// string `detail`.
fn refusal_code(body: &Value) -> Option<String> {
    let detail = body.get("detail")?;
    detail
        .get("error")
        .and_then(Value::as_str)
        .or_else(|| detail.as_str())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_names_fit_the_registry_pattern() {
        let id = Uuid::parse_str("6f1c2a3b-4d5e-4f60-8a7b-9c0d1e2f3a4b").unwrap();
        assert_eq!(
            registry_name("Acme Health B.V.", id),
            "acme-health-b-v-1e2f3a4b"
        );
        assert_eq!(registry_name("  ---  ", id), "t-1e2f3a4b");
        assert_eq!(registry_name("Zürich", id), "z-rich-1e2f3a4b");
        let long = registry_name(&"a".repeat(100), id);
        assert_eq!(long, format!("{}-1e2f3a4b", "a".repeat(40)));
    }

    #[test]
    fn refusal_codes_are_read_from_licences_error_bodies() {
        assert_eq!(
            refusal_code(&json!({"detail": {"error": "long_lived_requires_license_admin"}})),
            Some("long_lived_requires_license_admin".to_string())
        );
        assert_eq!(
            refusal_code(&json!({"detail": "License not found"})),
            Some("License not found".to_string())
        );
        assert_eq!(refusal_code(&json!({"detail": {"remedy": "x"}})), None);
        assert_eq!(refusal_code(&json!({"error": "x"})), None);
    }
}
