// P16 pure helpers: how a connector template version measures up against the canonical operation catalogue and a
// job's expectations, and which template versions need a platform decision. Mirrors doc 10 §4.2–§4.5, K48 (the
// counting unit is the (object, verb) pair) and doc 12 §3.3 (a variant falls back to its unqualified `*` op).
import type {
	ConnectorTemplateOverview,
	ConnectorTemplateSummary,
	CoverageJob,
	TemplateCoverage,
	TemplateWave,
} from '$lib/api/types/connectors';

export const tplId = (t: { key: string; version: string }) => `${t.key}@${t.version}`;

export const templateHref = (t: { key: string; version: string }) =>
	`/platform/connector-templates/${encodeURIComponent(t.key)}/${encodeURIComponent(t.version)}`;

const cmpVersion = (a: string, b: string) => a.localeCompare(b, 'en', { numeric: true });

/** Does this version serve `key` (`case.create{return}`)? Directly, via its `*` op, or because it is generic. */
export function serves(cov: Pick<TemplateCoverage, 'any' | 'implements'>, key: string): boolean {
	if (cov.any) return true;
	if (cov.implements.includes(key)) return true;
	const star = key.replace(/\{[^}]+\}$/, '');
	return star !== key && cov.implements.includes(star);
}

const pairOf = (key: string) => key.replace(/\{[^}]+\}$/, '');

/**
 * Required-op coverage in the K48 unit: a required (object, verb) pair is covered when the version serves every
 * required variant of it, partial when it serves some.
 */
export function requiredCoverage(
	cov: Pick<TemplateCoverage, 'any' | 'implements'>,
	job: CoverageJob,
): { covered: number; partial: number; total: number; missing: string[] } {
	const pairs = new Map<string, string[]>();
	for (const o of job.ops.filter((x) => x.required))
		pairs.set(pairOf(o.key), [...(pairs.get(pairOf(o.key)) ?? []), o.key]);
	let covered = 0;
	let partial = 0;
	const missing: string[] = [];
	for (const [pair, keys] of pairs) {
		const hit = keys.filter((k) => serves(cov, k)).length;
		if (hit === keys.length) covered++;
		else if (hit > 0) partial++;
		else missing.push(pair);
	}
	return { covered, partial, total: pairs.size, missing };
}

/** Which job a wave's templates are measured against: v1 + "next" serve customer care, the ITSD wave the desk. */
export function jobForWave(jobs: CoverageJob[], wave: TemplateWave): CoverageJob | null {
	const want = wave === 'itsd' ? 'it_service_desk' : 'customer_care';
	return jobs.find((j) => j.key.startsWith(want)) ?? jobs[0] ?? null;
}

/** Newest version per key (any status), in first-seen key order. */
export function latestPerKey<T extends { key: string; version: string }>(rows: T[]): T[] {
	const by = new Map<string, T>();
	for (const t of rows) {
		const cur = by.get(t.key);
		if (!cur || cmpVersion(t.version, cur.version) > 0) by.set(t.key, t);
	}
	return [...by.values()];
}

/** Every version of each key, newest first, grouped by key. */
export function versionsByKey(rows: ConnectorTemplateSummary[]): [string, ConnectorTemplateSummary[]][] {
	const by = new Map<string, ConnectorTemplateSummary[]>();
	for (const t of rows) by.set(t.key, [...(by.get(t.key) ?? []), t]);
	return [...by].map(([k, v]) => [k, [...v].sort((a, b) => cmpVersion(b.version, a.version))]);
}

export type Served = 'active' | 'draft' | 'none';

/** Best template status that serves `key`: an active version, only drafts, or nothing (a gap). */
export function servedBy(o: ConnectorTemplateOverview, key: string): { level: Served; by: string[] } {
	const status = new Map(o.templates.map((t) => [tplId(t), t.status]));
	const hits = o.coverage.filter((c) => !c.any && serves(c, key));
	const active = hits.filter((c) => status.get(tplId(c)) === 'active').map((c) => c.key);
	if (active.length) return { level: 'active', by: [...new Set(active)] };
	const draft = hits.filter((c) => status.get(tplId(c)) === 'draft').map((c) => c.key);
	if (draft.length) return { level: 'draft', by: [...new Set(draft)] };
	return { level: 'none', by: [] };
}

export type AttentionKind = 'drift' | 'blocked' | 'publishable' | 'draft_in_use' | 'old_pin' | 'status_map';

export interface Attention {
	kind: AttentionKind;
	key: string;
	version: string;
	title: string;
	detail: string;
}

/**
 * The decision queue, most urgent first: vendor drift, drafts tenants already run, drafts blocked from publishing,
 * drafts ready to publish, tenants on an older version than the newest active one, status maps that target
 * states the object model does not know.
 */
export function attention(o: ConnectorTemplateOverview): Attention[] {
	const out: Attention[] = [];
	const cov = new Map(o.coverage.map((c) => [tplId(c), c]));
	const newestActive = new Map<string, string>();
	for (const t of o.templates)
		if (t.status === 'active') {
			const cur = newestActive.get(t.key);
			if (!cur || cmpVersion(t.version, cur) > 0) newestActive.set(t.key, t.version);
		}
	for (const t of o.templates) {
		const c = cov.get(tplId(t));
		const base = { key: t.key, version: t.version };
		if (t.drift)
			out.push({
				...base,
				kind: 'drift',
				title: `Vendor version drift — ${tplId(t)}`,
				detail: `${t.drift.vendor_version}: ${t.drift.note} Re-record opened as ${t.drift.pr}.`,
			});
		if (t.status === 'draft' && c) {
			const pins = c.pinned_by;
			if (pins.length)
				out.push({
					...base,
					kind: 'draft_in_use',
					title: 'Draft already pinned by a tenant',
					detail: `${pins.map((p) => `${p.tenant} (${p.env})`).join(', ')} — a draft serves test-env routes only until it is published.`,
				});
			if (c.publish_blockers.length)
				out.push({
					...base,
					kind: 'blocked',
					title: 'Cannot publish yet',
					detail: c.publish_blockers.join('; '),
				});
			else
				out.push({
					...base,
					kind: 'publishable',
					title: t.t2.status === 'green' ? 'Ready to publish' : 'Publishable — run T2 first',
					detail:
						t.t2.status === 'green'
							? 'T2 sandbox green, no validation blockers.'
							: 'No validation blockers; doc 10 §4.5 wants one green T2 run before tenants are offered it.',
				});
		}
		const newest = newestActive.get(t.key);
		if (c && t.status === 'active' && newest && cmpVersion(t.version, newest) < 0 && c.pinned_by.length)
			out.push({
				...base,
				kind: 'old_pin',
				title: `Tenants on an older version (newest is @${newest})`,
				detail: `${c.pinned_by.map((p) => `${p.tenant} (${p.env})`).join(', ')} keep this pin until they re-point their system.`,
			});
		const unknown = c?.status_maps.filter((m) => m.unknown.length) ?? [];
		if (unknown.length)
			out.push({
				...base,
				kind: 'status_map',
				title: 'Maps onto non-canonical statuses',
				detail: unknown.map((m) => `${m.object}: ${m.unknown.join(', ')}`).join(' · '),
			});
	}
	const rank: Record<AttentionKind, number> = {
		drift: 0,
		draft_in_use: 1,
		blocked: 2,
		publishable: 3,
		old_pin: 4,
		status_map: 5,
	};
	return out.sort((a, b) => rank[a.kind] - rank[b.kind]);
}

/**
 * A job's required (object, verb) pairs (K48 unit) and how many of them the active catalogue serves end to end —
 * every required variant routed to some active template version.
 */
export function jobServed(
	o: ConnectorTemplateOverview,
	job: CoverageJob,
): { served: number; total: number; unserved: string[] } {
	const pairs = new Map<string, string[]>();
	for (const x of job.ops.filter((y) => y.required))
		pairs.set(pairOf(x.key), [...(pairs.get(pairOf(x.key)) ?? []), x.key]);
	const unserved = [...pairs]
		.filter(([, keys]) => keys.some((k) => servedBy(o, k).level !== 'active'))
		.map(([p]) => p);
	return { served: pairs.size - unserved.length, total: pairs.size, unserved };
}

/** Attention items grouped by kind, in rank order (the list stays scannable when one kind repeats). */
export function groupAttention(items: Attention[]): { kind: AttentionKind; items: Attention[] }[] {
	const out: { kind: AttentionKind; items: Attention[] }[] = [];
	for (const a of items) {
		const g = out.find((x) => x.kind === a.kind);
		if (g) g.items.push(a);
		else out.push({ kind: a.kind, items: [a] });
	}
	return out;
}
