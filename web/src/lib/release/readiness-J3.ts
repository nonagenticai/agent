// J3 Readiness board — display helpers only (D67: the backend owns readiness; `readinessBoard()` /
// `releaseReadiness()` compute it, see `$lib/api/areas/readiness`). Tested in tests/unit/release-J3.test.ts.
import type { ReadinessBoard, ReadinessGap } from '$lib/api/types/release';

export const FIXER_LABEL: Record<ReadinessGap['fixer'], string> = {
	tenant_admin: 'Tenant admin',
	super_admin: 'Platform owner',
};

/** The headline the page leads with. */
export function verdictHeadline(b: ReadinessBoard): { title: string; body: string } {
	const apply = b.gaps.filter((g) => g.blocks === 'apply').length;
	const live = b.gaps.filter((g) => g.blocks === 'go_live').length;
	const testing = b.gaps.filter((g) => g.blocks === 'testing').length;
	const plural = (n: number, w: string) => `${n} ${w}${n === 1 ? '' : 's'}`;
	switch (b.verdict) {
		case 'no_release':
			return {
				title: 'No release to check yet',
				body: `Nothing has been cut for this job. The checks below run against your current selection, so you can close ${plural(apply + live, 'gap')} before the first release.${testNote(testing)}`,
			};
		case 'blocked':
			return {
				title: `${b.release!.id} cannot be applied: ${plural(apply, 'gap')}`,
				body: `Apply re-runs these checks and refuses with 409 release_not_ready until they are fixed.${live ? ` ${plural(live, 'more item')} would not stop apply, but customers would meet ${live === 1 ? 'it' : 'them'}.` : ''}${testNote(testing)}`,
			};
		case 'go_live_gaps':
			return {
				title: `${b.release!.id} can be applied, with ${plural(live, 'gap')}`,
				body: `Apply would pass. The agent would then answer customers with these holes, so close them first.${testNote(testing)}`,
			};
		default:
			return b.release!.id === b.current_id
				? {
						title: `${b.release!.id} is live and complete`,
						body: `Every check passes for the current release.${testNote(testing)}`,
					}
				: {
						title: `${b.release!.id} is ready to apply`,
						body: `Every check passes. Apply it on the Release page.${testNote(testing)}`,
					};
	}
}

/** The Test-env warning sentence (never part of the verdict). */
function testNote(n: number): string {
	return n
		? ` The Test env misses ${n} route${n === 1 ? '' : 's'}: that limits testing the draft, not publishing.`
		: '';
}

/** The design the J3 page checks, from its URL (`/readiness?of=draft`): `'draft'` or `'release'` when asked, else
 * `undefined` (the API's default, the release). Passed through to `readinessBoard({of})`. */
export function boardOf(q: URLSearchParams): 'draft' | 'release' | undefined {
	const of = q.get('of');
	return of === 'draft' || of === 'release' ? of : undefined;
}
