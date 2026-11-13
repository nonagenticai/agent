// P19 Platform audit — pure helpers shared by the fixture API and the page.
// Mental model (04 §9b K12/K44, 05 §grants): every audited action lands in exactly ONE log. The tenant's own audit
// if the tenant row exists and the event is not a pre-create failure; otherwise the platform log (this page):
// signups, licence-issue failures, failed imports, unresolved first-admin invites, key reseals, and platform
// catalogue writes (`agent_platform` writes audit into `platform_audit_events`).
import type { PlatformAuditEvent, PlatformAuditFamily, PlatformAuditSummary } from '$lib/api/types/platform';

export interface FamilyDef {
	key: PlatformAuditFamily;
	label: string;
	/** One line: what this family records. */
	blurb: string;
	/** Action prefixes (`signup` matches `signup.*`). */
	prefixes: string[];
}

export const FAMILIES: FamilyDef[] = [
	{
		key: 'signup',
		label: 'Signups',
		blurb: 'Early-access requests and your approve / reject decisions',
		prefixes: ['signup'],
	},
	{
		key: 'creation',
		label: 'Tenant creation',
		blurb: 'Imports, and create attempts that failed before a tenant row existed',
		prefixes: ['tenant.import', 'tenant.create', 'invite.unresolved', 'license.issue_failed'],
	},
	{
		key: 'keys',
		label: 'Licence keys',
		blurb: 'Re-sealing tenant tokens after a key rotation (counts only)',
		prefixes: ['license.reseal'],
	},
	{
		key: 'catalogue',
		label: 'Platform catalogue',
		blurb: 'Standard dimension values and layer tags every tenant builds on',
		prefixes: ['dimension', 'layer', 'template', 'business_model', 'industry', 'department'],
	},
];

const matches = (action: string, prefix: string) => action === prefix || action.startsWith(`${prefix}.`);

export function familyOf(action: string): PlatformAuditFamily {
	return FAMILIES.find((f) => f.prefixes.some((p) => matches(action, p)))?.key ?? 'other';
}

export function inFamily(action: string, family: string): boolean {
	return familyOf(action) === family;
}

/** Events that tell a story together share a subject name: "Northwind IT Services (first attempt)" and
 * "Northwind IT Services". Counts ("3 tenants") and catalogue writes are not threads. */
export function threadKey(e: PlatformAuditEvent): string | null {
	const fam = familyOf(e.action);
	if (fam !== 'signup' && fam !== 'creation') return null;
	return e.subject_label
		.replace(/\s*\(.*\)\s*$/, '')
		.trim()
		.toLowerCase();
}

/** Actions after which the subject IS a tenant. */
const CREATES = new Set(['tenant.import', 'tenant.create', 'signup.approve']);

/** For a failed event: was it followed (later, same subject) by a success that created the tenant? */
export function outcomeOf(e: PlatformAuditEvent, all: PlatformAuditEvent[]): PlatformAuditEvent['outcome'] {
	if (e.status !== 'failed') return undefined;
	const key = threadKey(e);
	const fix = key
		? all
				.filter((x) => x.status === 'ok' && CREATES.has(x.action) && threadKey(x) === key && x.at > e.at)
				.sort((a, b) => a.at.localeCompare(b.at))[0]
		: undefined;
	return fix
		? { state: 'resolved', by_id: fix.id, by_action: fix.action, by_at: fix.at }
		: { state: 'open', by_id: null, by_action: null, by_at: null };
}

/** Other events in the same thread, oldest first. */
export function relatedOf(e: PlatformAuditEvent, all: PlatformAuditEvent[]): PlatformAuditEvent[] {
	const key = threadKey(e);
	if (!key) return [];
	return all.filter((x) => x.id !== e.id && threadKey(x) === key).sort((a, b) => a.at.localeCompare(b.at));
}

const str = (v: unknown) => (v == null ? '' : String(v));

/** What happened, in one plain sentence (ids and codes only — the details never hold PII). */
export function sentence(e: PlatformAuditEvent): string {
	const d = e.details;
	const who = e.subject_label;
	switch (e.action) {
		case 'signup.intake':
			return `New early-access request from ${who}${d.source ? ` (via ${str(d.source)})` : ''}.`;
		case 'signup.approve':
			return `Approved ${who}'s request; the tenant was created.`;
		case 'signup.reject':
			return `Rejected ${who}'s request${d.reason ? `: ${str(d.reason)}` : ''}.`;
		case 'license.issue_failed':
			return `Could not issue a licence for ${who}${d.upstream_status ? ` (licences answered ${str(d.upstream_status)})` : ''}. Nothing was created.`;
		case 'license.reseal':
			return `Re-sealed ${str(d.resealed)} tenant token${d.resealed === 1 ? '' : 's'} to key ${str(d.kid)}; ${str(d.remaining)} left on older keys.`;
		case 'tenant.import':
			return e.status === 'ok'
				? `Imported ${who}, keeping its existing tenant id.`
				: `Import of ${who} refused (${str(d.code)}). No tenant row was written.`;
		case 'tenant.create':
			return e.status === 'ok' ? `Created ${who}.` : `Create of ${who} failed (${str(d.code)}).`;
		case 'tenant.attachment_quota':
			return d.quota_bytes == null
				? `Reset ${who}'s attachment quota to the platform default.`
				: `Set ${who}'s attachment quota to ${Math.round(Number(d.quota_bytes) / 1048576)} MB.`;
		case 'invite.unresolved':
			return `The first admin for ${who} matched no account (${str(d.matches)} matches), so the tenant was not created.`;
		case 'dimension.value_add':
			return `Added the standard value “${str(d.value)}” to ${str(d.dimension)}. Values are insert-only.`;
		case 'layer.tag':
			return `Tagged ${str(d.tag)}; jobs pick it up at their next release cut.`;
		default:
			return `${e.action} · ${who}`;
	}
}

/** What to do about an OPEN failure, and where. */
export function nextStep(e: PlatformAuditEvent): { text: string; href: string; link: string } {
	switch (e.action) {
		case 'license.issue_failed':
			return {
				text: 'Create the tenant again once licences answers. The first-admin slot was given back.',
				href: '/platform/tenants',
				link: 'Tenants',
			};
		case 'invite.unresolved':
			return {
				text: "Confirm the first admin's address with the customer, then create again.",
				href: '/platform/tenants',
				link: 'Tenants',
			};
		case 'tenant.import':
			return { text: 'Import again with the full tenant uuid.', href: '/platform/tenants', link: 'Tenants' };
		default:
			return { text: 'Look at the subject and retry.', href: '/platform/tenants', link: 'Tenants' };
	}
}

/** The object the event changed (or, for a failure, where the retry happens). */
export function objectLink(e: PlatformAuditEvent): { href: string; label: string } {
	const fam = familyOf(e.action);
	if (e.subject_tenant) return { href: `/platform/tenants/${e.subject_tenant}`, label: e.subject_label };
	if (fam === 'signup') return { href: '/platform/signups', label: 'Signup queue' };
	if (e.action === 'license.reseal') return { href: '/platform/licensing', label: 'Licensing & keys' };
	if (e.action.startsWith('dimension')) return { href: '/platform/dimensions', label: 'Dimensions' };
	if (e.action.startsWith('layer')) return { href: '/platform/layers', label: 'Layers' };
	return { href: '/platform/tenants', label: 'Tenants' };
}

/** Group newest-first events by UTC day. */
export function byDay(events: PlatformAuditEvent[]): { day: string; events: PlatformAuditEvent[] }[] {
	const out: { day: string; events: PlatformAuditEvent[] }[] = [];
	for (const e of events) {
		const day = e.at.slice(0, 10);
		const last = out[out.length - 1];
		if (last && last.day === day) last.events.push(e);
		else out.push({ day, events: [e] });
	}
	return out;
}

export function summarize(all: PlatformAuditEvent[]): PlatformAuditSummary {
	const families = [...FAMILIES.map((f) => f.key), 'other' as const]
		.map((family) => {
			const rows = all.filter((e) => familyOf(e.action) === family);
			return {
				family,
				count: rows.length,
				failed: rows.filter((e) => e.status === 'failed').length,
				open: rows.filter((e) => e.outcome?.state === 'open').length,
			};
		})
		.filter((f) => f.family !== 'other' || f.count > 0);
	const actors = new Map<string, PlatformAuditSummary['actors'][number]>();
	for (const e of all) {
		const a = actors.get(e.actor_id) ?? {
			actor_id: e.actor_id,
			actor_name: e.actor_name ?? null,
			actor_type: e.actor_type,
			count: 0,
		};
		a.count++;
		actors.set(e.actor_id, a);
	}
	return {
		total: all.length,
		failed: all.filter((e) => e.status === 'failed').length,
		open: all.filter((e) => e.outcome?.state === 'open'),
		families,
		actors: [...actors.values()].sort((a, b) => b.count - a.count),
		actions: [...new Set(all.map((e) => e.action))].sort(),
	};
}
