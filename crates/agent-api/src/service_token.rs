//! Client-credentials tokens for this service's own Keycloak service accounts, and the error every
//! upstream admin call returns. Ported from strata_configurator `service_token.rs`; after commonpractice `keycloak-admin.ts` `getAdminToken` and
//! `licenses-admin.ts` `getServiceToken` (patterns only, no shared code): one confidential client, its
//! token cached until shortly before it expires. reqwest is built without its `form`/`query`
//! features, so form bodies and query values are encoded here.
use std::time::{Duration, Instant};

use reqwest::StatusCode;
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::http_base::{BaseUrl, BaseUrlError};

/// A cached token is refreshed this long before Keycloak says it expires.
const EXPIRY_MARGIN: Duration = Duration::from_secs(30);
/// The lifetime assumed when the token response carries no `expires_in` (commonpractice's 60 s).
const DEFAULT_LIFETIME: Duration = Duration::from_secs(60);

/// An upstream call failed: transport, a non-2xx answer, or a body that does not say what it must.
/// `status` is the upstream HTTP status when there was one. Never carries a secret.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct UpstreamError {
    pub status: Option<u16>,
    pub message: String,
}

impl UpstreamError {
    pub fn new(status: Option<u16>, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    /// `what` answered `status`, which is not the answer it must give.
    pub fn status(what: &str, status: StatusCode) -> Self {
        Self::new(
            Some(status.as_u16()),
            format!("{what} failed: HTTP {status}"),
        )
    }

    /// `what` could not be sent, or its body could not be read.
    ///
    /// ⛔ NEVER `{e}`: reqwest's message carries the request URL, and the Keycloak lookup's URL carries the
    /// requester's address (`users?email=…`). The message lands in `signup_provisioning_steps.message` (no
    /// personal data; the queue API returns it) and in logs, so only the KIND of failure is kept — no URL, no
    /// source chain (a decode error's source can quote the body).
    pub fn transport(what: &str, e: &reqwest::Error) -> Self {
        let kind = if e.is_timeout() {
            "timed out"
        } else if e.is_connect() {
            "could not connect"
        } else if e.is_decode() {
            "the response body could not be decoded"
        } else if e.is_body() {
            "the body could not be read"
        } else if e.is_redirect() {
            "too many redirects"
        } else if e.is_request() {
            "the request could not be sent"
        } else {
            "transport error"
        };
        Self::new(None, format!("{what} failed: {kind}"))
    }
}

/// `s` percent-encoded for a query value, a path segment or a form field: every byte except the
/// RFC 3986 unreserved characters.
pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// An `application/x-www-form-urlencoded` body.
pub fn form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    expires_in: Option<u64>,
}

/// One service account's access token, fetched with the client-credentials grant and cached.
pub struct ServiceToken {
    /// The token endpoint, parsed once (http(s), a host, no query or fragment). Construction cannot fail the boot,
    /// so an unusable URL is kept as the error and every [`Self::get`] fails closed with it — never a raw string
    /// handed to reqwest.
    token_url: Result<BaseUrl, BaseUrlError>,
    client_id: String,
    client_secret: String,
    http: reqwest::Client,
    /// The token and when it must be refreshed. The lock is held across a fetch, so concurrent
    /// callers wait for one grant instead of each sending their own.
    cached: Mutex<Option<(String, Instant)>>,
}

impl ServiceToken {
    pub fn new(
        token_url: &str,
        client_id: &str,
        client_secret: &str,
        http: reqwest::Client,
    ) -> Self {
        Self {
            token_url: BaseUrl::parse(token_url),
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            http,
            cached: Mutex::new(None),
        }
    }

    pub fn from_creds(c: &crate::config::ClientCredentials, http: reqwest::Client) -> Self {
        Self::new(&c.token_url, &c.client_id, &c.client_secret, http)
    }

    /// The cached token, or a fresh one when it is missing or about to expire.
    pub async fn get(&self) -> Result<String, UpstreamError> {
        let mut c = self.cached.lock().await;
        if let Some((token, refresh_at)) = c.as_ref()
            && Instant::now() < *refresh_at
        {
            return Ok(token.clone());
        }
        let body = form(&[
            ("grant_type", "client_credentials"),
            ("client_id", self.client_id.as_str()),
            ("client_secret", self.client_secret.as_str()),
        ]);
        let token_url = self.token_url.as_ref().map_err(|e| {
            UpstreamError::new(
                None,
                format!("service-account token endpoint is unusable: {e}"),
            )
        })?;
        let send = self
            .http
            .post(token_url.endpoint(&[]))
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(body)
            .send();
        let resp = crate::observability::upstream_http("keycloak", "token".into(), send)
            .await
            .map_err(|e| UpstreamError::transport("service-account token request", &e))?;
        if !resp.status().is_success() {
            return Err(UpstreamError::status(
                "service-account token request",
                resp.status(),
            ));
        }
        let t: TokenResponse = resp
            .json()
            .await
            .map_err(|e| UpstreamError::transport("service-account token response", &e))?;
        let token = t.access_token.filter(|s| !s.is_empty()).ok_or_else(|| {
            UpstreamError::new(None, "service-account token response has no access_token")
        })?;
        let lifetime = t
            .expires_in
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_LIFETIME);
        *c = Some((
            token.clone(),
            Instant::now() + lifetime.saturating_sub(EXPIRY_MARGIN),
        ));
        Ok(token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The URL (and so the requester's address in a lookup's query string) never reaches the message.
    #[tokio::test]
    async fn a_transport_error_names_no_url_and_no_query() {
        let e = reqwest::Client::new()
            .get("http://127.0.0.1:1/admin/realms/portal/users?email=ann%40globex.com&exact=true")
            .send()
            .await
            .unwrap_err();
        // control: reqwest itself does carry the URL, so the redaction is what is being tested
        assert!(e.to_string().contains("ann%40globex.com"), "{e}");
        let m = UpstreamError::transport("keycloak user lookup by email", &e).message;
        assert!(
            !m.contains("globex") && !m.contains("127.0.0.1") && !m.contains('?'),
            "{m}"
        );
        assert!(
            m.starts_with("keycloak user lookup by email failed: "),
            "{m}"
        );
    }

    #[test]
    fn encodes_everything_but_unreserved() {
        assert_eq!(encode("a.b-c_d~e"), "a.b-c_d~e");
        assert_eq!(encode("j+x@ex ample.com/"), "j%2Bx%40ex%20ample.com%2F");
        assert_eq!(encode("é"), "%C3%A9");
        assert_eq!(
            form(&[
                ("grant_type", "client_credentials"),
                ("client_secret", "a&b=c")
            ]),
            "grant_type=client_credentials&client_secret=a%26b%3Dc"
        );
    }
}
