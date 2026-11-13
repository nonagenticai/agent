// MCP tasks (`/api/jobs/{job}/mcp-tasks`): long-running work the agent started on an MCP server. Wire shapes of
// the agent-api task routes (TaskView).

export type McpTaskStatus = 'working' | 'input_required' | 'completed' | 'cancelled' | 'failed' | 'lost';
export type McpAnswerAction = 'accept' | 'decline' | 'cancel';

export type McpJsonSchema = {
	type?: string;
	required?: string[];
	properties?: Record<string, { type?: string; enum?: unknown[]; title?: string; description?: string }>;
};

export interface McpTaskQuestion {
	key: string;
	message: string;
	requested_schema: McpJsonSchema;
}

export interface McpTaskView {
	task_id: string;
	operation: string;
	tool: string;
	status: McpTaskStatus;
	status_message: string | null;
	questions: McpTaskQuestion[];
	result: unknown;
	cancel_requested: boolean;
	created_at: string;
	terminal_at: string | null;
	/** When staff were asked to take the task over, and why (`refused:<fr code>`, `no_answer`, `failed`, `handover`). */
	escalated_at: string | null;
	escalation_reason: string | null;
}

export interface McpTaskList {
	tasks: McpTaskView[];
}

export interface McpAnswerBody {
	key: string;
	action: McpAnswerAction;
	content?: Record<string, string>;
}

export interface McpAnswerResult {
	sent: boolean;
	already_answered: boolean;
}

export interface McpCancelResult {
	sent: boolean;
}

export interface McpToolInfo {
	name: string;
	description: string;
	operation: string;
}

export interface McpToolList {
	tools: McpToolInfo[];
	available: boolean;
}
