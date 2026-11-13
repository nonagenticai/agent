//! Shared connector vocabulary: what the mapping engine renders and what a transport answers.
//! Every other connector module depends on these types; change them only together with their users.
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `AuthKind` of `web/src/lib/api/types/connectors.ts` (same literals).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthKind {
    None,
    ApiKeyHeader,
    Basic,
    BearerStatic,
    Oauth2ClientCredentials,
    BundleToken,
    /// A platform app licence for THIS instance, minted on demand by the executor and carried in the call's
    /// secret map as `licence_token` (S06b: the conveyor's `/mcp`). Never a stored credential: the token lives
    /// minutes, so nothing in `connector_credential` can go stale.
    LicenceBearer,
    /// Mutual TLS: the credential's `secret.client_cert_pem` + `secret.client_key_pem` (required) are the client
    /// identity; `ca_pem` (secret or config, optional) is then the ONLY trusted root. No header is sent, and there
    /// is no way to turn certificate verification off.
    Mtls,
}

impl AuthKind {
    /// Whether this kind is authenticated by a stored `connector_credential`. A licence bearer is minted per
    /// call, so — like `none` — there is nothing stored to look for.
    pub fn needs_stored_credential(self) -> bool {
        !matches!(self, AuthKind::None | AuthKind::LicenceBearer)
    }
}

/// A request after placeholder rendering. `path` is relative to the instance `base_url` (never absolute: the
/// validator refuses `host_not_from_instance`); secrets never appear in `path` or `query` (`secret_in_url`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RenderedRequest {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub body: Option<Value>,
}

/// What a transport answers for one call: the HTTP status (0 = no response: timeout/connect) and the parsed
/// body (JSON when the response was JSON, else a JSON string of at most the netguard sample size).
#[derive(Debug, Clone, PartialEq)]
pub struct RawResponse {
    pub status: u16,
    pub body: Value,
    pub retry_after_secs: Option<u64>,
}

/// Canonical failure of one operation call. `code` is one of the UI's canonical codes
/// (`connector_auth_failed`, `connector_misconfigured`, `not_found`, `invalid_input`, `rate_limited`,
/// `upstream_unavailable`, `invalid_target`, `invalid_url`, …). `detail` never carries a secret or upstream body.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {detail}")]
pub struct OpError {
    pub code: &'static str,
    pub detail: String,
}

impl OpError {
    pub fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

/// Decrypted credential + non-secret configuration for one call. Never `Debug`-printed with values.
#[derive(Clone, Default)]
pub struct CallSecrets {
    /// Non-secret config fields (`env_ids` + credential_schema non-secret fields), the `{{config.*}}` namespace.
    pub config: serde_json::Map<String, Value>,
    /// Secret fields, the `{{secret.*}}` namespace.
    pub secret: serde_json::Map<String, Value>,
}

impl std::fmt::Debug for CallSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallSecrets")
            .field("config_keys", &self.config.keys().collect::<Vec<_>>())
            .field("secret_keys", &self.secret.keys().collect::<Vec<_>>())
            .finish()
    }
}
