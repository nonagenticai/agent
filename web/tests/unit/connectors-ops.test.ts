// Pure rules of the connectors area: op keys, the SSRF guard, the JSONPath subset used by mapping overrides.
import { describe, expect, it } from 'vitest';
import {
	checkTarget,
	isBuiltinOp,
	jsonPath,
	kvKey,
	kvLine,
	matchBodyFault,
	opKey,
	parseOpKey,
	scrubSecrets,
	transformAllowed,
} from '../../src/lib/connectors/ops';

describe('op keys and counts', () => {
	it('round-trips object.verb{variant} and drops the * variant', () => {
		expect(opKey({ object: 'case', verb: 'create', variant: 'return' })).toBe('case.create{return}');
		expect(opKey({ object: 'order', verb: 'read', variant: '*' })).toBe('order.read');
		expect(parseOpKey('rule.evaluate{PAY-04}')).toEqual({
			object: 'rule',
			verb: 'evaluate',
			variant: 'PAY-04',
		});
		expect(parseOpKey('order.read')).toEqual({ object: 'order', verb: 'read', variant: '*' });
	});
});

describe('isBuiltinOp', () => {
	it('matches the runtime builtin_family: knowledge.search/answer without a variant', () => {
		for (const [object, verb, variant] of <[string, string, string][]>[
			['knowledge', 'search', '*'],
			['knowledge', 'search', ''],
			['knowledge', 'answer', '*'],
			['rule', 'evaluate', '*'],
			['rule', 'evaluate', 'RET-01'],
		])
			expect(isBuiltinOp({ object, verb, variant }), `${object}.${verb}{${variant}}`).toBe(true);
		for (const [object, verb, variant] of <[string, string, string][]>[
			['knowledge', 'search', 'x'],
			['knowledge', 'answer', 'x'],
			['credential', 'verify', '*'],
			['knowledge', 'read', '*'],
			['order', 'read', '*'],
		])
			expect(isBuiltinOp({ object, verb, variant }), `${object}.${verb}{${variant}}`).toBe(false);
	});
});
describe('checkTarget (SSRF guard)', () => {
	it.each([
		['https://10.0.0.5', 'invalid_target'],
		['https://100.64.12.9/api', 'invalid_target'],
		['https://169.254.169.254', 'invalid_target'],
		['https://localhost:8443', 'invalid_target'],
		['https://registry.internal', 'invalid_target'],
		['https://192.168.1.20', 'invalid_target'],
		['http://shop.example.com', 'invalid_url'],
		['https://user:pw@shop.example.com', 'invalid_url'],
		['https://shop.example.com/?token=1', 'invalid_url'],
	])('%s → %s', (url, code) => {
		expect(checkTarget(url)?.code).toBe(code);
	});
	it('accepts a public https host', () => {
		expect(checkTarget('https://lumen-loom.myshopify.com')).toBeNull();
		expect(checkTarget('https://100.128.0.1')).toBeNull();
	});
});
describe('jsonPath', () => {
	const doc = {
		data: {
			order: { name: '#O-1001', fulfillments: [{ trackingInfo: [{ company: 'PostNL' }] }] },
			list: [{ a: 1 }, { a: 2 }],
		},
	};
	it('reads keys, indexes and wildcards', () => {
		expect(jsonPath(doc, '$.data.order.name')).toEqual({ ok: true, value: '#O-1001' });
		expect(jsonPath(doc, '$.data.order.fulfillments[0].trackingInfo[0].company')).toEqual({
			ok: true,
			value: 'PostNL',
		});
		expect(jsonPath(doc, '$.data.list[*].a')).toEqual({ ok: true, value: [1, 2] });
		expect(jsonPath(doc, "$['data'].order.name")).toEqual({ ok: true, value: '#O-1001' });
	});
	it('reports no match and bad syntax', () => {
		expect(jsonPath(doc, '$.data.order.missing')).toMatchObject({ ok: false });
		expect(jsonPath(doc, 'data.order')).toMatchObject({ ok: false, error: 'A JSONPath starts with $.' });
	});
});
describe('kv_line transform and BODY fault rule', () => {
	it('reads KEY:value lines, whole key, first line wins, CRLF tolerated', () => {
		const text = 'USERNAME:jdoe\r\nEMAIL: a@example.com \r\nSTATUS:SUCCESS\r\nA:1\nA:2';
		expect(kvLine(text, 'EMAIL')).toBe('a@example.com');
		expect(kvLine(text, 'STATUS')).toBe('SUCCESS');
		expect(kvLine(text, 'USER')).toBeNull();
		expect(kvLine(text, 'A')).toBe('1');
	});

	it('kv_line:<KEY> is the only parameterised transform', () => {
		expect(kvKey('kv_line:STATUS')).toBe('STATUS');
		for (const bad of ['kv_line', 'kv_line:', 'kv_line:status', 'kv_line:A-B', 'llm_summarise'])
			expect(transformAllowed(bad), bad).toBe(false);
		expect(transformAllowed('kv_line:EMAIL')).toBe(true);
		expect(transformAllowed('mask_last4')).toBe(true);
	});

	it('BODY rules match a 2xx body; anything else never does', () => {
		const body = { stdout: 'EXISTS:false\r\nSTATUS:FAILED\r\n', n: 3 };
		expect(matchBodyFault('BODY $.stdout kv_line:STATUS = FAILED', body)).toBe(true);
		expect(matchBodyFault('BODY $.stdout kv_line:EXISTS = false', body)).toBe(true);
		expect(matchBodyFault('BODY $.stdout kv_line:STATUS = SUCCESS', body)).toBe(false);
		expect(matchBodyFault('BODY $.n = 3', body)).toBe(true);
		expect(matchBodyFault('HTTP 500', body)).toBe(false);
	});
});
describe('scrubSecrets (= netguard::scrub_secrets)', () => {
	it('blanks secret keys at any depth and keeps flags and nulls', () => {
		const v = scrubSecrets({
			id: 'u-1',
			Password: 'Xy7-not-real',
			client_secret: { nested: 'Xy7-not-real' },
			accessToken: 'Xy7-not-real',
			'x-api-key': 'Xy7-not-real',
			Authorization: 'Bearer Xy7-not-real',
			items: [{ private_key: 'Xy7-not-real', name: 'Kettle' }, [{ refresh_token: 'Xy7-not-real' }]],
			temporary_password_sent: true,
			token: null,
			status: 'ok',
		});
		expect(v).toEqual({
			id: 'u-1',
			Password: '[redacted]',
			client_secret: '[redacted]',
			accessToken: '[redacted]',
			'x-api-key': '[redacted]',
			Authorization: '[redacted]',
			items: [{ private_key: '[redacted]', name: 'Kettle' }, [{ refresh_token: '[redacted]' }]],
			temporary_password_sent: true,
			token: null,
			status: 'ok',
		});
		expect(JSON.stringify(v)).not.toContain('Xy7-not-real');
	});
	it('blanks secret KEY:value lines and keeps the key', () => {
		expect(
			scrubSecrets({
				stdout: 'USERNAME:jdoe\nPASSWORD:Xy7-not-real\r\n  API_TOKEN:Xy7-not-real\nSTATUS:SUCCESS',
				list: ['DB_SECRET:Xy7-not-real', 'SSH_PRIVATE_KEY:Xy7-not-real'],
			}),
		).toEqual({
			stdout: 'USERNAME:jdoe\nPASSWORD:[redacted]\r\n  API_TOKEN:[redacted]\nSTATUS:SUCCESS',
			list: ['DB_SECRET:[redacted]', 'SSH_PRIVATE_KEY:[redacted]'],
		});
	});
	it('leaves ordinary content alone and is idempotent', () => {
		const plain = {
			title: 'Reset your password: see the portal',
			note: 'Password:Xy7 lower-case key is prose',
			url: 'https://example.test/a:b',
			lines: 'USERNAME:jdoe\nSTATUS:SUCCESS',
			n: 1,
		};
		expect(scrubSecrets(plain)).toEqual(plain);
		const once = scrubSecrets({ password: 'x', stdout: 'PASSWORD:Xy7-not-real' });
		expect(scrubSecrets(once)).toEqual(once);
	});
});
