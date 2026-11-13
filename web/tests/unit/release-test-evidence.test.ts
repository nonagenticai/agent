import { describe, expect, it } from 'vitest';
import type { TestCase } from '../../src/lib/api/types/live';
import { testEvidence } from '../../src/lib/release/pipeline-J14';

describe('test evidence', () => {
	it('the selection and Production routes are proven by the go-live checklist, not by a test chat', () => {
		const change = (id: string, key: string) =>
			({ id, key, label: key, in_release: false }) as unknown as Parameters<typeof testEvidence>[0][number];
		const drafts = [change('d-1', 'selection'), change('d-2', 'route:order.read:prod')];
		const ev = testEvidence(drafts, null, []);
		expect(ev.counts).toEqual({ tested: 0, stale: 0, not_tested: 0, checklist: 2 });
		expect(ev.state).toBe('tested');
		expect(ev.unexercised).toEqual([]);
		// a TEST route is what the test chat runs: nothing exercised it, so it still blocks
		const withTest = testEvidence([...drafts, change('d-3', 'route:order.read:test')], null, []);
		expect(withTest.state).toBe('not_tested');
		expect(withTest.unexercised.map((d) => d.key)).toEqual(['route:order.read:test']);
		const tc = { id: 'TC-1', exercised: ['route:order.read:test'], stale: false } as unknown as TestCase;
		expect(testEvidence([...drafts, change('d-3', 'route:order.read:test')], null, [tc]).state).toBe(
			'tested',
		);
	});
});
