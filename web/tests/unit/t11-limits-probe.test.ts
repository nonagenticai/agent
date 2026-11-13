// T11: a template without a rate-limit model (server sends `{}`) must not crash the chain; probe_input_missing names its keys.
import { afterEach, describe, expect, it } from 'vitest';
import { cleanup, render } from '@testing-library/svelte';
import T11Chain from '../../src/routes/admin/connections/systems/[id]/T11Chain.svelte';
import {
	NO_RATE_LIMIT,
	addEnvIdLine,
	mergeProbeIds,
	missingProbeKeys,
	probeKeys,
	probeLabel,
	rateLimitText,
} from '../../src/lib/connectors/T11-limits';

afterEach(cleanup);

const detail = {
	instance: {
		base_url: 'https://x.example.com',
		env: 'test',
		stub: false,
		platform: false,
		status: 'ok',
		env_ids: {},
		overrides: [],
		template_key: 'k',
		template_version: '1',
		template_scope: 'platform',
		credential: { set: true, fingerprint: 'abcdef1234', updated_at: null },
	},
	template: { label: 'X', vendor: 'X', status: 'active', auth_kind: 'api_key', ops: [] },
	used_by: [],
	op_readiness: {},
	names: {},
};

describe('rate limit {}', () => {
	it('renders the chain with an empty rate_limit', () => {
		const insight = {
			ops: [],
			rate_limit: {},
			vendor_api_pin: null,
			webhooks: [],
			upgrade: null,
			taxonomy: [],
		};
		const { container } = render(T11Chain, {
			d: detail as never,
			rows: [],
			insight: insight as never,
		});
		expect(container.textContent).not.toContain('undefined');
	});
	it('words it', () => {
		expect(rateLimitText({})).toBe(NO_RATE_LIMIT);
		expect(rateLimitText({ kind: 'none', value: '', honour_retry_after: false })).toBe(NO_RATE_LIMIT);
		expect(rateLimitText({ kind: 'rps', value: '100', honour_retry_after: true })).toBe('rps 100');
	});
});

describe('probe_input_missing', () => {
	const note = 'Set the env id “probe.order_ref” on this system to a test identifier, then run again.';
	it('reads the key from the note', () => {
		expect(missingProbeKeys('probe_input_missing', note)).toEqual(['probe.order_ref']);
		expect(missingProbeKeys('timeout', note)).toEqual([]);
		expect(missingProbeKeys(null, note)).toEqual([]);
	});
	it('appends the key once', () => {
		expect(addEnvIdLine('', 'probe.order_ref')).toBe('probe.order_ref = ');
		expect(addEnvIdLine('a = 1\n', 'probe.order_ref')).toBe('a = 1\nprobe.order_ref = ');
		expect(addEnvIdLine('probe.order_ref = 5', 'probe.order_ref')).toBe('probe.order_ref = 5');
	});
});

describe('test records helpers', () => {
	it('unions asked-for keys with probe ids already set, without duplicates', () => {
		expect(
			probeKeys(['probe.case_id', 'probe.order_ref'], {
				'probe.case_id': 'CS-1',
				'probe.sku': 'A',
				shop: 'x',
			}),
		).toEqual(['probe.case_id', 'probe.order_ref', 'probe.sku']);
		expect(probeKeys([], { shop: 'x' })).toEqual([]);
	});
	it('humanises the field name', () => {
		expect(probeLabel('probe.case_id')).toBe('Case ID');
		expect(probeLabel('probe.order_ref')).toBe('Order ref');
		expect(probeLabel('probe.customer-email')).toBe('Customer email');
	});
	it('merge keeps non-probe ids, sets and clears probe ids', () => {
		expect(
			mergeProbeIds(
				{ shop: 'x', 'probe.old': '1', 'probe.keep': '2' },
				{ 'probe.case_id': ' CS-50001 ', 'probe.old': '', 'probe.keep': '3' },
			),
		).toEqual({ shop: 'x', 'probe.keep': '3', 'probe.case_id': 'CS-50001' });
	});
});
