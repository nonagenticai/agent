// Who sees which inbox item, and what the sidebar badges count. Pure, so the Inbox component, the shell and the unit
// tests share one rule (console redesign, Review Focus 3).
import type { AttentionItem } from '$lib/api/types/tenant';
import type { JobKey, Role } from '$lib/api/types/core';

/**
 * An item is shown to a caller whose EFFECTIVE roles (held ∩ view-as) include its `owner_role`, and to every
 * tenant admin. An `automatic` item has no role owner: only a tenant admin sees it.
 */
export function visibleItems(items: AttentionItem[], effectiveRoles: readonly Role[]): AttentionItem[] {
	if (effectiveRoles.includes('tenant_admin')) return items;
	return items.filter((i) => i.owner_role !== 'automatic' && effectiveRoles.includes(i.owner_role));
}

/** What a sidebar item shows: the number of things that need a person, and whether any is urgent. */
export interface Badge {
	count: number;
	urgent: boolean;
}

/** Items that need a person now or soon (blocking + action); info does not badge. */
function badgeOf(items: AttentionItem[]): Badge | null {
	const n = items.filter((i) => i.severity !== 'info');
	return n.length ? { count: n.length, urgent: n.some((i) => i.severity === 'blocking') } : null;
}

/**
 * Badges keyed by the sidebar href they belong to: the workspace home counts everything the caller can act on, an
 * agent's home counts that agent's items.
 */
export function badgesFor(
	items: AttentionItem[],
	effectiveRoles: readonly Role[],
	job: JobKey | null,
	homes: { workspace: string; agent: string | null },
): Record<string, Badge> {
	const mine = visibleItems(items, effectiveRoles);
	const out: Record<string, Badge> = {};
	const all = badgeOf(mine);
	if (all) out[homes.workspace] = all;
	if (job && homes.agent) {
		const one = badgeOf(mine.filter((i) => i.job === job));
		if (one) out[homes.agent] = one;
	}
	return out;
}
