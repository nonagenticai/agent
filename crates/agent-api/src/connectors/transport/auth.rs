//! The v1 auth kinds (B7 T6): how a call presents the instance credential. Templates here are plain
//! `{{config.key}}` / `{{secret.key}}` substitution over [`CallSecrets`]. OAuth2 client-credentials tokens are
//! minted against a netguard-checked token URL and cached in memory per (tenant, instance, credential_version).
use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::{CallTarget, Transport, body_value, misconfigured, read_capped, transport_error};
use crate::connectors::model::TLS_VERIFY_OFF;
use crate::connectors::wire::{AuthKind, CallSecrets, OpError};
use crate::netguard::{self, ClientTls};

/// A template's `auth_kind` + `auth_params`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthSpec {
    pub kind: AuthKind,
    #[serde(default)]
    pub params: BTreeMap<String, String>,
}

impl AuthSpec {
    pub fn new(kind: AuthKind) -> Self {
        Self {
            kind,
            params: BTreeMap::new(),
        }
    }
}

/// The headers that present the credential, and every derived secret string (token, header value) that must be
/// scrubbed from response bodies. Deliberately not `Debug`.
#[derive(Default)]
pub struct AuthHeaders {
    pub headers: Vec<(HeaderName, HeaderValue)>,
    pub redact: Vec<String>,
}

fn auth_failed(detail: impl Into<String>) -> OpError {
    OpError::new("connector_auth_failed", detail)
}

fn scalar(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// The `bearer_static` token: `auth_params.token_field` when set, else `secret.token`, else the credential's only
/// secret field (platform templates name it `service_token`, `restricted_key`, …).
fn bearer_token(spec: &AuthSpec, secrets: &CallSecrets) -> Option<String> {
    if let Some(name) = spec
        .params
        .get("token_field")
        .filter(|n| !n.trim().is_empty())
    {
        return field(&secrets.secret, name.trim());
    }
    field(&secrets.secret, "token").or_else(|| match secrets.secret.len() {
        1 => secrets
            .secret
            .keys()
            .next()
            .and_then(|k| field(&secrets.secret, k)),
        _ => None,
    })
}

fn field(map: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    map.get(key).and_then(scalar).filter(|s| !s.is_empty())
}

/// Every non-empty secret value of the credential (scrubbed from error texts and response bodies).
pub fn secret_strings(secrets: &CallSecrets) -> Vec<String> {
    secrets
        .secret
        .values()
        .filter_map(scalar)
        .filter(|s| !s.is_empty())
        .collect()
}

/// Renders `{{config.key}}` / `{{secret.key}}` placeholders. A missing secret is `connector_auth_failed`, a
/// missing config field or a malformed placeholder `connector_misconfigured`; the error names the field, never
/// a value.
pub fn render(template: &str, secrets: &CallSecrets) -> Result<String, OpError> {
    render_with(template, secrets, true, None)
}

/// The server-side values of `auth_params.static_headers`: `{{tenant.id}}` (the calling instance's tenant) and
/// `{{system.env}}` (the env of the call, `test` | `prod`). The tenant cannot set either — instance config is
/// tenant-written, so a `{{config.*}}` stand-in would let a tenant pick another tenant's partition.
#[derive(Debug, Clone, Copy)]
pub struct ServerVars<'a> {
    pub tenant: Uuid,
    pub env: &'a str,
}

/// [`render`]; with `allow_secret` false a `{{secret.*}}` placeholder is `connector_misconfigured`; with `server`
/// set, `{{tenant.id}}` / `{{system.env}}` render from it (else they are unknown placeholders).
fn render_with(
    template: &str,
    secrets: &CallSecrets,
    allow_secret: bool,
    server: Option<ServerVars<'_>>,
) -> Result<String, OpError> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            return Err(misconfigured(
                "an auth template has an unclosed placeholder",
            ));
        };
        out.push_str(&lookup(after[..end].trim(), secrets, allow_secret, server)?);
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

fn lookup(
    name: &str,
    secrets: &CallSecrets,
    allow_secret: bool,
    server: Option<ServerVars<'_>>,
) -> Result<String, OpError> {
    match (name.split_once('.'), server) {
        (Some(("tenant", "id")), Some(v)) => return Ok(v.tenant.to_string()),
        (Some(("system", "env")), Some(v)) => return Ok(v.env.to_string()),
        (Some(("tenant" | "system", _)), _) => {
            return Err(misconfigured(format!(
                "unknown auth placeholder `{name}` (only {{{{tenant.id}}}} and {{{{system.env}}}}, in auth_params.static_headers)"
            )));
        }
        _ => {}
    }
    match name.split_once('.') {
        Some(("secret", _)) if !allow_secret => Err(misconfigured(
            "auth_params.static_headers may use {{config.*}}, {{tenant.id}} and {{system.env}} placeholders only, never {{secret.*}}",
        )),
        Some(("config", key)) => match secrets.config.get(key) {
            Some(v) => {
                scalar(v).ok_or_else(|| misconfigured(format!("config.{key} is not a text value")))
            }
            None => Err(misconfigured(format!("the instance has no config.{key}"))),
        },
        Some(("secret", key)) => match secrets.secret.get(key) {
            Some(v) => {
                scalar(v).ok_or_else(|| misconfigured(format!("secret.{key} is not a text value")))
            }
            None => Err(auth_failed(format!("the credential has no secret.{key}"))),
        },
        _ => Err(misconfigured(format!(
            "unknown auth placeholder `{name}` (use config.* or secret.*)"
        ))),
    }
}

/// One credential header; `inner` is the bare secret part of `value` (token, base64 pair), also redacted.
fn one(name: &str, value: String, inner: Option<String>) -> Result<AuthHeaders, OpError> {
    let n = HeaderName::from_bytes(name.trim().as_bytes())
        .map_err(|_| misconfigured("the auth header name is not a valid HTTP header name"))?;
    let mut v = HeaderValue::from_str(&value)
        .map_err(|_| misconfigured("the auth header value is not valid in an HTTP header"))?;
    v.set_sensitive(true);
    let mut redact = vec![value];
    redact.extend(inner);
    Ok(AuthHeaders {
        headers: vec![(n, v)],
        redact,
    })
}

/// The username/password of `basic`: `auth_params.username_template` / `password_template` when set (each
/// falling back to the default below), else `config.username` (or `secret.username`) + `secret.password`, else
/// the `secret.api_key` + `secret.api_secret` pair (Frappe).
fn basic_pair(spec: &AuthSpec, secrets: &CallSecrets) -> Result<(String, String), OpError> {
    let default_user =
        || field(&secrets.config, "username").or_else(|| field(&secrets.secret, "username"));
    let user_t = template_param(spec, "username_template");
    let pass_t = template_param(spec, "password_template");
    if user_t.is_some() || pass_t.is_some() {
        let u = match user_t {
            Some(t) => render(t, secrets)?,
            None => default_user()
                .ok_or_else(|| auth_failed("basic auth needs a username (config.username)"))?,
        };
        let p = match pass_t {
            Some(t) => render(t, secrets)?,
            None => field(&secrets.secret, "password")
                .ok_or_else(|| auth_failed("the credential has no secret.password"))?,
        };
        return Ok((u, p));
    }
    let user = default_user();
    if let (Some(u), Some(p)) = (user, field(&secrets.secret, "password")) {
        return Ok((u, p));
    }
    if let (Some(k), Some(s)) = (
        field(&secrets.secret, "api_key"),
        field(&secrets.secret, "api_secret"),
    ) {
        return Ok((k, s));
    }
    Err(auth_failed(
        "basic auth needs a username with secret.password, or secret.api_key with secret.api_secret",
    ))
}

/// `auth_params.static_headers`: a JSON-object string of header → value, values rendered over `{{config.*}}`
/// and the [`ServerVars`] `{{tenant.id}}` / `{{system.env}}` only (a `{{secret.*}}` placeholder is refused). Sent
/// on every request of the instance; not secret.
pub fn static_headers(
    spec: &AuthSpec,
    secrets: &CallSecrets,
    server: ServerVars<'_>,
) -> Result<Vec<(HeaderName, HeaderValue)>, OpError> {
    let Some(raw) = template_param(spec, "static_headers") else {
        return Ok(Vec::new());
    };
    let bad =
        || misconfigured("auth_params.static_headers must be a JSON object of header name → text");
    let map: serde_json::Map<String, Value> = serde_json::from_str(raw).map_err(|_| bad())?;
    let mut out = Vec::with_capacity(map.len());
    for (k, v) in &map {
        let template = v.as_str().ok_or_else(bad)?;
        // Saves refuse these (`check_auth_params`); a template stored before the rule existed loses the header.
        if let Some(why) = reserved_static_header(k, template) {
            tracing::warn!(
                header = %k.trim(),
                reason = why,
                "connector_reserved_header_dropped"
            );
            continue;
        }
        let value = render_with(template, secrets, false, Some(server))?;
        let name = HeaderName::from_bytes(k.trim().as_bytes()).map_err(|_| bad())?;
        let value = HeaderValue::from_str(&value).map_err(|_| bad())?;
        out.push((name, value));
    }
    Ok(out)
}

/// Header names `auth_params.static_headers` never sets: framing and host (the HTTP client's) and credentials (the
/// auth kind's: a static `Authorization` would bypass the credential path and its redaction).
const FORBIDDEN_STATIC_HEADERS: [&str; 7] = [
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "authorization",
    "proxy-authorization",
    "cookie",
];

/// Header names an auth kind's own credential header (`auth_params.header`, `csrf_header`) never is: framing and
/// host, and the server-side `X-Demo-*` partition headers (whose only source is [`ServerVars`]).
const FORBIDDEN_CREDENTIAL_HEADERS: [&str; 5] = [
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "proxy-authorization",
];

/// The placeholder name of a value that is exactly one `{{…}}` placeholder (whitespace inside the braces allowed,
/// as [`render_with`] trims it); `None` for anything else (literal text, several placeholders, surrounding text).
fn sole_placeholder(value: &str) -> Option<&str> {
    let inner = value.strip_prefix("{{")?.strip_suffix("}}")?;
    if inner.contains("{{") || inner.contains("}}") {
        return None;
    }
    Some(inner.trim())
}

/// The reserved-header rule of `auth_params.static_headers` (names case-insensitive), shared by the save check
/// (`tenant_templates::check_auth_params`) and the render ([`static_headers`] drops a violating header). `Some`
/// reason when `name: value` may not be a static header:
/// - [`FORBIDDEN_STATIC_HEADERS`] never;
/// - `X-Demo-Tenant` only as exactly `{{tenant.id}}`, `X-Demo-Env` only as exactly `{{system.env}}`, any other
///   `X-Demo-*` only as exactly one of the two — so a template can never name another tenant's demo partition.
pub(crate) fn reserved_static_header(name: &str, value: &str) -> Option<&'static str> {
    let n = name.trim().to_ascii_lowercase();
    if FORBIDDEN_STATIC_HEADERS.contains(&n.as_str()) {
        return Some("the platform owns this header (framing, host, credentials)");
    }
    let rest = n.strip_prefix("x-demo-")?;
    let placeholder = sole_placeholder(value);
    let (ok, why) = match rest {
        "tenant" => (
            placeholder == Some("tenant.id"),
            "X-Demo-Tenant may only be exactly {{tenant.id}}",
        ),
        "env" => (
            placeholder == Some("system.env"),
            "X-Demo-Env may only be exactly {{system.env}}",
        ),
        _ => (
            matches!(placeholder, Some("tenant.id" | "system.env")),
            "an X-Demo-* header may only be exactly {{tenant.id}} or {{system.env}}",
        ),
    };
    if ok { None } else { Some(why) }
}

/// Whether `name` (case-insensitive) may not be a credential header (`auth_params.header`, `csrf_header`).
pub(crate) fn reserved_credential_header(name: &str) -> bool {
    let n = name.trim().to_ascii_lowercase();
    FORBIDDEN_CREDENTIAL_HEADERS.contains(&n.as_str()) || n.starts_with("x-demo-")
}

/// The headers of one call: `static_headers` first, then the credential header (minting or reusing an OAuth
/// token), which wins on a name clash.
pub async fn headers_for(
    t: &Transport,
    target: &CallTarget,
    spec: &AuthSpec,
    secrets: &CallSecrets,
    timeout: Duration,
) -> Result<AuthHeaders, OpError> {
    let server = ServerVars {
        tenant: target.tenant,
        env: target.env,
    };
    let fixed = static_headers(spec, secrets, server)?;
    let mut creds = credential_headers(t, target, spec, secrets, timeout).await?;
    let mut headers = fixed;
    headers.append(&mut creds.headers);
    creds.headers = headers;
    Ok(creds)
}

async fn credential_headers(
    t: &Transport,
    target: &CallTarget,
    spec: &AuthSpec,
    secrets: &CallSecrets,
    timeout: Duration,
) -> Result<AuthHeaders, OpError> {
    match spec.kind {
        AuthKind::None => Ok(AuthHeaders::default()),
        AuthKind::ApiKeyHeader => {
            let name = spec
                .params
                .get("header")
                .map(|h| h.trim())
                .filter(|h| !h.is_empty())
                .unwrap_or("Authorization");
            if reserved_credential_header(name) {
                return Err(misconfigured(format!(
                    "auth_params.header cannot be {name} (a platform-owned header)"
                )));
            }
            let template = spec
                .params
                .get("value_template")
                .map(String::as_str)
                .filter(|v| !v.trim().is_empty())
                .unwrap_or("{{secret.api_key}}");
            one(name, render(template, secrets)?, None)
        }
        AuthKind::Basic => {
            let (u, p) = basic_pair(spec, secrets)?;
            let pair = STANDARD.encode(format!("{u}:{p}"));
            one(AUTHORIZATION.as_str(), format!("Basic {pair}"), Some(pair))
        }
        AuthKind::BearerStatic => {
            let token = bearer_token(spec, secrets)
                .ok_or_else(|| auth_failed("the credential has no bearer token field"))?;
            one(
                AUTHORIZATION.as_str(),
                format!("Bearer {token}"),
                Some(token),
            )
        }
        AuthKind::Oauth2ClientCredentials => {
            let token = oauth_token(t, target, spec, secrets, timeout).await?;
            one(
                AUTHORIZATION.as_str(),
                format!("Bearer {token}"),
                Some(token),
            )
        }
        // the client certificate is presented by the TLS layer (`client_tls`), never in a header
        AuthKind::Mtls => Ok(AuthHeaders::default()),
        AuthKind::BundleToken => Err(OpError::new(
            "not_applicable",
            "bundle_token connectors are platform connectors and are never called from a tenant instance",
        )),
        // Minted per call by the executor (`ConnectorExecutor::licence_token`), never stored: a bundle token
        // lives minutes, and a stale one in `connector_credential` would read as a revoked credential.
        AuthKind::LicenceBearer => {
            let token = licence_bearer(secrets)?;
            one(
                AUTHORIZATION.as_str(),
                format!("Bearer {token}"),
                Some(token),
            )
        }
    }
}

/// The licence bearer the executor minted into this call's secret map (`licence_token`).
///
/// A template of this kind with no token is `connector_auth_failed`: a call must never go out unauthenticated, and
/// "the minter returned nothing" is exactly the state a silent empty `Bearer ` would hide.
fn licence_bearer(secrets: &CallSecrets) -> Result<String, OpError> {
    field(&secrets.secret, "licence_token")
        .ok_or_else(|| auth_failed("the licence bearer was not minted for this call"))
}

/// The TLS client identity of an `mtls` instance (`None` for every other kind): `secret.client_cert_pem` +
/// `secret.client_key_pem` (required), `ca_pem` (secret or config, optional: then the only trusted root). A missing
/// or unparsable PEM is `connector_auth_failed`; the error names the field, never its content. A template asking to
/// turn certificate verification off is `connector_misconfigured` (there is no such option).
pub fn client_tls(spec: &AuthSpec, secrets: &CallSecrets) -> Result<Option<ClientTls>, OpError> {
    if let Some(key) = TLS_VERIFY_OFF
        .iter()
        .find(|k| spec.params.contains_key(**k))
    {
        return Err(misconfigured(format!(
            "auth_params.{key}: certificate verification cannot be turned off (trust a private CA with ca_pem)"
        )));
    }
    if spec.kind != AuthKind::Mtls {
        return Ok(None);
    }
    let cert = field(&secrets.secret, "client_cert_pem")
        .ok_or_else(|| auth_failed("the credential has no secret.client_cert_pem"))?;
    let key = field(&secrets.secret, "client_key_pem")
        .ok_or_else(|| auth_failed("the credential has no secret.client_key_pem"))?;
    let mut pem = cert.into_bytes();
    pem.push(b'\n');
    pem.extend_from_slice(key.as_bytes());
    let identity = reqwest::Identity::from_pem(&pem).map_err(|_| {
        auth_failed(
            "secret.client_cert_pem / secret.client_key_pem is not a PEM certificate and private key",
        )
    })?;
    let roots = field(&secrets.secret, "ca_pem")
        .or_else(|| field(&secrets.config, "ca_pem"))
        .map(|ca| {
            reqwest::Certificate::from_pem_bundle(ca.as_bytes())
                .ok()
                .filter(|c| !c.is_empty())
                .ok_or_else(|| auth_failed("ca_pem is not a PEM certificate"))
        })
        .transpose()?;
    Ok(Some(ClientTls { identity, roots }))
}

/// The token-cache key: a credential write bumps `credential_version`, so a derived token never outlives it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TokenKey {
    pub tenant: Uuid,
    pub instance_id: String,
    pub credential_version: i32,
}

impl TokenKey {
    pub fn of(target: &CallTarget) -> Self {
        Self {
            tenant: target.tenant,
            instance_id: target.instance_id.clone(),
            credential_version: target.credential_version,
        }
    }
}

/// In-memory OAuth access tokens with their expiry (`expires_in` minus 60 s).
#[derive(Default)]
pub struct TokenCache {
    tokens: Mutex<HashMap<TokenKey, (String, Instant)>>,
}

impl std::fmt::Debug for TokenCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenCache")
            .field("entries", &self.lock().len())
            .finish()
    }
}

impl TokenCache {
    fn lock(&self) -> MutexGuard<'_, HashMap<TokenKey, (String, Instant)>> {
        self.tokens.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The live token for `key` (expired entries are dropped).
    pub fn get(&self, key: &TokenKey) -> Option<String> {
        let now = Instant::now();
        let mut m = self.lock();
        m.retain(|_, (_, expires)| *expires > now);
        m.get(key).map(|(token, _)| token.clone())
    }

    /// Caches `token` for `ttl`; a zero `ttl` caches nothing.
    pub fn put(&self, key: TokenKey, token: String, ttl: Duration) {
        if ttl.is_zero() {
            return;
        }
        self.lock().insert(key, (token, Instant::now() + ttl));
    }

    pub fn invalidate(&self, key: &TokenKey) {
        self.lock().remove(key);
    }
}

async fn oauth_token(
    t: &Transport,
    target: &CallTarget,
    spec: &AuthSpec,
    secrets: &CallSecrets,
    timeout: Duration,
) -> Result<String, OpError> {
    let key = TokenKey::of(target);
    if let Some(token) = t.tokens().get(&key) {
        return Ok(token);
    }
    let (token, ttl) = mint(t, target.tenant, spec, secrets, timeout).await?;
    t.tokens().put(key, token.clone(), ttl);
    Ok(token)
}

fn template_param<'a>(spec: &'a AuthSpec, name: &str) -> Option<&'a str> {
    spec.params
        .get(name)
        .map(String::as_str)
        .filter(|s| !s.trim().is_empty())
}

/// One client-credentials grant: form POST to the (netguarded, pinned) token URL. Client authentication is
/// `client_secret_post` unless `auth_params.client_auth` is `basic`.
async fn mint(
    t: &Transport,
    tenant: Uuid,
    spec: &AuthSpec,
    secrets: &CallSecrets,
    timeout: Duration,
) -> Result<(String, Duration), OpError> {
    let token_url = match template_param(spec, "token_url_template") {
        Some(tpl) => render(tpl, secrets)?,
        None => field(&secrets.config, "token_url").ok_or_else(|| {
            misconfigured(
                "oauth2_client_credentials needs config.token_url or auth_params.token_url_template",
            )
        })?,
    };
    let url = netguard::check_url(&token_url, t.policy(), tenant)?;
    let client_id = field(&secrets.config, "client_id")
        .ok_or_else(|| misconfigured("oauth2_client_credentials needs config.client_id"))?;
    let client_secret = field(&secrets.secret, "client_secret")
        .ok_or_else(|| auth_failed("the credential has no secret.client_secret"))?;
    let scope = match template_param(spec, "scope_template") {
        Some(tpl) => Some(render(tpl, secrets)?),
        None => field(&secrets.config, "scope"),
    };
    let basic = template_param(spec, "client_auth") == Some("basic");
    let mut form: Vec<(&str, &str)> = vec![("grant_type", "client_credentials")];
    if !basic {
        form.push(("client_id", client_id.as_str()));
        form.push(("client_secret", client_secret.as_str()));
    }
    if let Some(s) = scope.as_deref().filter(|s| !s.is_empty()) {
        form.push(("scope", s));
    }
    let client = netguard::pinned_client(&url, t.policy(), tenant, timeout).await?;
    let mut req = client
        .post(url)
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(ACCEPT, "application/json")
        .body(form_encode(&form));
    if basic {
        req = req.basic_auth(client_id.as_str(), Some(client_secret.as_str()));
    }
    let mut resp = req
        .send()
        .await
        .map_err(|e| transport_error(&e, "the token endpoint"))?;
    let status = resp.status();
    let bytes = read_capped(&mut resp, "the token endpoint").await?;
    if !status.is_success() {
        let code = if matches!(status.as_u16(), 400 | 401 | 403) {
            "connector_auth_failed"
        } else {
            "upstream_unavailable"
        };
        return Err(OpError::new(
            code,
            format!("the token endpoint answered HTTP {}", status.as_u16()),
        ));
    }
    let body = body_value(&bytes);
    let token = body
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| auth_failed("the token endpoint answered without an access_token"))?
        .to_string();
    let expires_in = match body.get("expires_in") {
        Some(Value::Number(n)) => n.as_u64(),
        Some(Value::String(s)) => s.trim().parse::<u64>().ok(),
        _ => None,
    }
    .unwrap_or(300);
    Ok((token, Duration::from_secs(expires_in.saturating_sub(60))))
}

/// `application/x-www-form-urlencoded` (the `form` feature of reqwest is not enabled).
fn form_encode(pairs: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        encode_into(&mut out, k);
        out.push('=');
        encode_into(&mut out, v);
    }
    out
}

fn encode_into(out: &mut String, s: &str) {
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'*' => {
                out.push(char::from(b));
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_licence_bearer_comes_from_the_secret_map_and_never_renders_empty() {
        let with = CallSecrets {
            config: Default::default(),
            secret: json!({"licence_token": "t-123"})
                .as_object()
                .cloned()
                .unwrap(),
        };
        assert_eq!(licence_bearer(&with).unwrap(), "t-123");
        let without = CallSecrets {
            config: Default::default(),
            secret: Default::default(),
        };
        assert_eq!(
            licence_bearer(&without).unwrap_err().code,
            "connector_auth_failed"
        );
    }

    #[test]
    fn bearer_token_field_resolution() {
        let spec = |p: &[(&str, &str)]| AuthSpec {
            kind: AuthKind::BearerStatic,
            params: p
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        };
        let sec = |v: serde_json::Value| CallSecrets {
            config: Default::default(),
            secret: v.as_object().cloned().unwrap_or_default(),
        };
        let only = sec(json!({"service_token": "x"}));
        assert_eq!(bearer_token(&spec(&[]), &only).as_deref(), Some("x"));
        let two = sec(json!({"a": "x", "b": "y"}));
        assert_eq!(bearer_token(&spec(&[]), &two), None);
        assert_eq!(
            bearer_token(&spec(&[("token_field", "b")]), &two).as_deref(),
            Some("y")
        );
        let named = sec(json!({"token": "t", "other": "o"}));
        assert_eq!(bearer_token(&spec(&[]), &named).as_deref(), Some("t"));
    }

    const SERVER: ServerVars<'static> = ServerVars {
        tenant: Uuid::from_u128(0x7e7a_0000_0000_4000_8000_0000_0000_0001),
        env: "test",
    };

    fn secrets() -> CallSecrets {
        CallSecrets {
            config: json!({"shop": "acme", "n": 3}).as_object().unwrap().clone(),
            secret: json!({"api_key": "k-123"}).as_object().unwrap().clone(),
        }
    }

    #[test]
    fn render_substitutes_config_and_secret() {
        let s = secrets();
        assert_eq!(
            render(
                "Key {{secret.api_key}} for {{ config.shop }}/{{config.n}}",
                &s
            )
            .unwrap(),
            "Key k-123 for acme/3"
        );
        assert_eq!(
            render("{{secret.missing}}", &s).unwrap_err().code,
            "connector_auth_failed"
        );
        assert_eq!(
            render("{{config.missing}}", &s).unwrap_err().code,
            "connector_misconfigured"
        );
        assert_eq!(
            render("{{canonical.x}}", &s).unwrap_err().code,
            "connector_misconfigured"
        );
        assert_eq!(
            render("{{secret.api_key", &s).unwrap_err().code,
            "connector_misconfigured"
        );
    }

    #[test]
    fn static_headers_render_config_only() {
        let s = secrets();
        let mut spec = AuthSpec::new(AuthKind::None);
        assert!(static_headers(&spec, &s, SERVER).unwrap().is_empty());
        spec.params.insert(
            "static_headers".into(),
            r#"{"Prefer":"return=representation","X-Shop":"{{config.shop}}"}"#.into(),
        );
        let h = static_headers(&spec, &s, SERVER).unwrap();
        let got: Vec<(String, String)> = h
            .iter()
            .map(|(n, v)| (n.as_str().to_string(), v.to_str().unwrap().to_string()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("prefer".to_string(), "return=representation".to_string()),
                ("x-shop".to_string(), "acme".to_string()),
            ]
        );
        spec.params.insert(
            "static_headers".into(),
            r#"{"X-Key":"{{secret.api_key}}"}"#.into(),
        );
        let e = static_headers(&spec, &s, SERVER).unwrap_err();
        assert_eq!(e.code, "connector_misconfigured");
        assert!(!e.detail.contains("k-123"));
        spec.params.insert("static_headers".into(), "[1]".into());
        assert_eq!(
            static_headers(&spec, &s, SERVER).unwrap_err().code,
            "connector_misconfigured"
        );
    }

    #[test]
    fn static_headers_render_the_calling_tenant_and_env() {
        let s = secrets();
        let mut spec = AuthSpec::new(AuthKind::BearerStatic);
        spec.params.insert(
            "static_headers".into(),
            r#"{"X-Demo-Tenant":"{{tenant.id}}","X-Demo-Env":"{{ system.env }}"}"#.into(),
        );
        let got = |h: Vec<(HeaderName, HeaderValue)>| {
            let mut v: Vec<(String, String)> = h
                .iter()
                .map(|(n, v)| (n.as_str().to_string(), v.to_str().unwrap().to_string()))
                .collect();
            v.sort();
            v
        };
        assert_eq!(
            got(static_headers(&spec, &s, SERVER).unwrap()),
            vec![
                ("x-demo-env".to_string(), "test".to_string()),
                ("x-demo-tenant".to_string(), SERVER.tenant.to_string()),
            ]
        );
        let prod = ServerVars {
            env: "prod",
            ..SERVER
        };
        assert!(
            got(static_headers(&spec, &s, prod).unwrap())
                .contains(&("x-demo-env".to_string(), "prod".to_string()))
        );
        // Instance config never stands in for a server-side value.
        let mut forged = secrets();
        forged.config.insert("tenant".into(), json!("someone-else"));
        forged.config.insert("id".into(), json!("someone-else"));
        assert!(
            got(static_headers(&spec, &forged, SERVER).unwrap())
                .contains(&("x-demo-tenant".to_string(), SERVER.tenant.to_string()))
        );
        for unknown in ["{{tenant.name}}", "{{system.url}}"] {
            spec.params.insert(
                "static_headers".into(),
                format!(r#"{{"X-Partition":"{unknown}"}}"#),
            );
            assert_eq!(
                static_headers(&spec, &s, SERVER).unwrap_err().code,
                "connector_misconfigured",
                "{unknown}"
            );
        }
        // Outside static headers (credential templates) they are not placeholders.
        for t in ["{{tenant.id}}", "{{system.env}}"] {
            assert_eq!(render(t, &s).unwrap_err().code, "connector_misconfigured");
        }
    }

    #[test]
    fn static_headers_drop_a_stored_reserved_header() {
        let s = secrets();
        let mut spec = AuthSpec::new(AuthKind::BearerStatic);
        // A template stored before the save check: another tenant's partition, a static credential, framing.
        spec.params.insert(
            "static_headers".into(),
            r#"{"X-Demo-Tenant":"6f1c2d3e-0000-4000-8000-00000000beef","authorization":"Bearer x","Host":"evil.example","X-Demo-Actor":"{{config.shop}}","x-demo-env":"{{system.env}}","Prefer":"return=minimal"}"#
                .into(),
        );
        let mut got: Vec<(String, String)> = static_headers(&spec, &s, SERVER)
            .unwrap()
            .iter()
            .map(|(n, v)| (n.as_str().to_string(), v.to_str().unwrap().to_string()))
            .collect();
        got.sort();
        assert_eq!(
            got,
            vec![
                ("prefer".to_string(), "return=minimal".to_string()),
                ("x-demo-env".to_string(), "test".to_string()),
            ]
        );
        // The rule itself: names case-insensitive, X-Demo-* values exactly one server-side placeholder.
        assert!(reserved_static_header("X-DEMO-TENANT", "{{tenant.id}}").is_none());
        assert!(reserved_static_header("x-demo-tenant", "{{ tenant.id }}").is_none());
        assert!(reserved_static_header("X-Demo-Tenant", "{{system.env}}").is_some());
        assert!(reserved_static_header("X-Demo-Tenant", "{{tenant.id}}x").is_some());
        assert!(reserved_static_header("X-Demo-Tenant", "{{config.tenant}}").is_some());
        assert!(reserved_static_header("X-Demo-Env", "test").is_some());
        assert!(reserved_static_header("X-Demo-Other", "{{system.env}}").is_none());
        assert!(reserved_static_header("Cookie", "a=b").is_some());
        assert!(reserved_static_header("Proxy-Authorization", "x").is_some());
        assert!(reserved_static_header("X-Odoo-Database", "{{config.database}}").is_none());
        assert!(reserved_credential_header("X-Demo-Tenant"));
        assert!(!reserved_credential_header("Authorization"));
    }

    #[test]
    fn basic_templates_and_defaults() {
        let s = CallSecrets {
            config: json!({"email": "a@b.example", "username": "u0"})
                .as_object()
                .unwrap()
                .clone(),
            secret: json!({"api_token": "t1", "password": "p0"})
                .as_object()
                .unwrap()
                .clone(),
        };
        let mut spec = AuthSpec::new(AuthKind::Basic);
        assert_eq!(basic_pair(&spec, &s).unwrap(), ("u0".into(), "p0".into()));
        spec.params
            .insert("username_template".into(), "{{config.email}}".into());
        assert_eq!(
            basic_pair(&spec, &s).unwrap(),
            ("a@b.example".into(), "p0".into())
        );
        spec.params
            .insert("password_template".into(), "{{secret.api_token}}".into());
        assert_eq!(
            basic_pair(&spec, &s).unwrap(),
            ("a@b.example".into(), "t1".into())
        );
    }

    #[test]
    fn form_encoding() {
        assert_eq!(
            form_encode(&[("grant_type", "client_credentials"), ("scope", "a b/c&d")]),
            "grant_type=client_credentials&scope=a+b%2Fc%26d"
        );
    }

    #[test]
    fn token_cache_expires_and_invalidates() {
        let c = TokenCache::default();
        let k = TokenKey {
            tenant: Uuid::from_u128(1),
            instance_id: "shop".into(),
            credential_version: 1,
        };
        c.put(k.clone(), "t".into(), Duration::ZERO);
        assert_eq!(c.get(&k), None);
        c.put(k.clone(), "t".into(), Duration::from_secs(60));
        assert_eq!(c.get(&k).as_deref(), Some("t"));
        c.invalidate(&k);
        assert_eq!(c.get(&k), None);
    }
}
