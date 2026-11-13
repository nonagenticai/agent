// T5 Usage: pure helpers for the page (unit-tested in tests/unit/tenant-T5.test.ts).
import type { Funding, UsageDriver, UsageReport } from '$lib/api/types/tenant';

/** Rounded tokens per turn; 0 when there are no turns. */
export const tokensPerTurn = (x: { turns: number; tokens: number }): number =>
	x.turns > 0 ? Math.round(x.tokens / x.turns) : 0;

/** Relative change vs the previous window, or null when there is nothing to compare with. */
export function delta(cur: number, prev: number | null | undefined): number | null {
	if (prev == null || prev <= 0) return null;
	return (cur - prev) / prev;
}

/** Empty only when nothing at all was metered: no turns, no ingest, no tenant generator run. */
export const isEmptyUsage = (r: UsageReport): boolean =>
	r.totals.turns === 0 && (r.knowledge?.ingest_jobs ?? 0) === 0 && (r.generator?.length ?? 0) === 0;

/** The report's catch-all driver key for turns with neither an activity nor a request type. */
const UNQUALIFIED = 'unqualified';
const isQualifiedDriver = (d: UsageDriver): boolean => d.activity.trim() !== '' && d.activity !== UNQUALIFIED;

/**
 * The activity that costs most relative to its volume, as one sentence: "Claim warranty: 8.4 % of tokens on 6.0 % of
 * turns." Null when no qualified driver is over-represented by at least 15 %.
 *
 * A driver key is the case's activity (`support:claim_warranty`) or, when the runtime wrote none, its request type
 * (`order_status`, agent#46); both are qualified. The only non-qualified key the report emits is `unqualified`
 * (generator/discovery sources are excluded from drivers server-side), so that bucket — and a blank key — never wins.
 */
export function driverHeadline(
	drivers: UsageDriver[],
	totals: { turns: number; tokens: number },
): string | null {
	if (totals.turns <= 0 || totals.tokens <= 0) return null;
	let best: { d: UsageDriver; ratio: number } | null = null;
	for (const d of drivers) {
		if (d.turns <= 0 || !isQualifiedDriver(d)) continue;
		const ratio = d.tokens / totals.tokens / (d.turns / totals.turns);
		if (!best || ratio > best.ratio) best = { d, ratio };
	}
	if (!best || best.ratio < 1.15) return null;
	const p = (a: number, b: number) => `${((a / b) * 100).toFixed(1)} %`;
	return `${best.d.label} is the most expensive per turn: ${p(best.d.tokens, totals.tokens)} of tokens on ${p(best.d.turns, totals.turns)} of turns.`;
}

/**
 * D56: tokens per funding lane across every metered source (customer turns, staff test chats, generator runs). Falls
 * back to the turn ledger's `funding` when the report has no source split.
 */
export function laneTotals(r: Pick<UsageReport, 'funding' | 'sources'>): Record<Funding, number> {
	if (!r.sources?.length) return { ...r.funding };
	const out: Record<Funding, number> = { local_gpu: 0, platform_external: 0, tenant_byo: 0 };
	for (const s of r.sources) for (const k of Object.keys(out) as Funding[]) out[k] += s.funding[k];
	return out;
}

/** D56: generator tokens in the window — the tenant's usage under the `generator` activity, not a bill. */
export const generatorTokens = (r: Pick<UsageReport, 'generator'>): number =>
	(r.generator ?? []).reduce((a, g) => a + g.tokens, 0);
