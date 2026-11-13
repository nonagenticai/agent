// D59: a region change is not customer-visible in a release diff.
import { describe, expect, it } from 'vitest';
import { customerImpact } from '../../src/lib/release/J15-detail';

describe('D59 · region selects nothing', () => {
	it('a region change is not customer-visible in a release diff', () => {
		const sec = (regions: string[]) => [
			{ key: 'channels', label: 'Channels', items: ['telegram'] },
			{ key: 'regions', label: 'Region', items: regions },
		];
		expect(customerImpact(sec(['nl', 'be', 'de']), sec(['nl']))).toEqual([]);
	});
});
