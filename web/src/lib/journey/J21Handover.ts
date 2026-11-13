// J21 handover queue (D64) — pure logic shared by the fixture API and the page: the reason taxonomy, the state machine
// (`queued → assigned → waiting_customer → resolved | returned_to_agent`), SLA due times + breach state, the
// take-next order, and the queue filters. No world access: the API calls these and the tests pin them.
import type {
	HandoverItem,
	HandoverPolicyPriority,
	HandoverPriority,
	HandoverProposal,
	HandoverReason,
	HandoverSlaTargets,
	HandoverState,
	HandoverTransition,
} from '$lib/api/types/journey';
import { duration } from '$lib/format';
import { FIXTURE_NOW } from './format';

// ---- reasons ------------------------------------------------------------------------------------------------------

export const REASONS: HandoverReason[] = [
	'customer_asked',
	'identity_failed',
	'gate_blocked',
	'tool_failed',
	'not_offered',
	'success_criteria_unmet',
	'unknown_outcome',
	'out_of_scope',
	'gate_unavailable',
	'staff_decision',
];

export const REASON: Record<HandoverReason, { label: string; hint: string; chip: string; fix: string }> = {
	customer_asked: {
		label: 'Customer asked',
		hint: 'Asked for a person, or frustration stayed high (B4).',
		chip: 'accent',
		fix: 'Usually right. If it repeats on one activity, look at the answer the agent gave.',
	},
	identity_failed: {
		label: 'Identity failed',
		hint: 'The customer could not be verified to the level the activity needs.',
		chip: 'warn',
		fix: 'Check the credential route and the level the activity asks for — not the customer.',
	},
	gate_blocked: {
		label: 'Gate blocked',
		hint: 'A gate or rule stopped the reply: safety, legal, or a rule that says a person decides.',
		chip: 'err',
		fix: 'By design. Change the rule value only if the decision really is routine.',
	},
	tool_failed: {
		label: 'Tool failed',
		hint: 'A connected system answered with an error or was down; nothing was changed.',
		chip: 'tool',
		fix: 'Fix the system or route, not the agent. Return it to the agent once the system is back.',
	},
	not_offered: {
		label: 'Required action not available',
		hint: 'The request needs an operation this job does not offer, so the agent could not do it.',
		chip: 'warn',
		fix: 'A journey gap: offer the operation on the activity, or route the request elsewhere.',
	},
	success_criteria_unmet: {
		label: 'Success criteria not met',
		hint: 'Every step answered ok, but the procedure’s success criteria do not hold afterwards.',
		chip: 'err',
		fix: 'Check the system of record: the step may not have taken effect. Then fix the procedure or the criteria.',
	},
	unknown_outcome: {
		label: 'Unknown outcome',
		hint: 'A write left and no answer came back — it may or may not have happened. Never retried blindly.',
		chip: 'err',
		fix: 'Check the system of record first; act once.',
	},
	out_of_scope: {
		label: 'Out of scope',
		hint: 'No activity of the job covers the request, or there was nothing to answer from.',
		chip: 'muted',
		fix: 'Often a journey or knowledge gap — the best source of improvements.',
	},
	gate_unavailable: {
		label: 'Gate service unavailable',
		hint: 'The gate service did not answer, so the gate failed closed and a person takes the conversation.',
		chip: 'err',
		fix: 'Fix the gate service, not the agent. One item per conversation: later messages join it.',
	},
	staff_decision: {
		label: 'Staff decision',
		hint: 'A staff member handed the case over from the case page.',
		chip: 'muted',
		fix: 'Nothing to fix in the agent unless the same case type keeps being taken over.',
	},
};

// ---- states + transitions -------------------------------------------------------------------------------------------

export const STATES: HandoverState[] = [
	'queued',
	'assigned',
	'waiting_customer',
	'resolved',
	'returned_to_agent',
];

export const STATE: Record<HandoverState, { label: string; chip: string }> = {
	queued: { label: 'queued', chip: 'warn' },
	assigned: { label: 'assigned', chip: 'accent' },
	waiting_customer: { label: 'waiting on customer', chip: 'muted' },
	resolved: { label: 'resolved', chip: 'ok' },
	returned_to_agent: { label: 'returned to agent', chip: 'tool' },
};

/** Closed states: the item left the queue; no further transitions. */
export const isClosed = (s: HandoverState) => s === 'resolved' || s === 'returned_to_agent';
export const isOpen = (h: Pick<HandoverItem, 'state'>) => !isClosed(h.state);

/**
 * Allowed state changes. `queued → assigned` happens only through an assignment (assign / take next);
 * `waiting_customer → assigned` = the customer answered and the person works it again. A queued item may go straight
 * back to the agent (a person sees the agent can carry on), but is resolved only by its assignee.
 */
export const TRANSITIONS: Record<HandoverState, HandoverState[]> = {
	queued: ['assigned', 'returned_to_agent'],
	assigned: ['waiting_customer', 'resolved', 'returned_to_agent'],
	waiting_customer: ['assigned', 'resolved', 'returned_to_agent'],
	resolved: [],
	returned_to_agent: [],
};

/** Why `from → to` is refused, or `null` when it is allowed. */
export function transitionError(from: HandoverState, to: HandoverState): string | null {
	if (isClosed(from)) return 'closed';
	if (from === to) return 'same_state';
	return TRANSITIONS[from].includes(to) ? null : 'invalid_transition';
}

export interface Actor {
	sub: string;
	name: string;
}

/**
 * Move `h` to `to` (mutates), recording the transition. Stamps `first_response_at` on the first move to
 * `waiting_customer` (a person answered) and `closed_at` on a close. Throws on a refused transition — callers check
 * `transitionError` first.
 */
export function applyTransition(
	h: HandoverItem,
	to: HandoverState,
	by: Actor,
	at: string,
	note = '',
	assignee?: string | null,
): HandoverItem {
	const err = transitionError(h.state, to);
	if (err) throw new Error(`${h.id}: ${h.state} → ${to} refused (${err})`);
	const ev: HandoverTransition = { at, actor: by.sub, actor_name: by.name, from: h.state, to, note };
	if (assignee !== undefined) ev.assignee = assignee;
	h.history.push(ev);
	h.state = to;
	if (to === 'waiting_customer' && !h.first_response_at) h.first_response_at = at;
	if (isClosed(to)) h.closed_at = at;
	return h;
}

/** (Re)assign `h` to `who` (mutates). A queued item becomes `assigned`; an open one keeps its state. */
export function applyAssign(h: HandoverItem, who: Actor, by: Actor, at: string, note = ''): HandoverItem {
	if (isClosed(h.state)) throw new Error(`${h.id} is closed`);
	const to: HandoverState = h.state === 'queued' ? 'assigned' : h.state;
	h.history.push({
		at,
		actor: by.sub,
		actor_name: by.name,
		from: h.state,
		to,
		assignee: who.sub,
		note: note || (who.sub === by.sub ? `${who.name} took it` : `assigned to ${who.name}`),
	});
	h.state = to;
	h.assignee = who.sub;
	h.assignee_name = who.name;
	return h;
}

// ---- SLA ----------------------------------------------------------------------------------------------------------

/** The priorities a policy sets targets for (the editor's rows). */
export const PRIORITIES: HandoverPolicyPriority[] = ['high', 'normal', 'low'];
/** Every priority an item can carry, most pressing first (filters, sorting). */
export const ITEM_PRIORITIES: HandoverPriority[] = ['urgent', 'high', 'normal', 'low'];
const RANK: Record<HandoverPriority, number> = { urgent: 0, high: 1, normal: 2, low: 3 };
/** Sort rank of a priority: urgent first. */
export const priorityRank = (p: HandoverPriority): number => RANK[p];

/** `urgent` is half of `high` (rounded up, at least a minute), as agent-api `Targets::of`. */
export function targetOf(targets: HandoverSlaTargets | null, priority: HandoverPriority) {
	if (!targets) return null;
	if (priority !== 'urgent') return targets[priority] ?? null;
	const h = targets.high;
	const half = (m: number) => Math.max(1, Math.ceil(m / 2));
	return h ? { first_response_min: half(h.first_response_min), resolve_min: half(h.resolve_min) } : null;
}

/** Share of a target window left at which a running clock turns "at risk". */
export const AT_RISK_SHARE = 0.25;

const add = (iso: string, min: number) =>
	new Date(Date.parse(iso) + min * 60_000).toISOString().replace('.000Z', 'Z');

/** Due times for an item entering the queue at `queuedAt` under `targets` (the job's LIVE policy). */
export function dueTimes(
	targets: HandoverSlaTargets | null,
	priority: HandoverPriority,
	queuedAt: string,
): { first_response_due: string | null; resolve_due: string | null } {
	const t = targetOf(targets, priority);
	return t
		? { first_response_due: add(queuedAt, t.first_response_min), resolve_due: add(queuedAt, t.resolve_min) }
		: { first_response_due: null, resolve_due: null };
}

/** `none` no target · `running`/`at_risk` open clock · `breached` open past due · `met`/`missed` stopped clock. */
export type ClockState = 'none' | 'running' | 'at_risk' | 'breached' | 'met' | 'missed';

export interface Clock {
	state: ClockState;
	due: string | null;
	/** Minutes left (negative = overdue) on a running clock; minutes early (+) / late (−) on a stopped one. */
	left: number | null;
}

export interface SlaStatus {
	first_response: Clock;
	resolve: Clock;
	/** Either clock is `breached` (open) or `missed` (stopped late). */
	breached: boolean;
	/** Either running clock is at risk (and none breached). */
	at_risk: boolean;
}

const mins = (a: string, b: string) => Math.round((Date.parse(b) - Date.parse(a)) / 60000);

function clock(start: string, due: string | null, stoppedAt: string | null, now: string): Clock {
	if (!due) return { state: 'none', due: null, left: null };
	if (stoppedAt) {
		const left = mins(stoppedAt, due);
		return { state: left >= 0 ? 'met' : 'missed', due, left };
	}
	const left = mins(now, due);
	if (left < 0) return { state: 'breached', due, left };
	const window = Math.max(1, mins(start, due));
	return { state: left <= window * AT_RISK_SHARE ? 'at_risk' : 'running', due, left };
}

/**
 * SLA state of an item at `now`. The first-response clock stops at the first reply to the customer — or at the close
 * when the item closed without one (returned to the agent, who answers). The resolve clock stops at the close.
 */
export function slaOf(h: HandoverItem, now = FIXTURE_NOW): SlaStatus {
	const first_response = clock(h.queued_at, h.first_response_due, h.first_response_at ?? h.closed_at, now);
	const resolve = clock(h.queued_at, h.resolve_due, h.closed_at, now);
	const bad = (c: Clock) => c.state === 'breached' || c.state === 'missed';
	const breached = bad(first_response) || bad(resolve);
	return {
		first_response,
		resolve,
		breached,
		at_risk: !breached && (first_response.state === 'at_risk' || resolve.state === 'at_risk'),
	};
}

/** The clock a person acts on next: first response until answered, then resolve. */
export function activeClock(h: HandoverItem, now = FIXTURE_NOW): Clock {
	const s = slaOf(h, now);
	return h.first_response_at || isClosed(h.state) ? s.resolve : s.first_response;
}

/** Minutes as the shared human duration (`$lib/format`). */
export const dur = (m: number): string => duration(m);

/** Chip text + variant for a clock. */
export function clockText(c: Clock): { text: string; variant: 'ok' | 'warn' | 'err' | 'muted' } {
	switch (c.state) {
		case 'none':
			return { text: 'no SLA', variant: 'muted' };
		case 'running':
			return { text: `${dur(c.left!)} left`, variant: 'ok' };
		case 'at_risk':
			return { text: `${dur(c.left!)} left`, variant: 'warn' };
		case 'breached':
			return { text: `breached ${dur(c.left!)} ago`, variant: 'err' };
		case 'met':
			return { text: 'met', variant: 'ok' };
		case 'missed':
			return { text: `missed by ${dur(c.left!)}`, variant: 'err' };
	}
}

// ---- queue view -------------------------------------------------------------------------------------------------------

export type StateFilter = 'open' | HandoverState | 'all';
/** `''` any assignee · `none` unassigned · otherwise a sub. */
export type AssigneeFilter = string;
export type BreachFilter = 'any' | 'breached' | 'at_risk';

export function filterQueue(
	items: HandoverItem[],
	f: { state: StateFilter; assignee: AssigneeFilter; breach: BreachFilter; reason?: HandoverReason | null },
	now = FIXTURE_NOW,
): HandoverItem[] {
	return items.filter((h) => {
		if (f.reason && h.reason !== f.reason) return false;
		if (f.state === 'open' ? !isOpen(h) : f.state !== 'all' && h.state !== f.state) return false;
		if (f.assignee === 'none' ? !!h.assignee : f.assignee && h.assignee !== f.assignee) return false;
		if (f.breach !== 'any') {
			const s = slaOf(h, now);
			if (f.breach === 'breached' ? !s.breached : !s.at_risk) return false;
		}
		return true;
	});
}

/** Open items first (soonest active due first, no SLA last), then closed ones newest first. */
export function queueOrder(items: HandoverItem[], now = FIXTURE_NOW): HandoverItem[] {
	const key = (h: HandoverItem) => activeClock(h, now).due ?? '9999';
	return [...items].sort((a, b) =>
		isOpen(a) !== isOpen(b)
			? isOpen(a)
				? -1
				: 1
			: isOpen(a)
				? key(a).localeCompare(key(b)) ||
					RANK[a.priority] - RANK[b.priority] ||
					a.queued_at.localeCompare(b.queued_at)
				: (b.closed_at ?? '').localeCompare(a.closed_at ?? ''),
	);
}

export interface QueueSummary {
	open: number;
	queued: number;
	breached: number;
	at_risk: number;
	/** Longest wait among open items since entering the queue, minutes. */
	oldest: number | null;
	byReason: Record<HandoverReason, { open: number; total: number }>;
	/** Feedback loop: cases with a verdict, how many the agent should have handled, proposals in the draft. */
	reviewed: number;
	shouldHandle: number;
	drafted: number;
}

export function summarize(items: HandoverItem[], now = FIXTURE_NOW): QueueSummary {
	const open = items.filter(isOpen);
	const byReason = Object.fromEntries(
		REASONS.map((r) => [r, { open: 0, total: 0 }]),
	) as QueueSummary['byReason'];
	for (const h of items) {
		byReason[h.reason].total++;
		if (isOpen(h)) byReason[h.reason].open++;
	}
	const sla = open.map((h) => slaOf(h, now));
	const flagged = items.filter((h) => h.feedback?.verdict === 'agent_should_handle');
	return {
		open: open.length,
		queued: open.filter((h) => h.state === 'queued').length,
		breached: sla.filter((s) => s.breached).length,
		at_risk: sla.filter((s) => s.at_risk).length,
		oldest: open.length ? Math.max(...open.map((h) => mins(h.queued_at, now))) : null,
		byReason,
		reviewed: items.filter((h) => h.feedback).length,
		shouldHandle: flagged.length,
		drafted: flagged.filter((h) => h.feedback?.drafted_at).length,
	};
}

// ---- SLA policy (config, D54) --------------------------------------------------------------------------------------

/** Why a policy cannot be saved, or `null`. Minutes are whole, 1 min … 30 days; first response ≤ resolve. */
export function policyError(t: HandoverSlaTargets): string | null {
	for (const p of PRIORITIES) {
		const x = t[p];
		if (!x) return `${p}: missing`;
		for (const v of [x.first_response_min, x.resolve_min])
			if (!Number.isInteger(v) || v < 1 || v > 43_200) return `${p}: minutes must be whole, 1 to 43200`;
		if (x.first_response_min > x.resolve_min) return `${p}: first response must come before resolve`;
	}
	return null;
}

export function policyText(t: HandoverSlaTargets | null): string {
	if (!t) return 'no SLA';
	return PRIORITIES.map((p) => `${p} ${dur(t[p].first_response_min)} / ${dur(t[p].resolve_min)}`).join(' · ');
}

// ---- feedback loop -----------------------------------------------------------------------------------------------------

/** A proposal the queue can save to the job's draft itself (journey refinements); others link to their page. */
export const draftable = (p: HandoverProposal | null | undefined) =>
	!!p && !!p.activity && (p.kind === 'tenant_extension' || p.kind === 'enable_activity');

export const PROPOSAL_KIND: Record<HandoverProposal['kind'], string> = {
	tenant_extension: 'New activity (journey extension)',
	enable_activity: 'Enable a hidden activity',
	knowledge_gap: 'Knowledge gap',
	gate_threshold: 'Gate / answer review',
};
