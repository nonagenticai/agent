import { describe, expect, it } from 'vitest';
import {
	answerContent,
	answerProblem,
	isTaskTerminal,
	questionFields,
	taskStatusWord,
	validateAnswer,
} from '../../src/lib/mcp-tasks/tasks';
import { httpMcpTasks } from '../../src/lib/api/http/mcp-tasks';

const schema = {
	type: 'object',
	required: ['decision'],
	properties: {
		comment: { type: 'string' },
		reason: { type: 'string' },
		decision: { type: 'string', enum: ['approved', 'rejected'] },
	},
};

describe('mcp task answer form', () => {
	it('orders decision, reason, comment and marks enums as choices', () => {
		const f = questionFields(schema);
		expect(f.map((x) => x.name)).toEqual(['decision', 'reason', 'comment']);
		expect(f[0]).toMatchObject({ kind: 'choice', options: ['approved', 'rejected'], required: true });
		expect(f[1]!.kind).toBe('text');
	});
	it('requires a reason when rejected, and a valid choice', () => {
		expect(validateAnswer(schema, { decision: 'rejected' })).toMatch(/reason is required/i);
		expect(validateAnswer(schema, { decision: 'rejected', reason: ' too risky ' })).toBeNull();
		expect(validateAnswer(schema, { decision: 'approved' })).toBeNull();
		expect(validateAnswer(schema, {})).toMatch(/decision is required/);
		expect(validateAnswer(schema, { decision: 'maybe' })).toMatch(/must be one of/);
		expect(validateAnswer(schema, { decision: 'approved', other: 'x' })).toMatch(/Unknown field/);
	});
	it('trims and drops blank values', () => {
		expect(answerContent({ decision: 'approved', reason: '  ', comment: ' ok ' })).toEqual({
			decision: 'approved',
			comment: 'ok',
		});
	});
	it('words statuses and refusals', () => {
		expect(isTaskTerminal('completed')).toBe(true);
		expect(isTaskTerminal('input_required')).toBe(false);
		expect(taskStatusWord('input_required')).toBe('open');
		expect(answerProblem('answered_elsewhere', 'x')).toMatch(/Someone else/);
		expect(answerProblem('invalid_answer', 'bad')).toBe('bad');
	});
});

describe('mcp tasks client', () => {
	it('calls the job-scoped routes', async () => {
		const calls: [string, string, unknown][] = [];
		const req = ((verb: string, path: string, opts?: { body?: unknown }) => {
			calls.push([verb, path, opts?.body]);
			return Promise.resolve({});
		}) as never;
		const c = httpMcpTasks(req, () => 'my job');
		await c.mcpTasks(true);
		await c.answerMcpTask('t/1', { key: 'k', action: 'accept', content: { decision: 'approved' } });
		await c.cancelMcpTask('t1');
		expect(calls).toEqual([
			['GET', '/api/jobs/my%20job/mcp-tasks?open=true', undefined],
			[
				'POST',
				'/api/jobs/my%20job/mcp-tasks/t%2F1/answers',
				{ key: 'k', action: 'accept', content: { decision: 'approved' } },
			],
			['POST', '/api/jobs/my%20job/mcp-tasks/t1/cancel', undefined],
		]);
	});
	it('refuses without a job', async () => {
		await expect(
			httpMcpTasks((() => Promise.resolve({})) as never, () => null).mcpTasks(true),
		).rejects.toMatchObject({
			code: 'job_not_found',
		});
	});
});
