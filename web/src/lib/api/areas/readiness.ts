// The publish-refusal item mapping (`MissingItem` -> 409 `release_not_ready` blocking item), shared by `ReleaseError`.
import type { MissingItem, ReadinessItem } from '$lib/api/types/release';

/**
 * One item a publish refuses on (409 `release_not_ready`): a W5 slot without a deployment, an unready prod route, a
 * selection without the industry / department hierarchy (KH T1), or a required knowledge kind of a department with
 * nothing to resolve or nothing in the job's primary language (KH T8, D110/D115).
 */
export function blockingItem(m: MissingItem): ReadinessItem {
	if (m.dependency === 'selection')
		return { code: 'hierarchy_incomplete', subject: m.slot, message: m.detail, href: m.href };
	if (m.dependency === 'knowledge')
		return { code: 'knowledge_unbound', subject: m.slot, message: m.detail, href: m.href };
	if (m.dependency === 'routing') {
		const [op, env] = splitRoute(m.slot);
		return {
			code: 'route_not_ready',
			subject: m.slot,
			message: `${op} has no tested route on ${env}`,
			href: m.href,
		};
	}
	const slot = `${m.dependency}.${m.slot}`;
	return {
		code: 'slot_not_deployed',
		subject: slot,
		message: `No provider deployed for ${slot}`,
		href: `/admin/providers?tab=${m.dependency === 'strata_ingest' ? 'ingest' : 'knowledge'}`,
	};
}

/** `case.create{return}@prod` → [`case.create{return}`, `prod`]. */
export function splitRoute(key: string): [string, string] {
	const at = key.lastIndexOf('@');
	return at < 0 ? [key, ''] : [key.slice(0, at), key.slice(at + 1)];
}
