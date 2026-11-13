// Gate rules (`fire_when` gte / lt / in + minimum confidence): the pure evaluation the fixture laya and the
// pages share, the rule summary, rule validation, verdict-log filtering and the delete permission of a question.
import type {
	GateAction,
	GateDecision,
	GateFail,
	GatePoint,
	GateQuestion,
	GateRule,
	GateRunPoint,
	GateType,
	GateVerdictFilter,
	GateVerdictRow,
} from '$lib/api/types/ontology';

/** Turn point (P1 / P2 / R1) ↔ runtime point (request / tool / response). */
export const RUN_POINT: Record<GatePoint, GateRunPoint> = { P1: 'request', P2: 'tool', R1: 'response' };
export const GATE_POINT: Record<GateRunPoint, GatePoint> = { request: 'P1', tool: 'P2', response: 'R1' };
export const RUN_POINT_LABEL: Record<GateRunPoint, string> = {
	request: 'Request',
	tool: 'Tool',
	response: 'Response',
};
export const ACTION_TEXT: Record<GateAction, string> = {
	handover: 'hand over to a person',
	block: 'block the text',
	annotate: 'annotate the turn',
};
export const DECISION_TEXT: Record<GateDecision, string> = {
	allow: 'Allowed',
	block: 'Blocked',
	handover: 'Handed over',
};

const OP: Record<GateRule['fire_when'], string> = { gte: '≥', lt: '<', in: '∈' };

/** The value a rule compares: noul p(true), choice top confidence, score level. */
export interface RawAnswer {
	/** Chosen option key (`true` / `false` for noul, the level for score). */
	value: string;
	/** noul: p(true); choice: confidence; score: level. */
	measure: number;
	confidence: number;
}

/** `noul ≥ 0.5`, `choice ∈ {issue, need_change} with confidence ≥ 0.5`, `level ≥ 4`. */
export function ruleSummary(type: GateType, rule: GateRule): string {
	let cond: string;
	if (rule.fire_when === 'in') {
		const vals = rule.fire_values ?? [];
		cond = vals.length === 1 ? `answer is ${vals[0]}` : `answer ∈ {${vals.join(', ')}}`;
	} else {
		const subject = type === 'noul' ? 'p(true)' : type === 'score' ? 'level' : 'confidence';
		cond = `${subject} ${OP[rule.fire_when]} ${rule.threshold ?? '?'}`;
	}
	if (rule.min_confidence != null) cond += ` · min confidence ${rule.min_confidence}`;
	return cond;
}

/** One line for a whole question: `fires when … → hand over · fail closed`. */
export function questionRuleText(q: Pick<GateQuestion, 'type' | 'rule' | 'action' | 'fail'>): string {
	return `fires when ${ruleSummary(q.type, q.rule)} → ${ACTION_TEXT[q.action]} · fail ${q.fail}`;
}

/**
 * Apply a rule to one answer. An answer under `min_confidence` is unsure: it never fires and the fail policy applies
 * (fail closed → a person takes over, fail open → the turn continues). Otherwise gte / lt compare the measure with the
 * threshold and `in` checks the chosen option.
 */
export function applyRule(
	rule: GateRule,
	a: RawAnswer,
	fail: GateFail,
	action: GateAction,
): { fired: boolean; unsure: boolean; action: GateAction | null } {
	const unsure = rule.min_confidence != null && a.confidence < rule.min_confidence;
	if (unsure) return { fired: false, unsure, action: fail === 'closed' ? 'handover' : null };
	let fired: boolean;
	if (rule.fire_when === 'in') fired = (rule.fire_values ?? []).includes(a.value);
	else if (rule.threshold == null) fired = false;
	else fired = rule.fire_when === 'gte' ? a.measure >= rule.threshold : a.measure < rule.threshold;
	return { fired, unsure, action: fired ? action : null };
}

/** The turn's decision from the answers' actions: any handover wins, then any block, else allow. */
export function decide(actions: (GateAction | null)[]): GateDecision {
	if (actions.includes('handover')) return 'handover';
	if (actions.includes('block')) return 'block';
	return 'allow';
}

/** Why a rule cannot be saved for a question, or null. */
export function ruleError(q: Pick<GateQuestion, 'type' | 'options'>, rule: GateRule): string | null {
	const prob = (v: number | null) => v != null && Number.isFinite(v) && v >= 0 && v <= 1;
	if (rule.min_confidence != null && !prob(rule.min_confidence))
		return 'The minimum confidence is between 0 and 1 (blank: any).';
	if (rule.fire_when === 'in') {
		if (q.type !== 'choice') return '“in” only applies to choice questions.';
		const vals = rule.fire_values ?? [];
		if (!vals.length) return 'Pick at least one answer that fires.';
		const keys = new Set(q.options.map((o) => o.key));
		const unknown = vals.filter((v) => !keys.has(v));
		if (keys.size && unknown.length) return `Not an option: ${unknown.join(', ')}.`;
		return null;
	}
	if (rule.threshold == null || !Number.isFinite(rule.threshold)) return 'Set a threshold.';
	if (q.type === 'score') {
		const n = q.options.length;
		if (!Number.isInteger(rule.threshold) || rule.threshold < 1 || rule.threshold > n)
			return `A score threshold is a level from 1 to ${n}.`;
	} else if (!prob(rule.threshold)) return 'The threshold is a probability between 0 and 1.';
	return null;
}

/** The rule as it would be saved: `in` keeps fire values and drops the threshold, gte / lt the reverse. */
export function normaliseRule(rule: GateRule): GateRule {
	const isIn = rule.fire_when === 'in';
	const num = (v: number | null | undefined) => (typeof v === 'number' && Number.isFinite(v) ? v : null);
	return {
		fire_when: rule.fire_when,
		threshold: isIn ? null : num(rule.threshold),
		fire_values: isIn ? [...(rule.fire_values ?? [])] : null,
		min_confidence: num(rule.min_confidence),
	};
}

/** Verdict-log filter (point, decision, question, below-the-line, free text over question / answer / case). */
export function filterVerdicts(rows: GateVerdictRow[], f: GateVerdictFilter): GateVerdictRow[] {
	const q = f.q?.trim().toLowerCase() ?? '';
	return rows.filter(
		(r) =>
			(!f.point || f.point === 'all' || r.point === f.point) &&
			(!f.action || f.action === 'all' || r.action === f.action) &&
			(!f.gate || f.gate === 'all' || r.gate === f.gate) &&
			(!f.unsure || r.below_threshold) &&
			(!q || [r.gate, r.question, r.answer, r.case_id].some((s) => s.toLowerCase().includes(q))),
	);
}

/** Whether this caller may delete the question from this job, with the reason when not. */
export function deletePermission(
	q: Pick<GateQuestion, 'origin' | 'layer'>,
	canWrite: boolean,
	writeHint: string,
): { ok: boolean; reason: string | null } {
	if (q.origin !== 'tenant')
		return {
			ok: false,
			reason: `From the ${q.layer} template layer (platform library): it cannot be deleted per job. Hide options or tune the rule instead.`,
		};
	if (!canWrite) return { ok: false, reason: writeHint };
	return { ok: true, reason: null };
}

/** Comma / space separated tool names, trimmed and deduplicated. */
export function parseTools(s: string): string[] {
	return [...new Set(s.split(/[\s,]+/).filter(Boolean))];
}
