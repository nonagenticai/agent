// P3 Signups: the funnel and intake-signal helpers.
import { describe, expect, it } from 'vitest';
import { funnel, orderedSignals, riskCount, waiting } from '../../src/lib/platform/signupsP3';
import type { Signup } from '../../src/lib/api/types/platform';

describe('P3 helpers', () => {
	it('orders risk before info before ok and counts risks', () => {
		const s = {
			signals: [
				{ key: 'turnstile', level: 'ok', label: 'a', detail: '' },
				{ key: 'duplicate', level: 'risk', label: 'b', detail: '' },
				{ key: 'detail', level: 'info', label: 'c', detail: '' },
			],
		} as unknown as Signup;
		expect(orderedSignals(s).map((g) => g.level)).toEqual(['risk', 'info', 'ok']);
		expect(riskCount(s)).toBe(1);
		expect(riskCount({} as Signup)).toBe(0);
	});

	it('waiting reads the server clock; funnel has no counts without a queue', () => {
		expect(waiting('2026-10-16T18:00:00Z', '2026-10-18T18:00:00Z')).toBe('2 d');
		expect(waiting('2026-10-16T18:00:00Z', undefined)).toBe('');
		expect(funnel(null).map((s) => s.count)).toEqual([null, null, null, null]);
	});
});
