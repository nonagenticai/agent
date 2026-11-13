// T4 tenant Audit log — pure helpers shared by the fixture API and the page.
// Mental model (04 §9b K12/K44, 05 §4 K41, 12 §9): ONE append-only log per tenant. Every row names who acted
// (a person by their sub, an AI agent role, a system client — and the platform owner when they act in this tenant
// after `tenant.switch`), what they did, and the object it touched. Releases always write two rows
// (`release.approve` = the cut, `release.apply` = goes live). Failed rows stay "open" until a later success on the
// same object resolves them. Details hold ids and codes only (K50), never message text or secrets.
import type { AuditEvent, AuditFamily, AuditOutcome, AuditSummary } from '$lib/api/types/tenant';
import { ROLE_KEYS, rolesText, type Role } from '$lib/api/types/core';

export interface FamilyDef {
	key: AuditFamily;
	label: string;
	/** One line: what this family records. */
	blurb: string;
	/** Action prefixes (`user` matches `user.*`). */
	prefixes: string[];
}

export const FAMILIES: FamilyDef[] = [
	{
		key: 'access',
		label: 'People & access',
		blurb: 'Members invited, added, re-roled, disabled; the platform owner stepping in',
		prefixes: ['user', 'tenant'],
	},
	{
		key: 'releases',
		label: 'Releases & refinements',
		blurb: 'Edits to the job, release cuts (approve) and go-lives (apply), test chats',
		prefixes: ['release', 'refinement', 'job', 'chat'],
	},
	{
		key: 'connections',
		label: 'Systems & providers',
		blurb: 'Connected systems, LLM providers, keys and provider slots',
		prefixes: ['connection', 'provider', 'provider_slot', 'channel'],
	},
	{
		key: 'knowledge',
		label: 'Knowledge',
		blurb: 'Knowledge stores, documents and ingest jobs',
		prefixes: ['knowledge_store', 'knowledge_document', 'ingest', 'source'],
	},
	{
		key: 'platform',
		label: 'Provisioning & licence',
		blurb: 'Set-up work the system does for you, and licence-gate refusals',
		prefixes: ['provision', 'licence', 'license'],
	},
];

const matches = (action: string, prefix: string) => action === prefix || action.startsWith(`${prefix}.`);

export function familyOf(action: string): AuditFamily {
	if (action === 'execute') return 'calls';
	return FAMILIES.find((f) => f.prefixes.some((p) => matches(action, p)))?.key ?? 'other';
}

export const isCall = (e: Pick<AuditEvent, 'action'>) => e.action === 'execute';

/** Failure-like statuses. `denied` is policy working as designed, not a failure to fix. */
const FAILED = new Set<AuditEvent['status']>(['failure', 'error']);

/**
 * For a failed row: did a later success on the same object resolve it? Agent calls only get an outcome when the
 * runtime raised an alert (an unknown outcome on a money-moving call) — those stay open until someone checks.
 */
export function outcomeOf(e: AuditEvent, all: AuditEvent[]): AuditOutcome | undefined {
	if (!FAILED.has(e.status)) return undefined;
	if (isCall(e)) {
		return e.details.alert === true
			? { state: 'open', by_id: null, by_action: null, by_at: null }
			: undefined;
	}
	const fix = all
		.filter((x) => x.status === 'success' && !isCall(x) && x.resource_id === e.resource_id && x.at > e.at)
		.sort((a, b) => a.at.localeCompare(b.at))[0];
	return fix
		? { state: 'resolved', by_id: fix.id, by_action: fix.action, by_at: fix.at }
		: { state: 'open', by_id: null, by_action: null, by_at: null };
}

/** For `release.approve` / `release.apply`: the other half of the pair (K41 — two events always). */
export function releasePartner(r: AuditEvent, rows: AuditEvent[]): AuditEvent | null {
	if (r.action !== 'release.approve' && r.action !== 'release.apply') return null;
	const other = r.action === 'release.approve' ? 'release.apply' : 'release.approve';
	return rows.find((x) => x.action === other && x.resource_id === r.resource_id) ?? null;
}

const str = (v: unknown) => (v == null ? '' : Array.isArray(v) ? v.join(', ') : String(v));
/** A D88 role set as labels (“Job designer, Support agent”), “Observer” when empty. */
const roleSet = (v: unknown) =>
	rolesText(
		(Array.isArray(v) ? v : []).filter((r): r is Role => (ROLE_KEYS as readonly unknown[]).includes(r)),
	);
const obj = (e: AuditEvent) => e.resource_label || e.resource_id;

/** What happened, in one plain sentence — without the actor (the page puts the actor's name in front). */
export function sentence(e: AuditEvent): string {
	const d = e.details;
	const o = obj(e);
	switch (e.action) {
		case 'tenant.create':
			return `created this tenant${d.first_admin_name || d.first_admin ? ` with ${str(d.first_admin_name || d.first_admin)} as first admin` : ''}.`;
		case 'tenant.switch':
			return `stepped into this tenant from the platform${d.reason ? ` (${str(d.reason)})` : ''}. What they do next is listed under their name.`;
		case 'user.invite':
			return `invited ${o} as ${roleSet(d.roles)}${d.email_domain ? ` (@${str(d.email_domain)})` : ''}.`;
		case 'user.add':
			return `added ${o} as ${roleSet(d.roles)}.`;
		case 'user.role':
			return `changed ${o}'s roles from ${roleSet(d.from)} to ${roleSet(d.to)}.`;
		case 'user.view_as.start':
			return `started viewing as ${roleSet(d.roles)}${d.expires_at ? ` until ${str(d.expires_at).slice(11, 16)} UTC` : ''}. Rows they write meanwhile carry the narrowed roles.`;
		case 'user.view_as.end':
			return `stopped viewing as fewer roles${d.reason === 'replaced' ? ' (replaced by a new view)' : ''}.`;
		case 'user.disable':
			return `disabled ${o}${d.reason ? ` (${str(d.reason)})` : ''}. They can no longer sign in here.`;
		case 'user.enable':
			return `re-enabled ${o}.`;
		case 'release.approve':
			return `cut release ${o} (hash ${str(d.hash)}). Frozen, not live yet.`;
		case 'release.apply':
			return e.status === 'success'
				? `put release ${o} live${d.previous ? `, replacing ${str(d.previous)}` : ' (first release)'}.`
				: `tried to put release ${o} live; refused (${str(d.code)}${d.missing ? `: ${str(d.missing)}` : ''}). The previous release stays live.`;
		case 'refinement.save':
			return `saved a refinement to ${o}${d.draft_rev ? ` (revision ${str(d.draft_rev)})` : ''} to the draft — live in Production only once tested and published in a release.`;
		case 'job.target':
			return `changed which templates ${o} builds on (${str(d.selection)}) in the draft — live in Production only once tested and published in a release.`;
		case 'chat.test':
			return `ran a test chat ${o} in ${str(d.env) || 'test'}.`;
		case 'connection.test':
			return e.status === 'success'
				? `tested the connection to ${o}: it answered${d.latency_ms ? ` in ${str(d.latency_ms)} ms` : ''}.`
				: `tested the connection to ${o}: it failed (${str(d.code)}).`;
		case 'connection.update':
			return `changed ${o} (${str(d.fields)})${d.secret_changed ? '; the stored key was replaced' : ''}.`;
		case 'provider.create':
			return `added LLM provider ${d.name ? str(d.name) : o}.`;
		case 'provider.update':
			return `changed LLM provider ${d.name ? str(d.name) : o}${d.fields ? ` (${str(d.fields)})` : ''}${d.secret_changed ? '; the API key was replaced' : ''}.`;
		case 'provider.delete':
			return `removed LLM provider ${d.name ? str(d.name) : o}.`;
		case 'provider.bind':
			return `pointed ${o} at model ${str(d.model)}.`;
		case 'provider_slot.save':
			return `saved provider slot ${o}.`;
		case 'provider_slot.adopt_remote':
			return `adopted the remote version of provider slot ${o}.`;
		case 'knowledge_store.create':
			return `created knowledge store ${o}.`;
		case 'knowledge_store.delete':
			return `deleted knowledge store ${o}.`;
		case 'knowledge_document.delete':
			return `deleted a document from ${str(d.store) || 'a store'}.`;
		case 'ingest.upload':
			return `uploaded ${str(d.file) || 'a file'} into ${str(d.store) || 'a store'}.`;
		case 'ingest.failed':
			return `could not ingest ${o} (${str(d.code)}${d.slot ? `: no ${str(d.slot)} slot` : ''}).`;
		case 'ingest.retried':
			return `retried ingest job ${o}${d.outcome ? `: ${str(d.outcome)}` : ''}.`;
		case 'ingest.cancelled':
			return `cancelled ingest job ${o}.`;
		case 'provision.succeeded':
			return `finished set-up step ${o}.`;
		case 'provision.enqueued':
			return `queued set-up step ${o}.`;
		case 'provision.retried':
			return `retried set-up step ${o}.`;
		case 'provision.cancelled':
			return `cancelled set-up step ${o}.`;
		case 'provision.failed':
			return `could not finish set-up step ${o} (${str(d.code)}${d.http_status ? `, HTTP ${str(d.http_status)}` : ''}). It retries on its own until it gives up.`;
		case 'job.create':
			return `created job ${o}${d.template ? ` from template ${str(d.template)}` : ''}.`;
		case 'provision.drift':
			return `found that ${o} was changed outside this app (local v${str(d.local_version)}, remote v${str(d.remote_version)}).`;
		case 'licence.gate_refused':
			return `refused ${str(d.conversations) || 'a'} conversation(s) on ${o}: ${str(d.reason)}. Logged at most once an hour.`;
		case 'execute': {
			const c = d.case_id ? ` for case ${str(d.case_id)}` : '';
			if (e.status === 'success')
				return `called ${o}${c}${d.instance_id ? ` via ${str(d.instance_id)}` : ''}${d.latency_ms ? ` · ${str(d.latency_ms)} ms` : ''}.`;
			if (e.status === 'denied')
				return `was not allowed to call ${o}${c} (${str(d.outcome)}${d.rule ? `, rule ${str(d.rule)}` : ''}).`;
			if (e.status === 'fallback')
				return `called ${o}${c}; the system was unavailable, so it used ${str(d.fallback)} instead.`;
			return `called ${o}${c} and it failed (${str(d.outcome)}).`;
		}
		default:
			return `${e.action} on ${e.resource_type} ${o}.`;
	}
}

/** What to do about an OPEN failure, and where. */
export function nextStep(e: AuditEvent): { text: string; href: string; link: string } {
	const job = e.job ?? '';
	switch (e.action) {
		case 'release.apply':
			return {
				text: 'Fix what readiness lists, then apply the release again.',
				href: `/admin/jobs/${job}/readiness`,
				link: 'Readiness',
			};
		case 'ingest.failed':
			return {
				text: `Configure the ${str(e.details.slot) || 'missing'} slot, then retry the ingest job.`,
				href: '/admin/providers',
				link: 'Providers',
			};
		case 'provision.failed':
			return {
				text: 'Look at the failing step on the overview; fix its input (usually a provider slot) and retry it.',
				href: '/admin',
				link: 'Tenant overview',
			};
		case 'provision.drift':
			return {
				text: 'Adopt the remote version, or save yours again to overwrite it.',
				href: '/admin/providers',
				link: 'Providers',
			};
		case 'connection.test':
			return {
				text: 'Check the stored key and base URL, then test again.',
				href: `/admin/connections/systems/${e.resource_id}`,
				link: 'System',
			};
		case 'execute':
			return {
				text: 'The agent cannot tell whether this went through. Check it in the source system; the case was handed to a person.',
				href: `/admin/jobs/${job}/cases/${str(e.details.case_id)}`,
				link: `Case ${str(e.details.case_id)}`,
			};
		default:
			return { text: 'Open the object and retry.', href: objectLink(e).href, link: objectLink(e).label };
	}
}

/** The object the event touched. */
export function objectLink(e: AuditEvent): { href: string; label: string } {
	const job = e.job ?? '';
	switch (e.resource_type) {
		case 'release':
			return { href: `/admin/jobs/${job}/release/${e.resource_id}`, label: `Release ${e.resource_id}` };
		case 'refinement':
			return { href: `/admin/jobs/${job}/refinements`, label: 'Refinements' };
		case 'job':
			return { href: `/admin/jobs/${e.resource_id}`, label: 'Job' };
		case 'user':
			return { href: '/admin/users', label: 'Users' };
		case 'tenant':
		case 'provision':
			return { href: '/admin', label: 'Tenant overview' };
		case 'llm_provider':
		case 'llm_use':
		case 'provider_slot':
			return { href: '/admin/providers', label: 'Providers' };
		case 'knowledge_store':
		case 'knowledge_document':
			return { href: '/admin/knowledge/stores', label: 'Knowledge stores' };
		case 'ingest_job':
			return { href: '/admin/knowledge/jobs', label: 'Ingest jobs' };
		case 'connector_instance':
			return { href: `/admin/connections/systems/${e.resource_id}`, label: `System ${e.resource_id}` };
		case 'channel':
			return { href: '/admin/channels', label: 'Channels' };
		case 'operation':
		case 'conversation': {
			const id = str(e.details.case_id) || (e.resource_type === 'conversation' ? e.resource_id : '');
			return id
				? { href: `/admin/jobs/${job}/cases/${id}`, label: `Case ${id}` }
				: { href: `/admin/jobs/${job}/operation-calls`, label: 'Operation calls' };
		}
		default:
			return { href: '/admin', label: 'Tenant overview' };
	}
}

/** Group newest-first events by UTC day. */
export function byDay(events: AuditEvent[]): { day: string; events: AuditEvent[] }[] {
	const out: { day: string; events: AuditEvent[] }[] = [];
	for (const e of events) {
		const day = e.at.slice(0, 10);
		const last = out[out.length - 1];
		if (last && last.day === day) last.events.push(e);
		else out.push({ day, events: [e] });
	}
	return out;
}

/** Whole-log summary (all rows, not one page): who acts, what kinds of events, what is still open. */
export function summarize(all: AuditEvent[]): AuditSummary {
	const changes = all.filter((e) => !isCall(e));
	const calls = all.filter(isCall);
	const families = FAMILIES.map((f) => {
		const rows = changes.filter((e) => familyOf(e.action) === f.key);
		return {
			family: f.key,
			count: rows.length,
			open: rows.filter((e) => e.outcome?.state === 'open').length,
		};
	});
	const actors = new Map<string, AuditSummary['actors'][number]>();
	for (const e of all) {
		const a = actors.get(e.actor_id) ?? {
			actor_id: e.actor_id,
			actor_name: e.actor_name ?? null,
			actor_type: e.actor_type,
			platform: !!e.platform_actor,
			count: 0,
		};
		a.count++;
		actors.set(e.actor_id, a);
	}
	const by = (s: AuditEvent['status']) => calls.filter((e) => e.status === s).length;
	return {
		total: all.length,
		changes: changes.length,
		calls: {
			total: calls.length,
			success: by('success'),
			denied: by('denied'),
			error: by('error') + by('failure'),
			fallback: by('fallback'),
		},
		open: all.filter((e) => e.outcome?.state === 'open').sort((a, b) => b.at.localeCompare(a.at)),
		families,
		actors: [...actors.values()].sort((a, b) => b.count - a.count),
	};
}
