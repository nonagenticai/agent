// J3 Readiness `?of=draft`: which board the URL names.
import { describe, expect, it } from 'vitest';
import { boardOf } from '../../src/lib/release/readiness-J3';

describe('boardOf', () => {
	it('reads draft / release from the query; anything else is the API default', () => {
		expect(boardOf(new URLSearchParams('of=draft'))).toBe('draft');
		expect(boardOf(new URLSearchParams('of=release'))).toBe('release');
		expect(boardOf(new URLSearchParams(''))).toBeUndefined();
		expect(boardOf(new URLSearchParams('of=bogus'))).toBeUndefined();
	});
});
