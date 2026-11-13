//! One upstream HTTP call to an in-cluster service (strata_rag, strata_ingest) and its error mapping (B3, after
//! care-bus-agents@96c608c `crates/carebus-api/src/upstream.rs`; plan §1, §0 R3). strata URLs are config, so these
//! calls never go through crate::netguard; tenant-entered hosts do (crate::providers::check_url +
//! `netguard::pinned_client`).
//!
//! Mapping ([`Call::send`], [`refusal`]):
//! - 2xx: the JSON body (`204` or an empty body is `null`); an unreadable 2xx body is 503 `unavailable`.
//! - A 4xx (not 401/403) whose `error.code` is on the static allow-list ([`pass_through`]) keeps its status, code and
//!   extra fields (those of `error` and, for strata's `empty_sync_run`, the report beside it; secret-named fields
//!   scrubbed). The message is agent's own per code ([`message_for`]): strata's text names strata ids, paths and
//!   routes a console user cannot use, so it goes to the debug log only. A 404 `not_found` naming a store (`not found:
//!   store <uuid>`) carries that id as the extra field `store` (crate::strata::rag::unreadable_store reads it).
//!   A 409 `eval_not_ready` carries strata_rag's problem lines (about the tenant's own golden questions) as the
//!   extra field `problems` (≤ 200 lines of ≤ 300 characters), which the eval page lists.
//! - A 502 `upstream_unavailable` (the service's own upstream is down) passes through as 502.
//! - Everything else, connection failures, timeouts, every other 5xx, 401/403 (our credentials, which no console
//!   user can fix) and unlisted codes, is 503 `unavailable` naming the service, logged without the body.
use std::time::Duration;

use axum::http::StatusCode;
use reqwest::Method;
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::error::{ApiError, ApiResult};
use crate::tenants::app_licence::Renewal;

/// The strata services' tenant header: the bare tenant uuid this request acts for (strata_rag requires it to match
/// the tenant-pinned token).
pub const TENANT_HEADER: &str = "x-tenant-id";
/// The acting person's `sub` (else agent's service account, `SERVICE_ACTOR_ID`), sent only when it is a non-nil uuid
/// (never invented).
pub const ACTOR_HEADER: &str = "x-actor-id";
/// The calling app's licences-registry key (crate::providers::llm::APP_ID); strata_rag relays it to its own upstreams.
pub const APP_HEADER: &str = "x-app-id";

/// The default bound on one call (plan §1).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
/// Multipart uploads and `/api/query`.
pub const LONG_TIMEOUT: Duration = Duration::from_secs(30);
/// `DELETE …/ingest/jobs/{id}` (rag's 202 path waits 5 s).
pub const INGEST_DELETE_TIMEOUT: Duration = Duration::from_secs(8);

/// Longest upstream message logged.
const MESSAGE_CHARS: usize = 300;
/// Largest upstream body read; a longer one is 503 `unavailable`.
pub const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Who we are to an upstream. `Debug` never prints the token.
#[derive(Clone, Copy, Default)]
pub struct Auth<'a> {
    /// Sent as `Authorization: Bearer`.
    pub token: Option<&'a str>,
    /// Sent as `X-Tenant-Id` when set.
    pub tenant: Option<Uuid>,
    /// Sent as `X-Actor-Id` when set and not nil.
    pub actor: Option<Uuid>,
    /// Sent as `X-App-Id` when set.
    pub app: Option<&'a str>,
}

impl std::fmt::Debug for Auth<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Auth")
            .field("token", &self.token.map(|_| "[redacted]"))
            .field("tenant", &self.tenant)
            .field("actor", &self.actor)
            .field("app", &self.app)
            .finish()
    }
}

impl Auth<'_> {
    /// Bearer, tenant, actor and app headers onto `req`.
    pub fn apply(self, mut req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(t) = self.token {
            req = req.bearer_auth(t);
        }
        if let Some(t) = self.tenant {
            req = req.header(TENANT_HEADER, t.to_string());
        }
        if let Some(a) = self.actor.filter(|a| !a.is_nil()) {
            req = req.header(ACTOR_HEADER, a.to_string());
        }
        if let Some(app) = self.app {
            req = req.header(APP_HEADER, app);
        }
        req
    }
}

/// What a request carries.
pub enum Payload {
    Empty,
    Json(Value),
    Multipart(reqwest::multipart::Form),
}

/// A successful upstream answer.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: StatusCode,
    pub body: Value,
}

/// One upstream request: `Call::new(..)`, then [`Call::auth`] / [`Call::json`] / [`Call::multipart`] /
/// [`Call::timeout`], then [`Call::send`].
pub struct Call<'a> {
    name: &'static str,
    op: &'static str,
    base: &'a str,
    method: Method,
    path: &'a str,
    auth: Auth<'a>,
    payload: Payload,
    timeout: Option<Duration>,
    /// Set when `auth.token` is a tenant bundle token: a 401 re-mints it and resends once ([`Call::send_any`]).
    renew: Option<Renewal>,
}

impl<'a> Call<'a> {
    /// `method` `base` + `path` (path and query, starting with `/`) to the upstream `name`; `op` labels the metric.
    pub fn new(
        name: &'static str,
        op: &'static str,
        base: &'a str,
        method: Method,
        path: &'a str,
    ) -> Self {
        Self {
            name,
            op,
            base,
            method,
            path,
            auth: Auth::default(),
            payload: Payload::Empty,
            timeout: None,
            renew: None,
        }
    }

    pub fn auth(mut self, auth: Auth<'a>) -> Self {
        self.auth = auth;
        self
    }

    pub fn json(mut self, body: Value) -> Self {
        self.payload = Payload::Json(body);
        self
    }

    pub fn multipart(mut self, form: reqwest::multipart::Form) -> Self {
        self.payload = Payload::Multipart(form);
        self
    }

    /// The call carries a tenant bundle token: on a 401, drop it, mint a fresh one and resend ONCE (a second 401 is
    /// the answer). A multipart call cannot be resent (its form is consumed), so it never retries.
    pub fn renewal(mut self, renew: Option<Renewal>) -> Self {
        self.renew = renew;
        self
    }

    /// Overrides the bound ([`DEFAULT_TIMEOUT`]; [`LONG_TIMEOUT`] for multipart).
    pub fn timeout(mut self, t: Duration) -> Self {
        self.timeout = Some(t);
        self
    }

    /// Sends the request and maps the answer (module doc).
    pub async fn send(self, http: &reqwest::Client) -> ApiResult<Reply> {
        let name = self.name;
        let r = self.send_any(http).await?;
        if r.status.is_success() {
            return Ok(r);
        }
        Err(refusal(name, r.status, &r.body))
    }

    /// Sends the request and answers whatever status came back (the caller maps a refusal itself, e.g. a route
    /// that is not deployed yet); `Err` only when there is no readable answer (503 `unavailable`).
    pub async fn send_any(mut self, http: &reqwest::Client) -> ApiResult<Reply> {
        // the body of a possible resend, cloned before the first send consumes the payload
        let again = match (&self.renew, &self.payload) {
            (Some(_), Payload::Empty) => Some(Payload::Empty),
            (Some(_), Payload::Json(b)) => Some(Payload::Json(b.clone())),
            _ => None,
        };
        let payload = std::mem::replace(&mut self.payload, Payload::Empty);
        let first = self.exchange(http, self.auth, payload).await?;
        if first.status == StatusCode::UNAUTHORIZED
            && let (Some(renew), Some(body)) = (&self.renew, again)
            && let Some(fresh) = renew.fresh().await
        {
            tracing::warn!(upstream = self.name, op = self.op, tenant = %renew.tenant(), "upstream_tenant_token_refused");
            let auth = Auth {
                token: Some(&fresh),
                ..self.auth
            };
            return self.exchange(http, auth, body).await;
        }
        if first.status == StatusCode::UNAUTHORIZED
            && let Some(renew) = &self.renew
        {
            // a multipart upload cannot be resent: drop the refused token so the next call mints a fresh one
            renew.invalidate();
        }
        Ok(first)
    }

    /// One round trip of this call under `auth` with `payload`.
    async fn exchange(
        &self,
        http: &reqwest::Client,
        auth: Auth<'_>,
        payload: Payload,
    ) -> ApiResult<Reply> {
        let name = self.name;
        let url = format!("{}{}", self.base.trim_end_matches('/'), self.path);
        let default = match payload {
            Payload::Multipart(_) => LONG_TIMEOUT,
            _ => DEFAULT_TIMEOUT,
        };
        let req = auth
            .apply(http.request(self.method.clone(), url))
            .timeout(self.timeout.unwrap_or(default));
        let req = match payload {
            Payload::Empty => req,
            Payload::Json(b) => req.json(&b),
            Payload::Multipart(f) => req.multipart(f),
        };
        let mut resp = crate::observability::upstream_http(name, self.op.to_string(), req.send())
            .await
            .map_err(|e| {
                tracing::warn!(
                    upstream = name,
                    timeout = e.is_timeout(),
                    "upstream unreachable"
                );
                unavailable(name)
            })?;
        let status = StatusCode::from_u16(resp.status().as_u16()).map_err(|_| unavailable(name))?;
        let mut bytes: Vec<u8> = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|_| unavailable(name))? {
            if bytes.len() + chunk.len() > MAX_BODY_BYTES {
                tracing::warn!(
                    upstream = name,
                    status = status.as_u16(),
                    "body over the read cap"
                );
                return Err(unavailable(name));
            }
            bytes.extend_from_slice(&chunk);
        }
        let body = if bytes.iter().all(u8::is_ascii_whitespace) {
            Value::Null
        } else {
            match serde_json::from_slice::<Value>(&bytes) {
                Ok(v) => v,
                Err(_) if status.is_success() => {
                    tracing::warn!(upstream = name, status = status.as_u16(), "unreadable body");
                    return Err(unavailable(name));
                }
                Err(_) => Value::Null,
            }
        };
        Ok(Reply { status, body })
    }
}

/// 503 `unavailable` naming the upstream.
pub fn unavailable(name: &str) -> ApiError {
    ApiError::Unavailable(
        "unavailable",
        format!("{name} is unavailable; try again shortly"),
        None,
    )
}

/// The upstream codes passed on (plan §1 and §0 R3), each with agent's own message: `(code, message)`. Codes are
/// `&'static str`, so the list is a match. strata's provider rejections (`ProviderRejected`) are the last group.
fn listed(code: &str) -> Option<(&'static str, &'static str)> {
    Some(match code {
        "ambiguous_root" => (
            "ambiguous_root",
            "This knowledge has more than one root; choose the one to search.",
        ),
        "child_of_parent_job" => (
            "child_of_parent_job",
            "That ingest job is part of a larger one; manage the parent job instead.",
        ),
        "conflict" => (
            "conflict",
            "The knowledge service refused the change because it conflicts with its current state; reload and try \
             again.",
        ),
        "duplicate_name" => (
            "duplicate_name",
            "Something with that name already exists here; choose another name.",
        ),
        "embed_model_mismatch" => (
            "embed_model_mismatch",
            "This knowledge was embedded with a different model; re-embed it before using this model.",
        ),
        "empty_sync_run" => (
            "empty_sync_run",
            "The sync found no documents, so nothing was removed; check the source and run it again.",
        ),
        "eval_not_ready" => ("eval_not_ready", "The evaluation is not ready yet."),
        "eval_running" => (
            "eval_running",
            "An evaluation is already running; wait for it to finish.",
        ),
        "has_children" => (
            "has_children",
            "This knowledge still has stores below it; remove those first.",
        ),
        "invalid_input" => (
            "invalid_input",
            "The knowledge service could not accept that input; check the values and try again.",
        ),
        "invalid_target" => (
            "invalid_target",
            "That address cannot be used: it must be a public https address.",
        ),
        "leaf_full" => (
            "leaf_full",
            "This knowledge store is full; add another store for more documents.",
        ),
        "not_found" => (
            "not_found",
            "That knowledge item was not found; it may have been removed.",
        ),
        "pack_published" => (
            "pack_published",
            "That knowledge pack version is published and cannot be changed.",
        ),
        "payload_too_large" => ("payload_too_large", "The file is too large."),
        "provider_not_configured" => (
            "provider_not_configured",
            "No embedding provider is configured for this knowledge; set one under Providers.",
        ),
        "question_exists" => (
            "question_exists",
            "That evaluation question already exists.",
        ),
        "too_many_questions" => (
            "too_many_questions",
            "There are too many evaluation questions; remove some first.",
        ),
        "unknown_store" => ("unknown_store", "That knowledge store is not known."),
        "unsupported_source_kind" => (
            "unsupported_source_kind",
            "The ingest service cannot read this kind of source.",
        ),
        "unsupported_type" => ("unsupported_type", "That file type is not supported."),
        "upstream_unavailable" => (
            "upstream_unavailable",
            "A provider the knowledge service calls is unavailable; try again shortly.",
        ),
        // strata `ProviderRejected`
        "api_key_required" => ("api_key_required", "That provider needs an API key."),
        "dialect_not_allowed" => (
            "dialect_not_allowed",
            "That provider type is not allowed here.",
        ),
        "dims_must_be_1024" => (
            "dims_must_be_1024",
            "The embedding model must produce 1024-dimensional vectors.",
        ),
        "invalid_provider" => ("invalid_provider", "The provider settings are not valid."),
        "probe_failed" => (
            "probe_failed",
            "The provider did not answer the test call; check its address, model and key.",
        ),
        "provider_host_not_allowed" => (
            "provider_host_not_allowed",
            "That provider address cannot be used: it must be a public https host.",
        ),
        "region_not_allowed" => (
            "region_not_allowed",
            "That provider's region is not allowed for this tenant.",
        ),
        "secret_required_for_new_host" => (
            "secret_required_for_new_host",
            "Enter the key again: a stored key is only sent to the host it was saved for.",
        ),
        "unknown_capability" => (
            "unknown_capability",
            "The knowledge service does not serve that slot.",
        ),
        "zdr_required" => (
            "zdr_required",
            "This tenant needs a provider that retains no data (zero data retention).",
        ),
        _ => return None,
    })
}

/// The upstream codes passed on (module doc), as our `&'static str`.
pub fn pass_through(code: &str) -> Option<&'static str> {
    listed(code).map(|(c, _)| c)
}

/// agent's message for a passed-through code (never strata's text).
pub fn message_for(code: &str) -> Option<&'static str> {
    listed(code).map(|(_, m)| m)
}

/// The store id a strata `not found: store <uuid>` message names.
fn store_named(message: &str) -> Option<Uuid> {
    let at = message.find("store ")? + "store ".len();
    Uuid::parse_str(message.get(at..at + 36)?).ok()
}

/// A non-2xx upstream answer as our error (module doc).
pub fn refusal(name: &'static str, status: StatusCode, body: &Value) -> ApiError {
    let err = body.get("error").and_then(Value::as_object);
    let code = err
        .and_then(|e| e.get("code"))
        .and_then(Value::as_str)
        .and_then(pass_through);
    let passes = match code {
        Some("upstream_unavailable") => status == StatusCode::BAD_GATEWAY,
        Some(_) => {
            status.is_client_error()
                && status != StatusCode::UNAUTHORIZED
                && status != StatusCode::FORBIDDEN
        }
        None => false,
    };
    let (Some(code), true) = (code, passes) else {
        tracing::warn!(
            upstream = name,
            status = status.as_u16(),
            "upstream refused or failed"
        );
        return unavailable(name);
    };
    // strata's own text: the debug log only (it names strata ids, paths and routes)
    let upstream_message: String = err
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .map(|m| m.chars().take(MESSAGE_CHARS).collect())
        .unwrap_or_default();
    tracing::debug!(
        upstream = name,
        status = status.as_u16(),
        code,
        upstream_message = %upstream_message,
        "upstream refusal passed on"
    );
    let message = message_for(code).unwrap_or("The knowledge service refused the request.");
    // the error object's extra fields, and fields strata puts beside it (`empty_sync_run`'s report)
    let mut extra: Map<String, Value> = Map::new();
    if let Some(top) = body.as_object() {
        for (k, v) in top.iter().filter(|(k, _)| k.as_str() != "error") {
            extra.insert(k.clone(), v.clone());
        }
    }
    if let Some(e) = err {
        for (k, v) in e
            .iter()
            .filter(|(k, _)| k.as_str() != "code" && k.as_str() != "message")
        {
            extra.insert(k.clone(), v.clone());
        }
    }
    let mut out = ApiError::detailed(status, code, message);
    if let Value::Object(extra) = crate::netguard::scrub_secrets(Value::Object(extra)) {
        for (k, v) in extra {
            out = out.with(&k, v);
        }
    }
    if code == "not_found"
        && let Some(store) = store_named(&upstream_message)
    {
        out = out.with("store", store.to_string());
    }
    if code == "eval_not_ready" {
        // the eval page lists what to fix before the set can run: strata's problem lines are about the tenant's own
        // golden questions (`question N: no document `X` in the evaluated tree`), so they pass, bounded, as `problems`
        let full = err
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        out = out.with("problems", eval_problems(full));
    }
    out
}

/// Most `eval_not_ready` problem lines passed on, and the longest one.
const MAX_PROBLEMS: usize = 200;
const PROBLEM_CHARS: usize = 300;

/// The problem lines of strata_rag's `eval_not_ready` message (`eval not ready: a\nb` → [a, b]).
fn eval_problems(message: &str) -> Vec<String> {
    let body = message.strip_prefix("eval not ready:").unwrap_or(message);
    body.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .take(MAX_PROBLEMS)
        .map(|l| l.chars().take(PROBLEM_CHARS).collect())
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn eval_not_ready_carries_the_problem_lines_and_keeps_agents_message() {
        let body = json!({"error": {"code": "eval_not_ready", "message":
            "eval not ready: question 1: no document `Not a document` in the evaluated tree\nquestion 3: chunk c-9 is not in the evaluated tree\n"}});
        let ApiError::Detailed {
            code,
            message,
            extra,
            ..
        } = refusal("strata_rag", StatusCode::CONFLICT, &body)
        else {
            panic!("a detailed refusal");
        };
        assert_eq!(code, "eval_not_ready");
        assert_eq!(message, "The evaluation is not ready yet.");
        assert_eq!(
            extra.get("problems"),
            Some(&json!([
                "question 1: no document `Not a document` in the evaluated tree",
                "question 3: chunk c-9 is not in the evaluated tree"
            ]))
        );
    }

    #[test]
    fn eval_problems_are_bounded() {
        let many = (0..500)
            .map(|i| format!("question {i}: {}", "x".repeat(400)))
            .collect::<Vec<_>>()
            .join("\n");
        let p = eval_problems(&many);
        assert_eq!(p.len(), MAX_PROBLEMS);
        assert!(p.iter().all(|l| l.chars().count() <= PROBLEM_CHARS));
    }
}
