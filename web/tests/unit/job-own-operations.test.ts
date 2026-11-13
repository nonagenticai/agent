import { describe, expect, it } from 'vitest';
import { addableOwnOps, availableKey, levelLabel } from '../../src/lib/connectors/J6-routes';
import { pendingAreas } from '../../src/lib/release/pending-kinds';

describe('job own operations helpers', () => {
	it('offers only ops not yet added', () => {
		const a = [
			{ op: 'a.b', added: true },
			{ op: 'c.d', added: false },
		];
		expect(addableOwnOps(a).map((x) => x.op)).toEqual(['c.d']);
	});
	it('keys an available op by template, version and op', () => {
		expect(availableKey({ template: 'erp', version: '2', op: 'order.read' })).toBe('erp@2:order.read');
	});
	it('labels identity levels', () => {
		expect(levelLabel('2')).toBe('2 · Verified');
	});
	it('files job_operation under operations', () => {
		expect(pendingAreas(['job_operation', 'route'])).toEqual(['operations']);
	});
});
