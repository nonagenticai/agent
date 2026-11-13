// KYC customer context (owner ruling 2026-10-26): the pure wording helpers.
import { describe, expect, it } from 'vitest';
import { focusLabel, itemsSummary, shortDate } from '../../src/lib/support/context';

describe('wording helpers', () => {
	it('formats dates and items', () => {
		expect(shortDate('2026-10-12T08:00:00Z', Date.parse('2026-10-26T00:00:00Z'))).toBe('12 Oct');
		expect(shortDate('2025-10-12T08:00:00Z', Date.parse('2026-10-26T00:00:00Z'))).toBe('12 Oct 2025');
		const o = {
			order_id: '1',
			status: 'x',
			placed_at: '2026-10-12T08:00:00Z',
			items: [
				{ name: 'Rug', qty: 1 },
				{ name: 'Lamp', qty: 2 },
				{ name: 'Vase', qty: 1 },
			],
		};
		expect(itemsSummary(o)).toBe('Rug, Lamp ×2 +1 more');
		expect(
			focusLabel(
				{ kind: 'order', id: '1', why: 'w' },
				{ orders: [o], payments: [], deliveries: [], cases: [] } as never,
				Date.parse('2026-10-26T00:00:00Z'),
			).label,
		).toBe('Order 12 Oct · Rug +2 more');
	});
});
