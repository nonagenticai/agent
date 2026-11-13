// Breadcrumbs for the top bar: where you are, as workspace > agent > page (or Studio > page).
import { jobPath } from '$lib/scope';
import { scopeForRoute, titleForRoute } from '$lib/pages';

export interface Crumb {
	label: string;
	/** null = the current location (not a link). */
	href: string | null;
}

export function crumbsFor(
	routeId: string | null,
	tenantLabel: string,
	jobLabel: string | null,
	jobKey: string | null = null,
): Crumb[] {
	if (!routeId) return [];
	const title = titleForRoute(routeId);
	const scope = scopeForRoute(routeId);
	if (scope === 'studio') {
		const home = routeId === '/platform';
		return home
			? [{ label: 'Studio', href: null }]
			: [{ label: 'Studio', href: '/platform' }, ...(title ? [{ label: title, href: null }] : [])];
	}
	if (routeId === '/admin') return [{ label: tenantLabel, href: null }];
	const out: Crumb[] = [{ label: tenantLabel, href: '/admin' }];
	if (scope === 'agent' && jobLabel) {
		const home = routeId === '/admin/jobs/[job]';
		out.push({ label: jobLabel, href: home || !jobKey ? null : jobPath(jobKey) });
		if (home) return out;
	}
	if (title) out.push({ label: title, href: null });
	return out;
}
