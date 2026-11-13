// J12 (B5 T7a, F-15, F-20): procedure step conditions, the per-case cap and success criteria. A port of B4's
// `runtime/conditions.rs` `validate` (the server checks every create with it) plus the T7a create rules, so the
// fixture refuses exactly what `POST /api/jobs/{job}/procedures` refuses and the editor can warn before saving.
import type { CondOp, Condition, ProcedureStep, SideEffect } from '$lib/api/types/ontology';

/** The case facts a condition may read (B4 `FACT_PATHS`; `*` is one name, the last `*` of `steps.*.out.*` may be
 *  dotted, and a segment of it may be the filter `items[order_id=$order_id]`: the first list element whose field
 *  equals a slot or a literal; none matching, the fact is missing and the condition unmet). */
export const FACT_PATHS = [
	'slots.*',
	'identity.level',
	'customer.tier',
	'steps.*.outcome',
	'steps.*.out.*',
	'case.activity',
] as const;

export const COND_OPS: CondOp[] = ['eq', 'ne', 'in', 'exists', 'absent', 'gte', 'lte'];

export const COND_OP_LABEL: Record<CondOp, string> = {
	eq: '=',
	ne: '≠',
	in: 'is one of',
	exists: 'is known',
	absent: 'is unknown',
	gte: '≥',
	lte: '≤',
};

/** The highest per-case cap (B4 `runner::cap::MAX_CAP`); a step without one runs once per case. */
export const MAX_CAP = 20;
/** The longest PDDL domain (bytes; migration 0147). */
export const MAX_PDDL_BYTES = 64 * 1024;

/** Only X and P steps take a per-case cap (R and W have none). */
export const takesCap = (s: SideEffect): boolean => s === 'X' || s === 'P';

/** Why a condition list is not valid (B4 `ConditionError`): the condition's id and a sentence. */
export interface ConditionProblem {
	id: string;
	message: string;
}

const ID = /^[a-z][a-z0-9_]{0,31}$/;

const FILTER_NAME = /^[A-Za-z_][A-Za-z0-9_]*$/;

/** One path segment `key[field=value]` (B4 `conditions::parse_filter`): the list at `key`, narrowed to its first
 *  element whose `field` equals `value` (`$name` = a slot, else a literal). null: the segment holds no `[` or `]`
 *  (a plain name); false: it does but is no valid filter. */
export function parseFilter(seg: string): { key: string; field: string; value: string } | false | null {
	if (!/[[\]]/.test(seg)) return null;
	if (!seg.endsWith(']')) return false;
	const inner = seg.slice(0, -1);
	const open = inner.indexOf('[');
	if (open < 0) return false;
	const key = inner.slice(0, open);
	const cond = inner.slice(open + 1);
	const eq = cond.indexOf('=');
	if (eq < 0) return false;
	const field = cond.slice(0, eq);
	const value = cond.slice(eq + 1);
	const valueOk = value.startsWith('$')
		? FILTER_NAME.test(value.slice(1))
		: value !== '' && value.trim() === value && !/[.[\]=$]/.test(value);
	return FILTER_NAME.test(key) && FILTER_NAME.test(field) && valueOk ? { key, field, value } : false;
}

/** A non-empty dotted name with no blank segment; a segment holding `[` or `]` must be a valid filter. */
const validName = (s: string): boolean =>
	s !== '' && s.split('.').every((p) => p.trim() !== '' && parseFilter(p) !== false);

/** The step a fact path reads (`steps.<step>.…`), null for a case fact, undefined for no fact path. */
function stepOf(fact: string): string | null | undefined {
	if (fact === 'identity.level' || fact === 'customer.tier' || fact === 'case.activity') return null;
	if (fact.startsWith('slots.')) {
		const name = fact.slice(6);
		return name && !name.includes('.') ? null : undefined;
	}
	if (!fact.startsWith('steps.')) return undefined;
	const rest = fact.slice(6);
	const dot = rest.indexOf('.');
	if (dot <= 0) return undefined;
	const step = rest.slice(0, dot);
	const field = rest.slice(dot + 1);
	if (field === 'outcome') return step;
	if (!field.startsWith('out.')) return undefined;
	const f = field.slice(4);
	return validName(f) ? step : undefined;
}

const scalar = (v: unknown) => typeof v === 'string' || typeof v === 'number' || typeof v === 'boolean';

/** B4 `validate`: ids, fact paths, step references (only `stepIds`: the steps that run before the condition is
 *  evaluated) and operator/value pairs. The first problem, or null. */
export function conditionProblem(cs: Condition[], stepIds: string[]): ConditionProblem | null {
	const seen = new Set<string>();
	for (const c of cs) {
		if (!ID.test(c.id)) return { id: c.id, message: `condition id "${c.id}" is not a lowercase key` };
		if (seen.has(c.id)) return { id: c.id, message: `condition id "${c.id}" is used twice` };
		seen.add(c.id);
		const step = stepOf(c.fact);
		if (step === undefined) return { id: c.id, message: `condition ${c.id}: "${c.fact}" is not a case fact` };
		if (step !== null && !stepIds.includes(step))
			return { id: c.id, message: `condition ${c.id}: step "${step}" does not run before it` };
		const v = c.value;
		const ok =
			c.op === 'exists' || c.op === 'absent'
				? v === undefined || v === null
				: c.op === 'eq' || c.op === 'ne'
					? v !== undefined && scalar(v)
					: c.op === 'gte' || c.op === 'lte'
						? typeof v === 'string' || typeof v === 'number'
						: Array.isArray(v) && v.length > 0 && v.every(scalar);
		if (!ok) return { id: c.id, message: `condition ${c.id}: the value does not fit the operator` };
	}
	return null;
}

/** One refusal of the T7a create rules (the route's 422 code and its `details`). */
export interface T7aProblem {
	code: 'invalid_condition' | 'invalid_cap' | 'unknown_role' | 'unknown_operation' | 'invalid_pddl';
	message: string;
	details: Record<string, unknown>;
}

/** Steps as the server checks them (side effects already the catalogue's): `performed_by` a job role, a cap only on
 *  X/P steps and 1..=20, then every condition list (a precondition reads EARLIER phases, a postcondition those and
 *  its own step, a success criterion every step). */
export function stepsProblem(
	steps: ProcedureStep[],
	roles: string[],
	success: Condition[],
): T7aProblem | null {
	for (const s of steps) {
		if (s.performed_by != null && !roles.includes(s.performed_by))
			return {
				code: 'unknown_role',
				message: `Step ${s.step_number} names role ${s.performed_by}, which this job does not have.`,
				details: { step: s.step_number },
			};
		if (s.max_per_case == null) continue;
		if (!takesCap(s.side_effect))
			return {
				code: 'invalid_cap',
				message: `Step ${s.step_number} reads or writes (${s.operation}): only X and P steps take a per-case cap.`,
				details: { step: s.step_number },
			};
		if (!Number.isInteger(s.max_per_case) || s.max_per_case < 1 || s.max_per_case > MAX_CAP)
			return {
				code: 'invalid_cap',
				message: `Step ${s.step_number}: the per-case cap is a whole number from 1 to ${MAX_CAP}.`,
				details: { step: s.step_number },
			};
	}
	for (const s of steps) {
		const before = steps.filter((o) => o.phase < s.phase).map((o) => o.name);
		for (const [list, cs, ids] of [
			['preconditions', s.preconditions ?? [], before],
			['postconditions', s.postconditions ?? [], [...before, s.name]],
		] as const) {
			const p = conditionProblem(cs, [...ids]);
			if (p)
				return {
					code: 'invalid_condition',
					message: `Step ${s.step_number} ${list}: ${p.message}.`,
					details: { step: s.step_number, list, condition: p.id },
				};
		}
	}
	const p = conditionProblem(
		success,
		steps.map((s) => s.name),
	);
	return p
		? {
				code: 'invalid_condition',
				message: `Success criteria: ${p.message}.`,
				details: { step: null, list: 'success_criteria', condition: p.id },
			}
		: null;
}

/** `object.verb` / `object.verb{variant}` → [op, variant], or null when it is not that shape. */
export function parseTool(key: string): [string, string | null] | null {
	const m = /^([a-z_]+\.[a-z_]+)(?:\{([^{}\n]+)\})?$/.exec(key);
	return m ? [m[1]!, m[2] ?? null] : null;
}

/** UTF-8 length of `s` (the server counts bytes). */
export const byteLength = (s: string): number => new TextEncoder().encode(s).length;

/** `slots.order_id = "A-1"`, `identity.level ≥ 2`, `customer.tier is one of gold, silver`. */
export function conditionText(c: Condition): string {
	const v = c.value;
	const val =
		v === undefined || v === null ? '' : Array.isArray(v) ? v.map(String).join(', ') : JSON.stringify(v);
	return `${c.fact} ${COND_OP_LABEL[c.op]}${val ? ` ${val}` : ''}`;
}

/** The value an editor's text box means for `op`: a list for `in`, nothing for `exists`/`absent`, a number when it
 *  reads as one, else the text. */
export function parseValue(op: CondOp, text: string): Condition['value'] {
	if (op === 'exists' || op === 'absent') return undefined;
	const one = (s: string): string | number | boolean => {
		const t = s.trim();
		if (t === 'true' || t === 'false') return t === 'true';
		return t !== '' && !Number.isNaN(Number(t)) ? Number(t) : t;
	};
	return op === 'in'
		? text
				.split(',')
				.map((x) => x.trim())
				.filter(Boolean)
				.map(one)
		: one(text);
}

/** The editor's text for a condition's value. */
export const valueText = (v: Condition['value']): string =>
	v === undefined || v === null ? '' : Array.isArray(v) ? v.map(String).join(', ') : String(v);
