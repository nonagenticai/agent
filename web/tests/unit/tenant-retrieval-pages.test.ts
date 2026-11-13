// The retrieval UI registry: the eval is a T7 route in the Knowledge nav.
import { describe, expect, it } from 'vitest';
import { navFor, pageForRoute } from '../../src/lib/pages';

describe('registry', () => {
	it('the eval is a T7 route in the Knowledge nav for any tenant member', () => {
		expect(pageForRoute('/admin/knowledge/eval')?.id).toBe('T7');
		const more = navFor(
			{ superAdmin: false, member: true, effectiveRoles: [] },
			'customer_care_agent',
			'workspace',
		).find((g) => g.group === 'more')!;
		expect(more.sections.flatMap((x) => x.items.map((i) => i.label))).toEqual([
			'Ingest jobs',
			'Retrieval eval',
			'Model changes',
			'Platform knowledge', // PK T9: tenant platform exclusions
			'Providers',
			'Connector templates',
		]);
	});
});
