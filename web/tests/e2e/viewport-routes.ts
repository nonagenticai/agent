// Every page URL of the registry on the REAL local platform stack, with every concrete id read from that stack at
// run time (the product's own GET routes, as the person who owns the page): the journey's job (plus a second job of the
// tenant when one exists), its tenant, a template layer tag, connector template, system, release, case, procedure,
// activity, customer, conversation, object, journey, generator run. A route whose object does not exist on this stack
// is returned in `skipped` with the reason (the spec writes it to the results JSON and annotates the test): never
// dropped silently. Shared by viewports.spec.ts.
import { PAGES, routesOf } from '../../src/lib/pages';
import { Api, J, type Who } from './stack';

/** The second job of the tenant the check also visits, when the stack has it. */
export const SECOND_JOB = 'b4_lib_policies';

export interface Target {
	id: string;
	area: string;
	route: string;
	url: string;
	job: string;
	who: Who;
}
export interface Skipped {
	id: string;
	route: string;
	job: string;
	reason: string;
}

/** Who a person would be to open a route: platform pages as the super admin, /support as a support agent, the rest as the tenant admin. */
export function whoOf(route: string): Who {
	if (route === '/platform' || route.startsWith('/platform/')) return 'sa';
	if (route === '/support' || route.startsWith('/support/')) return 'agent';
	return 'admin';
}

type Body = Record<string, unknown>;
const enc = encodeURIComponent;

/** First item of a list response (`{items}`, `{<key>}` or a bare array) with a note on why there is none. */
async function first(api: Api, path: string, key = 'items'): Promise<{ item?: Body; why?: string }> {
	const r = await api.get(path);
	if (r.status !== 200) return { why: `GET ${path} answered ${r.status}` };
	const b = r.body as Body | Body[] | null;
	const rows = Array.isArray(b) ? b : ((b?.[key] as Body[] | undefined) ?? []);
	return rows.length ? { item: rows[0]! } : { why: `GET ${path} lists nothing on this stack` };
}

/** Ids shared by every job: platform objects and tenant-wide ones. */
async function commonIds(sa: Api, admin: Api): Promise<Map<string, string | { skip: string }>> {
	const out = new Map<string, string | { skip: string }>();
	const put = (route: string, path: string | undefined, why?: string) =>
		out.set(route, path ?? { skip: why ?? 'not found' });

	put('/platform/tenants/[id]', `/platform/tenants/${enc(J().tenant)}`);

	const layers = await sa.get('/api/layers');
	const layer = (((layers.body as Body | null)?.items as Body[] | undefined) ?? []).find((l) =>
		/@v\d+$/.test(String(l.tag ?? '')),
	);
	put(
		'/platform/templates/[layer]/[v]',
		layer
			? `/platform/templates/${enc(String(layer.key).replaceAll('/', '.'))}/v${String(layer.tag).split('@v')[1]}`
			: undefined,
		`GET /api/layers (${layers.status}) lists no tagged layer`,
	);

	const j = await first(sa, '/api/platform/journeys');
	put(
		'/platform/journeys/[industry]',
		j.item ? `/platform/journeys/${enc(String(j.item.industry))}` : undefined,
		j.why,
	);

	const o = await first(sa, '/api/platform/objects', 'objects');
	put(
		'/platform/objects/[object]',
		o.item ? `/platform/objects/${enc(String(o.item.object))}` : undefined,
		o.why,
	);

	const ct = await first(sa, '/api/platform/connector-templates');
	put(
		'/platform/connector-templates/[key]/[v]',
		ct.item
			? `/platform/connector-templates/${enc(String(ct.item.key))}/${enc(String(ct.item.version))}`
			: undefined,
		ct.why,
	);

	const g = await first(sa, '/api/platform/generator/runs');
	put(
		'/platform/generator/[run]',
		g.item ? `/platform/generator/${enc(String(g.item.id))}` : undefined,
		g.why,
	);

	const s = await first(admin, '/api/connections/systems');
	put(
		'/admin/connections/systems/[id]',
		s.item ? `/admin/connections/systems/${enc(String(s.item.id))}` : undefined,
		s.why,
	);

	const p = await first(admin, '/api/parties');
	put('/admin/parties/[id]', p.item ? `/admin/parties/${enc(String(p.item.id))}` : undefined, p.why);
	return out;
}

/** Ids of one job (its journey, procedures, releases, cases and the conversation of a case). */
async function jobIds(admin: Api, job: string): Promise<Map<string, string | { skip: string }>> {
	const out = new Map<string, string | { skip: string }>();
	const base = `/admin/jobs/${enc(job)}`;
	const put = (route: string, path: string | undefined, why?: string) =>
		out.set(route, path ?? { skip: why ?? 'not found' });

	const jr = await admin.get(`/api/jobs/${enc(job)}/journey`);
	const act = (((jr.body as Body | null)?.activities as Body[] | undefined) ?? [])[0];
	put(
		'/admin/jobs/[job]/journey/[activity]',
		act ? `${base}/journey/${enc(String(act.key))}` : undefined,
		`GET /api/jobs/${job}/journey (${jr.status}) lists no activity`,
	);

	const pr = await first(admin, `/api/jobs/${enc(job)}/procedures`);
	put(
		'/admin/jobs/[job]/procedures/[name]',
		pr.item ? `${base}/procedures/${enc(String(pr.item.name))}` : undefined,
		pr.why,
	);

	const rel = await first(admin, `/api/jobs/${enc(job)}/releases`);
	put(
		'/admin/jobs/[job]/release/[id]',
		rel.item ? `${base}/release/${enc(String(rel.item.id))}` : undefined,
		rel.why,
	);

	const cs = await first(admin, `/api/jobs/${enc(job)}/cases`);
	put(
		'/admin/jobs/[job]/cases/[id]',
		cs.item ? `${base}/cases/${enc(String(cs.item.case_id))}` : undefined,
		cs.why,
	);
	const withConv = (
		(((await admin.get(`/api/jobs/${enc(job)}/cases`)).body as Body | null)?.items as Body[] | undefined) ??
		[]
	).find((c) => c.conversation_id);
	put(
		'/support/[job]/c/[id]',
		withConv ? `/support/${enc(job)}/c/${enc(String(withConv.conversation_id))}` : undefined,
		cs.why ?? `no case of ${job} has a conversation`,
	);
	return out;
}

/** The pages a viewport check visits: `targets` (resolved URLs, who opens each) and `skipped` (with reasons). */
export async function resolveViewportTargets(): Promise<{ targets: Target[]; skipped: Skipped[] }> {
	const sa = await Api.as('sa');
	const admin = await Api.as('admin');
	try {
		const jobs = [J().job];
		const all = await admin.get('/api/jobs');
		const keys = (((all.body as Body | null)?.items as Body[] | undefined) ?? []).map((x) => String(x.key));
		if (keys.includes(SECOND_JOB) && SECOND_JOB !== J().job) jobs.push(SECOND_JOB);

		const common = await commonIds(sa, admin);
		const perJob = new Map<string, Map<string, string | { skip: string }>>();
		for (const job of jobs) perJob.set(job, await jobIds(admin, job));

		const targets: Target[] = [];
		const skipped: Skipped[] = [];
		for (const p of PAGES)
			for (const r of routesOf(p)) {
				const jobScoped = r.route.includes('[job]');
				for (const job of jobScoped ? jobs : [jobs[0]!]) {
					let url: string | null = null;
					let why = '';
					const hasId = /\[(?!job\])/.test(r.route);
					if (hasId) {
						const hit = (jobScoped ? perJob.get(job)! : common).get(r.route) ?? common.get(r.route);
						if (typeof hit === 'string') url = hit;
						else why = hit ? hit.skip : 'no resolver for this route';
					} else {
						url = r.route.replaceAll('[job]', enc(job));
					}
					if (url && r.hash && !hasId) url = `${url}#${r.hash}`;
					if (!url) skipped.push({ id: p.id, route: r.route, job, reason: why });
					else if (!targets.some((t) => t.url === url))
						targets.push({ id: p.id, area: p.area, route: r.route, url, job, who: whoOf(r.route) });
				}
			}
		// identity first, so a chunk signs in once per identity
		targets.sort((a, b) => a.who.localeCompare(b.who));
		return { targets, skipped };
	} finally {
		await Promise.all([sa.dispose(), admin.dispose()]);
	}
}
