//! Where agent-api may send a request on a tenant's behalf (B7 T6, ported from care-bus-agents
//! `crates/carebus-api/src/netguard.rs`). Only public internet hosts over https: never loopback, private,
//! link-local, CGNAT (the tailnet), cluster or LAN names. The one exception is the operator-set private-target
//! allow-list (`CONNECTOR_PRIVATE_TARGETS`): an exact host + port + scheme match for a listed tenant.
//!
//! Two layers: [`check_target`] is the UI's shape rule set (`web/src/lib/connectors/ops.ts` `checkTarget`, same codes
//! and messages); [`check_url`] adds the server-only rules (every non-public literal address, dotless and LAN names)
//! and the allow-list; [`pinned_client`] resolves once, checks every answer and pins the connection to it.
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::connectors::wire::OpError;

/// Largest response body a connector call reads; a bigger body is refused (streamed, stopped at the cap).
pub const MAX_BODY_BYTES: usize = 1024 * 1024;
/// A non-JSON body is kept as a JSON string of at most this many characters.
pub const SAMPLE_CHARS: usize = 2000;
const REDACTED: &str = "[redacted]";

/// One operator-set private target (`CONNECTOR_PRIVATE_TARGETS`): admitted only on an exact host + port + scheme
/// match for an instance of one of `tenants`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivateTarget {
    pub host: String,
    pub port: u16,
    pub scheme: String,
    pub tenants: Vec<Uuid>,
}

/// The outbound policy. `allow_loopback` is for tests only (127/8 and ::1, over http or https); it opens nothing else.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Policy {
    pub allow_loopback: bool,
    pub private_targets: Vec<PrivateTarget>,
}

impl Policy {
    /// Parses `CONNECTOR_PRIVATE_TARGETS` (a JSON array of [`PrivateTarget`]); empty means no private targets.
    /// The error text names the problem, never echoes the value.
    pub fn from_env_json(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Ok(Self::default());
        }
        let mut targets: Vec<PrivateTarget> = serde_json::from_str(raw).map_err(|e| {
            format!(
                "CONNECTOR_PRIVATE_TARGETS must be a JSON array of {{host, port, scheme, tenants}} (line {}, column {})",
                e.line(),
                e.column()
            )
        })?;
        for (i, t) in targets.iter_mut().enumerate() {
            t.host = t.host.trim().to_ascii_lowercase();
            t.scheme = t.scheme.trim().to_ascii_lowercase();
            if t.host.is_empty() {
                return Err(format!("CONNECTOR_PRIVATE_TARGETS[{i}]: host is empty"));
            }
            if !matches!(t.scheme.as_str(), "http" | "https") {
                return Err(format!(
                    "CONNECTOR_PRIVATE_TARGETS[{i}]: scheme must be http or https"
                ));
            }
            if t.port == 0 {
                return Err(format!(
                    "CONNECTOR_PRIVATE_TARGETS[{i}]: port must not be 0"
                ));
            }
            if t.tenants.is_empty() {
                return Err(format!(
                    "CONNECTOR_PRIVATE_TARGETS[{i}]: tenants must list at least one tenant"
                ));
            }
        }
        Ok(Self {
            allow_loopback: false,
            private_targets: targets,
        })
    }

    /// The allow-list entry admitting `url` for `tenant`: exact scheme, host and (effective) port, tenant listed.
    /// Callers audit `connector.private_target` when this is `Some`.
    pub fn private_target(&self, url: &Url, tenant: Uuid) -> Option<&PrivateTarget> {
        let host = url.host_str()?;
        let port = url.port_or_known_default()?;
        self.private_targets.iter().find(|t| {
            t.scheme == url.scheme()
                && t.host.eq_ignore_ascii_case(host)
                && t.port == port
                && t.tenants.contains(&tenant)
        })
    }
}

fn invalid_url(message: &str) -> OpError {
    OpError::new("invalid_url", message)
}

fn non_public(host: &str) -> OpError {
    OpError::new(
        "invalid_target",
        format!(
            "{host} resolves to a non-public address (private, loopback, link-local or CGNAT). The runner only calls public hosts."
        ),
    )
}

/// The UI's shape check + SSRF guard (`checkTarget` in `web/src/lib/connectors/ops.ts`): the
/// refusal (`invalid_url` / `invalid_target` with the UI's
/// message), or `None` when acceptable. Two TS bugs are NOT copied: IPv6 literals are classified as addresses
/// (so `[::ffff:127.0.0.1]` and every other non-global IPv6 is refused), and ordinary hostnames that merely
/// start with `fc` / `fd` / `fe80` pass.
pub fn check_target(raw: &str) -> Option<OpError> {
    let Ok(u) = Url::parse(raw.trim()) else {
        return Some(invalid_url(
            "Enter a full URL such as https://shop.example.com.",
        ));
    };
    if u.scheme() != "https" {
        return Some(invalid_url(
            "Only https:// base URLs are allowed for connector instances.",
        ));
    }
    if let Some(e) = userinfo_query_fragment(&u) {
        return Some(e);
    }
    let host = bare_host(&u);
    let internal = host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".internal")
        || host.ends_with(".local")
        || host.ends_with(".svc")
        || host.ends_with(".cluster.local")
        || host
            .parse::<Ipv6Addr>()
            .is_ok_and(|v6| !is_public(IpAddr::V6(v6)));
    let private_v4 = dotted_quad(&host).is_some_and(|[a, b]| {
        a == 10
            || a == 127
            || a == 0
            || (a == 172 && (16..=31).contains(&b))
            || (a == 192 && b == 168)
            || (a == 169 && b == 254)
            || (a == 100 && (64..=127).contains(&b))
    });
    if internal || private_v4 {
        return Some(non_public(&host));
    }
    None
}

/// `u.hostname` without IPv6 brackets, lower-cased (the TS `host`).
fn bare_host(u: &Url) -> String {
    u.host_str()
        .unwrap_or_default()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase()
}

/// The first two octets of a `a.b.c.d` decimal host (the TS regex `^(\d+)\.(\d+)\.(\d+)\.(\d+)$`).
fn dotted_quad(host: &str) -> Option<[u64; 2]> {
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() != 4
        || parts
            .iter()
            .any(|p| p.is_empty() || !p.bytes().all(|c| c.is_ascii_digit()))
    {
        return None;
    }
    // a run of digits too long for u64 is still "digits" in the TS regex; saturate instead of failing
    let n = |s: &str| s.parse::<u64>().unwrap_or(u64::MAX);
    Some([n(parts[0]), n(parts[1])])
}

/// The UI's credential / query / fragment rules (empty `?` and `#` pass, as `u.search` / `u.hash` are empty).
fn userinfo_query_fragment(u: &Url) -> Option<OpError> {
    if !u.username().is_empty() || u.password().is_some() {
        return Some(invalid_url(
            "Put credentials in the credential fields, never in the URL.",
        ));
    }
    if u.query().is_some_and(|q| !q.is_empty()) || u.fragment().is_some_and(|f| !f.is_empty()) {
        return Some(invalid_url("A base URL has no query or fragment."));
    }
    None
}

/// Hostname suffixes that name hosts inside the cluster, the tailnet or a LAN (server-side, stricter than the UI).
const INTERNAL_SUFFIXES: [&str; 7] = [
    ".internal",
    ".svc",
    ".cluster.local",
    ".local",
    ".localhost",
    ".lan",
    ".home.arpa",
];

/// Dotless names (`localhost`, `minio`) and names under an internal suffix. Case-insensitive, trailing dots ignored.
pub fn internal_hostname(host: &str) -> bool {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    !h.contains('.')
        || INTERNAL_SUFFIXES
            .iter()
            .any(|s| h.ends_with(s) || h == s[1..])
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback() // 127/8
        || ip.is_private() // 10/8, 172.16/12, 192.168/16
        || ip.is_link_local() // 169.254/16 (cloud metadata)
        || ip.is_broadcast()
        || ip.is_multicast() // 224/4
        || ip.is_documentation() // 192.0.2/24, 198.51.100/24, 203.0.113/24
        || a == 0 // 0/8 "this network"
        || (a == 100 && (64..=127).contains(&b)) // 100.64/10 CGNAT (the tailnet)
        || (a == 192 && b == 0 && c == 0) // 192.0.0/24 IETF protocol assignments
        || (a == 198 && (b == 18 || b == 19)) // 198.18/15 benchmarking
        || a >= 240) // 240/4 reserved
}

fn public_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return public_v4(v4); // ::ffff:a.b.c.d
    }
    let s = ip.segments();
    if s[0] == 0x0064 && s[1] == 0xff9b && s[2..6] == [0, 0, 0, 0] {
        // 64:ff9b::/96 NAT64 embeds an IPv4 address
        return public_v4(Ipv4Addr::new(
            (s[6] >> 8) as u8,
            s[6] as u8,
            (s[7] >> 8) as u8,
            s[7] as u8,
        ));
    }
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        || (s[0] & 0xfe00) == 0xfc00 // fc00::/7 unique local
        || (s[0] & 0xffc0) == 0xfe80 // fe80::/10 link local
        || (s[0] == 0x2001 && s[1] == 0x0db8) // 2001:db8::/32 documentation
        || s[0] == 0x2002 // 2002::/16 6to4 embeds an IPv4 address that is not checked
        || (s[0] == 0x2001 && s[1] == 0) // 2001::/32 Teredo embeds an IPv4 address that is not checked
        || s[0..6] == [0, 0, 0, 0, 0, 0]) // ::/96 IPv4-compatible (deprecated), incl. ::a.b.c.d
}

/// True only for addresses on the public internet.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => public_v4(v4),
        IpAddr::V6(v6) => public_v6(v6),
    }
}

/// 127/8 and ::1 only: the test-only loopback allowance never opens any other range.
pub fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|m| m.is_loopback()),
    }
}

fn allowed(ip: IpAddr, allow_loopback: bool) -> bool {
    is_public(ip) || (allow_loopback && is_loopback(ip))
}

fn literal_ip(u: &Url) -> Option<IpAddr> {
    bare_host(u).parse::<IpAddr>().ok()
}

/// The server's base-URL / token-URL check for `tenant`'s instance: an allow-listed private target (exact match)
/// passes with the shape rules only; the test-only loopback allowance admits a loopback literal over http(s);
/// everything else must pass [`check_target`] AND the stricter server rules (every non-public literal address,
/// dotless and LAN names).
pub fn check_url(raw: &str, policy: &Policy, tenant: Uuid) -> Result<Url, OpError> {
    let url = Url::parse(raw.trim())
        .map_err(|_| invalid_url("Enter a full URL such as https://shop.example.com."))?;
    let loopback_literal = policy.allow_loopback
        && matches!(url.scheme(), "http" | "https")
        && literal_ip(&url).is_some_and(is_loopback);
    if loopback_literal || policy.private_target(&url, tenant).is_some() {
        return match userinfo_query_fragment(&url) {
            Some(e) => Err(e),
            None => Ok(url),
        };
    }
    if let Some(e) = check_target(raw) {
        return Err(e);
    }
    let host = bare_host(&url);
    match literal_ip(&url) {
        Some(ip) if !is_public(ip) => return Err(non_public(&host)),
        Some(_) => {}
        None if internal_hostname(&host) => return Err(non_public(&host)),
        None => {}
    }
    Ok(url)
}

/// After DNS: every answer must be allowed (one private answer refuses the host). Returns the address to pin.
pub fn check_addrs(
    addrs: &[SocketAddr],
    allow_loopback: bool,
    host: &str,
) -> Result<SocketAddr, OpError> {
    let first = *addrs.first().ok_or_else(unresolved)?;
    if addrs.iter().any(|a| !allowed(a.ip(), allow_loopback)) {
        return Err(non_public(host));
    }
    Ok(first)
}

fn unresolved() -> OpError {
    OpError::new("upstream_unavailable", "the host name does not resolve")
}

/// The TLS client identity of an `mtls` connector, already parsed (so a bad PEM is refused before any connect).
/// Certificate verification stays on: `roots`, when given, REPLACE the built-in roots (only that CA is trusted).
/// Deliberately not `Debug` (it holds a private key).
#[derive(Clone)]
pub struct ClientTls {
    pub identity: reqwest::Identity,
    pub roots: Option<Vec<reqwest::Certificate>>,
}

/// A client for calls to `url` (which already passed [`check_url`] for `tenant`): no proxy, no redirects, a total
/// `timeout` per request, and for a host name ONE lookup (bounded by `timeout`) whose every answer passes
/// [`check_addrs`], the connection pinned to the checked address (no second lookup: DNS rebinding cannot swap
/// it). An allow-listed private target is resolved and pinned the same way, without the address check.
pub async fn pinned_client(
    url: &Url,
    policy: &Policy,
    tenant: Uuid,
    timeout: Duration,
) -> Result<reqwest::Client, OpError> {
    pinned_client_tls(url, policy, tenant, timeout, None).await
}

/// [`pinned_client`] presenting a client certificate (`mtls`). Every guard of [`pinned_client`] applies unchanged.
/// A client that cannot be built with the identity (a key that does not match its certificate, an unusable key
/// type) is `connector_auth_failed`, never the TLS library's text.
pub async fn pinned_client_tls(
    url: &Url,
    policy: &Policy,
    tenant: Uuid,
    timeout: Duration,
    tls: Option<&ClientTls>,
) -> Result<reqwest::Client, OpError> {
    let private = policy.private_target(url, tenant).is_some();
    let port = url
        .port_or_known_default()
        .ok_or_else(|| invalid_url("Enter a full URL such as https://shop.example.com."))?;
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .timeout(timeout);
    let host = bare_host(url);
    if host.is_empty() {
        return Err(invalid_url(
            "Enter a full URL such as https://shop.example.com.",
        ));
    }
    match host.parse::<IpAddr>() {
        Ok(ip) => {
            if !private && !allowed(ip, policy.allow_loopback) {
                return Err(non_public(&host));
            }
        }
        Err(_) => {
            let addrs: Vec<SocketAddr> =
                match tokio::time::timeout(timeout, tokio::net::lookup_host((host.as_str(), port)))
                    .await
                {
                    Ok(Ok(a)) => a.collect(),
                    _ => return Err(unresolved()),
                };
            let pinned = if private {
                *addrs.first().ok_or_else(unresolved)?
            } else {
                check_addrs(&addrs, policy.allow_loopback, &host)?
            };
            builder = builder.resolve(&host, pinned);
        }
    }
    if let Some(tls) = tls {
        builder = builder.identity(tls.identity.clone());
        if let Some(roots) = &tls.roots {
            builder = builder.tls_certs_only(roots.iter().cloned());
        }
        return builder.build().map_err(|_| {
            OpError::new(
                "connector_auth_failed",
                "the client certificate and key could not be used for TLS (check that the key matches the certificate)",
            )
        });
    }
    builder
        .build()
        .map_err(|_| OpError::new("upstream_unavailable", "the HTTP client could not be built"))
}

/// The first item of a JSON array, a JSON object as is, other JSON as is, else the first [`SAMPLE_CHARS`]
/// characters as text. `fields` are the sample's top-level keys, sorted.
pub fn sample_of(body: &[u8]) -> (Value, Vec<String>) {
    let sample = match serde_json::from_slice::<Value>(body) {
        Ok(Value::Array(a)) => a.into_iter().next().unwrap_or(Value::Null),
        Ok(v) => v,
        Err(_) => Value::String(
            String::from_utf8_lossy(body)
                .chars()
                .take(SAMPLE_CHARS)
                .collect(),
        ),
    };
    // sorted: the order is the same whether or not serde_json keeps insertion order
    let mut fields: Vec<String> = sample
        .as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    fields.sort();
    (sample, fields)
}

/// Replaces every occurrence of `secret` with `[redacted]`: in text, in JSON string values and in object keys.
pub fn redact(v: Value, secret: &str) -> Value {
    if secret.is_empty() {
        return v;
    }
    match v {
        Value::String(s) => Value::String(s.replace(secret, REDACTED)),
        Value::Array(a) => Value::Array(a.into_iter().map(|x| redact(x, secret)).collect()),
        Value::Object(o) => Value::Object(
            o.into_iter()
                .map(|(k, x)| (k.replace(secret, REDACTED), redact(x, secret)))
                .collect(),
        ),
        other => other,
    }
}

/// [`redact`] for plain text.
pub fn redact_text(text: &str, secret: &str) -> String {
    if secret.is_empty() {
        text.to_string()
    } else {
        text.replace(secret, REDACTED)
    }
}

/// A JSON object key whose value is a secret: case-insensitive, `_`/`-` ignored, `authorization` exactly, else
/// containing `password`, `passwd`, `secret`, `token`, `privatekey` or `apikey` (`client_secret`, `access_token`,
/// `refreshToken`, `x-api-key`, `private_key`, …).
fn secret_key(key: &str) -> bool {
    let k: String = key
        .chars()
        .filter(|c| !matches!(c, '_' | '-'))
        .flat_map(char::to_lowercase)
        .collect();
    k == "authorization"
        || [
            "password",
            "passwd",
            "secret",
            "token",
            "privatekey",
            "apikey",
        ]
        .iter()
        .any(|s| k.contains(s))
}

/// A `KEY:value` line whose KEY (`^[A-Z0-9_]+$`) names a secret (`PASSWORD`, `SECRET`, `TOKEN`, `PRIVATE_KEY`) →
/// `KEY:[redacted]`; any other line as is (a trailing `\r` is kept).
fn scrub_line(line: &str) -> String {
    let body = line.trim_start();
    let Some((key, value)) = body.split_once(':') else {
        return line.to_owned();
    };
    let shaped = !key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_');
    let named = ["PASSWORD", "SECRET", "TOKEN", "PRIVATE_KEY"]
        .iter()
        .any(|s| key.contains(s));
    if !(shaped && named) {
        return line.to_owned();
    }
    let indent = &line[..line.len() - body.len()];
    let cr = if value.ends_with('\r') { "\r" } else { "" };
    format!("{indent}{key}:{REDACTED}{cr}")
}

/// Scrubs secrets a vendor answers in a response body, before it is stored or served (contract-test responses,
/// samples): the value of every object key that names a secret ([`secret_key`]) becomes `[redacted]` (a bool or
/// null is kept: it carries no secret and may be a mapped flag such as `temporary_password_sent`), and in every
/// string value every `KEY:value` line naming a secret becomes `KEY:[redacted]` (the key stays, so mapping authors
/// still see the shape). Recursive; idempotent. Complements [`redact`], which removes the known credential strings.
pub fn scrub_secrets(v: Value) -> Value {
    match v {
        Value::String(s) if s.contains(':') => {
            Value::String(s.split('\n').map(scrub_line).collect::<Vec<_>>().join("\n"))
        }
        Value::Array(a) => Value::Array(a.into_iter().map(scrub_secrets).collect()),
        Value::Object(o) => Value::Object(
            o.into_iter()
                .map(|(k, x)| {
                    let x = match x {
                        // a flag or a null carries no secret (`temporary_password_sent: true` stays mappable)
                        x @ (Value::Bool(_) | Value::Null) => x,
                        _ if secret_key(&k) => Value::String(REDACTED.to_owned()),
                        x => scrub_secrets(x),
                    };
                    (k, x)
                })
                .collect(),
        ),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: Uuid = Uuid::from_u128(7);

    #[test]
    fn private_and_reserved_ranges_are_not_public() {
        for ip in [
            "127.0.0.1",
            "127.255.0.9",
            "10.0.0.1",
            "10.255.255.255",
            "172.16.0.1",
            "172.31.9.9",
            "192.168.1.119",
            "169.254.169.254",
            "100.64.0.1",
            "100.64.0.9",
            "100.127.255.255",
            "0.0.0.0",
            "0.1.2.3",
            "255.255.255.255",
            "224.0.0.1",
            "240.0.0.1",
            "192.0.0.8",
            "198.18.0.1",
            "192.0.2.1",
            "::1",
            "::",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:10.1.2.3",
            "::ffff:169.254.169.254",
            "64:ff9b::a9fe:a9fe",
            "2001:db8::1",
            "::127.0.0.1",
            "2002:7f00:1::1",
            "2002:0a00:0001::",
            "2002:808:808::1",
            "2001::1",
            "2001:0:4136:e378:8000:63bf:3fff:fdd2",
        ] {
            assert!(!is_public(ip.parse().unwrap()), "{ip} must be refused");
        }
    }

    #[test]
    fn public_addresses_pass() {
        for ip in [
            "1.1.1.1",
            "217.160.28.34",
            "100.63.255.255",
            "100.128.0.1",
            "172.32.0.1",
            "2a00:1450:4001::1",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
        ] {
            assert!(is_public(ip.parse().unwrap()), "{ip} is public");
        }
    }

    #[test]
    fn internal_names() {
        for h in [
            "localhost",
            "minio",
            "postgres.internal",
            "carebus-api.care-bus.svc",
            "x.svc.cluster.local",
            "mini1.local",
            "API.INTERNAL.",
            "internal",
            "router.lan",
        ] {
            assert!(internal_hostname(h), "{h}");
        }
        for h in ["api.example.com", "internal.example.com", "svc.example.org"] {
            assert!(!internal_hostname(h), "{h}");
        }
    }

    #[test]
    fn check_url_refuses_internal_and_private_targets() {
        let strict = Policy::default();
        for u in [
            "https://127.0.0.1:9/x",
            "https://10.0.0.1/x",
            "https://169.254.169.254/latest/meta-data",
            "https://100.64.0.1/",
            "https://[::1]/",
            "https://[::ffff:10.0.0.1]/",
            "https://2130706433/",
            "https://0x7f.1/",
            "https://192.0.2.1/",
            "https://api.internal/x",
            "https://carebus-api.care-bus.svc/x",
            "https://x.svc.cluster.local/",
            "https://localhost/x",
            "https://minio:9000/",
            "https://router.lan/",
            "http://api.example.com/x",
            "ftp://example.com/x",
            "https://user:pw@example.com/",
            "https://example.com/#frag",
            "https://example.com/?page=1",
            "not a url",
        ] {
            assert!(check_url(u, &strict, T).is_err(), "{u} must be refused");
        }
        assert!(check_url("https://api.example.com/v2", &strict, T).is_ok());
        let test = Policy {
            allow_loopback: true,
            private_targets: vec![],
        };
        assert!(check_url("http://127.0.0.1:9/x", &test, T).is_ok());
        assert!(check_url("http://10.0.0.1/x", &test, T).is_err());
        assert!(check_url("https://localhost/x", &test, T).is_err());
        assert!(check_url("http://127.0.0.1:9/x?a=1", &test, T).is_err());
    }

    #[test]
    fn every_resolved_address_must_be_public() {
        let a = |s: &str| s.parse::<SocketAddr>().unwrap();
        assert_eq!(
            check_addrs(&[a("93.184.216.34:443")], false, "h"),
            Ok(a("93.184.216.34:443"))
        );
        assert_eq!(
            check_addrs(&[a("93.184.216.34:443"), a("10.0.0.5:443")], false, "h")
                .unwrap_err()
                .code,
            "invalid_target",
            "one private answer refuses the host"
        );
        assert!(check_addrs(&[a("[::1]:443")], false, "h").is_err());
        assert!(check_addrs(&[], false, "h").is_err());
        assert!(check_addrs(&[a("127.0.0.1:80")], true, "h").is_ok());
        assert!(check_addrs(&[a("192.168.1.1:80")], true, "h").is_err());
    }

    #[test]
    fn sample_is_the_first_item_the_object_or_a_text_excerpt() {
        let (s, f) = sample_of(br#"[{"b":1,"a":2},{"c":3}]"#);
        assert_eq!(s, serde_json::json!({"a": 2, "b": 1}));
        assert_eq!(f, ["a", "b"]);
        assert_eq!(sample_of(b"[]").0, Value::Null);
        let long = "x".repeat(5000);
        assert_eq!(
            sample_of(long.as_bytes()).0.as_str().unwrap().len(),
            SAMPLE_CHARS
        );
    }

    #[test]
    fn redact_reaches_keys_values_and_text() {
        let v = redact(serde_json::json!({"s3cret": ["x s3cret y", 1]}), "s3cret");
        assert_eq!(v, serde_json::json!({"[redacted]": ["x [redacted] y", 1]}));
        assert_eq!(redact_text("a s3cret", "s3cret"), "a [redacted]");
    }

    #[test]
    fn scrub_secrets_blanks_secret_keys_at_any_depth() {
        let v = scrub_secrets(serde_json::json!({
            "id": "u-1",
            "Password": "Xy7-not-real",
            "client_secret": {"nested": "Xy7-not-real"},
            "accessToken": "Xy7-not-real",
            "x-api-key": "Xy7-not-real",
            "Authorization": "Bearer Xy7-not-real",
            "items": [{"private_key": "Xy7-not-real", "name": "Kettle"}, [{"refresh_token": "Xy7-not-real"}]],
            "status": "ok",
            "count": 2
        }));
        assert_eq!(
            v,
            serde_json::json!({
                "id": "u-1",
                "Password": "[redacted]",
                "client_secret": "[redacted]",
                "accessToken": "[redacted]",
                "x-api-key": "[redacted]",
                "Authorization": "[redacted]",
                "items": [{"private_key": "[redacted]", "name": "Kettle"}, [{"refresh_token": "[redacted]"}]],
                "status": "ok",
                "count": 2
            })
        );
        assert!(!v.to_string().contains("Xy7-not-real"));
    }

    #[test]
    fn scrub_secrets_blanks_secret_kv_lines_and_keeps_the_key() {
        let v = scrub_secrets(serde_json::json!({
            "stdout": "USERNAME:jdoe\nPASSWORD:Xy7-not-real\r\n  API_TOKEN:Xy7-not-real\nSTATUS:SUCCESS",
            "list": ["DB_SECRET:Xy7-not-real", "SSH_PRIVATE_KEY:Xy7-not-real"]
        }));
        assert_eq!(
            v,
            serde_json::json!({
                "stdout": "USERNAME:jdoe\nPASSWORD:[redacted]\r\n  API_TOKEN:[redacted]\nSTATUS:SUCCESS",
                "list": ["DB_SECRET:[redacted]", "SSH_PRIVATE_KEY:[redacted]"]
            })
        );
    }

    #[test]
    fn scrub_secrets_leaves_ordinary_content_alone_and_is_idempotent() {
        let plain = serde_json::json!({
            "id": "o-1",
            "title": "Reset your password: see the portal",
            "note": "Password:Xy7 lower-case key is prose, not a KEY line",
            "url": "https://example.test/a:b",
            "lines": "USERNAME:jdoe\nSTATUS:SUCCESS",
            "temporary_password_sent": true, "token": null,
            "n": 1, "ok": true, "none": null
        });
        assert_eq!(scrub_secrets(plain.clone()), plain);
        let leaky = serde_json::json!({"password": "x", "stdout": "PASSWORD:Xy7-not-real"});
        let once = scrub_secrets(leaky);
        assert_eq!(scrub_secrets(once.clone()), once);
    }
}
