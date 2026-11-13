// Owner frames are published after each command's commit, so two commands close together (a Take, then a Resolve) can
// reach a client out of order. `since` is when the owner changed: a frame older than the owner already shown is stale.
import type { OwnerView } from '$lib/api/types/support';

/** `next` unless it is older than `cur` (equal times take `next`, the later arrival). */
export function newerOwner<T extends Pick<OwnerView, 'since'>>(cur: T | null | undefined, next: T): T {
	if (!cur) return next;
	const a = Date.parse(cur.since);
	const b = Date.parse(next.since);
	if (Number.isFinite(a) && Number.isFinite(b) && b < a) return cur;
	return next;
}
