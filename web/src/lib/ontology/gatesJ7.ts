// J7 job gates: the plain-language reading of one gate in one job — when it acts (with the tenant's tuned
// threshold), how often it acted / was unsure in the last 7 days, and what a threshold change would have done to
// the same week of traffic. Pure functions over EffGateQuestion so the page and the tests share them.
import type { EffGateQuestion, GateChange, GatePoint } from '$lib/api/types/ontology';

export interface TurnStep {
	point: GatePoint;
	label: string;
	/** What the gate looks at and before what. Vocabulary shared with the P14 library flow. */
	sub: string;
	/** What this job's answers decide at this point. */
	decides: string;
}

/** The three gate points of one conversation turn, in order (spec 03 §3.1/§3.2, 10 §6). */
export const TURN: TurnStep[] = [
	{
		point: 'P1',
		label: 'Message gate',
		sub: 'Reads the customer message before the qualifier does',
		decides: 'Safety stops, then theme → stage → activity → object; urgency and frustration',
	},
	{
		point: 'P2',
		label: 'Request gate',
		sub: 'Checks the qualified request before a case opens',
		decides: 'Which record the customer means',
	},
	{
		point: 'R1',
		label: 'Resolution gate',
		sub: 'Fact-checks the draft reply before it is sent',
		decides: 'Block, regenerate or fall back when the draft is not supported',
	},
];

/** The threshold text with the tenant's tuned value in place of the template's first number. */
export function ruleText(q: EffGateQuestion): string {
	const t = q.threshold.trim();
	if (!t || t === '—') return 'always (no threshold)';
	if (q.type === 'noul' && /any true/.test(t)) return 'if true';
	if (q.threshold_refined && q.threshold_value !== null)
		return `if ${t.replace(/\d+(\.\d+)?/, String(q.threshold_value))}`;
	return `if ${t}`;
}

/** Verdicts that crossed the line in the last 7 days: true answers, levels at or above the threshold, or
 *  confident choices. null when the job has no traffic. */
export function acted(q: EffGateQuestion): number | null {
	if (!q.stats.verdicts_7d) return null;
	if (q.type === 'noul') return q.options.find((o) => o.key === 'true')?.verdicts ?? 0;
	if (q.type === 'score') {
		if (q.threshold_value === null) return null;
		return q.options.filter((o) => Number(o.key) >= q.threshold_value!).reduce((a, o) => a + o.verdicts, 0);
	}
	return q.stats.verdicts_7d - q.stats.below_threshold;
}

/** One short line for "how often did this gate act" in this job, as a reader says it. */
export function actedText(q: EffGateQuestion): string {
	const n = acted(q);
	if (n === null) return 'no traffic yet';
	if (q.type === 'noul') return q.safety ? `fired ${n}×` : `true ${n}×`;
	if (q.type === 'score') return `${n}× at level ≥ ${q.threshold_value}`;
	return `${q.stats.below_threshold}× unsure`;
}

/** Below-the-line count the same 7 days would have had at threshold `v` (choice: from the confidence
 *  histogram; score: levels under `v`). null when there is no data to say. */
export function belowAt(q: EffGateQuestion, v: number): number | null {
	if (q.type === 'score') {
		if (!q.stats.verdicts_7d) return null;
		return q.options.filter((o) => Number(o.key) < v).reduce((a, o) => a + o.verdicts, 0);
	}
	const h = q.stats.confidence;
	if (q.type !== 'choice' || !h?.length) return null;
	const k = Math.round(v / 0.05);
	return h.slice(0, Math.max(0, Math.min(h.length, k))).reduce((a, b) => a + b, 0);
}

/** What happens to a message that falls below the line, per point. */
export function belowMeans(q: EffGateQuestion): string {
	if (q.type === 'score') return 'treated as normal';
	if (q.point === 'R1') return 'the draft is regenerated once, else the fallback text is sent';
	if (q.point === 'P2') return 'the agent asks the customer which one they mean';
	return 'the gate does not decide; the qualifier routes on its own or asks';
}

/** A threshold preview sentence for the drawer, or null when the gate has no data to preview. */
export function thresholdPreview(q: EffGateQuestion, v: number): string | null {
	const total = q.stats.verdicts_7d;
	if (!total || q.threshold_value === null || !Number.isFinite(v)) return null;
	const now = belowAt(q, q.threshold_value);
	const then = belowAt(q, v);
	if (now === null || then === null) return null;
	const pct = (n: number) => `${Math.round((n / total) * 100)}%`;
	if (q.type === 'score') {
		const a0 = total - now;
		const a1 = total - then;
		return a1 === a0
			? `Same week at level ≥ ${v}: no change (${a1} of ${total}).`
			: `Same week at level ≥ ${v}: ${a1} of ${total} messages (${pct(a1)}) would act, ${a1 > a0 ? 'up' : 'down'} from ${a0}.`;
	}
	return then === now
		? `Same week at ${v}: no change, ${now} of ${total} below the line.`
		: `Same week at ${v}: ${then} of ${total} messages (${pct(then)}) would fall below the line, ${then > now ? 'up' : 'down'} from ${now} — ${belowMeans(q)}.`;
}

/** Where a refinement stands (D54): draft until a release that contains it is applied to Production. */
export const SHIPS_TEXT: Record<GateChange['ships'], string> = {
	live: 'live',
	approved: 'approved, not applied',
	next_cut: 'draft',
};

/** The toast after a gate refinement is saved (D54): it goes to the draft, never straight to customers. */
export function draftToast(what: string): string {
	return `${what} Saved to draft — try it in the test chat; customers get it when you publish.`;
}

/** `GS · Exit → Leaving us` style summary of one change. */
export function changeText(c: GateChange): string {
	const what = c.option === null ? 'threshold' : `“${c.from || c.option}”`;
	if (c.field === 'threshold') return `threshold ${c.from} → ${c.to}`;
	if (c.field === 'rule') return `rule ${c.from} → ${c.to}`;
	if (c.field === 'question') return `question ${c.to}`;
	if (c.field === 'hidden') return `${c.option} hidden`;
	if (c.field === 'description') return `${c.option} description reworded`;
	return `${what} → “${c.to}”`;
}
