// B8 web gaps (A1): the P8 index at /platform/journeys lists every industry with status and red counts from the
// seeded findings, the not-started industries (linking to a generator run), and the B8 pages are live pages.
import { describe, expect, it } from 'vitest';
import type { IndustryJourneySummary } from '../../src/lib/api/types/journey';
import { groupJourneys, journeyHref } from '../../src/routes/platform/journeys/logic';
const row = (o: Partial<IndustryJourneySummary>): IndustryJourneySummary => ({
	industry: 'retail',
	label: 'Retail',
	status: 'tagged',
	tag: 'industry/retail@v1',
	activities: 3,
	red: 0,
	...o,
});

describe('groupJourneys', () => {
	it('groups department grids under their industry; the least complete status and the red sum win', () => {
		const g = groupJourneys([
			row({
				industry: 'telco',
				label: 'Telco',
				department: 'customer_service',
				tag: null,
				status: 'draft',
				red: 2,
			}),
			row({
				industry: 'telco',
				label: 'Telco',
				department: 'sales',
				status: 'tagged',
				red: 1,
				activities: 5,
			}),
			row({}),
			row({ industry: 'energy', label: 'Energy', status: 'not_started', tag: null, activities: 0 }),
		]);
		expect(g.started.map((i) => i.industry)).toEqual(['telco', 'retail']);
		const telco = g.started[0]!;
		expect(telco.status).toBe('draft');
		expect(telco.red).toBe(3);
		expect(telco.activities).toBe(8);
		expect(telco.grids.map((d) => d.label)).toEqual(['customer service', 'sales']);
		expect(telco.grids[1]!.href).toBe('/platform/journeys/telco?department=sales');
		expect(g.notStarted).toEqual([
			{ industry: 'energy', label: 'Energy', href: '/platform/generator?new=energy' },
		]);
		expect(g.totals).toEqual({ industries: 2, red: 3, notStarted: 1 });
	});

	it('a not-started row for an industry that has a grid does not list it twice', () => {
		const g = groupJourneys([row({}), row({ status: 'not_started', tag: null, department: 'sales' })]);
		expect(g.notStarted).toEqual([]);
		expect(journeyHref('it services', 'a b')).toBe('/platform/journeys/it%20services?department=a%20b');
	});
});
