// The interaction API in the web: the SSE parser (every line-ending and chunk split) and the page helpers.
import { describe, expect, it } from 'vitest';
import { createSseParser, type SseEvent } from '../../src/lib/api/sse';
import { ApiError } from '../../src/lib/api/errors';
import {
	apiSnippets,
	base64ToBlob,
	explainError,
	explainSecretError,
	maskSecret,
	mcpServerName,
	mcpSnippets,
	pickRecordingMime,
	SAMPLE_CALLBACK,
	webhookSnippets,
} from '../../src/lib/interact/interact';

function parseAll(chunks: string[]): SseEvent[] {
	const out: SseEvent[] = [];
	const p = createSseParser((e) => out.push(e));
	for (const c of chunks) p.push(c);
	p.end();
	return out;
}

describe('createSseParser', () => {
	const STREAM =
		'event: interaction.created\ndata: {"id":"i1"}\n\n' +
		'event: output_text.delta\ndata: {"delta":"Hel"}\n\n' +
		'event: output_text.delta\ndata: {"delta":"lo"}\n\n' +
		'event: interaction.completed\ndata: {"id":"i1","output_text":"Hello"}\n\n';

	it('parses named events in order', () => {
		expect(parseAll([STREAM]).map((e) => [e.event, e.data])).toEqual([
			['interaction.created', '{"id":"i1"}'],
			['output_text.delta', '{"delta":"Hel"}'],
			['output_text.delta', '{"delta":"lo"}'],
			['interaction.completed', '{"id":"i1","output_text":"Hello"}'],
		]);
	});

	it('gives the same events for every split of the stream into two chunks', () => {
		const whole = parseAll([STREAM]);
		for (let i = 1; i < STREAM.length; i++)
			expect(parseAll([STREAM.slice(0, i), STREAM.slice(i)])).toEqual(whole);
	});

	it('handles one character at a time', () => {
		expect(parseAll([...STREAM])).toEqual(parseAll([STREAM]));
	});

	it('accepts CRLF and CR line endings, also split between chunks', () => {
		const crlf = STREAM.replace(/\n/g, '\r\n');
		const cr = STREAM.replace(/\n/g, '\r');
		const whole = parseAll([STREAM]);
		expect(parseAll([crlf])).toEqual(whole);
		expect(parseAll([cr])).toEqual(whole);
		for (let i = 1; i < crlf.length; i++) expect(parseAll([crlf.slice(0, i), crlf.slice(i)])).toEqual(whole);
	});

	it('joins multi-line data, defaults the event to message, ignores comments and retry, keeps the id', () => {
		const out = parseAll([': keep-alive\n\nretry: 1000\nid: 7\ndata: a\ndata:b\ndata:  c\n\n']);
		expect(out).toEqual([{ event: 'message', data: 'a\nb\n c', id: '7' }]);
	});

	it('drops an event with no data and resets the name after each dispatch', () => {
		const out = parseAll(['event: ping\n\nevent: x\ndata: 1\n\ndata: 2\n\n']);
		expect(out.map((e) => [e.event, e.data])).toEqual([
			['x', '1'],
			['message', '2'],
		]);
	});

	it('dispatches the last event when the stream closes without the blank line', () => {
		expect(parseAll(['event: interaction.completed\ndata: {"ok":true}'])).toEqual([
			{ event: 'interaction.completed', data: '{"ok":true}', id: '' },
		]);
	});

	it('a field without a colon has an empty value', () => {
		expect(parseAll(['data\n\n'])).toEqual([{ event: 'message', data: '', id: '' }]);
	});
});

describe('Talk to a job helpers', () => {
	it('explainError words the backend codes', () => {
		expect(explainError(new ApiError(503, 'interaction_backend_unavailable', 'x'))).toMatch(/unavailable/);
		expect(explainError(new ApiError(503, 'voice_unavailable', 'x'))).toMatch(/Voice is unavailable/);
		expect(explainError(new ApiError(422, 'no_speech', 'x'))).toMatch(/No speech/);
		expect(explainError(new ApiError(409, 'job_not_live', 'x'))).toMatch(/no live release/);
		expect(explainError(new ApiError(500, 'boom', 'Broke'))).toBe('Broke (500 boom)');
		expect(explainError(new Error('plain'))).toBe('plain');
	});

	it('pickRecordingMime prefers webm/opus and falls back', () => {
		expect(pickRecordingMime(() => true)).toBe('audio/webm;codecs=opus');
		expect(pickRecordingMime((t) => t === 'audio/mp4')).toBe('audio/mp4');
		expect(pickRecordingMime(() => false)).toBe('');
	});

	it('base64ToBlob round-trips', async () => {
		const b = base64ToBlob(btoa('abc'), 'audio/wav');
		expect(b.type).toBe('audio/wav');
		expect(new Uint8Array(await b.arrayBuffer())).toEqual(new Uint8Array([97, 98, 99]));
	});

	it('snippets use the job base URL and add the tenant header only when acting in one', () => {
		const s = apiSnippets({ origin: 'https://agent.example', job: 'customer care' });
		expect(s.base).toBe('https://agent.example/api/jobs/customer%20care');
		const all = s.items.map((i) => i.code).join('\n');
		expect(all).toContain('base_url="https://agent.example/api/jobs/customer%20care"');
		expect(all).toContain("baseURL: 'https://agent.example/api/jobs/customer%20care'");
		expect(all).toContain('$BASE/responses');
		expect(all).toContain('$BASE/chat/completions');
		expect(all).toContain('$BASE/voice/turns');
		expect(all).not.toContain('X-Active-Tenant-Id');
		const t = apiSnippets({ origin: 'https://a', job: 'j', tenant: 'T9' });
		expect(t.items.map((i) => i.code).join('\n')).toContain('X-Active-Tenant-Id: T9');
	});

	const OPTS = { origin: 'https://agent.example', job: 'customer care' };
	const BASE = 'https://agent.example/api/jobs/customer%20care';
	const codeOf = (g: { items: { code: string }[] }) => g.items.map((i) => i.code).join('\n');

	it('webhook snippets: async turn with callback_url, the secret route, and both receivers verify', () => {
		const g = webhookSnippets(OPTS);
		expect(g.items.map((i) => i.id)).toEqual(['curl-webhook', 'node-webhook', 'python-webhook']);
		const [curl, node, py] = g.items;
		const body = /-d '([^']+)'/.exec(curl!.code)![1]!;
		expect(JSON.parse(body)).toEqual({ input: 'Where is my order?', callback_url: SAMPLE_CALLBACK });
		expect(SAMPLE_CALLBACK.startsWith('https://')).toBe(true);
		expect(curl!.code).toContain('"$BASE/interactions/webhook-secret"');
		expect(node!.code).toContain("import { Webhook } from 'standardwebhooks';");
		expect(node!.code).toContain('new Webhook(process.env.AGENT_WEBHOOK_SECRET)');
		expect(node!.code).toContain('wh.verify(');
		expect(node!.code).toContain('express.raw(');
		expect(py!.code).toContain('from standardwebhooks.webhooks import Webhook');
		expect(py!.code).toContain('wh.verify(request.get_data(), dict(request.headers))');
		expect(codeOf(g)).not.toMatch(/whsec_[A-Za-z0-9]/);
	});

	it('MCP snippets: claude mcp add line and a JSON config, token from the environment', () => {
		const g = mcpSnippets(OPTS);
		const [cli, json] = g.items;
		expect(cli!.code).toBe(
			`claude mcp add --transport http agent-customer-care ${BASE}/mcp \\\n  --header "Authorization: Bearer $AGENT_TOKEN"`,
		);
		expect(JSON.parse(json!.code)).toEqual({
			mcpServers: {
				'agent-customer-care': {
					type: 'http',
					url: `${BASE}/mcp`,
					headers: { Authorization: 'Bearer ${AGENT_TOKEN}' },
				},
			},
		});
		const t = mcpSnippets({ ...OPTS, tenant: 'T9' });
		expect(t.items[0]!.code).toContain('--header "X-Active-Tenant-Id: T9"');
		expect(JSON.parse(t.items[1]!.code).mcpServers['agent-customer-care'].headers).toEqual({
			Authorization: 'Bearer ${AGENT_TOKEN}',
			'X-Active-Tenant-Id': 'T9',
		});
		expect(mcpServerName('a.b/c d')).toBe('agent-a-b-c-d');
	});

	it('the tenant header appears in every connector snippet only when acting in a tenant', () => {
		for (const make of [webhookSnippets]) {
			expect(codeOf(make(OPTS))).not.toContain('X-Active-Tenant-Id');
			expect(codeOf(make({ ...OPTS, tenant: 'T9' }))).toContain('-H "X-Active-Tenant-Id: T9"');
		}
	});

	it('maskSecret keeps the prefix and the last 4 characters only', () => {
		const m = maskSecret('whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw');
		expect(m.startsWith('whsec_')).toBe(true);
		expect(m.endsWith('LaSw')).toBe(true);
		expect(m).not.toContain('MfKQ');
		expect(maskSecret('short')).not.toContain('short');
	});
});
