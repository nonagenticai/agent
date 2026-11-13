// The `McpTasksApi` contract: a job's MCP tasks (staff answer questions, cancel) and the MCP tools it can start.
import type {
	McpAnswerBody,
	McpAnswerResult,
	McpCancelResult,
	McpTaskList,
	McpTaskView,
	McpToolList,
} from '$lib/api/types/mcp-tasks';

export type McpTasksApi = {
	/** `GET /api/jobs/{job}/mcp-tasks?open=true|false` — the job's tasks, newest first; `open` keeps unfinished ones. */
	mcpTasks(open: boolean): Promise<McpTaskList>;
	/** `GET /api/jobs/{job}/mcp-tasks/{task_id}` — one task (404 when unknown). */
	mcpTask(taskId: string): Promise<McpTaskView>;
	/** `POST /api/jobs/{job}/mcp-tasks/{task_id}/answers` — staff or tenant_admin; 409 `not_waiting` |
	 * `answered_elsewhere` | `task_lost`; 422 `invalid_answer`. */
	answerMcpTask(taskId: string, body: McpAnswerBody): Promise<McpAnswerResult>;
	/** `POST /api/jobs/{job}/mcp-tasks/{task_id}/cancel` — staff or tenant_admin; 409 `task_finished`. */
	cancelMcpTask(taskId: string): Promise<McpCancelResult>;
	/** `GET /api/jobs/{job}/mcp-tools` — the MCP tools the job can start; `available` false when no server answers. */
	mcpTools(): Promise<McpToolList>;
};
