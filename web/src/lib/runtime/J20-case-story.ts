// J20 Case detail — the case as the agent experienced it: turns (a customer message and everything the agent did
// before the next one), each operation's side-effect class and canonical outcome family (doc 10 §3.1 taxonomy,
// 12 §3.3), object state changes, how the case ended and — for `unknown_outcome` — what a person must do.
// Pure functions over `CaseDetail`; owned by the J20 page.
import type { CaseDetail, ObjectChange, OpStep, TimelineEvent } from '$lib/api/types/runtime';

export type SideEffect = 'R' | 'W' | 'X' | 'P';

/** Side-effect class per operation (doc 10 §3.1, 11 §3.2). Variants (`{…}`) share their verb's class. */
const SIDE: Record<string, SideEffect> = {
	'payment.refund': 'X',
	'payment.link': 'P',
	'order.cancel': 'W',
	'order.modify': 'W',
	'case.create': 'W',
	'case.update': 'W',
	'profile.update': 'W',
	'interaction.send': 'W',
	'delivery.rebook': 'W',
};

export function sideEffectOf(op: string): SideEffect {
	const base = op.replace(/\{.*\}$/, '');
	return SIDE[base] ?? 'R';
}

export const SIDE_LABEL: Record<SideEffect, string> = {
	R: 'read',
	W: 'write',
	X: 'irreversible write',
	P: 'payment',
};

export type OutcomeFamily = 'ok' | 'user' | 'gate' | 'system' | 'unknown';

export interface OutcomeClass {
	family: OutcomeFamily;
	/** Canonical code (taxonomy), e.g. `ineligible`, `invalid_state`, `timeout`. */
	code: string;
	/** What it means for the case, in one line. */
	meaning: string;
}

export const FAMILY_LABEL: Record<OutcomeFamily, string> = {
	ok: 'ok',
	user: 'customer outcome',
	gate: 'refused by a check',
	system: 'system error',
	unknown: 'unknown outcome',
};

/** Map a call's outcome code onto the common taxonomy (doc 10 §3.1): user outcomes get a reply, system errors
 *  escalate, gate refusals come from the runner's checks (12 §3.3), `unknown_outcome` is never retried. */
export function outcomeClass(outcome: string): OutcomeClass {
	const [head = outcome, arg] = outcome.split(':');
	switch (head) {
		case 'ok':
		case 'eligible':
		case 'deep_link':
			return { family: 'ok', code: head, meaning: arg ? `ok (${arg.replace('_', ' ')})` : 'done' };
		case 'not_eligible':
		case 'ineligible':
			return {
				family: 'user',
				code: 'ineligible',
				meaning: 'a rule said no — the customer gets the reason',
			};
		case 'invalid_state':
			return {
				family: 'user',
				code: 'invalid_state',
				meaning: `not possible while the object is ${arg?.replace('_', ' ') ?? 'in this state'}`,
			};
		case 'not_found':
		case 'ambiguous':
		case 'invalid_input':
		case 'duplicate':
			return { family: 'user', code: head, meaning: 'answered to the customer, no escalation' };
		case 'identity_required':
			return { family: 'gate', code: head, meaning: 'identity level too low — the agent asks to verify' };
		case 'confirmation_required':
			return { family: 'gate', code: head, meaning: 'no customer “yes” yet — nothing was changed' };
		case 'not_entitled':
			return { family: 'gate', code: head, meaning: 'not in this job’s toolset — refused, audited' };
		case 'not_offered':
			return { family: 'gate', code: head, meaning: 'no route for this operation — not offered' };
		case 'route_changed':
			return { family: 'gate', code: head, meaning: 'routing changed mid-case — re-planned' };
		case 'case_cap_reached':
		case 'rate_limited':
			return { family: 'system', code: head, meaning: 'limit hit — escalates' };
		case 'unknown_outcome':
			return {
				family: 'unknown',
				code: head,
				meaning: 'the write was sent but never answered — a person verifies before any retry',
			};
		default:
			return { family: 'system', code: head, meaning: 'system error — escalates to staff' };
	}
}

export type Op = Extract<TimelineEvent, { kind: 'operation' }>;
export type Msg = Extract<TimelineEvent, { kind: 'message' }>;
export type Gate = Extract<TimelineEvent, { kind: 'gate' }>;

export interface Turn {
	n: number;
	/** The message that opened the turn (null for steps before the first message). */
	opener: Msg | null;
	/** Everything after the opener, in order, up to the next message. */
	steps: TimelineEvent[];
	/** Activity · object · modality the agent settled on in this turn, if it qualified the message. */
	qualified: { activity: string; object: string; modality: string } | null;
	ops: Op[];
	/** Worst outcome family among the turn's calls. */
	worst: OutcomeFamily | null;
}

const RANK: OutcomeFamily[] = ['ok', 'user', 'gate', 'system', 'unknown'];

/** Split the timeline into turns: each customer/staff message starts one. */
export function turnsOf(d: CaseDetail): Turn[] {
	const turns: Turn[] = [];
	let cur: Turn | null = null;
	for (const e of d.timeline) {
		if (e.kind === 'message' || !cur) {
			cur = {
				n: turns.length + 1,
				opener: e.kind === 'message' ? e : null,
				steps: [],
				qualified: null,
				ops: [],
				worst: null,
			};
			turns.push(cur);
			if (e.kind === 'message') continue;
		}
		cur.steps.push(e);
	}
	for (const t of turns) {
		t.ops = t.steps.filter((e): e is Op => e.kind === 'operation');
		const q = t.steps.find((e) => e.kind === 'qualified');
		if (q && q.kind === 'qualified')
			t.qualified = { activity: q.activity, object: q.object, modality: q.modality };
		else {
			const p1 = t.steps.find((e): e is Gate => e.kind === 'gate' && e.point === 'P1');
			const v = (k: string) => p1?.verdicts.find((x) => x.key === k)?.value;
			if (v('GA')) t.qualified = { activity: v('GA')!, object: v('GO') ?? '—', modality: v('GM') ?? '—' };
		}
		for (const o of t.ops) {
			const f = o.state === 'unknown_outcome' ? 'unknown' : outcomeClass(o.outcome).family;
			if (!t.worst || RANK.indexOf(f) > RANK.indexOf(t.worst)) t.worst = f;
		}
	}
	return turns;
}

/** The procedures (name@version) whose steps issued calls in this case, in first-use order. */
export function proceduresOf(d: CaseDetail): (OpStep & { steps: number[] })[] {
	const out = new Map<string, OpStep & { steps: number[] }>();
	for (const e of d.timeline) {
		if (e.kind !== 'operation' || !e.step) continue;
		const k = `${e.step.procedure}@${e.step.version}`;
		const row = out.get(k) ?? { ...e.step, steps: [] };
		if (!row.steps.includes(e.step.n)) row.steps.push(e.step.n);
		out.set(k, row);
	}
	return [...out.values()];
}

/** Object state changes the case caused, in order (writes only). */
export function objectChangesOf(d: CaseDetail): (ObjectChange & { op: string; at: string })[] {
	return d.timeline.flatMap((e) =>
		e.kind === 'operation' && e.object_change ? [{ ...e.object_change, op: e.op, at: e.at }] : [],
	);
}

/** Customer message a write's confirm_ref points at. */
export function confirmMessage(d: CaseDetail, ref: string | undefined): Msg | null {
	if (!ref) return null;
	return d.timeline.find((e): e is Msg => e.kind === 'message' && e.msg_id === ref) ?? null;
}

export type EndingKind = 'contained' | 'handover' | 'alert' | 'waiting' | 'declined' | 'cancelled';

export interface Ending {
	kind: EndingKind;
	headline: string;
	why: string;
}

const REASON: Record<string, string> = {
	frustration: 'the customer’s frustration stayed high on two turns (gate B4)',
	human_request: 'the customer asked for a person',
	upstream_unavailable: 'a system the agent needed was down',
	safety: 'a safety question (B1–B8) fired — a person always takes these',
	unknown_outcome: 'a write was sent and never answered',
	not_offered: 'the request needs an action this job does not offer',
	success_criteria_unmet: 'every step answered ok but the success criteria were not met',
	staff_decision: 'a staff member handed it over',
};

/** How the case ended (or what it waits for), in plain words. */
export function endingOf(d: CaseDetail): Ending {
	const c = d.case;
	const reason = c.escalation_reason
		? (REASON[c.escalation_reason] ?? c.escalation_reason.replaceAll('_', ' '))
		: '';
	if (c.alert)
		return {
			kind: 'alert',
			headline: 'Stopped — needs a person to check a system',
			why: `${reason[0]?.toUpperCase() ?? ''}${reason.slice(1)}. The agent told the customer honestly and handed the check to staff.`,
		};
	switch (c.status) {
		case 'resolved':
			return {
				kind: 'contained',
				headline: c.env === 'test' ? 'Answered in the test chat' : 'Contained — the agent finished it',
				why: 'No person was needed. Every claim in the reply was backed by an ok operation or a cited passage.',
			};
		case 'handover':
			return {
				kind: 'handover',
				headline: 'Handed over to staff',
				why: `Why: ${reason || 'handover'}.${c.sla ? ` SLA ${c.sla.hours} h.` : ''}`,
			};
		case 'not_eligible':
			return {
				kind: 'declined',
				headline: 'Answered — not eligible',
				why: `A rule said no (${c.outcome ?? 'rule outcome'}); the customer got the reason.`,
			};
		case 'cancelled_offboard':
			return {
				kind: 'cancelled',
				headline: 'Cancelled — tenant offboarded',
				why: 'The tenant was suspended while the case was open; the customer got the unavailable reply.',
			};
		case 'failed':
			return { kind: 'alert', headline: 'Failed', why: reason || (c.outcome ?? 'failed') };
		default:
			return {
				kind: 'waiting',
				headline: 'Still open — waiting',
				why:
					c.status === 'identity_required'
						? 'Waiting for the customer to verify their identity.'
						: c.status === 'confirmation_required'
							? 'Waiting for the customer’s “yes” before the write runs.'
							: 'Waiting for the customer or an outside event.',
			};
	}
}

export interface UnknownOutcomeTask {
	call: Op;
	side: SideEffect;
	key: string | null;
	confirmedBy: Msg | null;
	change: ObjectChange | null;
}

/** The open `unknown_outcome` call a person must verify, or null. */
export function unknownOutcomeTask(d: CaseDetail): UnknownOutcomeTask | null {
	if (!d.case.alert) return null;
	const call = [...d.timeline]
		.reverse()
		.find((e): e is Op => e.kind === 'operation' && e.state === 'unknown_outcome');
	if (!call) return null;
	return {
		call,
		side: sideEffectOf(call.op),
		key: /idempotency key (\S+)/.exec(call.detail ?? '')?.[1] ?? null,
		confirmedBy: confirmMessage(d, call.confirm_ref),
		change: call.object_change ?? null,
	};
}
