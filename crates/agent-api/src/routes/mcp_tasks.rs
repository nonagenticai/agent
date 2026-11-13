//! The job's MCP tasks and tools (crate::mcp_client): `mcpTasks`, `mcpTask`, `answerMcpTask`, `cancelMcpTask` over
//! `mcp_tasks` / `mcp_task_answers` (0240), and `mcpTools`, the tools the MCP server offers the tenant.
//!
//! Reads are any member. Answering a task's question and cancelling a task are `operator`, and only a PERSON can do
//! either: the call goes to the MCP server as the tenant's agent (`agent:<tenant>`) on behalf of that person (`sub`). No
//! agent tool, operation or runner path reaches these handlers; the runner only starts tasks and polls them.
//!
//! Without `MCP_URL` the task reads and answers still work for a task whose row names a connector system; `tools`
//! is 503 `mcp_unconfigured`.
use axum::Router;
use axum::extract::State;
use axum::response::Json;
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::mcp_client::store::{self, AnswerInput};
use crate::mcp_client::tasks::StaffActor;
use crate::state::AppState;
use crate::tenancy::{JobMember, JobRequires, role};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/jobs/{job}/mcp-tasks", get(list))
        .route("/api/jobs/{job}/mcp-tasks/{task_id}", get(one))
        .route("/api/jobs/{job}/mcp-tasks/{task_id}/answers", post(answer))
        .route("/api/jobs/{job}/mcp-tasks/{task_id}/cancel", post(cancel))
        .route("/api/jobs/{job}/mcp-tools", get(tools))
}

/// The most tasks one read returns.
pub const TASKS_MAX: i64 = 100;

fn unconfigured() -> ApiError {
    ApiError::Unavailable(
        "mcp_unconfigured",
        "The MCP server is not configured on this platform.".into(),
        None,
    )
}

fn mcp(st: &AppState) -> ApiResult<&crate::connectors::transport::mcp::McpTransport> {
    st.connectors.mcp().ok_or_else(unconfigured)
}

#[derive(Deserialize)]
struct ListQuery {
    /// `true` (the default): only tasks that are still running or waiting for an answer.
    open: Option<bool>,
}

async fn list(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<ListQuery>,
) -> ApiResult<Json<Value>> {
    let tasks = store::list(
        &st.db,
        j.t.tenant,
        j.job_id,
        q.open.unwrap_or(true),
        TASKS_MAX,
    )
    .await?;
    Ok(Json(json!({ "tasks": tasks })))
}

async fn one(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiPath((_, task_id)): ApiPath<(String, String)>,
) -> ApiResult<Json<Value>> {
    let task = store::one(&st.db, j.t.tenant, j.job_id, &task_id).await?;
    Ok(Json(
        serde_json::to_value(task).map_err(|e| ApiError::Internal(e.into()))?,
    ))
}

async fn answer(
    JobRequires(j, _): JobRequires<role::Operator>,
    State(st): State<AppState>,
    ApiPath((_, task_id)): ApiPath<(String, String)>,
    ApiJson(input): ApiJson<AnswerInput>,
) -> ApiResult<Json<Value>> {
    let clients = st.task_clients();
    let actor = StaffActor {
        tenant: j.t.tenant,
        staff_user: j.t.caller.sub.clone(),
    };
    let r = store::answer(&st.db, &clients, &actor, j.job_id, &task_id, &input, &[]).await?;
    Ok(Json(
        json!({ "sent": r.sent, "already_answered": r.already_answered }),
    ))
}

async fn cancel(
    JobRequires(j, _): JobRequires<role::Operator>,
    State(st): State<AppState>,
    ApiPath((_, task_id)): ApiPath<(String, String)>,
) -> ApiResult<Json<Value>> {
    let clients = st.task_clients();
    let sent = store::cancel(&st.db, &clients, j.t.tenant, j.job_id, &task_id).await?;
    Ok(Json(json!({ "sent": sent })))
}

/// The tools the MCP server offers this tenant (name, description, `_meta.operation`); `available: false` with no
/// tools when the server cannot be asked.
async fn tools(JobMember(j): JobMember, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let m = mcp(&st)?;
    Ok(Json(match m.tools(j.t.tenant).await {
        Ok(tools) => json!({
            "available": true,
            "tools": tools.iter().map(|t| json!({
                "name": t.name, "description": t.description, "operation": t.operation,
            })).collect::<Vec<_>>(),
        }),
        Err(e) => {
            tracing::warn!(code = e.code, detail = %e.detail, "mcp: tools/list failed");
            json!({ "available": false, "tools": [], "reason": e.code })
        }
    }))
}
