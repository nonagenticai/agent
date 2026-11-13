//! The licences introspect client (B7 T2, from P3 T7's introspect half).
//!
//! Licences' endpoint is `POST /license/introspect` with body `{"token": "<raw token>"}` (licenses `origin/main`
//! `76e4877`, `main.py` `introspect_token` + `TokenIntrospectRequest` / `TokenIntrospectResponse`; the answer is built
//! by `models.py` `_introspect_from_session`). It is public by default (`INTROSPECT_REQUIRE_AUTH=false`: the token in
//! the body is the credential); agent sends its licences-admin service-account bearer anyway, so arming that flag does
//! not break the refresher. The answers:
//!
//! - active: `{active: true, tenant_id, license_id, subject_id, app: {key, ...}, plan: {code, ...}, status,
//!   expires_at, ...}`;
//! - inactive: `{active: false, reason}` with `reason` one of `token_not_found`, `token_invalid` (the token is revoked
//!   or expired; a licence revoke revokes all its tokens, `models.py` `revoke_license`), `license_not_found`,
//!   `license_inactive` (the licence is expired, suspended or not yet started). No `status` on an inactive answer.
//!
//! `LICENSE_INTROSPECT_URL` is the FULL endpoint URL (e.g. `http://licenses:8000/license/introspect`).
//! Unset: no client exists ([`Introspector::for_state`] is `None`), the refresher idles and says so at boot.
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::ClientCredentials;
use crate::service_token::{ServiceToken, UpstreamError};
use crate::state::AppState;

/// One introspect call is bounded well under the HTTP client's 10 s: the refresher walks tenants one by one.
pub const INTROSPECT_TIMEOUT: Duration = Duration::from_secs(3);

const WHAT: &str = "licences introspect";

/// What licences said about one token.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Introspection {
    pub active: bool,
    /// The licence's stored status (`active` on an active answer); licences sends none on an inactive answer.
    pub status: Option<String>,
    /// Why the token is inactive (`token_not_found`, `token_invalid`, `license_not_found`, `license_inactive`).
    pub reason: Option<String>,
    pub expires_at: Option<String>,
    /// The licence's app key (`app.key`).
    pub app: Option<String>,
    pub tenant_id: Option<String>,
    pub license_id: Option<String>,
    /// The licence's plan code (`plan.code`).
    pub plan: Option<String>,
}

#[derive(Deserialize)]
struct Wire {
    active: Option<bool>,
    status: Option<String>,
    reason: Option<String>,
    expires_at: Option<String>,
    app: Option<Value>,
    tenant_id: Option<String>,
    license_id: Option<String>,
    plan: Option<Value>,
}

impl From<Wire> for Introspection {
    fn from(w: Wire) -> Self {
        let field = |v: &Option<Value>, k: &str| {
            v.as_ref()
                .and_then(|o| o.get(k))
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        Self {
            active: w.active == Some(true),
            status: w.status,
            reason: w.reason,
            expires_at: w.expires_at,
            app: field(&w.app, "key"),
            tenant_id: w.tenant_id,
            license_id: w.license_id,
            plan: field(&w.plan, "code"),
        }
    }
}

/// The client: the endpoint, the shared HTTP client and the licences-admin bearer.
pub struct Introspector {
    url: String,
    http: reqwest::Client,
    bearer: ServiceToken,
}

impl Introspector {
    pub fn new(url: &str, creds: &ClientCredentials, http: reqwest::Client) -> Self {
        Self {
            url: url.to_string(),
            http: http.clone(),
            bearer: ServiceToken::from_creds(creds, http),
        }
    }

    /// The process's client for `st`'s configuration (`None` while `LICENSE_INTROSPECT_URL` is unset). Cached per
    /// (URL, service account), so the bearer is fetched once per lifetime, not per call.
    pub fn for_state(st: &AppState) -> Option<Arc<Self>> {
        let url = st.cfg.license_introspect_url.as_deref()?;
        let creds = &st.cfg.identity.licenses_admin.creds;
        let key = format!("{url}\n{}\n{}", creds.token_url, creds.client_id);
        static CACHE: OnceLock<Mutex<HashMap<String, Arc<Introspector>>>> = OnceLock::new();
        let cache = CACHE.get_or_init(Mutex::default);
        let mut map = cache.lock().unwrap_or_else(|p| p.into_inner());
        Some(
            map.entry(key)
                .or_insert_with(|| Arc::new(Self::new(url, creds, st.http.clone())))
                .clone(),
        )
    }

    /// Introspect `token`. `Err` = licences could not be asked or did not answer 2xx with a body (the caller writes
    /// `inactive` / `license_unavailable`, never keeps an old `active`). A missing bearer is logged and the call is
    /// sent without it: the endpoint is public by default.
    pub async fn check(&self, token: &str) -> Result<Introspection, UpstreamError> {
        let mut req = self
            .http
            .post(self.url.as_str())
            .timeout(INTROSPECT_TIMEOUT)
            .json(&json!({ "token": token }));
        match self.bearer.get().await {
            Ok(b) => req = req.bearer_auth(b),
            Err(e) => tracing::warn!(error = %e, "licence_introspect_without_bearer"),
        }
        let resp = crate::observability::upstream_http("licences", "introspect".into(), req.send())
            .await
            .map_err(|e| UpstreamError::transport(WHAT, &e))?;
        if !resp.status().is_success() {
            return Err(UpstreamError::status(WHAT, resp.status()));
        }
        let w: Wire = resp
            .json()
            .await
            .map_err(|e| UpstreamError::transport(WHAT, &e))?;
        if w.active.is_none() {
            return Err(UpstreamError::new(None, "licences introspect: no `active`"));
        }
        Ok(w.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_reads_app_key_and_plan_code() {
        let w: Wire = serde_json::from_value(json!({
            "active": true, "tenant_id": "t", "license_id": "l", "status": "active",
            "app": {"id": "a1", "key": "agent"}, "plan": {"id": "p1", "code": "byo"},
            "expires_at": null, "roles": []
        }))
        .unwrap();
        let i = Introspection::from(w);
        assert!(i.active);
        assert_eq!(i.app.as_deref(), Some("agent"));
        assert_eq!(i.plan.as_deref(), Some("byo"));
        let w: Wire =
            serde_json::from_value(json!({"active": false, "reason": "token_invalid"})).unwrap();
        let i = Introspection::from(w);
        assert!(!i.active);
        assert_eq!(i.reason.as_deref(), Some("token_invalid"));
        assert_eq!(i.app, None);
    }
}
