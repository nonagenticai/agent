// J19 Cases — the job's live caseload. A case is always held by someone: the agent works it, it waits on the
// customer (verify identity, confirm a write), staff own it after a handover, or it stopped (failed / a write with
// an unknown outcome, never retried automatically). The page leads with the cases that need a person now.
// Pure helpers; owned by the J19 page (runtime area). Spec 03 §2.7 (release per case), 14 §2 J19, D54 (test = draft).
import type { AgentFleet, CaseRow } from '$lib/api/types/runtime';
import { duration } from '$lib/format';

export type Holder = 'agent' | 'customer' | 'staff' | 'failed' | 'closed';
export type ActiveHolder = Exclude<Holder, 'closed'>;

/** Who holds the case right now. A raised alert (`unknown_outcome`) puts any case in the failed lane. */
export function holderOf(c: Pick<CaseRow, 'status' | 'alert'>): Holder {
	if (c.alert || c.status === 'failed') return 'failed';
	if (c.status === 'identity_required' || c.status === 'confirmation_required') return 'customer';
	if (c.status === 'handover') return 'staff';
	if (c.status === 'open') return 'agent';
	return 'closed';
}

export interface LaneDef {
	key: ActiveHolder;
	label: string;
	/** What it means that the case sits here, in the operator's words. */
	hint: string;
	/** Who acts next. */
	next: string;
	tone: 'accent' | 'warn' | 'work' | 'err';
}

/** Same lanes and words as J1 "Right now", in the order a case travels. */
export const LANES: LaneDef[] = [
	{
		key: 'agent',
		label: 'Agent working',
		hint: 'The agent is answering.',
		next: 'Nobody needs to act.',
		tone: 'accent',
	},
	{
		key: 'customer',
		label: 'Waiting on customer',
		hint: 'Asked to verify identity or confirm a change.',
		next: 'Continues when the customer answers.',
		tone: 'warn',
	},
	{
		key: 'staff',
		label: 'With staff',
		hint: 'Handed over to a person, with an SLA.',
		next: 'A staff member takes it and replies.',
		tone: 'work',
	},
	{
		key: 'failed',
		label: 'Failed',
		hint: 'Stopped on an error or an unknown write outcome.',
		next: 'A person checks the system. Never retried automatically.',
		tone: 'err',
	},
];

/** Status filter values (J1 links `?status=`) → the lane they belong to. */
export function laneOfStatus(status: string): ActiveHolder | null {
	if (status === 'all' || !status) return null;
	const h = holderOf({ status: status as CaseRow['status'], alert: null });
	return h === 'closed' ? null : h;
}

export function byLane(rows: CaseRow[]): Record<ActiveHolder, CaseRow[]> {
	const out: Record<ActiveHolder, CaseRow[]> = { agent: [], customer: [], staff: [], failed: [] };
	for (const c of rows) {
		const h = holderOf(c);
		if (h !== 'closed') out[h].push(c);
	}
	return out;
}

/** The shared human duration (`$lib/format`): `95` → `1 h 35 min`, `3000` → `2 d 2 h`. */
export { duration };

export const minutesBetween = (from: string, to: string) => (Date.parse(to) - Date.parse(from)) / 60_000;

export interface SlaState {
	kind: 'overdue' | 'due_soon' | 'ok';
	/** Minutes left (negative = overdue). */
	left: number;
	text: string;
}

/** SLA of a handed-over case at `asOf`: overdue, due within the hour, or on time. Null without an SLA. */
export function slaOf(c: Pick<CaseRow, 'sla'>, asOf: string): SlaState | null {
	if (!c.sla) return null;
	const left = minutesBetween(asOf, c.sla.due_at);
	if (left < 0) return { kind: 'overdue', left, text: `SLA overdue ${duration(left)}` };
	return { kind: left <= 60 ? 'due_soon' : 'ok', left, text: `SLA due in ${duration(left)}` };
}

export type Urgency = 'now' | 'soon' | 'chase';

export interface PersonItem {
	c: CaseRow;
	urgency: Urgency;
	/** One sentence: why a person is needed. */
	why: string;
	/** What the person does here. */
	action: 'claim' | 'check' | 'chase';
	sla: SlaState | null;
	rank: number;
}

/**
 * The cases that need a person now, most urgent first:
 * 1 unclaimed handovers past their SLA · 2 writes with an unknown outcome (a person must check the system before
 * anything is retried) · 3 unassigned handovers still in SLA · 4 other failed cases · 5 assigned handovers past SLA.
 * Cases the agent or the customer hold never appear: nobody on staff can move them.
 */
export function needsPerson(rows: CaseRow[], asOf: string): PersonItem[] {
	const out: PersonItem[] = [];
	for (const c of rows) {
		const h = holderOf(c);
		const sla = slaOf(c, asOf);
		const reason = (c.escalation_reason ?? 'handover').replaceAll('_', ' ');
		if (h === 'staff' && !c.assignee) {
			const late = sla?.kind === 'overdue';
			out.push({
				c,
				urgency: late ? 'now' : 'soon',
				why: `Handed over (${reason}) and nobody has taken it.`,
				action: 'claim',
				sla,
				rank: late ? 1 : 3,
			});
		} else if (h === 'failed') {
			const unknown = c.alert === 'unknown_outcome' || c.outcome === 'unknown_outcome';
			out.push({
				c,
				urgency: unknown ? 'now' : 'soon',
				why: unknown
					? 'A write was sent but never answered. Check the system before anything is retried.'
					: `Stopped with ${c.outcome ?? c.alert ?? 'an error'}. Check what the customer was told.`,
				action: 'check',
				sla,
				rank: unknown ? 2 : 4,
			});
		} else if (h === 'staff' && sla?.kind === 'overdue') {
			out.push({
				c,
				urgency: 'chase',
				why: `Held by ${c.assignee_name ?? 'a staff member'}, but the customer is still waiting.`,
				action: 'chase',
				sla,
				rank: 5,
			});
		}
	}
	return out.sort((a, b) => a.rank - b.rank || (a.sla?.left ?? 0) - (b.sla?.left ?? 0));
}

export const URGENCY_LABEL: Record<Urgency, string> = { now: 'Now', soon: 'Soon', chase: 'Chase' };

export interface ReleaseNote {
	kind: 'current' | 'older' | 'draft';
	/** Short badge text. */
	text: string;
	/** Tooltip / sentence. */
	title: string;
}

/**
 * What the case ran on (03 §2.7, D63: pinned per case). Production cases ran on a release, current or older;
 * test-env cases ran on the draft at a draft revision (D54).
 */
export function releaseNote(
	c: Pick<CaseRow, 'env' | 'release_id' | 'draft_rev'>,
	fleet: AgentFleet | null,
): ReleaseNote {
	if (c.env === 'test' || !c.release_id)
		return {
			kind: 'draft',
			text: 'draft',
			title: `Test environment: ran on the draft (rev ${c.draft_rev ?? '?'}). Nothing it did reached production.`,
		};
	const current = fleet?.production.release?.id ?? null;
	if (current && c.release_id !== current)
		return {
			kind: 'older',
			text: 'older release',
			title: `Ran on ${c.release_id}. Production has run ${current} since; this case keeps the release it started on.`,
		};
	return {
		kind: 'current',
		text: 'current',
		title: `Ran on the release production runs now: ${c.release_id}.`,
	};
}

export const IDENTITY_SHORT = ['Public', 'Holds an identifier', 'Verified', 'Step-up'] as const;

/** Identity level as `L2 · Verified`. */
export const identityText = (level: number) => `L${level} · ${IDENTITY_SHORT[level] ?? '?'}`;

/** Cases that are new or moved (status, holder, assignee or last update) between two snapshots. */
export function changedIds(prev: CaseRow[] | null, next: CaseRow[]): string[] {
	if (!prev) return [];
	const before = new Map(
		prev.map((c) => [c.case_id, `${c.status}|${c.updated_at}|${c.assignee ?? ''}|${c.alert ?? ''}`]),
	);
	return next
		.filter(
			(c) => before.get(c.case_id) !== `${c.status}|${c.updated_at}|${c.assignee ?? ''}|${c.alert ?? ''}`,
		)
		.map((c) => c.case_id);
}
