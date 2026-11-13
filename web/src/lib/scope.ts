/**
 * Job-scoped admin URLs. Job pages live under `/admin/jobs/{job}/…`; the tenant pages (audit, providers, users,
 * knowledge stores, ingest jobs, connections, …) stay at `/admin/…`. The page registry is `$lib/pages`.
 */

const JOB_ROUTE_ID = '/admin/jobs/[job]';

/** The admin URL of `sub` (a job sub-path such as `/release`, optionally with more path or a query) in `job`. */
export function jobPath(job: string, sub = ''): string {
	return `/admin/jobs/${encodeURIComponent(job)}${sub}`;
}

/** The job a route acts in, when it is a job page (`/admin/jobs/[job]/…`); else null. */
export function routeJob(routeId: string | null, params: Record<string, string | undefined>): string | null {
	if (!routeId || !(routeId === JOB_ROUTE_ID || routeId.startsWith(`${JOB_ROUTE_ID}/`))) return null;
	return params.job || null;
}

/** The same page in another job (`/admin/jobs/a/ontology` → `/admin/jobs/b/ontology`); null off job pages. */
export function switchJobPath(pathname: string, job: string): string | null {
	const m = /^\/admin\/jobs\/[^/]+(\/.*)?$/.exec(pathname);
	if (!m) return null;
	return jobPath(job, m[1] ?? '');
}
