// J18 discovery (spec 11 §7, 10 §8, D54, D56): the labels, lifecycle steps and tally the discovery pages show. The
// world functions that build proposals live with the journey area (`$lib/api/areas/journey-J18`).
import type { DiscoveryProposal, DiscoveryProposalStatus } from '$lib/api/types/journey';

/** Tokens per classified row for the GA gate calls (fixture rate; D56: usage is counted in tokens, not priced). */
export const J18_USAGE = { classify_tokens_per_row: 90 };

export const J18_KIND_LABEL: Record<DiscoveryProposal['kind'], string> = {
	new_activity: 'New activity',
	enable_activity: 'Hidden activity',
	knowledge_gap: 'Missing knowledge',
	operation_route: 'Operation needed',
};

export const J18_SOURCE_LABEL: Record<DiscoveryProposal['evidence']['source'], string> = {
	import: 'Ticket export',
	live: 'Live conversations',
	shadow: 'Shadow traffic',
	knowledge_search: 'Knowledge searches',
};

/** Where an accepted proposal is in the D54 lifecycle, ordered (used by the per-card stepper). */
export const J18_LIFECYCLE: { key: DiscoveryProposalStatus; label: string }[] = [
	{ key: 'draft', label: 'Saved to draft' },
	{ key: 'in_release', label: 'In release' },
	{ key: 'live', label: 'Live' },
];

/** Counts per status group for the summary strip. */
export function tallyJ18(ps: DiscoveryProposal[]) {
	const n = (f: (p: DiscoveryProposal) => boolean) => ps.filter(f).length;
	return {
		open: n((p) => p.status === 'open'),
		draft: n((p) => p.status === 'draft'),
		in_release: n((p) => p.status === 'in_release'),
		live: n((p) => p.status === 'live'),
		rejected: n((p) => p.status === 'rejected' || p.status === 'discarded'),
	};
}
