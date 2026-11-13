//! MCP Tasks (SEP-2663, extension `io.modelcontextprotocol/tasks`), client side
//!
//! nonagenticai_mcp_server answers a `tools/call` for long-running work (an n8n workflow, an fr project) with a durable
//! task instead of a result, but only to a client that declares the extension ([`client_config`], sent on every
//! session). This module starts such calls, polls them, relays an answer to their questions (a person's, or the
//! job's own model's — crate::runtime::task_answer, owner 2026-10-31) and cancels them. The durable bookkeeping
//! (table `mcp_tasks`, the poller, the staff routes) lives in the agent platform's store; this file depends on the
//! rest of the crate only through [`HttpMcp`] and [`ToolError`].
//!
//! [`HttpMcp::answer_task`] takes a [`StaffActor`], which only the platform builds — a human-authenticated route, or
//! the task relay answering for the job itself; no agent tool set offers it.

use std::collections::BTreeMap;
use std::time::Duration;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, CancelTaskParams, ClientCapabilities,
    ClientConfig, ExtensionCapabilities, GetTaskParams, GetTaskResult, Implementation, JsonObject,
    RequestMetaObject, TASKS_EXTENSION_ID, UpdateTaskParams,
};
use rmcp::service::ServiceError;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::ToolError;
use super::client::{HttpMcp, tool_error_from_text, tool_result_to_value};

/// `_meta` key naming the human who answers a task's question (resolved by fr through `user_channel_prefs`).
pub const ACTOR_META: &str = "ai.nonagentic/actor";
/// `_meta` key the server puts a task's fr project on: `{project_id, activity, signals}` (the wire contract of
/// 2026-11-06). rmcp flattens the task object and drops unknown top-level fields, so `_meta` is the only channel.
pub const FR_META: &str = "ai.nonagentic/fr";
/// `_meta` key the conveyor (a tenant's external MCP server) puts a task's run on: `{project_id, run_id, status,
/// events}`, the progress feed fr carries under [`FR_META`] without its signals. rmcp drops unknown top-level
/// fields, so `_meta` is the only channel.
pub const CONVEYOR_META: &str = "ai.nonagentic/conveyor";
/// JSON-RPC error of an unknown, expired or other-tenant `taskId`: the task is gone for good.
pub const UNKNOWN_TASK: i32 = -32602;
/// Poll interval bounds and the default when the server names none.
pub const POLL_FLOOR: Duration = Duration::from_secs(1);
pub const POLL_CAP: Duration = Duration::from_secs(60);
pub const POLL_DEFAULT: Duration = Duration::from_secs(5);
/// Env var listing the MCP tools that are started as tasks (comma-separated). Configuration decides, never the model.
pub const TASK_TOOLS_VAR: &str = "MCP_TASK_TOOLS";

/// The client config every agent MCP session is opened with: the tasks extension declared.
pub fn client_config() -> ClientConfig {
    let mut capabilities = ClientCapabilities::default();
    capabilities.extensions = Some(ExtensionCapabilities::from([(
        TASKS_EXTENSION_ID.to_string(),
        JsonObject::new(),
    )]));
    ClientConfig::new(
        capabilities,
        Implementation::new("agent", env!("CARGO_PKG_VERSION")),
    )
}

/// The long-running tools of `MCP_TASK_TOOLS`: trimmed, empty entries dropped.
pub fn task_tools(value: Option<&str>) -> Vec<String> {
    value
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// The server's `pollIntervalMs`, held to [`POLL_FLOOR`]..[`POLL_CAP`].
pub fn poll_delay(poll_interval_ms: Option<u64>) -> Duration {
    poll_interval_ms
        .map(Duration::from_millis)
        .unwrap_or(POLL_DEFAULT)
        .clamp(POLL_FLOOR, POLL_CAP)
}

/// What a task-capable call returned.
#[derive(Clone, Debug, PartialEq)]
pub enum TaskStart {
    /// The call finished at once (or the server does not offer tasks): the same value `call_tool` returns.
    Completed(Value),
    /// The server materialized a task; poll it with [`HttpMcp::get_task`].
    Task(TaskHandle),
}

#[derive(Clone, Debug, PartialEq)]
pub struct TaskHandle {
    pub task_id: String,
    pub status: String,
    pub status_message: Option<String>,
    pub poll_interval_ms: Option<u64>,
    pub ttl_ms: Option<u64>,
}

/// Why a task operation did not get an answer.
#[derive(Clone, Debug, PartialEq)]
pub enum TaskError {
    /// `-32602`: the server no longer knows the task (expired, unknown, another tenant's). Stop polling.
    Lost(String),
    /// Anything else, classified as for tool calls (retryable transport failures, permanent protocol errors).
    Tool(ToolError),
}

impl TaskError {
    pub fn message(&self) -> &str {
        match self {
            TaskError::Lost(m) => m,
            TaskError::Tool(e) => &e.message,
        }
    }
}

/// A task as its row stores it: the server that started it, so every get/answer/cancel reaches that server.
pub struct TaskRef<'a> {
    pub tenant: Uuid,
    pub job: Uuid,
    /// The row's key (`mcp_tasks.task_id`): a platform task's is the server's id, a connector task's is namespaced
    /// `tenant:instance:server id` (a task id is unique only on its own server, SEP-2663).
    pub task_id: &'a str,
    /// The server's own task id (`mcp_tasks.server_task_id`): what `tasks/get` / `tasks/update` / `tasks/cancel`
    /// send. Equal to `task_id` for a platform task.
    pub server_task_id: &'a str,
    /// `mcp_tasks.origin`: `platform` (the platform MCP server) or `connector` (a tenant's own system).
    pub origin: &'a str,
    /// `mcp_tasks.origin_instance`: the connector instance, when the origin is `connector`.
    pub instance: Option<&'a str>,
}

/// One `tasks/get` answer: the status, its text, the next poll interval, and the whole wire object (`inputRequests`,
/// `result` or `error` inlined) for storage.
#[derive(Clone, Debug, PartialEq)]
pub struct TaskSnapshot {
    pub status: String,
    pub status_message: Option<String>,
    pub poll_interval_ms: Option<u64>,
    pub detail: Value,
}

impl TaskSnapshot {
    /// From the `DetailedTask` wire object (`{taskId, status, statusMessage?, pollIntervalMs?, ...}`).
    pub fn from_detail(detail: Value) -> Self {
        Self {
            status: detail["status"].as_str().unwrap_or("working").to_string(),
            status_message: detail["statusMessage"].as_str().map(String::from),
            poll_interval_ms: detail["pollIntervalMs"].as_u64(),
            detail,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self.status.as_str(), "completed" | "failed" | "cancelled")
    }
}

/// How a finished task ends up, as a tool result would.
#[derive(Clone, Debug, PartialEq)]
pub enum TaskOutcome {
    /// `completed`: the tool's own result, mapped like `call_tool` (an `isError` result is a [`ToolError`]).
    Completed(Result<Value, ToolError>),
    /// `failed`: the JSON-RPC error of the task. Permanent.
    Failed(ToolError),
    /// `cancelled`: stopped at a staff user's request.
    Cancelled,
    /// The server lost the task (`-32602`). Permanent.
    Lost(ToolError),
}

impl TaskOutcome {
    /// The stored form (`mcp_tasks.result`): `{"outcome", "ok", "result" | "error", "retryable"}`.
    pub fn to_json(&self) -> Value {
        let err = |outcome: &str, e: &ToolError| json!({"outcome": outcome, "ok": false, "error": e.message, "retryable": e.retryable});
        match self {
            TaskOutcome::Completed(Ok(v)) => {
                json!({"outcome": "completed", "ok": true, "result": v})
            }
            TaskOutcome::Completed(Err(e)) => err("completed", e),
            TaskOutcome::Failed(e) => err("failed", e),
            TaskOutcome::Lost(e) => err("lost", e),
            TaskOutcome::Cancelled => json!({"outcome": "cancelled", "ok": false}),
        }
    }

    /// The `mcp_tasks.status` this outcome ends the row in.
    pub fn status(&self) -> &'static str {
        match self {
            TaskOutcome::Completed(_) => "completed",
            TaskOutcome::Failed(_) => "failed",
            TaskOutcome::Cancelled => "cancelled",
            TaskOutcome::Lost(_) => "lost",
        }
    }
}

/// The outcome of a terminal snapshot; `None` while it is still `working` / `input_required`.
pub fn outcome_of(s: &TaskSnapshot) -> Option<TaskOutcome> {
    match s.status.as_str() {
        "completed" => Some(TaskOutcome::Completed(completed_value(&s.detail["result"]))),
        "failed" => {
            let e = &s.detail["error"];
            let code = e["code"]
                .as_i64()
                .map(|c| format!(" ({c})"))
                .unwrap_or_default();
            let message = e["message"].as_str().unwrap_or("the task failed");
            Some(TaskOutcome::Failed(ToolError::fatal(format!(
                "task failed{code}: {message}"
            ))))
        }
        "cancelled" => Some(TaskOutcome::Cancelled),
        _ => None,
    }
}

/// The stored form of a `tasks/get` answer: the WHOLE result — `_meta` beside the flattened task — so what the
/// server puts there (fr's project id, activity and signals) survives into `mcp_tasks.last_detail` for the relay.
pub fn snapshot_of(r: &GetTaskResult) -> TaskSnapshot {
    TaskSnapshot::from_detail(serde_json::to_value(r).unwrap_or(Value::Null))
}

/// A `completed` task's `result` (a `CallToolResult` dump) as a tool result.
fn completed_value(result: &Value) -> Result<Value, ToolError> {
    match serde_json::from_value::<CallToolResult>(result.clone()) {
        Ok(r) => tool_result_to_value(r),
        // Not a CallToolResult: report its text as the tool would, permanent.
        Err(_) => Err(tool_error_from_text(&format!(
            "[permanent] task result is not a tool result: {}",
            truncate(&result.to_string(), 500)
        ))),
    }
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// One open question of an `input_required` task: its key, the text shown to staff, and the schema of the answer.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct InputQuestion {
    pub key: String,
    pub message: String,
    pub requested_schema: Value,
}

/// The `elicitation/create` entries of `inputRequests`; anything else (sampling, roots) is not shown to staff.
pub fn input_questions(detail: &Value) -> Vec<InputQuestion> {
    let Some(map) = detail["inputRequests"].as_object() else {
        return vec![];
    };
    map.iter()
        .filter(|(_, r)| r["method"] == "elicitation/create")
        .map(|(key, r)| InputQuestion {
            key: key.clone(),
            message: r["params"]["message"].as_str().unwrap_or("").to_string(),
            requested_schema: r["params"]["requestedSchema"].clone(),
        })
        .collect()
}

/// Who a task call is made for: the tenant, and its staff user (a human, built only by a human-authenticated route) or
/// the job's service user ([`StaffActor::job_user`]) when no person is behind the call.
#[derive(Clone, Debug, PartialEq)]
pub struct StaffActor {
    pub tenant: Uuid,
    pub staff_user: String,
}

impl StaffActor {
    /// `agent:<tenant_id>`: the agent product's one service-account member of the tenant in fr (owner 2026-10-31,
    /// fr#1683), linked by an admin; never a person and never per job.
    pub fn address(&self) -> String {
        format!("agent:{}", self.tenant)
    }

    /// `_meta["ai.nonagentic/actor"]`: the address, plus the job (`job`) when the call is the job's own, else the
    /// staff user it is made for (`on_behalf_of`, the requester; fr still routes approvals to humans).
    pub fn meta(&self) -> Value {
        let mut m = json!({"channel": "mcp", "address": self.address()});
        match self.staff_user.strip_prefix("job:") {
            Some(job) => m["job"] = json!(job),
            None => m["on_behalf_of"] = json!(self.staff_user),
        }
        m
    }

    /// The job's service user (`job:<job>`): who a call is made for when an end user, not staff, starts or answers a
    /// task. It travels as `job` in the actor meta; the address stays the tenant's.
    pub fn job_user(job: Uuid) -> String {
        format!("job:{job}")
    }

    /// The job's service actor.
    pub fn for_job(tenant: Uuid, job: Uuid) -> Self {
        Self {
            tenant,
            staff_user: Self::job_user(job),
        }
    }
}

/// Check a staff answer against the question's `requestedSchema` and build the `ElicitResult` to send.
/// `accept` needs `content` that meets the schema (required keys present and non-empty, enum values, strings only,
/// no keys the schema does not name) and a `reason` when the decision is `rejected`; `decline` / `cancel` carry no
/// content. The error names what is wrong, for the UI.
pub fn validate_answer(
    schema: &Value,
    action: &str,
    content: Option<&Value>,
) -> Result<Value, String> {
    match action {
        "decline" | "cancel" => return Ok(json!({"action": action})),
        "accept" => {}
        other => {
            return Err(format!(
                "unknown action {other:?}: accept, decline or cancel"
            ));
        }
    }
    let Some(content) = content.and_then(Value::as_object) else {
        return Err("an accepted answer needs content".into());
    };
    let props = schema["properties"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    let mut clean = Map::new();
    for (k, v) in content {
        let Some(p) = props.get(k) else {
            return Err(format!("{k} is not a field of this question"));
        };
        let Some(s) = v.as_str() else {
            if v.is_null() {
                continue;
            }
            return Err(format!("{k} must be text"));
        };
        let s = s.trim();
        if s.is_empty() {
            continue;
        }
        if let Some(allowed) = p["enum"].as_array()
            && !allowed.iter().any(|a| a.as_str() == Some(s))
        {
            return Err(format!(
                "{k} must be one of {}",
                Value::Array(allowed.clone())
            ));
        }
        if p["type"].as_str().is_some_and(|t| t != "string") {
            return Err(format!("{k} has an unsupported type"));
        }
        clean.insert(k.clone(), Value::String(s.to_string()));
    }
    for r in schema["required"].as_array().into_iter().flatten() {
        if let Some(r) = r.as_str()
            && !clean.contains_key(r)
        {
            return Err(format!("{r} is required"));
        }
    }
    if clean.get("decision").and_then(Value::as_str) == Some("rejected")
        && !clean.contains_key("reason")
    {
        return Err("reason is required when rejecting".into());
    }
    Ok(json!({"action": "accept", "content": clean}))
}

pub(crate) fn task_error(e: &ServiceError) -> Option<TaskError> {
    match e {
        ServiceError::McpError(d) if d.code.0 == UNKNOWN_TASK => Some(TaskError::Lost(format!(
            "task unknown to the server: {}",
            d.message
        ))),
        _ => None,
    }
}

impl HttpMcp {
    /// C2: a `tools/call` that may come back as a task. A server without the extension, or an immediate result, gives
    /// [`TaskStart::Completed`] (today's envelope, e.g. `{run_id, status:"queued"}`). An MRTR `input_required` at
    /// call time is not something this server sends: permanent error. `actor` is the person the task starts for (C9,
    /// sent as `_meta["ai.nonagentic/actor"]`); the plain-call fallback sends none, since no task is started there.
    pub async fn start_task(
        &self,
        actor: &StaffActor,
        name: &str,
        arguments: Value,
    ) -> Result<TaskStart, ToolError> {
        let tenant = actor.tenant;
        let (generation, svc, auth, tasks) = self.session_for_tasks(tenant).await?;
        if !tasks {
            return self
                .call_tool_for(tenant, name, arguments)
                .await
                .map(TaskStart::Completed);
        }
        let mut params = CallToolRequestParams::new(name.to_string());
        if let Value::Object(m) = arguments {
            params = params.with_arguments(m);
        }
        // C9: the person on whose behalf the task starts. The server stores it on the run; an fr project run without
        // one fails with `actor_required`.
        params.meta = Some(actor_meta(actor));
        match svc.peer().call_tool_once(params).await {
            Ok(CallToolResponse::Complete(r)) => tool_result_to_value(r).map(TaskStart::Completed),
            Ok(CallToolResponse::Task(t)) => Ok(TaskStart::Task(TaskHandle {
                task_id: t.task.task_id,
                status: status_str(&t.task.status),
                status_message: t.task.status_message,
                poll_interval_ms: t.task.poll_interval_ms,
                ttl_ms: t.task.ttl_ms,
            })),
            Ok(CallToolResponse::InputRequired(_)) => Err(ToolError::fatal(format!(
                "mcp call {name}: the server asked for input at call time, which agent does not answer"
            ))),
            Ok(_) => Err(ToolError::fatal(format!(
                "mcp call {name}: unexpected response"
            ))),
            Err(e) => Err(self.fail(tenant, generation, &auth, &e, name).await),
        }
    }

    /// C3: `tasks/get`.
    pub async fn get_task(&self, tenant: Uuid, task_id: &str) -> Result<TaskSnapshot, TaskError> {
        let (generation, svc, auth, _) = self
            .session_for_tasks(tenant)
            .await
            .map_err(TaskError::Tool)?;
        match svc.peer().get_task(GetTaskParams::new(task_id)).await {
            // the whole result, not only `r.task`: `_meta` is where the server's fr project state travels
            Ok(r) => Ok(snapshot_of(&r)),
            Err(e) => Err(match task_error(&e) {
                Some(t) => t,
                None => {
                    TaskError::Tool(self.fail(tenant, generation, &auth, &e, "tasks/get").await)
                }
            }),
        }
    }

    /// C4: `tasks/update` with one human answer (an `ElicitResult` from [`validate_answer`]) and the actor `_meta`.
    /// The ack is eventually consistent: keep polling.
    pub async fn answer_task(
        &self,
        actor: &StaffActor,
        task_id: &str,
        key: &str,
        response: Value,
    ) -> Result<(), TaskError> {
        let (generation, svc, auth, _) = self
            .session_for_tasks(actor.tenant)
            .await
            .map_err(TaskError::Tool)?;
        let mut params =
            UpdateTaskParams::new(task_id, BTreeMap::from([(key.to_string(), response)]));
        params.meta = Some(actor_meta(actor));
        match svc.peer().update_task(params).await {
            Ok(()) => Ok(()),
            Err(e) => Err(match task_error(&e) {
                Some(t) => t,
                None => TaskError::Tool(
                    self.fail(actor.tenant, generation, &auth, &e, "tasks/update")
                        .await,
                ),
            }),
        }
    }

    /// C5: `tasks/cancel`, a cooperative ack: the task may still complete. Keep polling.
    pub async fn cancel_task(&self, tenant: Uuid, task_id: &str) -> Result<(), TaskError> {
        let (generation, svc, auth, _) = self
            .session_for_tasks(tenant)
            .await
            .map_err(TaskError::Tool)?;
        match svc.peer().cancel_task(CancelTaskParams::new(task_id)).await {
            Ok(()) => Ok(()),
            Err(e) => Err(match task_error(&e) {
                Some(t) => t,
                None => TaskError::Tool(
                    self.fail(tenant, generation, &auth, &e, "tasks/cancel")
                        .await,
                ),
            }),
        }
    }
}

/// The request `_meta` naming `actor` (`ai.nonagentic/actor`).
pub(crate) fn actor_meta(actor: &StaffActor) -> RequestMetaObject {
    let mut meta = RequestMetaObject::new();
    meta.insert(ACTOR_META.to_string(), actor.meta());
    meta
}

pub(crate) fn status_str(s: &rmcp::model::TaskStatus) -> String {
    serde_json::to_value(s)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_else(|| "working".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(v: Value) -> TaskSnapshot {
        TaskSnapshot::from_detail(v)
    }

    #[test]
    fn the_client_config_declares_the_tasks_extension() {
        let v = serde_json::to_value(client_config()).unwrap();
        assert_eq!(
            v["capabilities"]["extensions"][TASKS_EXTENSION_ID],
            json!({})
        );
        assert_eq!(v["clientInfo"]["name"], "agent");
    }

    #[test]
    fn task_tools_come_from_configuration() {
        assert_eq!(
            task_tools(Some(" n8n_run , ,fr_project_start")),
            ["n8n_run", "fr_project_start"]
        );
        assert!(task_tools(None).is_empty());
    }

    #[test]
    fn poll_interval_is_floored_and_capped() {
        assert_eq!(poll_delay(Some(10)), POLL_FLOOR);
        assert_eq!(poll_delay(Some(600_000)), POLL_CAP);
        assert_eq!(poll_delay(Some(2_500)), Duration::from_millis(2_500));
        assert_eq!(poll_delay(None), POLL_DEFAULT);
    }

    /// The server's fr project state rides `_meta`; rmcp flattens the task beside it. Storing the whole result keeps
    /// both, so the relay can read `_meta` and still sees the task's own `status` at the top level.
    #[test]
    fn a_tasks_get_keeps_meta_beside_the_flattened_task() {
        let r: GetTaskResult = serde_json::from_value(json!({
            "resultType": "complete",
            "taskId": "t-1",
            "status": "working",
            "createdAt": "2026-11-06T10:00:00Z",
            "lastUpdatedAt": "2026-11-06T10:00:01Z",
            "_meta": {(FR_META): {
                "project_id": "91afede0-f7f7-5633-9522-050c0db00196",
                "activity": [{"seq": 2, "at": "2026-11-06T10:00:01Z", "kind": "phase",
                              "text": "Writing the brief.", "severity": "info"}],
                "signals": []
            }}
        }))
        .expect("a tasks/get result with _meta");
        let s = snapshot_of(&r);
        assert_eq!(s.status, "working");
        assert_eq!(s.detail["status"], "working");
        assert_eq!(
            s.detail["_meta"][FR_META]["project_id"],
            "91afede0-f7f7-5633-9522-050c0db00196"
        );
        assert_eq!(s.detail["_meta"][FR_META]["activity"][0]["seq"], 2);
    }

    #[test]
    fn each_terminal_status_maps_to_its_outcome() {
        let ok = snap(json!({"taskId": "t", "status": "completed", "result": {
            "content": [], "isError": false,
            "structuredContent": {"run_id": "r", "status": "done", "result": {"x": 1}, "error": null}}}));
        assert_eq!(
            outcome_of(&ok),
            Some(TaskOutcome::Completed(Ok(
                json!({"run_id": "r", "status": "done", "result": {"x": 1}, "error": null})
            )))
        );
        let text = snap(json!({"status": "completed", "result": {
            "content": [{"type": "text", "text": "{\"a\":2}"}], "isError": false}}));
        assert_eq!(
            outcome_of(&text),
            Some(TaskOutcome::Completed(Ok(json!({"a": 2}))))
        );
        let tool_err = snap(json!({"status": "completed", "result": {
            "content": [{"type": "text", "text": "[retryable] n8n timed out"}], "isError": true}}));
        let Some(TaskOutcome::Completed(Err(e))) = outcome_of(&tool_err) else {
            panic!("isError is a tool error")
        };
        assert!(e.retryable);
        assert_eq!(e.message, "n8n timed out");
        let failed =
            snap(json!({"status": "failed", "error": {"code": -32603, "message": "boom"}}));
        let Some(TaskOutcome::Failed(e)) = outcome_of(&failed) else {
            panic!()
        };
        assert!(!e.retryable);
        assert!(e.message.contains("-32603") && e.message.contains("boom"));
        assert_eq!(
            outcome_of(&snap(json!({"status": "cancelled"}))),
            Some(TaskOutcome::Cancelled)
        );
        assert_eq!(outcome_of(&snap(json!({"status": "working"}))), None);
        assert_eq!(outcome_of(&snap(json!({"status": "input_required"}))), None);
        assert_eq!(TaskOutcome::Cancelled.to_json()["outcome"], "cancelled");
        assert_eq!(TaskOutcome::Lost(ToolError::fatal("gone")).status(), "lost");
    }

    fn approval_schema() -> Value {
        json!({"type": "object", "required": ["decision"], "properties": {
            "decision": {"type": "string", "enum": ["approved", "rejected"]},
            "reason": {"type": "string"}, "comment": {"type": "string"}}})
    }

    #[test]
    fn input_questions_are_the_elicitations_only() {
        let d = json!({"status": "input_required", "inputRequests": {
            "t:00aa": {"method": "elicitation/create", "params": {"message": "Ship it?", "requestedSchema": approval_schema()}},
            "t:00bb": {"method": "sampling/createMessage", "params": {}}}});
        let q = input_questions(&d);
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].key, "t:00aa");
        assert_eq!(q[0].message, "Ship it?");
        assert_eq!(q[0].requested_schema, approval_schema());
        assert!(input_questions(&json!({"status": "working"})).is_empty());
    }

    #[test]
    fn a_rejection_needs_a_reason_and_the_schema_is_enforced() {
        let s = approval_schema();
        assert_eq!(
            validate_answer(&s, "accept", Some(&json!({"decision": "rejected"}))).unwrap_err(),
            "reason is required when rejecting"
        );
        assert_eq!(
            validate_answer(
                &s,
                "accept",
                Some(&json!({"decision": "rejected", "reason": " too risky "}))
            )
            .unwrap(),
            json!({"action": "accept", "content": {"decision": "rejected", "reason": "too risky"}})
        );
        assert!(validate_answer(&s, "accept", Some(&json!({"decision": "maybe"}))).is_err());
        assert!(
            validate_answer(&s, "accept", Some(&json!({"reason": "x"}))).is_err(),
            "decision required"
        );
        assert!(
            validate_answer(
                &s,
                "accept",
                Some(&json!({"decision": "approved", "extra": "x"}))
            )
            .is_err()
        );
        assert!(validate_answer(&s, "accept", None).is_err());
        assert_eq!(
            validate_answer(&s, "decline", None).unwrap(),
            json!({"action": "decline"})
        );
        assert!(validate_answer(&s, "approve", None).is_err());
    }

    #[test]
    fn the_actor_is_the_tenant_agent_with_the_job_or_the_requester() {
        let t = "00000000-0000-0000-0000-000000000001";
        let staff = StaffActor {
            tenant: Uuid::from_u128(1),
            staff_user: "u-1".into(),
        };
        assert_eq!(staff.address(), format!("agent:{t}"));
        assert_eq!(
            staff.meta(),
            json!({"channel": "mcp", "address": format!("agent:{t}"), "on_behalf_of": "u-1"})
        );
        let job = StaffActor::for_job(Uuid::from_u128(1), Uuid::from_u128(2));
        assert_eq!(
            job.meta(),
            json!({"channel": "mcp", "address": format!("agent:{t}"),
                   "job": "00000000-0000-0000-0000-000000000002"})
        );
    }
}
