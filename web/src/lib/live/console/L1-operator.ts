// L1 operator console — what the operator reads off a conversation: its turns (each customer message and what the
// agents did with it: qualifier → procedure → post-gate → reply), the phase it is in, and whether a person has to
// step in now. Pure functions over ConsoleConversation, unit-tested in tests/unit/live-L1.test.ts.
import type { ConsoleConversation } from '$lib/api/types/live';
import type { CaseStatus, TimelineEvent } from '$lib/api/types/runtime';

type Ev<K extends TimelineEvent['kind']> = Extract<TimelineEvent, { kind: K }>;

/** One turn: a customer (or staff) message and everything the agents did until the next customer message. */
export interface Turn {
	n: number;
	at: string;
	customer: Ev<'message'> | null;
	staff: Ev<'message'>[];
	/** P1/P2 gate verdicts and the qualification (activity, object, modality, slots). */
	qualifier: { gates: Ev<'gate'>[]; qualified: Ev<'qualified'> | null };
	/** Operations the procedure called (ids + outcome codes, K50) and the knowledge it searched. */
	procedure: { ops: Ev<'operation'>[]; knowledge: Ev<'knowledge'>[] };
	/** R1: the reply checked against the facts it was built from. */
	postGate: Ev<'gate'>[];
	replies: Ev<'reply'>[];
	states: Ev<'state'>[];
	alerts: Ev<'alert'>[];
}

export function turnsOf(events: TimelineEvent[]): Turn[] {
	const turns: Turn[] = [];
	const fresh = (at: string, customer: Ev<'message'> | null): Turn => ({
		n: turns.length + 1,
		at,
		customer,
		staff: [],
		qualifier: { gates: [], qualified: null },
		procedure: { ops: [], knowledge: [] },
		postGate: [],
		replies: [],
		states: [],
		alerts: [],
	});
	let cur: Turn | null = null;
	const open = (at: string) => {
		if (!cur) {
			cur = fresh(at, null);
			turns.push(cur);
		}
		return cur;
	};
	for (const e of events) {
		if (e.kind === 'message' && e.from === 'customer') {
			cur = fresh(e.at, e);
			turns.push(cur);
			continue;
		}
		const t = open(e.at);
		switch (e.kind) {
			case 'message':
				t.staff.push(e);
				break;
			case 'gate':
				if (e.point === 'R1') t.postGate.push(e);
				else t.qualifier.gates.push(e);
				break;
			case 'qualified':
				t.qualifier.qualified = e;
				break;
			case 'operation':
				t.procedure.ops.push(e);
				break;
			case 'knowledge':
				t.procedure.knowledge.push(e);
				break;
			case 'reply':
				t.replies.push(e);
				break;
			case 'state':
				t.states.push(e);
				break;
			case 'alert':
				t.alerts.push(e);
				break;
		}
	}
	return turns;
}

export type Phase = 'needs_you' | 'staff' | 'working' | 'waiting' | 'done';

export const PHASE: Record<Phase, { label: string; hint: string }> = {
	needs_you: { label: 'Needs you', hint: 'The agent stopped or cannot know the outcome; a person decides.' },
	staff: {
		label: 'Staff answering',
		hint: 'A colleague has it: the agent is silent until it is handed back.',
	},
	working: {
		label: 'Agent working',
		hint: 'The agent is mid-turn: qualifying, calling operations, replying.',
	},
	waiting: { label: 'Waiting on customer', hint: 'The agent replied and waits for the customer.' },
	done: { label: 'Finished', hint: 'Resolved or not eligible; nothing left to do.' },
};
export const PHASE_ORDER: Phase[] = ['needs_you', 'staff', 'working', 'waiting', 'done'];

export interface Attention {
	severity: 'err' | 'warn';
	kind: 'unknown_outcome' | 'failed' | 'dead_letter' | 'handover' | 'gate_stop';
	title: string;
	/** What the operator should do, in one sentence. */
	hint: string;
}

const ESCALATION: Record<string, string> = {
	frustration: 'the customer is frustrated (asked twice)',
	customer_request: 'the customer asked for a person',
	human_request: 'the customer asked for a person',
	safety: 'a safety gate fired (a person handles this)',
	upstream_unavailable: 'a system the agent needs is down',
	unknown_outcome: 'an operation outcome is unknown',
	not_offered: 'the request needs an action this job does not offer',
	success_criteria_unmet: 'the steps ran but the success criteria were not met',
};
export const escalationLabel = (r: string | null) =>
	r ? (ESCALATION[r] ?? r.replaceAll('_', ' ')) : 'the agent handed over';

const FINISHED: CaseStatus[] = ['resolved', 'not_eligible', 'cancelled_offboard'];

/** Whether a staff member holds the conversation. */
export const heldByStaff = (c: ConsoleConversation) => c.owner.owner.startsWith('user:');

/** Whether a person has to step in now, and why. Nothing while a staff member holds the conversation. */
export function attentionOf(c: ConsoleConversation): Attention | null {
	if (heldByStaff(c)) return null;
	const ops = c.events.filter((e): e is Ev<'operation'> => e.kind === 'operation');
	const unknown = ops.find((o) => o.state === 'unknown_outcome' || o.outcome === 'unknown_outcome');
	if (c.case.alert === 'unknown_outcome' || unknown) {
		const op = unknown?.op ?? 'An operation';
		const where = unknown?.instance ? ` in ${unknown.instance}` : '';
		return {
			severity: 'err',
			kind: 'unknown_outcome',
			title: `Unknown outcome: ${op}`,
			hint: `It may have happened${where}. Check there before anyone tells the customer — it is never retried automatically.`,
		};
	}
	const alert = [...c.events].reverse().find((e): e is Ev<'alert'> => e.kind === 'alert');
	if (alert)
		return {
			severity: 'err',
			kind: 'dead_letter',
			title: `Stopped: ${alert.code.replaceAll('_', ' ')}`,
			hint: `${alert.text} Open it in the workspace to answer the customer yourself.`,
		};
	if (c.case.status === 'failed') {
		const note = [...c.events].reverse().find((e): e is Ev<'state'> => e.kind === 'state');
		return {
			severity: 'err',
			kind: 'failed',
			title: 'The case failed',
			hint: `${note?.note ?? 'The agent could not finish.'} Open it in the workspace to answer the customer.`,
		};
	}
	if (c.case.status === 'handover')
		return {
			severity: 'warn',
			kind: 'handover',
			title: 'Handover requested',
			hint: `Because ${escalationLabel(c.case.escalation_reason)}.${c.case.sla ? ` Answer within ${c.case.sla.hours} h.` : ''}`,
		};
	const lastGate = [...c.events].reverse().find((e): e is Ev<'gate'> => e.kind === 'gate');
	if (
		lastGate &&
		(lastGate.verdict === 'block' || lastGate.verdict === 'handover') &&
		!FINISHED.includes(c.case.status)
	)
		return {
			severity: 'warn',
			kind: 'gate_stop',
			title:
				lastGate.verdict === 'handover'
					? `Handing over (${lastGate.point} gate)`
					: `Blocked by the ${lastGate.point} gate`,
			hint: lastGate.note ?? 'A gate stopped the agent on this turn.',
		};
	return null;
}

export function phaseOf(c: ConsoleConversation): Phase {
	if (heldByStaff(c)) return 'staff';
	if (attentionOf(c)) return 'needs_you';
	if (FINISHED.includes(c.case.status)) return 'done';
	const last = c.events.at(-1);
	if (
		c.active &&
		last &&
		!(last.kind === 'reply' || last.kind === 'state' || (last.kind === 'gate' && last.point === 'R1'))
	)
		return 'working';
	return 'waiting';
}

/** Severity first (errors, then warnings), then the oldest problem first; the rest by latest activity. */
export function byUrgency(a: ConsoleConversation, b: ConsoleConversation): number {
	const pa = PHASE_ORDER.indexOf(phaseOf(a));
	const pb = PHASE_ORDER.indexOf(phaseOf(b));
	if (pa !== pb) return pa - pb;
	if (pa === 0) {
		const sa = attentionOf(a)!.severity === 'err' ? 0 : 1;
		const sb = attentionOf(b)!.severity === 'err' ? 0 : 1;
		if (sa !== sb) return sa - sb;
	}
	return a.case.updated_at < b.case.updated_at ? 1 : a.case.updated_at > b.case.updated_at ? -1 : 0;
}

/** The agent step the latest turn is in, while the agent works (for the "working…" marker). */
export function stepOf(t: Turn): 'qualifier' | 'procedure' | 'post_gate' | 'reply' {
	if (t.replies.length || t.postGate.length) return 'post_gate';
	if (t.procedure.ops.length || t.procedure.knowledge.length) return 'procedure';
	if (t.qualifier.qualified) return 'procedure';
	return 'qualifier';
}

/** Gate verdicts worth showing: below threshold, or a non-pass verdict's signals. */
export const notable = (g: Ev<'gate'>) => g.verdicts.filter((v) => v.below_threshold);

export const lastText = (c: ConsoleConversation): string => {
	const m = [...c.events].reverse().find((e) => e.kind === 'message' || e.kind === 'reply');
	if (!m) return '';
	if (m.kind === 'reply') return `Agent: ${m.text}`;
	if (m.kind === 'message') return m.from === 'staff' ? `${m.author}: ${m.text}` : m.text;
	return '';
};

/** Local wall-clock time of an event (the operator's clock). */
export const hhmmss = (iso: string) =>
	Number.isNaN(Date.parse(iso)) ? '' : new Date(iso).toLocaleTimeString('en-GB', { hour12: false });
