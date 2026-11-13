// T7 Knowledge stores: joins the tenant's store tree (strata_rag binding, content) with the current job's knowledge
// kinds (J10, 11 §4.3.3) into a coverage view: for every journey stage, which kinds the agent needs and whether a
// store that actually answers for each one exists, is created in strata_rag, holds documents and is fresh.
import type { KnowledgeStore } from '$lib/api/types/tenant';
import type { KnowledgeKindRow } from '$lib/api/types/ontology';

/** A store not updated for this many days is shown as stale. */
export const STALE_DAYS = 30;

/**
 * One kind's coverage, worst first: no store bound · bound to a path that does not exist · store not in strata_rag
 * (or being created) · store empty · store stale · ok.
 */
export type Coverage = 'no_store' | 'missing_store' | 'unbound' | 'ensuring' | 'empty' | 'stale' | 'ok';

export const COVERAGE: Record<
	Coverage,
	{ text: string; variant: 'ok' | 'warn' | 'err' | 'muted'; gap: boolean; hint: string }
> = {
	no_store: {
		text: 'no store',
		variant: 'err',
		gap: true,
		hint: 'No store is bound to this kind: the agent cannot cite it.',
	},
	missing_store: {
		text: 'store gone',
		variant: 'err',
		gap: true,
		hint: 'Bound to a store that no longer exists here.',
	},
	unbound: {
		text: 'not in strata_rag',
		variant: 'err',
		gap: true,
		hint: 'The store is laid out here but not created in strata_rag, so search skips it.',
	},
	ensuring: {
		text: 'being created',
		variant: 'warn',
		gap: true,
		hint: 'store_ensure is queued; search finds it once strata_rag has created it.',
	},
	empty: {
		text: 'empty',
		variant: 'warn',
		gap: true,
		hint: 'The store exists but holds no documents yet.',
	},
	stale: {
		text: 'stale',
		variant: 'warn',
		gap: false,
		hint: `Not updated for more than ${STALE_DAYS} days.`,
	},
	ok: { text: 'covered', variant: 'ok', gap: false, hint: 'Created in strata_rag, holds documents, fresh.' },
};

/** `faq-policies/policy.returns` — the path J10 binds kinds to. */
export function pathOf(all: KnowledgeStore[], s: KnowledgeStore): string {
	const out = [s.key];
	let p = s.parent_id ? all.find((x) => x.id === s.parent_id) : undefined;
	while (p && out.length < 16) {
		out.unshift(p.key);
		p = p.parent_id ? all.find((x) => x.id === p!.parent_id) : undefined;
	}
	return out.join('/');
}

export function ageDays(iso: string, now: number): number {
	return Math.max(0, Math.floor((now - Date.parse(iso)) / 86_400_000));
}

/** The state of one store on its own (no kind involved). */
export function storeState(s: KnowledgeStore, now: number): Coverage {
	if (s.ensuring) return 'ensuring';
	if (!s.strata_store_id) return 'unbound';
	if (s.kind === 'leaf' && s.documents === 0) return 'empty';
	if (s.kind === 'leaf' && ageDays(s.updated_at, now) > STALE_DAYS) return 'stale';
	return 'ok';
}

export interface KindCoverage {
	row: KnowledgeKindRow;
	store: KnowledgeStore | null;
	state: Coverage;
	/** An existing leaf store whose key equals the kind: bind it on J10 instead of creating one. */
	candidate: KnowledgeStore | null;
}

export function kindCoverage(rows: KnowledgeKindRow[], all: KnowledgeStore[], now: number): KindCoverage[] {
	return rows.map((row) => {
		const candidate = all.find((s) => s.kind === 'leaf' && s.key === row.kind) ?? null;
		if (!row.store) return { row, store: null, state: 'no_store' as const, candidate };
		const store = all.find((s) => pathOf(all, s) === row.store) ?? null;
		if (!store) return { row, store: null, state: 'missing_store' as const, candidate };
		return { row, store, state: storeState(store, now), candidate: null };
	});
}

export interface StageCoverage {
	stage: string;
	label: string;
	kinds: KindCoverage[];
	/** Required kinds that are a gap. */
	gaps: number;
}

export function byStage(cov: KindCoverage[], stages: [string, string, string][]): StageCoverage[] {
	return stages.map(([stage, label]) => {
		const kinds = cov
			.filter((c) => c.row.stage === stage)
			.sort(
				(a, b) => Number(b.row.required) - Number(a.row.required) || a.row.kind.localeCompare(b.row.kind),
			);
		return { stage, label, kinds, gaps: kinds.filter((k) => k.row.required && COVERAGE[k.state].gap).length };
	});
}

/** The stages (and whether any of them requires it) that a store answers for in this job. */
export function neededBy(
	cov: KindCoverage[],
	storeId: string,
): { stage: string; kind: string; required: boolean }[] {
	return cov
		.filter((c) => c.store?.id === storeId)
		.map((c) => ({ stage: c.row.stage, kind: c.row.kind, required: c.row.required }));
}

export interface Summary {
	required: number;
	requiredCovered: number;
	/** Distinct required kinds with no usable store. */
	gapKinds: string[];
	leaves: number;
	groups: number;
	bound: number;
	ensuring: number;
	documents: number;
	passages: number;
	regions: ('eu' | 'us')[];
	stale: string[];
}

export function summarize(all: KnowledgeStore[], cov: KindCoverage[], now: number): Summary {
	const req = new Map<string, boolean>();
	for (const c of cov)
		if (c.row.required) req.set(c.row.kind, (req.get(c.row.kind) ?? true) && !COVERAGE[c.state].gap);
	const leaves = all.filter((s) => s.kind === 'leaf');
	return {
		required: req.size,
		requiredCovered: [...req.values()].filter(Boolean).length,
		gapKinds: [...req.entries()]
			.filter(([, ok]) => !ok)
			.map(([k]) => k)
			.sort(),
		leaves: leaves.length,
		groups: all.length - leaves.length,
		bound: all.filter((s) => s.strata_store_id).length,
		ensuring: all.filter((s) => s.ensuring).length,
		documents: leaves.reduce((n, s) => n + s.documents, 0),
		passages: leaves.reduce((n, s) => n + s.chunks, 0),
		regions: [...new Set(all.map((s) => s.region).filter((r): r is 'eu' | 'us' => !!r))].sort(),
		stale: leaves.filter((s) => storeState(s, now) === 'stale').map((s) => s.key),
	};
}

export const REGION_LABEL: Record<'eu' | 'us', string> = { eu: 'EU', us: 'US' };

/** `today`, `1 day ago`, `12 days ago`. */
export function ago(iso: string, now: number): string {
	const d = ageDays(iso, now);
	return d === 0 ? 'today' : d === 1 ? '1 day ago' : `${d} days ago`;
}

/** A store key suggested for a kind with no store (kinds are store keys, 11 §4.3.3). */
export function draftFor(kind: string): { key: string; label: string } {
	const tail = kind.split('.').pop() ?? kind;
	const label = tail.replace(/_/g, ' ');
	return { key: kind, label: label.charAt(0).toUpperCase() + label.slice(1) };
}
