import { describe, expect, it } from 'vitest';
import { deriveRoleGrants, grantsFromExpectations } from '$lib/ontology/roleGrants';

const ops = (r: ReturnType<typeof deriveRoleGrants>) => r.map((g) => `${g.object}.${g.verb}`);

describe('D119: agent-role grants derive from the job’s expectations', () => {
	const expected = ['order.list', 'order.cancel', 'knowledge.search', 'unknown.read', 'order.list'];

	it('the qualifier gets the expected observe operations, never knowledge.search', () => {
		expect(ops(deriveRoleGrants('qualifier', 'R', expected))).toEqual(['order.list']);
	});

	it('the post-gate keeps knowledge.search first, then the observe operations', () => {
		const post = deriveRoleGrants('post_gate', 'R', expected);
		expect(ops(post)).toEqual(['knowledge.search', 'order.list']);
		expect(post.every((g) => g.derived && !g.derive_from_activity && g.modalities[0] === 'observe')).toBe(
			true,
		);
		expect(grantsFromExpectations(post)).toBe(true);
	});

	it('other roles derive nothing; a hand grant is not derived', () => {
		expect(deriveRoleGrants('procedure', 'P', expected)).toEqual([]);
		expect(deriveRoleGrants('assistant', 'W', expected)).toEqual([]);
		expect(grantsFromExpectations([])).toBe(false);
	});
});
