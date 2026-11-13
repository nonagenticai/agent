// MCP tasks over HTTP: job-scoped routes served by agent-api; they act in the page's job.
import type { Api } from '../client';
import { ApiError } from '../errors';
import { seg, type Req } from '../http-client';
import type { LiveMethod } from './routes';

export const MCP_TASKS_METHODS = [
	'mcpTasks',
	'mcpTask',
	'answerMcpTask',
	'cancelMcpTask',
	'mcpTools',
] as const satisfies readonly LiveMethod[];

export type McpTasksMethod = (typeof MCP_TASKS_METHODS)[number];

/** The answer type of `m`. */
type R<M extends keyof Api> = Awaited<ReturnType<Api[M]>>;

export function httpMcpTasks(req: Req, job: () => string | null): Pick<Api, McpTasksMethod> {
	const jobSeg = () => {
		const j = job();
		if (!j) throw new ApiError(404, 'job_not_found', 'Pick a job first.');
		return seg(j);
	};
	return {
		mcpTasks: async (open) => {
			const flag = open ? 'true' : 'false';
			return req<R<'mcpTasks'>>('GET', `/api/jobs/${jobSeg()}/mcp-tasks?open=${flag}`);
		},
		mcpTask: async (id) => req<R<'mcpTask'>>('GET', `/api/jobs/${jobSeg()}/mcp-tasks/${seg(id)}`),
		answerMcpTask: async (id, body) =>
			req<R<'answerMcpTask'>>('POST', `/api/jobs/${jobSeg()}/mcp-tasks/${seg(id)}/answers`, {
				body: {
					key: body.key,
					action: body.action,
					...(body.content ? { content: body.content } : {}),
				},
			}),
		cancelMcpTask: async (id) =>
			req<R<'cancelMcpTask'>>('POST', `/api/jobs/${jobSeg()}/mcp-tasks/${seg(id)}/cancel`),
		mcpTools: async () => req<R<'mcpTools'>>('GET', `/api/jobs/${jobSeg()}/mcp-tools`),
	};
}
