//! The `rest` transport: one HTTP request to `base_url` + the rendered relative path, over a netguard-pinned
//! client. Any HTTP status is a [`RawResponse`]; an OAuth2 401 drops the cached token, re-mints once and retries once.
//! An `mtls` instance presents its client certificate on that client. A write (`write`: the op is not R) of a
//! template with a CSRF prefetch ([`CsrfPrefetch`], `auth_params.csrf_fetch_path`) first fetches the token on the
//! same client and sends it, with the prefetch's session cookies, on the write only.
use std::time::Duration;

use reqwest::header::{
    ACCEPT, COOKIE, HeaderMap, HeaderName, HeaderValue, RETRY_AFTER, SET_COOKIE,
};
use reqwest::{Method, Url};
use serde_json::Value;

use super::auth::{self, AuthHeaders, AuthSpec, TokenKey};
use super::{
    CallTarget, Transport, body_value, misconfigured, read_capped, redact_body, stub,
    transport_error,
};
use crate::connectors::model::CsrfPrefetch;
use crate::connectors::wire::{AuthKind, CallSecrets, OpError, RawResponse, RenderedRequest};
use crate::netguard;

/// Request headers an operation may not set (the client owns framing and the host).
const OWNED_HEADERS: [&str; 4] = ["host", "content-length", "transfer-encoding", "connection"];

pub async fn call(
    t: &Transport,
    target: &CallTarget,
    auth_spec: &AuthSpec,
    secrets: &CallSecrets,
    req: &RenderedRequest,
    timeout: Duration,
    write: bool,
) -> Result<RawResponse, OpError> {
    let base = netguard::check_url(&target.base_url, t.policy(), target.tenant)?;
    let url = join_url(&base, req)?;
    let method = Method::from_bytes(req.method.trim().to_ascii_uppercase().as_bytes())
        .map_err(|_| misconfigured("the operation's HTTP method is not valid"))?;
    let headers = request_headers(&req.headers)?;
    let tls = auth::client_tls(auth_spec, secrets)?;
    let client =
        netguard::pinned_client_tls(&base, t.policy(), target.tenant, timeout, tls.as_ref())
            .await?;
    let csrf = if write {
        CsrfPrefetch::from_auth_params(&auth_spec.params)
    } else {
        None
    };
    let mut redact = auth::secret_strings(secrets);
    let creds = auth::headers_for(t, target, auth_spec, secrets, timeout).await?;
    redact.extend(creds.redact.iter().cloned());
    let body = req.body.as_ref();
    let call = Call {
        client: &client,
        base: &base,
        csrf: csrf.as_ref(),
        method: &method,
        url: &url,
        headers: &headers,
        body,
    };
    let mut resp = call.send(&creds, &mut redact).await?;
    if resp.status == 401 && auth_spec.kind == AuthKind::Oauth2ClientCredentials {
        t.tokens().invalidate(&TokenKey::of(target));
        let fresh = auth::headers_for(t, target, auth_spec, secrets, timeout).await?;
        redact.extend(fresh.redact.iter().cloned());
        resp = call.send(&fresh, &mut redact).await?;
    }
    resp.body = redact_body(resp.body, &redact);
    Ok(resp)
}

/// One operation request, before its credential headers.
struct Call<'a> {
    client: &'a reqwest::Client,
    base: &'a Url,
    csrf: Option<&'a CsrfPrefetch>,
    method: &'a Method,
    url: &'a Url,
    headers: &'a HeaderMap,
    body: Option<&'a Value>,
}

impl Call<'_> {
    /// Sends the request with `creds`; with a CSRF prefetch, fetches the token first (with the same `creds`) and
    /// adds it and the prefetch's cookies to this request only. The token and cookies join `redact`.
    async fn send(
        &self,
        creds: &AuthHeaders,
        redact: &mut Vec<String>,
    ) -> Result<RawResponse, OpError> {
        let Some(csrf) = self.csrf else {
            return send(
                self.client,
                self.method,
                self.url,
                self.headers,
                creds,
                self.body,
            )
            .await;
        };
        let got = csrf_prefetch(self.client, self.base, csrf, creds).await?;
        let mut h = self.headers.clone();
        if let Ok(s) = got.token.to_str() {
            redact.push(s.to_owned());
        }
        h.insert(got.header, got.token);
        if let Some(cookie) = got.cookie {
            if let Ok(s) = cookie.to_str() {
                redact.push(s.to_owned());
            }
            h.insert(COOKIE, cookie);
        }
        send(self.client, self.method, self.url, &h, creds, self.body).await
    }
}

/// What a CSRF prefetch answered: the header to send, its token, and the session cookies (`name=value; …`).
struct CsrfToken {
    header: HeaderName,
    token: HeaderValue,
    cookie: Option<HeaderValue>,
}

/// `GET {base}{fetch_path}` with the credential headers and `{header}: Fetch`. 401/403 is
/// `connector_auth_failed`; 429 `rate_limited`; 5xx or no response `upstream_unavailable`; any other non-2xx, or a
/// 2xx without a token, `connector_misconfigured` (the fetch path is wrong). Never echoes the token or a cookie.
async fn csrf_prefetch(
    client: &reqwest::Client,
    base: &Url,
    csrf: &CsrfPrefetch,
    creds: &AuthHeaders,
) -> Result<CsrfToken, OpError> {
    const WHAT: &str = "the CSRF token prefetch";
    let header = HeaderName::from_bytes(csrf.header.as_bytes())
        .map_err(|_| misconfigured("auth_params.csrf_header is not a valid HTTP header name"))?;
    if !csrf.fetch_path.starts_with('/') || csrf.fetch_path.starts_with("//") {
        return Err(misconfigured(
            "auth_params.csrf_fetch_path must be a path relative to the instance base URL, starting with /",
        ));
    }
    let url = join_url(
        base,
        &RenderedRequest {
            method: "GET".into(),
            path: csrf.fetch_path.clone(),
            ..Default::default()
        },
    )?;
    let mut h = HeaderMap::new();
    for (name, value) in &creds.headers {
        h.insert(name.clone(), value.clone());
    }
    h.insert(ACCEPT, HeaderValue::from_static("application/json"));
    h.insert(header.clone(), HeaderValue::from_static("Fetch"));
    let resp = client
        .get(url)
        .headers(h)
        .send()
        .await
        .map_err(|e| transport_error(&e, WHAT))?;
    let status = resp.status().as_u16();
    match status {
        200..=299 => {}
        401 | 403 => {
            return Err(OpError::new(
                "connector_auth_failed",
                format!("{WHAT} answered HTTP {status}"),
            ));
        }
        429 => {
            return Err(OpError::new(
                "rate_limited",
                format!("{WHAT} answered HTTP 429"),
            ));
        }
        500..=599 => {
            return Err(OpError::new(
                "upstream_unavailable",
                format!("{WHAT} answered HTTP {status}"),
            ));
        }
        _ => {
            return Err(misconfigured(format!(
                "{WHAT} answered HTTP {status} (check auth_params.csrf_fetch_path)"
            )));
        }
    }
    let mut token = resp
        .headers()
        .get(&header)
        .filter(|v| {
            let b = v.as_bytes();
            !b.trim_ascii().is_empty() && !b.eq_ignore_ascii_case(b"required")
        })
        .cloned()
        .ok_or_else(|| {
            misconfigured(format!(
                "{WHAT} answered without a {} header",
                header.as_str()
            ))
        })?;
    token.set_sensitive(true);
    let pairs: Vec<&str> = resp
        .headers()
        .get_all(SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|v| v.split(';').next())
        .map(str::trim)
        .filter(|p| p.contains('='))
        .collect();
    let cookie = if pairs.is_empty() {
        None
    } else {
        let mut v = HeaderValue::from_str(&pairs.join("; "))
            .map_err(|_| misconfigured(format!("{WHAT} answered an unusable session cookie")))?;
        v.set_sensitive(true);
        Some(v)
    };
    Ok(CsrfToken {
        header,
        token,
        cookie,
    })
}

/// `base` (query/fragment dropped) + the relative `path` + the rendered query. The host, port and scheme can
/// never change: the path is set through the URL's path setter (`?`/`#` are percent-encoded) and re-checked.
pub fn join_url(base: &Url, req: &RenderedRequest) -> Result<Url, OpError> {
    let mut url = base.clone();
    url.set_query(None);
    url.set_fragment(None);
    let rel = req.path.trim().trim_start_matches('/');
    if !rel.is_empty() {
        let joined = format!("{}/{rel}", base.path().trim_end_matches('/'));
        url.set_path(&joined);
    }
    if !req.query.is_empty() {
        let mut q = url.query_pairs_mut();
        for (k, v) in &req.query {
            q.append_pair(k, v);
        }
    }
    if url.scheme() != base.scheme()
        || url.host_str() != base.host_str()
        || url.port_or_known_default() != base.port_or_known_default()
    {
        return Err(misconfigured(
            "the operation path must stay on the instance base URL",
        ));
    }
    Ok(url)
}

fn request_headers(pairs: &[(String, String)]) -> Result<HeaderMap, OpError> {
    let mut map = HeaderMap::new();
    for (k, v) in pairs {
        let name = HeaderName::from_bytes(k.trim().as_bytes()).map_err(|_| {
            misconfigured("an operation header name is not a valid HTTP header name")
        })?;
        if OWNED_HEADERS.contains(&name.as_str()) || name.as_str() == stub::FIXTURE_HEADER {
            continue;
        }
        let value = HeaderValue::from_str(v).map_err(|_| {
            misconfigured("an operation header value is not valid in an HTTP header")
        })?;
        map.append(name, value);
    }
    Ok(map)
}

async fn send(
    client: &reqwest::Client,
    method: &Method,
    url: &Url,
    headers: &HeaderMap,
    creds: &AuthHeaders,
    body: Option<&Value>,
) -> Result<RawResponse, OpError> {
    let mut h = headers.clone();
    for (name, value) in &creds.headers {
        h.insert(name.clone(), value.clone());
    }
    if !h.contains_key(ACCEPT) {
        h.insert(ACCEPT, HeaderValue::from_static("application/json"));
    }
    let mut rb = client.request(method.clone(), url.clone()).headers(h);
    if let Some(b) = body {
        rb = rb.json(b);
    }
    let mut resp = rb
        .send()
        .await
        .map_err(|e| transport_error(&e, "the upstream"))?;
    let status = resp.status().as_u16();
    let retry_after_secs = resp
        .headers()
        .get(RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok());
    let bytes = read_capped(&mut resp, "the upstream").await?;
    Ok(RawResponse {
        status,
        body: body_value(&bytes),
        retry_after_secs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(path: &str, query: &[(&str, &str)]) -> RenderedRequest {
        RenderedRequest {
            method: "GET".into(),
            path: path.into(),
            query: query
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn join_keeps_the_base_host() {
        let base = Url::parse("https://shop.example.com/api/").unwrap();
        assert_eq!(
            join_url(&base, &req("/v2/orders", &[("page", "2"), ("q", "a b")]))
                .unwrap()
                .as_str(),
            "https://shop.example.com/api/v2/orders?page=2&q=a+b"
        );
        for evil in ["//evil.com/x", "https://evil.com/x", "@evil.com", "x?y#z"] {
            let u = join_url(&base, &req(evil, &[])).unwrap();
            assert_eq!(u.host_str(), Some("shop.example.com"), "{evil}");
            assert_eq!(u.query(), None, "{evil}");
        }
        let root = Url::parse("https://shop.example.com").unwrap();
        assert_eq!(
            join_url(&root, &req("", &[])).unwrap().as_str(),
            "https://shop.example.com/"
        );
    }
}
