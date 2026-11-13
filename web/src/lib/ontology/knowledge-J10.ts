// J10 Knowledge kinds: pure helpers. A job's journey stages each need knowledge kinds (11 §4.3.3, from the job
// template, required or optional); the tenant binds every kind to one of its knowledge stores (a refinement); the
// store is filled on T7; the post-gate (R1, F2/F7) checks each reply's policy claims against passages of exactly
// these kinds. The tenant store tree (T7) is the source of truth for stores; J10 only binds to it.
import type { KnowledgeStore } from '$lib/api/types/tenant';
import type { KnowledgeKindRow, KnowledgeStoreOption } from '$lib/api/types/ontology';
import { COVERAGE, pathOf, type KindCoverage } from '$lib/tenant/knowledge-T7';

/** The bindable stores of a tenant: its leaf stores, addressed by their tree path (`faq-policies/policy.returns`). */
export function storeOptions(all: KnowledgeStore[]): KnowledgeStoreOption[] {
	return all
		.filter((s) => s.kind === 'leaf')
		.map((s) => ({ path: pathOf(all, s), label: s.label, documents: s.documents, updated_at: s.updated_at }))
		.sort((a, b) => a.path.localeCompare(b.path));
}

/** What R1 checks a reply against in each stage (11 §4.3.3 "post-gate facts"), short. */
export const J10_POST_GATE: Record<string, string> = {
	orientation:
		'Every policy claim cites a passage; prices and availability come from a system read, never from text.',
	investigation: 'No promised date or availability without a cited passage or a system read.',
	purchase: '“Paid” or “ordered” only with an ok operation outcome; terms cite their version.',
	delivery: 'Status and window from a system read; how-to advice cites a guide passage.',
	change: '“Changed” only with an ok operation outcome; no contact data beyond the verified party.',
	support: 'Rule reasons cite the policy passage they apply (RET-01, WAR-01, PAY-04 in retail).',
	exit: 'Never “deleted”: a request with a case reference and the cited retention text.',
};

/**
 * Where a kind's gap is fixed: `bind` here on J10 (no store, or the store is gone), `fill` on T7 (the store exists but
 * is not in strata_rag, being created, empty or stale), or nothing.
 */
export type J10Fix = 'bind' | 'fill' | null;
export function fixOf(c: KindCoverage): J10Fix {
	if (c.state === 'no_store' || c.state === 'missing_store') return 'bind';
	if (c.state === 'ok') return null;
	return 'fill';
}

export interface J10Summary {
	/** Distinct (stage, kind) pairs. */
	pairs: number;
	kinds: number;
	stages: number;
	required: number;
	/** Required pairs with a store bound. */
	requiredBound: number;
	/** Required pairs whose store can be cited today (bound, in strata_rag, filled; stale still counts). */
	requiredCited: number;
	/** Required pairs to bind here. */
	toBind: KindCoverage[];
	/** Required pairs bound to a store that cannot answer yet (fix on T7). */
	toFill: KindCoverage[];
	tenantKinds: number;
}

export function j10Summary(cov: KindCoverage[]): J10Summary {
	const req = cov.filter((c) => c.row.required);
	return {
		pairs: cov.length,
		kinds: new Set(cov.map((c) => c.row.kind)).size,
		stages: new Set(cov.map((c) => c.row.stage)).size,
		required: req.length,
		requiredBound: req.filter((c) => c.row.store).length,
		requiredCited: req.filter((c) => !COVERAGE[c.state].gap).length,
		toBind: req.filter((c) => fixOf(c) === 'bind'),
		toFill: req.filter((c) => fixOf(c) === 'fill' && COVERAGE[c.state].gap),
		tenantKinds: cov.filter((c) => c.row.origin === 'tenant').length,
	};
}

/** One store this job reads, and the (stage, kind) pairs that read it. */
export interface J10StoreUse {
	path: string;
	store: KnowledgeStore | null;
	reads: KnowledgeKindRow[];
}

export function storeUse(cov: KindCoverage[]): J10StoreUse[] {
	const m = new Map<string, J10StoreUse>();
	for (const c of cov) {
		if (!c.row.store) continue;
		const u = m.get(c.row.store) ?? { path: c.row.store, store: c.store, reads: [] };
		u.reads.push(c.row);
		m.set(c.row.store, u);
	}
	return [...m.values()].sort((a, b) => b.reads.length - a.reads.length || a.path.localeCompare(b.path));
}
