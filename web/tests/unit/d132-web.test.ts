// D132 (web half): the create drawer sends profile / region / languages, a null profile is pre-selected with the
// D132.1 default, the selection editor is reachable without an error, the company page carries languages.
import { describe, expect, it } from 'vitest';
import { defaultProfile, profileChoices } from '../../src/lib/tenant/profile-default';

describe('D132.1 default profile', () => {
	const ps = [
		{ key: 'a', label: 'A', industry: 'retail' },
		{ key: 'b', label: 'B', industry: 'utilities' },
		{ key: 'c', label: 'C', industry: null },
		{ key: 'd', label: 'D', industry: 'utilities' },
	];
	it('the only profile of the industry, else the only industry-less, else none', () => {
		expect(defaultProfile(ps, 'retail')).toBe('a');
		expect(defaultProfile(ps, 'utilities')).toBeNull();
		expect(defaultProfile(ps, 'telecom')).toBe('c');
		expect(defaultProfile(ps.slice(0, 2), 'telecom')).toBeNull();
		expect(profileChoices(ps, 'utilities').map((p) => p.key)).toEqual(['b', 'c', 'd']);
	});
});
