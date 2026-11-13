//! The `mcp` transport: one operation call of an MCP-transport connector template is one `tools/call` on the
//! nonagenticai MCP server (crate::mcp_client), as the call's tenant (its own mcp licence and binding; no shared
//! identity and no instance credential).
//!
//! The template op names the MCP tool in `path` (no leading `/`); `request` renders its arguments (a JSON object).
//! An empty `path` takes the tool whose `_meta.operation` (the server's `object.verb`) is the op's own key, read from
//! the tenant's `tools/list` (cached [`LIST_TTL`]). The tool's result comes back as a 200 [`RawResponse`] whose body
//! is the tool's structured content, so `field_map`, `status_map` and verify reads work as for REST. A tool error
//! (`isError`) is a 422 with `{"error": <message>}` for the template's fault mapping; a refused licence or binding is
//! `connector_auth_failed`, a tool the caller may not use is `connector_misconfigured`, and a transport failure or a
//! timeout is `upstream_unavailable`.
//!
//! Long-running tools (`MCP_TASK_TOOLS`) are not called here: the connector executor starts them as durable MCP
//! tasks (crate::mcp_client::store).
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::connectors::wire::{OpError, RawResponse, RenderedRequest};
use crate::mcp_client::ToolError;
use crate::mcp_client::client::{AUTH_REJECTED, HttpMcp, McpTool};

/// How long a tenant's `tools/list` is believed.
pub const LIST_TTL: Duration = Duration::from_secs(300);

/// The refusal text of a tool outside the caller's scope (nonagenticai_mcp_server `DENIED_MESSAGE`, prefix stripped).
const NOT_PERMITTED: &str = "tool not permitted for this caller";

/// tenant -> (listed at, tools).
type Listed = HashMap<Uuid, (Instant, Vec<McpTool>)>;

/// The MCP client of one process plus its long-running tool list. Cheap to clone.
#[derive(Clone)]
pub struct McpTransport {
    pub client: Arc<HttpMcp>,
    task_tools: Arc<Vec<String>>,
    listed: Arc<Mutex<Listed>>,
}

impl std::fmt::Debug for McpTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpTransport")
            .field("task_tools", &self.task_tools)
            .finish_non_exhaustive()
    }
}

impl McpTransport {
    pub fn new(client: Arc<HttpMcp>, task_tools: Vec<String>) -> Self {
        Self {
            client,
            task_tools: Arc::new(task_tools),
            listed: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Whether `tool` is started as a durable MCP task (configuration decides, never the model).
    pub fn is_task_tool(&self, tool: &str) -> bool {
        self.task_tools.iter().any(|t| t == tool)
    }

    /// The tools the server offers `tenant`, cached for [`LIST_TTL`].
    pub async fn tools(&self, tenant: Uuid) -> Result<Vec<McpTool>, OpError> {
        let cached = self
            .listed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&tenant)
            .filter(|(at, _)| at.elapsed() < LIST_TTL)
            .map(|(_, t)| t.clone());
        if let Some(t) = cached {
            return Ok(t);
        }
        let tools = self.client.list_tools(tenant).await.map_err(|e| {
            let m = format!("{e:#}");
            if m.contains(AUTH_REJECTED) {
                OpError::new("connector_auth_failed", m)
            } else {
                OpError::new("upstream_unavailable", m)
            }
        })?;
        self.listed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(tenant, (Instant::now(), tools.clone()));
        Ok(tools)
    }

    /// The MCP tool an op calls: `path` when the template names one, else the tool that serves `op_key`
    /// (`_meta.operations` key, else `_meta.operation`), with the fixed arguments of that `operations` entry.
    pub async fn tool_for(
        &self,
        tenant: Uuid,
        path: &str,
        op_key: &str,
    ) -> Result<(String, Map<String, Value>), OpError> {
        let named = path.trim().trim_start_matches('/');
        if !named.is_empty() {
            return Ok((named.to_owned(), Map::new()));
        }
        self.tools(tenant)
            .await?
            .into_iter()
            .find(|t| t.serves(op_key))
            .map(|t| {
                let fixed = t.operations.get(op_key).cloned().unwrap_or_default();
                (t.name, fixed)
            })
            .ok_or_else(|| {
                OpError::new(
                    "connector_misconfigured",
                    format!("the MCP server offers this tenant no tool for {op_key}"),
                )
            })
    }

    /// One `tools/call` as `tenant`, bounded by `timeout`.
    pub async fn call(
        &self,
        tenant: Uuid,
        op_key: &str,
        req: &RenderedRequest,
        timeout: Duration,
    ) -> Result<RawResponse, OpError> {
        let (tool, fixed) = self.tool_for(tenant, &req.path, op_key).await?;
        let mut args = arguments(req.body.as_ref())?;
        if let Value::Object(m) = &mut args {
            m.extend(fixed);
        }
        match tokio::time::timeout(timeout, self.client.call_tool_for(tenant, &tool, args)).await {
            Err(_) => Err(OpError::new(
                "upstream_unavailable",
                format!(
                    "mcp call {tool}: no answer within {} ms",
                    timeout.as_millis()
                ),
            )),
            Ok(r) => answer(r),
        }
    }
}

/// The rendered request body as `tools/call` arguments: absent → `{}`, an object as is, anything else refused.
pub fn arguments(body: Option<&Value>) -> Result<Value, OpError> {
    match body {
        None | Some(Value::Null) => Ok(Value::Object(Map::new())),
        Some(v @ Value::Object(_)) => Ok(v.clone()),
        Some(_) => Err(OpError::new(
            "connector_misconfigured",
            "an MCP operation's request must render a JSON object (the tool's arguments)",
        )),
    }
}

/// A tool call's answer as the mapping engine reads it.
pub fn answer(r: Result<Value, ToolError>) -> Result<RawResponse, OpError> {
    match r {
        Ok(body) => Ok(RawResponse {
            status: 200,
            body: match body {
                v @ Value::Object(_) => v,
                other => json!({ "result": other }),
            },
            retry_after_secs: None,
        }),
        Err(e) if e.message.contains(AUTH_REJECTED) => {
            Err(OpError::new("connector_auth_failed", e.message))
        }
        Err(e) if e.message.contains(NOT_PERMITTED) => {
            Err(OpError::new("connector_misconfigured", e.message))
        }
        Err(e) if e.retryable => Err(OpError::new("upstream_unavailable", e.message)),
        Err(e) => Ok(RawResponse {
            status: 422,
            body: json!({ "error": e.message }),
            retry_after_secs: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn results_errors_and_refusals_map_to_the_connector_contract() {
        let ok = answer(Ok(json!({"order": {"id": "A"}}))).unwrap();
        assert_eq!(
            (ok.status, ok.body["order"]["id"].as_str()),
            (200, Some("A"))
        );
        assert_eq!(
            answer(Ok(json!("text"))).unwrap().body,
            json!({"result": "text"})
        );
        let business = answer(Err(ToolError::fatal("order not found"))).unwrap();
        assert_eq!(
            (business.status, &business.body),
            (422, &json!({"error": "order not found"}))
        );
        let down = answer(Err(ToolError {
            message: "mcp connect: refused".into(),
            retryable: true,
        }))
        .unwrap_err();
        assert_eq!(down.code, "upstream_unavailable");
        let auth = answer(Err(ToolError::fatal(format!("{AUTH_REJECTED}: refused")))).unwrap_err();
        assert_eq!(auth.code, "connector_auth_failed");
        let scope = answer(Err(ToolError::fatal(NOT_PERMITTED))).unwrap_err();
        assert_eq!(scope.code, "connector_misconfigured");
    }

    #[test]
    fn arguments_are_an_object() {
        assert_eq!(arguments(None).unwrap(), json!({}));
        assert_eq!(arguments(Some(&json!({"a": 1}))).unwrap(), json!({"a": 1}));
        assert_eq!(
            arguments(Some(&json!([1]))).unwrap_err().code,
            "connector_misconfigured"
        );
    }
}
