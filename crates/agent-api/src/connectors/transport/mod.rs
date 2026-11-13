//! Connector transports (B7 T6): one entry point, [`execute`], performs one operation call against a tenant's
//! connector instance — `rest` and `graphql` over a netguard-pinned client (no redirects, no proxy, body cap
//! 1 MiB; an `mtls` instance presents its client certificate), `stub` by replaying the template op's recorded
//! fixtures. A write (non-R op) of a template with `auth_params.csrf_fetch_path` fetches a CSRF token first.
//!
//! Contract: any HTTP status comes back as a [`RawResponse`] (the mapping engine owns status/fault mapping);
//! an `Err` is a refused target (`invalid_url` / `invalid_target`), a transport failure (`upstream_unavailable`),
//! an auth set-up failure (`connector_auth_failed` / `connector_misconfigured` / `not_applicable`) or a GraphQL
//! error answer. No `OpError::detail` ever carries a secret, an auth header or an upstream body; response bodies
//! are scrubbed of every secret value and auth header value before they are returned.
pub mod auth;
pub mod graphql;
pub mod mcp;
pub mod rest;
pub mod stub;

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::connectors::model::SideEffect;
use crate::connectors::wire::{CallSecrets, OpError, RawResponse, RenderedRequest};
use crate::netguard::{self, MAX_BODY_BYTES, Policy, SAMPLE_CHARS};

pub use auth::{AuthSpec, TokenCache};

/// `Transport` of `web/src/lib/api/types/connectors.ts` (the ones this module performs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportKind {
    Rest,
    Graphql,
    Stub,
    /// `tools/call` on the MCP server ([`mcp`]); never reaches [`execute`].
    Mcp,
}

/// The instance an operation is called on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallTarget {
    pub tenant: Uuid,
    pub instance_id: String,
    /// The env of this call (`test` | `prod`), the `{{system.env}}` of `auth_params.static_headers`.
    pub env: &'static str,
    /// Bumped on every credential write: a derived (OAuth) token never outlives the credential it came from.
    pub credential_version: i32,
    pub base_url: String,
    /// A stub instance (`stub://<key>`): always replays fixtures, never touches the network.
    pub stub: bool,
}

/// Per-operation call options.
#[derive(Debug, Clone, Copy)]
pub struct CallOptions<'a> {
    pub kind: TransportKind,
    /// The op's `timeout_ms`: per HTTP request (DNS, connect, response and body).
    pub timeout: Duration,
    /// The template op's `fixtures` (`{name, status, outcome, body}`), replayed by the stub transport.
    pub stub_fixtures: &'a [Value],
    /// The op's side effect: a non-R op is a write (gets the template's CSRF prefetch, when it has one).
    pub side_effect: SideEffect,
}

/// The outbound policy plus the in-memory OAuth token cache. Cheap to clone (one `Arc`).
#[derive(Debug, Clone)]
pub struct Transport {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    policy: Policy,
    tokens: TokenCache,
    /// The MCP client (`MCP_URL`); `None` = MCP-transport templates are not callable.
    mcp: Option<mcp::McpTransport>,
    /// Sessions to external MCP servers (`mcp` transport, any auth kind but `none`); needs no env.
    external: crate::mcp_client::external::ExternalMcp,
}

impl Transport {
    pub fn new(policy: Policy) -> Self {
        Self {
            inner: Arc::new(Inner {
                policy,
                tokens: TokenCache::default(),
                mcp: None,
                external: Default::default(),
            }),
        }
    }

    /// This transport with the MCP client.
    pub fn with_mcp(policy: Policy, mcp: mcp::McpTransport) -> Self {
        Self {
            inner: Arc::new(Inner {
                policy,
                tokens: TokenCache::default(),
                mcp: Some(mcp),
                external: Default::default(),
            }),
        }
    }

    /// The MCP client, when `MCP_URL` is set.
    pub fn mcp(&self) -> Option<&mcp::McpTransport> {
        self.inner.mcp.as_ref()
    }

    /// The sessions to external MCP servers.
    pub fn external_mcp(&self) -> &crate::mcp_client::external::ExternalMcp {
        &self.inner.external
    }

    pub fn policy(&self) -> &Policy {
        &self.inner.policy
    }

    pub fn tokens(&self) -> &TokenCache {
        &self.inner.tokens
    }
}

/// Performs one operation call. Stub instances and the `stub` kind replay fixtures; `rest` / `graphql` go
/// through the netguard. Every `Err` detail is scrubbed of the call's secret values.
pub async fn execute(
    transport: &Transport,
    target: &CallTarget,
    auth: &AuthSpec,
    secrets: &CallSecrets,
    req: RenderedRequest,
    opts: CallOptions<'_>,
) -> Result<RawResponse, OpError> {
    if target.stub || opts.kind == TransportKind::Stub {
        return stub::replay(&req, opts.stub_fixtures);
    }
    if opts.kind == TransportKind::Mcp {
        return Err(misconfigured(
            "an MCP operation is called through the MCP client",
        ));
    }
    let write = opts.side_effect != SideEffect::R;
    let out = if opts.kind == TransportKind::Graphql {
        graphql::call(transport, target, auth, secrets, &req, opts.timeout, write).await
    } else {
        rest::call(transport, target, auth, secrets, &req, opts.timeout, write).await
    };
    out.map_err(|e| scrub_error(e, &auth::secret_strings(secrets)))
}

/// A template/instance set-up fault (bad header, missing config field, no matching fixture).
pub fn misconfigured(detail: impl Into<String>) -> OpError {
    OpError::new("connector_misconfigured", detail)
}

/// Replaces every listed secret in an error's detail.
pub fn scrub_error(mut e: OpError, secrets: &[String]) -> OpError {
    for s in secrets {
        e.detail = netguard::redact_text(&e.detail, s);
    }
    e
}

/// Secrets shorter than this are not redacted from response bodies (a 1–3 character "secret" would shred
/// ordinary data); they are still scrubbed from error texts.
const MIN_REDACT_LEN: usize = 4;

/// Scrubs every listed secret from a response body.
pub fn redact_body(mut body: Value, secrets: &[String]) -> Value {
    for s in secrets.iter().filter(|s| s.len() >= MIN_REDACT_LEN) {
        body = netguard::redact(body, s);
    }
    body
}

/// A transport-level failure of a request (no HTTP status): never the reqwest text (it carries the URL).
pub fn transport_error(e: &reqwest::Error, what: &str) -> OpError {
    let detail = if e.is_timeout() {
        format!("{what} timed out")
    } else if e.is_connect() {
        format!("{what} could not be reached")
    } else {
        format!("{what} failed before a response arrived")
    };
    OpError::new("upstream_unavailable", detail)
}

/// Reads the body, refusing it once it passes [`MAX_BODY_BYTES`] (stops reading at the cap).
pub async fn read_capped(resp: &mut reqwest::Response, what: &str) -> Result<Vec<u8>, OpError> {
    let too_big = || {
        OpError::new(
            "upstream_unavailable",
            format!("{what} answered with a body over 1 MiB, which is refused"),
        )
    };
    if resp
        .content_length()
        .is_some_and(|n| n > MAX_BODY_BYTES as u64)
    {
        return Err(too_big());
    }
    let mut body: Vec<u8> = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(c)) => {
                if body.len() + c.len() > MAX_BODY_BYTES {
                    return Err(too_big());
                }
                body.extend_from_slice(&c);
            }
            Ok(None) => return Ok(body),
            Err(e) => return Err(transport_error(&e, what)),
        }
    }
}

/// The JSON body, `null` for an empty body, else the text as a JSON string of at most [`SAMPLE_CHARS`] characters.
pub fn body_value(bytes: &[u8]) -> Value {
    if bytes.iter().all(|b| b.is_ascii_whitespace()) {
        return Value::Null;
    }
    match serde_json::from_slice::<Value>(bytes) {
        Ok(v) => v,
        Err(_) => Value::String(
            String::from_utf8_lossy(bytes)
                .chars()
                .take(SAMPLE_CHARS)
                .collect(),
        ),
    }
}
