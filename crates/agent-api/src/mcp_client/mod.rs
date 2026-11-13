//! MCP client for nonagenticai_mcp_server: streamable HTTP, one session per tenant, per-tenant credentials
//! (mcp-app licence + binding token), and the MCP tasks extension for long-running tools. Ported from care-bus
//! (`crates/carebus/src/mcp_http.rs`, `mcp_tasks.rs`, spec `2026-10-30-mcp-tasks-client.md`).
//!
//! - [`client`]: [`client::HttpMcp`], [`client::TenantCredentials`] and the [`client::CredentialSource`] a caller
//!   supplies; 401/403 refresh-once, fail-closed credentials, `tools/list` incl. each tool's `_meta.operation`.
//! - [`tasks`]: start / poll / answer / cancel a server-side task; answering needs a human [`tasks::StaffActor`].
//! - [`store`]: the platform's durable side (credential source, task bookkeeping).

pub mod client;
pub mod creds;
pub mod external;
pub mod store;
pub mod tasks;

use serde_json::Value;

/// Why a tool call failed, and whether trying again can help.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolError {
    pub message: String,
    pub retryable: bool,
}

impl ToolError {
    /// A failure that retrying will not fix.
    pub fn fatal(m: impl Into<String>) -> Self {
        Self {
            message: m.into(),
            retryable: false,
        }
    }

    /// A failure that may pass (transport trouble, a timeout, expired credentials).
    pub fn retryable(m: impl Into<String>) -> Self {
        Self {
            message: m.into(),
            retryable: true,
        }
    }
}

/// Calls one MCP tool as a fixed tenant.
pub trait McpClient: Send + Sync + 'static {
    fn call_tool(
        &self,
        name: &str,
        arguments: Value,
    ) -> impl Future<Output = Result<Value, ToolError>> + Send;
}
