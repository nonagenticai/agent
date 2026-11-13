// J15 Release detail: pure helpers that turn one immutable release (+ the release before it) into the story the page
// tells — what changed for customers, which tenant design changes it published (D54), which template upgrades it
// carried (D15), and its lifecycle (cut/approved → applied → live → superseded / rolled back).
import type { TemplatePin } from '$lib/api/types/core';
import type { AuditEvent } from '$lib/api/types/tenant';
import type { ReleaseWindow } from '$lib/api/types/runtime';
import type {
	BundleSection,
	DraftChange,
	PublishedChange,
	ReleaseDetail,
	ReleaseRow,
	ResolvedSelection,
	RollbackDirection,
} from '$lib/api/types/release';
import { parseContextSpaceKey } from '$lib/context-space-key';

const CHANNEL: Record<string, string> = { telegram: 'Telegram', web_chat: 'web chat' };

export type ImpactTone = 'add' | 'remove';
/** One line a customer would notice ("Customers can now reach the agent on Telegram"). */
export interface ImpactLine {
	section: string;
	tone: ImpactTone;
	text: string;
	/** The raw bundle item it comes from. */
	item: string;
}

/** The release applied (or cut) before `id`: the next older row in the newest-first list. */
export function previousOf(rows: ReleaseRow[], id: string): ReleaseRow | null {
	const i = rows.findIndex((r) => r.id === id);
	return i >= 0 ? (rows[i + 1] ?? null) : null;
}

/** The release cut after `id` (newer), if any. */
export function nextOf(rows: ReleaseRow[], id: string): ReleaseRow | null {
	const i = rows.findIndex((r) => r.id === id);
	return i > 0 ? (rows[i - 1] ?? null) : null;
}

/** Per-section adds/removes of `cur` against `prev` (no previous release = everything is new). */
export function bundleDiff(
	cur: BundleSection[],
	prev: BundleSection[] | null,
): { key: string; label: string; added: string[]; removed: string[]; kept: number }[] {
	return cur.map((s) => {
		const before = prev?.find((p) => p.key === s.key)?.items ?? [];
		const was = new Set(before);
		const now = new Set(s.items);
		return {
			key: s.key,
			label: s.label,
			added: s.items.filter((i) => !was.has(i)),
			removed: before.filter((i) => !now.has(i)),
			kept: s.items.filter((i) => was.has(i)).length,
		};
	});
}

const activityLabel = (item: string) => item.split(' — ')[1] ?? item;
const spaceLabel = (item: string) => {
	const p = parseContextSpaceKey(item);
	return p ? `${p.area} › ${p.slug}`.replace(/[-_]/g, ' ') : item;
};

/** The customer-visible part of a bundle diff, in plain words: channels, activities and context spaces. D59: region
 *  is a placeholder (recorded, selects nothing), so a region change is not customer-visible. */
export function customerImpact(cur: BundleSection[], prev: BundleSection[] | null): ImpactLine[] {
	if (!prev) return [];
	const out: ImpactLine[] = [];
	for (const d of bundleDiff(cur, prev)) {
		const line = (tone: ImpactTone, item: string, text: string) =>
			out.push({ section: d.key, tone, text, item });
		for (const i of d.added) {
			if (d.key === 'channels') line('add', i, `Customers can now reach the agent on ${CHANNEL[i] ?? i}`);
			else if (d.key === 'activities') line('add', i, `New request it handles: “${activityLabel(i)}”`);
			else if (d.key === 'context_spaces') line('add', i, `New topic it knows: ${spaceLabel(i)}`);
		}
		for (const i of d.removed) {
			if (d.key === 'channels') line('remove', i, `No longer answers on ${CHANNEL[i] ?? i}`);
			else if (d.key === 'activities') line('remove', i, `No longer handles “${activityLabel(i)}”`);
			else if (d.key === 'context_spaces') line('remove', i, `Topic dropped: ${spaceLabel(i)}`);
		}
	}
	return out;
}

/** Template layers whose pinned version differs from the previous release (D15 upgrades ride the cut). */
export function pinChanges(
	cur: TemplatePin[],
	prev: TemplatePin[] | null,
): { layer: string; from: number | null; to: number }[] {
	if (!prev) return [];
	return cur
		.map((p) => ({
			layer: p.layer,
			from: prev.find((x) => x.layer === p.layer)?.version ?? null,
			to: p.version,
		}))
		.filter((p) => p.from !== p.to);
}

/** Profile values that differ from the previous release's inlined values. */
export function profileValueChanges(
	cur: ResolvedSelection,
	prev: ResolvedSelection | null,
): { key: string; from: string | null; to: string | null }[] {
	if (!prev) return [];
	const keys = new Set([...Object.keys(cur.profile_values), ...Object.keys(prev.profile_values)]);
	return [...keys]
		.map((k) => ({ key: k, from: prev.profile_values[k] ?? null, to: cur.profile_values[k] ?? null }))
		.filter((x) => x.from !== x.to);
}

/** The tenant design changes this release carries: bundled at its cut (approved) or made live by it (applied). */
export function tenantChanges(d: ReleaseDetail, published: PublishedChange[]): DraftChange[] {
	const out = new Map<string, DraftChange>();
	for (const c of d.drafts ?? []) out.set(c.key, c);
	for (const p of published) if (p.published_in === d.id) out.set(p.key, p);
	return [...out.values()].sort((a, b) => a.at.localeCompare(b.at));
}

export type LifeStatus = 'done' | 'current' | 'todo' | 'blocked';
export interface LifeStep {
	key: 'cut' | 'apply' | 'live';
	label: string;
	status: LifeStatus;
	detail: string;
}

const plural = (n: number, w: string) => `${n} ${w}${n === 1 ? '' : 's'}`;
const day = (iso: string | null) =>
	iso
		? new Date(iso).toLocaleString('en-GB', { dateStyle: 'medium', timeStyle: 'short', timeZone: 'UTC' })
		: '—';

/**
 * The release's lifecycle as three steps: cut & approved (audit `release.approve`) → applied to Production (audit
 * `release.apply`; refused attempts make it `blocked`) → live / superseded.
 */
export function lifecycle(
	d: ReleaseDetail,
	events: AuditEvent[],
	successor: ReleaseRow | null,
	window: ReleaseWindow | null,
): LifeStep[] {
	const who = (sub: string | null) => (sub ? (d.names[sub] ?? sub) : 'unknown');
	const refused = events.filter((e) => e.action === 'release.apply' && e.status !== 'success');
	const lastRefusal = refused.sort((a, b) => b.at.localeCompare(a.at))[0];
	const cut: LifeStep = {
		key: 'cut',
		label: 'Cut & approved',
		status: 'done',
		detail: d.approved_by ? `${who(d.approved_by)} · ${day(d.approved_at)}` : 'Approver not recorded',
	};
	let apply: LifeStep;
	if (d.released_at) {
		const again =
			d.last_applied_at && d.last_applied_at !== d.released_at
				? ` · re-applied by ${who(d.applied_by)} ${day(d.last_applied_at)}`
				: '';
		apply = {
			key: 'apply',
			label: 'Applied to Production',
			status: 'done',
			detail: `${who(d.released_by)} · ${day(d.released_at)}${again}`,
		};
	} else if (lastRefusal) {
		const code = String(lastRefusal.details.code ?? 'refused');
		apply = {
			key: 'apply',
			label: 'Apply to Production',
			status: 'blocked',
			detail: `Refused ${day(lastRefusal.at)} (${code}) — fix the readiness gaps, then apply`,
		};
	} else {
		apply = {
			key: 'apply',
			label: 'Apply to Production',
			status: 'current',
			detail: 'Approved, waiting for a tenant_admin to apply it',
		};
	}
	let live: LifeStep;
	const back = (d.rollbacks ?? []).find((x) => x.to === d.id && x.direction === 'rollback');
	if (d.state === 'current' && back && d.last_applied_at === back.at)
		live = {
			key: 'live',
			label: 'Live again (rolled back to)',
			status: 'current',
			detail: `${back.by_name} rolled Production back from ${back.from ?? '—'} to this release ${day(back.at)}; ${plural(back.undone.length, 'design change')} undone`,
		};
	else if (d.state === 'current')
		live = {
			key: 'live',
			label: 'Live in Production',
			status: 'current',
			detail: `Every new conversation runs this release${window?.applied_at ? ` since ${day(window.applied_at)}` : ''}`,
		};
	else if (d.state === 'applied_earlier' && d.rolled_back)
		live = {
			key: 'live',
			label: 'Rolled back',
			status: 'done',
			detail: `Rolled back on ${day(d.rolled_back.at)} by ${d.rolled_back.by_name} to ${d.rolled_back.to} — ${plural(d.rolled_back.undone, 'design change')} undone; it can be applied again`,
		};
	else if (d.state === 'applied_earlier')
		live = {
			key: 'live',
			label: 'Superseded',
			status: 'done',
			detail: `Was live${window?.until ? ` until ${day(window.until)}` : ''}${
				successor ? `; replaced by ${successor.id}` : ''
			} — can be rolled back to`,
		};
	else live = { key: 'live', label: 'Live in Production', status: 'todo', detail: 'Not live yet' };
	return [cut, apply, live];
}

/** Containment rate of a release window (0–1), or null without traffic. */
export const containment = (w: ReleaseWindow | null | undefined) =>
	w && w.cases ? w.contained / w.cases : null;
export const escalation = (w: ReleaseWindow | null | undefined) =>
	w && w.cases ? w.escalated / w.cases : null;

/** Percentage-point change b − a, formatted ("+4.4 pt"), or null. */
export function ptDelta(a: number | null, b: number | null): string | null {
	if (a == null || b == null) return null;
	const d = (b - a) * 100;
	return `${d >= 0 ? '+' : '−'}${Math.abs(d).toFixed(1)} pt`;
}

/**
 * The rollback-preview drawer's title (J14 release page, J15 release detail): what confirming does. A preview of an
 * older release rolls Production back; of a rolled-back newer one re-applies it; of a never-applied cut applies it.
 * Until the preview has loaded (`direction` unknown) the title names only the target.
 */
export function rollbackPreviewTitle(
	direction: RollbackDirection | null | undefined,
	target: string,
): string {
	switch (direction) {
		case 'rollback':
			return `Roll back Production to ${target}?`;
		case 'reapply':
			return `Re-apply ${target} to Production?`;
		case 'apply':
			return `Apply ${target} to Production?`;
		default:
			return `Put ${target} in Production?`;
	}
}

export const pct = (x: number | null) => (x == null ? '—' : `${Math.round(x * 100)}%`);
export { day as when };
