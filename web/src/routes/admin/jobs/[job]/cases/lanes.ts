// Timeline lanes for the case detail (J20). Adapted from cb:web/src/routes/admin/jobs/[job]/cases/lanes.ts@45ee128:
// the bus-lane trace (customer / worker / tool bus) became a lane per step kind of the interaction layer — the
// customer conversation, the gates, knowledge, operation calls and the case's own state changes.
import type { CaseDetail, CaseStatus, TimelineEvent } from '$lib/api/types/runtime';

export type Lane = 'conversation' | 'gate' | 'knowledge' | 'operation' | 'case';

export function laneOf(e: TimelineEvent): Lane {
	switch (e.kind) {
		case 'message':
		case 'reply':
			return 'conversation';
		case 'gate':
			return 'gate';
		case 'knowledge':
			return 'knowledge';
		case 'operation':
			return 'operation';
		default:
			return 'case';
	}
}

export const LANE_LABEL: Record<Lane, string> = {
	conversation: 'Conversation',
	gate: 'Gates',
	knowledge: 'Knowledge',
	operation: 'Operations',
	case: 'Case',
};

export const LANE_VARIANT: Record<Lane, 'cust' | 'accent' | 'work' | 'tool' | 'muted'> = {
	conversation: 'cust',
	gate: 'accent',
	knowledge: 'work',
	operation: 'tool',
	case: 'muted',
};

export const GATE_POINT: Record<'P1' | 'P2' | 'R1', string> = {
	P1: 'Pre-gate P1 · qualify the message',
	P2: 'Pre-gate P2 · pick the record',
	R1: 'Post-gate R1 · check the reply',
};

/** Seconds since the case started, as `+12 s` / `+18 min`. */
export function offset(start: string, at: string): string {
	const s = Math.max(0, Math.round((Date.parse(at) - Date.parse(start)) / 1000));
	if (s < 60) return `+${s} s`;
	if (s < 3600) return `+${Math.round(s / 60)} min`;
	return `+${(s / 3600).toFixed(1)} h`;
}

/** How many of each lane the timeline holds (the filter chips above it). */
export function laneCounts(d: CaseDetail): Record<Lane, number> {
	const out: Record<Lane, number> = { conversation: 0, gate: 0, knowledge: 0, operation: 0, case: 0 };
	for (const e of d.timeline) out[laneOf(e)] += 1;
	return out;
}

/** The banner a case's status warrants: what the case waits for, or why it stopped. */
export function statusBanner(
	d: CaseDetail,
): { kind: 'gap' | 'info' | 'err'; title: string; text: string } | null {
	const c = d.case;
	const map: Partial<Record<CaseStatus, { kind: 'gap' | 'info' | 'err'; title: string; text: string }>> = {
		identity_required: {
			kind: 'gap',
			title: 'identity_required',
			text: `The next operation needs a higher identity level than ${c.identity_level}. The agent asked the customer to verify; the case continues when they answer.`,
		},
		confirmation_required: {
			kind: 'gap',
			title: 'confirmation_required',
			text: 'A write waits for the customer’s explicit “yes” (confirm-back). Nothing is changed until the confirm_ref is answered.',
		},
		handover: {
			kind: 'info',
			title: 'With staff',
			text: `Escalated (${c.escalation_reason?.replaceAll('_', ' ') ?? 'handover'})${c.sla ? ` · SLA ${c.sla.hours} h, due ${c.sla.due_at.slice(11, 16)} UTC` : ''}.`,
		},
		not_eligible: {
			kind: 'info',
			title: 'Not eligible',
			text: `The request was answered but could not be granted: ${c.outcome ?? 'rule outcome'}.`,
		},
		cancelled_offboard: {
			kind: 'info',
			title: 'cancelled_offboard',
			text: 'The tenant was suspended or offboarded while this case was open; it was cancelled and the customer got the unavailable reply.',
		},
	};
	if (c.alert)
		return {
			kind: 'err',
			title: c.alert,
			text: 'A write did not answer after the request was sent. It is never retried automatically: a person checks the system first.',
		};
	return map[c.status] ?? null;
}
