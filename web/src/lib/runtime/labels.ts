// Display helpers shared by the runtime pages (J1, J2, J19–J25). Owned by the runtime area agent.
// Adapted from care-bus-agents `routes/admin/jobs/[job]/cases/lanes.ts` and `buses/labels.ts` @45ee128, rebased
// onto the fixture shapes (no console/bus catalogue import: that module belongs to the live area).
import type { CaseStatus, GroupInfo, OpCallState, RouteKind, StreamInfo } from '$lib/api/types/runtime';

const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];

/** `2026-10-18T10:24:30Z` → `18 Oct 10:24` (UTC, deterministic). */
export function when(iso: string | null | undefined): string {
	if (!iso) return '—';
	const m = /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2})/.exec(iso);
	if (!m) return iso;
	return `${Number(m[3])} ${MONTHS[Number(m[2]) - 1]} ${m[4]}:${m[5]}`;
}

/** `10:24:30` part of an ISO time. */
export function clock(iso: string): string {
	return /T(\d{2}:\d{2}:\d{2})/.exec(iso)?.[1] ?? iso;
}

/** Time part of a stream entry id (`<ms>-<seq>`), or '' when it carries none. */
export function entryTime(id: string | null): string {
	if (!id) return '';
	const ms = Number(id.split('-')[0]);
	return Number.isFinite(ms) && ms > 0 ? when(new Date(ms).toISOString()) : '';
}

export const STATUS_LABEL: Record<CaseStatus, string> = {
	open: 'Open',
	identity_required: 'Identity required',
	confirmation_required: 'Confirmation required',
	handover: 'With staff',
	resolved: 'Resolved',
	not_eligible: 'Not eligible',
	failed: 'Failed',
	cancelled_offboard: 'Cancelled (offboard)',
};

/** Status word for the `Status` chip colour. */
export const STATUS_WORD: Record<CaseStatus, string> = {
	open: 'open',
	identity_required: 'warn',
	confirmation_required: 'warn',
	handover: 'running',
	resolved: 'ok',
	not_eligible: 'not_eligible',
	failed: 'failed',
	cancelled_offboard: 'cancelled',
};

export const CASE_STATUSES: CaseStatus[] = [
	'open',
	'identity_required',
	'confirmation_required',
	'handover',
	'resolved',
	'not_eligible',
	'failed',
	'cancelled_offboard',
];

export const ROUTE_LABEL: Record<RouteKind, string> = {
	instance: 'instance',
	builtin: 'builtin',
	fallback: 'fallback',
	deep_link: 'deep link',
	not_offered: 'not offered',
};

export const ROUTE_VARIANT: Record<RouteKind, 'tool' | 'accent' | 'warn' | 'cust' | 'muted'> = {
	instance: 'tool',
	builtin: 'accent',
	fallback: 'warn',
	deep_link: 'cust',
	not_offered: 'muted',
};

export const CALL_STATE_WORD: Record<OpCallState, string> = {
	pending: 'running',
	ok: 'ok',
	error: 'failed',
	unknown_outcome: 'err',
	denied: 'rejected',
};

export const CALL_STATE_LABEL: Record<OpCallState, string> = {
	pending: 'Pending',
	ok: 'OK',
	error: 'Error',
	unknown_outcome: 'Unknown outcome',
	denied: 'Denied',
};

/** `stage:activity` → `Stage · activity words`. */
export function activityLabel(a: string): string {
	const [stage, act] = a.split(':');
	if (!act) return a.replaceAll('_', ' ');
	return `${stage![0]!.toUpperCase()}${stage!.slice(1)} · ${act.replaceAll('_', ' ')}`;
}

/** `orchestrator` or `worker-order` → a person-readable agent name. */
export function agentName(id: string): string {
	if (id === 'agent-api-recorder') return 'Case history recorder';
	if (id === 'interaction') return 'Interaction agent';
	if (id === 'qualifier') return 'Pre-gate (P1/P2)';
	if (id === 'post-gate') return 'Post-gate (R1)';
	if (id === 'orchestrator') return 'Orchestrator agent';
	if (id === 'tool-executor') return 'Tool executor';
	if (id.startsWith('worker-')) return `${id.slice(7).replaceAll('-', ' ')} worker`;
	if (id.startsWith('worker:')) return `${id.slice(7)} worker`;
	return id;
}

/** Human name for a stream key: bus:customer:{area}, bus:worker:{area}:{x}, bus:tool:{system}, bus:dead. */
export function streamLabel(key: string): string {
	const p = key.split(':');
	if (p[0] !== 'bus') return key;
	const area = (s: string) => s.replaceAll('-', ' ').replace(/^./, (c) => c.toUpperCase());
	if (p[1] === 'customer' && p.length === 3) return `${area(p[2]!)} · customer bus`;
	if (p[1] === 'worker' && p.length === 4)
		return p[3] === 'results' ? `${area(p[2]!)} · work results` : `${area(p[2]!)} · ${p[3]} work`;
	if (p[1] === 'tool' && p.length === 3) return `${p[2]} calls`;
	if (p[1] === 'op' && p.length === 2) return 'Operation calls';
	if (p[1] === 'op' && p.length === 3 && p[2] === 'results') return 'Operation results';
	if (p[1] === 'dead' && p.length === 2) return 'Dead letters';
	return key;
}

export const busVariant = (b: StreamInfo['bus']) =>
	(({ customer: 'cust', worker: 'work', tool: 'tool', op: 'tool', dead: 'dead', other: 'muted' }) as const)[
		b
	];

export const busName = (b: StreamInfo['bus']) =>
	b === 'dead' ? 'dead-letter' : b === 'other' ? 'other' : `${b}-bus`;

export function groupHealth(g: GroupInfo): 'ok' | 'warn' | 'err' {
	const lag = g.lag ?? 0;
	if (lag > 20 || g.pending > 10) return 'err';
	if (lag > 5 || g.pending > 3) return 'warn';
	return 'ok';
}

export function streamHealth(s: StreamInfo): { word: 'ok' | 'warn' | 'err'; label: string } {
	if (s.bus === 'dead')
		return s.length > 0 ? { word: 'err', label: 'Has entries' } : { word: 'ok', label: 'Empty' };
	if (s.groups.length && s.groups.every((g) => g.consumers === 0))
		return { word: 'warn', label: 'No consumers' };
	const hs = s.groups.map(groupHealth);
	if (hs.includes('err')) return { word: 'err', label: 'Behind' };
	if (hs.includes('warn')) return { word: 'warn', label: 'Falling behind' };
	return { word: 'ok', label: 'Healthy' };
}

/** Run management (claim stuck, replay, discard) is the Operator's (§R.1). */
export const ROLE_HINT = 'Needs the Operator role';

/** Why Claim stuck is disabled, or '' when it is allowed. Entries idle 30 s or more can be claimed. */
export function claimBlocked(admin: boolean, pending: number): string {
	if (!admin) return ROLE_HINT;
	if (pending === 0) return 'Nothing is pending in this group';
	return '';
}

/** Why Replay is disabled, or '' when it is allowed. A scope_mismatch letter must never be replayed. */
export function replayBlocked(admin: boolean, reasonCode: string): string {
	if (!admin) return ROLE_HINT;
	if (reasonCode === 'scope_mismatch') return 'Belongs to another tenant or job — discard it, never replay';
	return '';
}

/** `1 case` / `3 cases`. */
export const plural = (n: number, one: string, many = `${one}s`) => `${n} ${n === 1 ? one : many}`;
