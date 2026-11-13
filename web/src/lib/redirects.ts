// Old routes that folded into another page. A bookmarked URL lands on the right tab, not a 404 or a dead page.
import { jobPath } from '$lib/scope';

export const REDIRECTS: Record<string, string> = {
	'/admin/jobs/[job]/refinements': '/admin/jobs/[job]/release?tab=changes',
};

/** Where a route id redirects to for this job, or null when it does not redirect. */
export function redirectTarget(routeId: string, job: string): string | null {
	const to = REDIRECTS[routeId];
	if (!to) return null;
	return to.replace('/admin/jobs/[job]', jobPath(job));
}
