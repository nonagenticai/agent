//! MCP over streamable HTTP to an EXTERNAL server (caller identity Part B, B1): the endpoint of a template whose
//! transport is `mcp` and whose auth kind is not `none` (kind `none` is the platform MCP server, [`super::client`]).
//!
//! The server is the instance's `base_url`; the request headers are the instance's static and credential headers,
//! built by `connectors::transport::auth::headers_for` exactly as for REST. The HTTP client is netguard's pinned
//! client (the same `check_url` / private-target / loopback rules as REST). One session per
//! (tenant, instance, credential version, licence token fingerprint), kept [`SESSION_TTL`] and dropped on a 401/403
//! or a transport failure: a session caches its headers, so a licence bearer (minted per call, short-lived) must
//! never be reused by a session opened with an older one.
//! A `tools/call` is normally plain, but an external server that implements the MCP tasks extension (SEP-2663) —
//! the conveyor's `projects.execute` — may answer with a task instead of a result. There is no separate protocol
//! path: [`ExternalMcp::start_task`] and [`ExternalMcp::call`] resolve the tool and its arguments identically, and
//! the four task methods reuse the platform client's types ([`super::tasks`]).
//!
//! Errors: a refused credential (401/403) is `connector_auth_failed`, a transport failure or a timeout
//! `upstream_unavailable`, a tool error (`isError`) a 422 `{"error"}` answer ([`crate::connectors::transport::mcp::answer`]);
//! every detail is scrubbed of the call's secret strings.
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::time::{Duration, Instant};

use reqwest::header::{HeaderName, HeaderValue};
use rmcp::ServiceExt;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CancelTaskParams, ClientConfig, ClientJsonRpcMessage,
    GetTaskParams, RequestMetaObject, UpdateTaskParams,
};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::common::client_side_sse::{BoxedSseResponse, ExponentialBackoff};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
};
use serde_json::{Map, Value};
use tokio::sync::Mutex;
use uuid::Uuid;

use super::client::{service_error_to_tool_error, tool_result_to_value};
use super::tasks::{TaskError, TaskHandle, TaskSnapshot};
use crate::connectors::transport::auth::{self, AuthSpec};
use crate::connectors::transport::mcp::{answer, arguments};
use crate::connectors::transport::{
    CallTarget, Transport, misconfigured, redact_body, scrub_error,
};
use crate::connectors::wire::{AuthKind, CallSecrets, OpError, RawResponse, RenderedRequest};
use crate::netguard;

/// How long a session is reused before the next call opens a new one (headers, OAuth tokens and the tool list are
/// re-read).
pub const SESSION_TTL: Duration = Duration::from_secs(600);

/// The pinned HTTP client's total time per request. A call is bounded by the op's own timeout on top of this.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// rmcp's own default (crate-private there); only the size-less trait entry points use it.
const MAX_SSE_EVENT_SIZE: usize = 16 * 1024 * 1024;

type HttpError = StreamableHttpError<reqwest::Error>;

/// One tool of an external server's `tools/list`, as the importer and the caller read it.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtTool {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
    pub output_schema: Option<Value>,
    /// `annotations.readOnlyHint == true`.
    pub read_only: bool,
    /// `_meta.operation`: the one op key this tool serves.
    pub operation: Option<String>,
    /// `_meta.operations`: op key -> the fixed arguments that select that op on a consolidated tool
    /// (`{"card.move": {"action": "move"}}`).
    pub operations: BTreeMap<String, Map<String, Value>>,
}

impl ExtTool {
    /// Whether this tool serves the op `op_key` (`_meta.operations` key, else `_meta.operation`).
    pub fn serves(&self, op_key: &str) -> bool {
        self.operations.contains_key(op_key) || self.operation.as_deref() == Some(op_key)
    }

    /// The tools of a `tools/list` result (`{"tools": [...]}`); entries without a name are skipped.
    pub fn parse_list(result: &Value) -> Vec<ExtTool> {
        result["tools"]
            .as_array()
            .map(|a| a.iter().filter_map(Self::parse).collect())
            .unwrap_or_default()
    }

    fn parse(t: &Value) -> Option<ExtTool> {
        let operations = t["_meta"]["operations"]
            .as_object()
            .map(|m| {
                m.iter()
                    .map(|(k, v)| (k.clone(), v.as_object().cloned().unwrap_or_default()))
                    .collect()
            })
            .unwrap_or_default();
        Some(ExtTool {
            name: t["name"].as_str()?.to_owned(),
            description: t["description"].as_str().map(str::to_owned),
            input_schema: t.get("inputSchema").cloned().unwrap_or(Value::Null),
            output_schema: t.get("outputSchema").filter(|v| v.is_object()).cloned(),
            read_only: t["annotations"]["readOnlyHint"].as_bool() == Some(true),
            operation: t["_meta"]["operation"].as_str().map(str::to_owned),
            operations,
        })
    }
}

/// The status of a 401 or 403 in any of the shapes rmcp's reqwest client reports it.
fn auth_refusal(e: &HttpError) -> Option<u16> {
    match e {
        StreamableHttpError::AuthRequired(_) => Some(401),
        StreamableHttpError::InsufficientScope(_) => Some(403),
        StreamableHttpError::Client(e) => e
            .status()
            .filter(|s| {
                *s == reqwest::StatusCode::UNAUTHORIZED || *s == reqwest::StatusCode::FORBIDDEN
            })
            .map(|s| s.as_u16()),
        StreamableHttpError::UnexpectedServerResponse(m) if m.starts_with("HTTP 401") => Some(401),
        StreamableHttpError::UnexpectedServerResponse(m) if m.starts_with("HTTP 403") => Some(403),
        _ => None,
    }
}

/// rmcp's reqwest client whose every request carries the instance's headers (and never rmcp's own `auth_header`),
/// recording the status of a 401/403 on the session.
#[derive(Clone)]
struct HeaderClient {
    http: reqwest::Client,
    headers: Arc<Vec<(HeaderName, HeaderValue)>>,
    /// 0 = none; else the status of the last auth refusal (cleared by a later success).
    rejected: Arc<AtomicU16>,
}

impl HeaderClient {
    fn with(&self, mut h: HashMap<HeaderName, HeaderValue>) -> HashMap<HeaderName, HeaderValue> {
        for (n, v) in self.headers.iter() {
            h.insert(n.clone(), v.clone());
        }
        h
    }

    fn note<T>(&self, r: Result<T, HttpError>) -> Result<T, HttpError> {
        match &r {
            Ok(_) => self.rejected.store(0, Ordering::SeqCst),
            Err(e) => {
                if let Some(s) = auth_refusal(e) {
                    self.rejected.store(s, Ordering::SeqCst);
                }
            }
        }
        r
    }
}

impl StreamableHttpClient for HeaderClient {
    type Error = reqwest::Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, HttpError> {
        self.post_message_with_max_sse_event_size(
            uri,
            message,
            session_id,
            None,
            custom_headers,
            MAX_SSE_EVENT_SIZE,
        )
        .await
    }

    async fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<StreamableHttpPostResponse, HttpError> {
        let r = self
            .http
            .post_message_with_max_sse_event_size(
                uri,
                message,
                session_id,
                None,
                self.with(custom_headers),
                max_sse_event_size,
            )
            .await;
        self.note(r)
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), HttpError> {
        let r = self
            .http
            .delete_session(uri, session_id, None, self.with(custom_headers))
            .await;
        self.note(r)
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxedSseResponse, HttpError> {
        self.get_stream_with_max_sse_event_size(
            uri,
            session_id,
            last_event_id,
            None,
            custom_headers,
            MAX_SSE_EVENT_SIZE,
        )
        .await
    }

    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<BoxedSseResponse, HttpError> {
        let r = self
            .http
            .get_stream_with_max_sse_event_size(
                uri,
                session_id,
                last_event_id,
                None,
                self.with(custom_headers),
                max_sse_event_size,
            )
            .await;
        self.note(r)
    }
}

/// One open session on an external server.
struct Session {
    opened: Instant,
    service: RunningService<RoleClient, ClientConfig>,
    rejected: Arc<AtomicU16>,
    /// The licence token this session's fixed headers carry (a fingerprint, [`licence_fingerprint`]): `None` for
    /// every other auth kind. [`ExternalMcp::drop_session`] rebuilds the session key from it.
    token: Option<u64>,
    /// Every derived secret string of the credential (header values, tokens): scrubbed from errors and bodies.
    redact: Vec<String>,
    tools: StdMutex<Option<Vec<ExtTool>>>,
}

/// (tenant, instance id, credential version, licence token fingerprint).
type Key = (Uuid, String, i32, Option<u64>);
type Slot = Arc<Mutex<Option<Arc<Session>>>>;

/// The sessions to external MCP servers of one process. Lives on `Transport`; cheap to share.
#[derive(Default)]
pub struct ExternalMcp {
    slots: StdMutex<HashMap<Key, Slot>>,
}

impl std::fmt::Debug for ExternalMcp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalMcp").finish_non_exhaustive()
    }
}

fn upstream(detail: impl Into<String>) -> OpError {
    OpError::new("upstream_unavailable", detail)
}

fn auth_failed(status: u16) -> OpError {
    OpError::new(
        "connector_auth_failed",
        format!("The MCP server answered {status}: the credential was refused."),
    )
}

/// A fingerprint of the licence bearer minted into this call (`secrets.secret["licence_token"]`), `None` for every
/// other auth kind. A session caches its headers for up to [`SESSION_TTL`] while a licence lives ~15 minutes, so a
/// session opened late in a token's life would send an expired one and be refused: keying the session on the token
/// makes a new token open a new session.
fn licence_fingerprint(spec: &AuthSpec, secrets: &CallSecrets) -> Option<u64> {
    if spec.kind != AuthKind::LicenceBearer {
        return None;
    }
    let token = secrets
        .secret
        .get("licence_token")
        .cloned()
        .unwrap_or(Value::Null);
    let mut hasher = DefaultHasher::new();
    token.to_string().hash(&mut hasher);
    Some(hasher.finish())
}

impl ExternalMcp {
    fn slot(&self, key: &Key) -> Slot {
        let mut map = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        // an older credential version — or licence token — of this instance is never used again
        map.retain(|k, _| !(k.0 == key.0 && k.1 == key.1 && (k.2 != key.2 || k.3 != key.3)));
        map.entry(key.clone()).or_default().clone()
    }

    /// The instance's live session: the cached one while it is younger than [`SESSION_TTL`], else a new one.
    async fn session(
        &self,
        t: &Transport,
        target: &CallTarget,
        spec: &AuthSpec,
        secrets: &CallSecrets,
        timeout: Duration,
    ) -> Result<Arc<Session>, OpError> {
        let token = licence_fingerprint(spec, secrets);
        let key = (
            target.tenant,
            target.instance_id.clone(),
            target.credential_version,
            token,
        );
        let slot = self.slot(&key);
        let mut guard = slot.lock().await;
        if let Some(s) = guard.as_ref().filter(|s| s.opened.elapsed() < SESSION_TTL) {
            return Ok(s.clone());
        }
        let mut opened = open(t, target, spec, secrets, timeout).await?;
        opened.token = token;
        let s = Arc::new(opened);
        *guard = Some(s.clone());
        Ok(s)
    }

    /// Forget `session` (a no-op when a newer one already replaced it).
    async fn drop_session(&self, target: &CallTarget, session: &Arc<Session>) {
        let key = (
            target.tenant,
            target.instance_id.clone(),
            target.credential_version,
            session.token,
        );
        let slot = self.slot(&key);
        let mut guard = slot.lock().await;
        if guard.as_ref().is_some_and(|s| Arc::ptr_eq(s, session)) {
            *guard = None;
        }
    }

    /// An rmcp failure as the connector error; the session is dropped when the failure calls for a reconnect.
    async fn failed(
        &self,
        target: &CallTarget,
        s: &Arc<Session>,
        what: &str,
        e: &rmcp::service::ServiceError,
    ) -> OpError {
        let (te, reset) = service_error_to_tool_error(e);
        let refused = s.rejected.load(Ordering::SeqCst);
        if reset || refused != 0 {
            self.drop_session(target, s).await;
        }
        if refused != 0 {
            return auth_failed(refused);
        }
        upstream(format!("mcp {what}: {}", te.message))
    }

    /// `tools/list` of the external server (every page), cached on the session.
    pub async fn list(
        &self,
        t: &Transport,
        target: &CallTarget,
        spec: &AuthSpec,
        secrets: &CallSecrets,
        timeout: Duration,
    ) -> Result<Vec<ExtTool>, OpError> {
        let out = self.list_inner(t, target, spec, secrets, timeout).await;
        out.map_err(|e| scrub_error(e, &auth::secret_strings(secrets)))
    }

    async fn list_inner(
        &self,
        t: &Transport,
        target: &CallTarget,
        spec: &AuthSpec,
        secrets: &CallSecrets,
        timeout: Duration,
    ) -> Result<Vec<ExtTool>, OpError> {
        let s = self.session(t, target, spec, secrets, timeout).await?;
        if let Some(tools) = s
            .tools
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        {
            return Ok(tools);
        }
        let listed = match tokio::time::timeout(timeout, s.service.peer().list_all_tools()).await {
            Err(_) => {
                self.drop_session(target, &s).await;
                return Err(upstream(format!(
                    "mcp tools/list: no answer within {} ms",
                    timeout.as_millis()
                )));
            }
            Ok(Err(e)) => return Err(self.failed(target, &s, "tools/list", &e).await),
            Ok(Ok(t)) => t,
        };
        let v = serde_json::to_value(&listed)
            .map_err(|e| upstream(format!("mcp tools/list: unreadable answer: {e}")))?;
        let tools = ExtTool::parse_list(&serde_json::json!({ "tools": v }));
        *s.tools.lock().unwrap_or_else(PoisonError::into_inner) = Some(tools.clone());
        Ok(tools)
    }

    /// One plain `tools/call` of op `op_key` on the instance. The tool is `req.path` when the template names one, else
    /// the listed tool that serves `op_key`; a tool found through `_meta.operations` gets that entry's fixed arguments
    /// (they win over the rendered ones).
    #[allow(clippy::too_many_arguments)]
    pub async fn call(
        &self,
        t: &Transport,
        target: &CallTarget,
        spec: &AuthSpec,
        secrets: &CallSecrets,
        op_key: &str,
        req: &RenderedRequest,
        timeout: Duration,
    ) -> Result<RawResponse, OpError> {
        let out = self
            .call_inner(t, target, spec, secrets, op_key, req, timeout)
            .await;
        let strings = auth::secret_strings(secrets);
        out.map(|mut r| {
            r.body = redact_body(r.body, &strings);
            r
        })
        .map_err(|e| scrub_error(e, &strings))
    }

    #[allow(clippy::too_many_arguments)]
    async fn call_inner(
        &self,
        t: &Transport,
        target: &CallTarget,
        spec: &AuthSpec,
        secrets: &CallSecrets,
        op_key: &str,
        req: &RenderedRequest,
        timeout: Duration,
    ) -> Result<RawResponse, OpError> {
        let mut args = arguments(req.body.as_ref())?;
        let named = req.path.trim().trim_start_matches('/');
        let tool = if named.is_empty() {
            let listed = self.list_inner(t, target, spec, secrets, timeout).await?;
            let found = listed.iter().find(|x| x.serves(op_key)).ok_or_else(|| {
                misconfigured(format!("the MCP server offers no tool for {op_key}"))
            })?;
            if let (Some(fixed), Value::Object(m)) = (found.operations.get(op_key), &mut args) {
                for (k, v) in fixed {
                    m.insert(k.clone(), v.clone());
                }
            }
            found.name.clone()
        } else {
            named.to_owned()
        };
        let s = self.session(t, target, spec, secrets, timeout).await?;
        let mut params = CallToolRequestParams::new(tool.clone());
        if let Value::Object(m) = args {
            params = params.with_arguments(m);
        }
        match tokio::time::timeout(timeout, s.service.peer().call_tool(params)).await {
            Err(_) => {
                self.drop_session(target, &s).await;
                Err(upstream(format!(
                    "mcp call {tool}: no answer within {} ms",
                    timeout.as_millis()
                )))
            }
            Ok(Err(e)) => Err(self.failed(target, &s, &format!("call {tool}"), &e).await),
            Ok(Ok(res)) => {
                let mut out = answer(tool_result_to_value(res));
                // a derived secret (an OAuth token) the plain secret strings do not cover
                if let Ok(r) = &mut out {
                    r.body = redact_body(std::mem::take(&mut r.body), &s.redact);
                }
                out.map_err(|e| scrub_error(e, &s.redact))
            }
        }
    }

    /// A `tools/call` that may come back as a task (SEP-2663). Tool resolution, fixed `_meta.operations` arguments,
    /// redaction and error mapping are [`Self::call`]'s, so the only difference is the response arm: a server that
    /// materializes a task returns [`ExtTaskStart::Task`] instead of a body. `meta` rides the request `_meta` (the
    /// job actor, `super::tasks::actor_meta`); a plain call sends none.
    #[allow(clippy::too_many_arguments)]
    pub async fn start_task(
        &self,
        t: &Transport,
        target: &CallTarget,
        spec: &AuthSpec,
        secrets: &CallSecrets,
        op_key: &str,
        req: &RenderedRequest,
        timeout: Duration,
        meta: Option<RequestMetaObject>,
    ) -> Result<ExtTaskStart, OpError> {
        let mut args = arguments(req.body.as_ref())?;
        let named = req.path.trim().trim_start_matches('/');
        let tool = if named.is_empty() {
            let listed = self.list_inner(t, target, spec, secrets, timeout).await?;
            let found = listed.iter().find(|x| x.serves(op_key)).ok_or_else(|| {
                misconfigured(format!("the MCP server offers no tool for {op_key}"))
            })?;
            if let (Some(fixed), Value::Object(m)) = (found.operations.get(op_key), &mut args) {
                for (k, v) in fixed {
                    m.insert(k.clone(), v.clone());
                }
            }
            found.name.clone()
        } else {
            named.to_owned()
        };
        let s = self.session(t, target, spec, secrets, timeout).await?;
        let mut params = CallToolRequestParams::new(tool.clone());
        if let Value::Object(m) = args {
            params = params.with_arguments(m);
        }
        params.meta = meta;
        match tokio::time::timeout(timeout, s.service.peer().call_tool_once(params)).await {
            Err(_) => {
                self.drop_session(target, &s).await;
                Err(upstream(format!(
                    "mcp call {tool}: no answer within {} ms",
                    timeout.as_millis()
                )))
            }
            Ok(Err(e)) => Err(self.failed(target, &s, &format!("call {tool}"), &e).await),
            Ok(Ok(CallToolResponse::Complete(r))) => {
                let mut out = answer(tool_result_to_value(r));
                if let Ok(r) = &mut out {
                    r.body = redact_body(std::mem::take(&mut r.body), &s.redact);
                }
                out.map(ExtTaskStart::Completed)
                    .map_err(|e| scrub_error(e, &s.redact))
            }
            Ok(Ok(CallToolResponse::Task(t))) => Ok(ExtTaskStart::Task(TaskHandle {
                task_id: t.task.task_id,
                status: super::tasks::status_str(&t.task.status),
                status_message: t.task.status_message,
                poll_interval_ms: t.task.poll_interval_ms,
                ttl_ms: t.task.ttl_ms,
            })),
            Ok(Ok(CallToolResponse::InputRequired(_))) => Err(upstream(format!(
                "mcp call {tool}: the server asked for input at call time, which agent does not answer"
            ))),
            Ok(Ok(_)) => Err(upstream(format!("mcp call {tool}: unexpected response"))),
        }
    }

    /// `tasks/get` on the instance. A `-32602` is `task_not_found`: the task is gone for good and polling stops.
    pub async fn get_task(
        &self,
        t: &Transport,
        target: &CallTarget,
        spec: &AuthSpec,
        secrets: &CallSecrets,
        timeout: Duration,
        task_id: &str,
    ) -> Result<TaskSnapshot, OpError> {
        let s = self.session(t, target, spec, secrets, timeout).await?;
        match tokio::time::timeout(
            timeout,
            s.service.peer().get_task(GetTaskParams::new(task_id)),
        )
        .await
        {
            Err(_) => {
                self.drop_session(target, &s).await;
                Err(upstream(format!(
                    "mcp tasks/get: no answer within {} ms",
                    timeout.as_millis()
                )))
            }
            Ok(Err(e)) => match super::tasks::task_error(&e) {
                Some(TaskError::Lost(m)) => Err(OpError::new("task_not_found", m)),
                _ => Err(self.failed(target, &s, "tasks/get", &e).await),
            },
            Ok(Ok(r)) => Ok(super::tasks::snapshot_of(&r)),
        }
    }

    /// `tasks/update` with one answer. The ack is cooperative: keep polling.
    #[allow(clippy::too_many_arguments)]
    pub async fn answer_task(
        &self,
        t: &Transport,
        target: &CallTarget,
        spec: &AuthSpec,
        secrets: &CallSecrets,
        timeout: Duration,
        task_id: &str,
        key: &str,
        response: Value,
        meta: Option<RequestMetaObject>,
    ) -> Result<(), OpError> {
        let s = self.session(t, target, spec, secrets, timeout).await?;
        let mut params =
            UpdateTaskParams::new(task_id, BTreeMap::from([(key.to_string(), response)]));
        params.meta = meta;
        match tokio::time::timeout(timeout, s.service.peer().update_task(params)).await {
            Err(_) => {
                self.drop_session(target, &s).await;
                Err(upstream(format!(
                    "mcp tasks/update: no answer within {} ms",
                    timeout.as_millis()
                )))
            }
            Ok(Err(e)) => match super::tasks::task_error(&e) {
                Some(TaskError::Lost(m)) => Err(OpError::new("task_not_found", m)),
                _ => Err(self.failed(target, &s, "tasks/update", &e).await),
            },
            Ok(Ok(())) => Ok(()),
        }
    }

    /// `tasks/cancel`, a cooperative ack: the task may still complete. Keep polling.
    pub async fn cancel_task(
        &self,
        t: &Transport,
        target: &CallTarget,
        spec: &AuthSpec,
        secrets: &CallSecrets,
        timeout: Duration,
        task_id: &str,
    ) -> Result<(), OpError> {
        let s = self.session(t, target, spec, secrets, timeout).await?;
        match tokio::time::timeout(
            timeout,
            s.service.peer().cancel_task(CancelTaskParams::new(task_id)),
        )
        .await
        {
            Err(_) => {
                self.drop_session(target, &s).await;
                Err(upstream(format!(
                    "mcp tasks/cancel: no answer within {} ms",
                    timeout.as_millis()
                )))
            }
            Ok(Err(e)) => match super::tasks::task_error(&e) {
                Some(TaskError::Lost(m)) => Err(OpError::new("task_not_found", m)),
                _ => Err(self.failed(target, &s, "tasks/cancel", &e).await),
            },
            Ok(Ok(())) => Ok(()),
        }
    }
}

/// What a task-capable external call returned.
#[derive(Clone, Debug)]
pub enum ExtTaskStart {
    /// The call answered at once (the server has no tasks, or the work finished): the same body `call` returns.
    Completed(RawResponse),
    /// The server materialized a task: poll it with [`ExternalMcp::get_task`].
    Task(TaskHandle),
}

/// Opens a session: the target passes netguard, the instance's headers are built, the MCP handshake is bounded by
/// `timeout`.
async fn open(
    t: &Transport,
    target: &CallTarget,
    spec: &AuthSpec,
    secrets: &CallSecrets,
    timeout: Duration,
) -> Result<Session, OpError> {
    let base = netguard::check_url(&target.base_url, t.policy(), target.tenant)?;
    let tls = auth::client_tls(spec, secrets)?;
    let http =
        netguard::pinned_client_tls(&base, t.policy(), target.tenant, HTTP_TIMEOUT, tls.as_ref())
            .await?;
    let creds = auth::headers_for(t, target, spec, secrets, timeout).await?;
    let mut redact = auth::secret_strings(secrets);
    redact.extend(creds.redact.iter().cloned());
    let mut headers = creds.headers;
    for (_, v) in headers.iter_mut() {
        v.set_sensitive(true);
    }
    let rejected = Arc::new(AtomicU16::new(0));
    let client = HeaderClient {
        http,
        headers: Arc::new(headers),
        rejected: rejected.clone(),
    };
    let mut cfg = StreamableHttpClientTransportConfig::with_uri(base.to_string());
    let mut backoff = ExponentialBackoff::default();
    backoff.max_delay = Some(Duration::from_secs(30));
    cfg.retry_config = Arc::new(backoff);
    let transport = StreamableHttpClientTransport::with_client(client, cfg);
    // Every external session declares the tasks extension (SEP-2663), exactly as the platform session does
    // (`super::tasks::client_config`). A server that does not implement it ignores the capability; one that does
    // (the conveyor) may answer a `tools/call` with a task. Whether a tool is STARTED as a task is configuration,
    // never the declared capability.
    let info = super::tasks::client_config();
    let service = match tokio::time::timeout(timeout, info.serve(transport)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            let refused = rejected.load(Ordering::SeqCst);
            return Err(scrub_error(
                if refused != 0 {
                    auth_failed(refused)
                } else {
                    upstream(format!("mcp connect: {e}"))
                },
                &redact,
            ));
        }
        Err(_) => {
            return Err(upstream(format!(
                "mcp connect: no answer within {} ms",
                timeout.as_millis()
            )));
        }
    };
    Ok(Session {
        opened: Instant::now(),
        service,
        rejected,
        // stamped by `ExternalMcp::session` on the session the key was computed for
        token: None,
        redact,
        tools: StdMutex::new(None),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tools_are_read_with_their_annotations_and_operations() {
        let tools = ExtTool::parse_list(&json!({"tools": [
            {"name": "card", "description": "Cards.", "inputSchema": {"type": "object"},
             "annotations": {"readOnlyHint": false},
             "_meta": {"operations": {"card.create": {"action": "create"}, "card.move": {"action": "move"}}}},
            {"name": "board_list", "inputSchema": {"type": "object"},
             "outputSchema": {"type": "object", "properties": {"boards": {}}},
             "annotations": {"readOnlyHint": true}, "_meta": {"operation": "board.list"}},
            {"description": "no name"}
        ]}));
        assert_eq!(tools.len(), 2);
        assert!(tools[0].serves("card.move") && tools[0].serves("card.create"));
        assert!(!tools[0].serves("card.read") && !tools[0].read_only);
        assert_eq!(tools[0].operations["card.move"]["action"], json!("move"));
        assert!(tools[1].serves("board.list") && tools[1].read_only);
        assert!(tools[1].output_schema.is_some());
    }
}
