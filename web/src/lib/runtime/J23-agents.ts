// J23 Agents — page logic: where each bus agent sits in the message path, which agent role it runs as (12 §2.1, the
// P15 vocabulary), whether it keeps up (lag / pending / heartbeat), and whether it serves the config it should
// (Production = published release, Test env = draft revision, D54). Owned by the J23 page.
import type { AgentFleet, AgentServing, BusAgent } from '$lib/api/types/runtime';

export type RoleKey = 'qualifier' | 'procedure' | 'post_gate' | 'assistant';

/** Agent roles (12 §2.1, P15): what an agent running as this role may do. */
export const ROLES: Record<RoleKey, { label: string; grant: string; effect: string }> = {
	qualifier: {
		label: 'Qualifier',
		grant: 'Observe only, a fixed list (party, order, case, offering reads). Pre-fills slots; never writes.',
		effect: 'read',
	},
	procedure: {
		label: 'Procedure',
		grant: 'Exactly the operations of the activity it runs — derived from the journey, never hand-authored.',
		effect: 'as the activity declares',
	},
	post_gate: {
		label: 'Post-gate',
		grant: 'knowledge.search plus reads of operations already in the case log. Fact-checks a reply.',
		effect: 'read',
	},
	assistant: {
		label: 'Assistant',
		grant: 'Defined, disabled in v1. Would allow free LLM tool use behind the same runner checks.',
		effect: 'off in v1',
	},
};

export const roleOf = (a: BusAgent): RoleKey =>
	(['qualifier', 'procedure', 'post_gate', 'assistant'] as const).find((r) => r === a.role) ?? 'procedure';

/** The message path, left to right. Each agent sits in one step. */
export type StepKey = 'understand' | 'plan' | 'prepare' | 'act' | 'check';
export const STEPS: { key: StepKey; label: string; does: string; role: RoleKey }[] = [
	{
		key: 'understand',
		label: 'Understand',
		does: 'Reads the customer message, qualifies the request.',
		role: 'qualifier',
	},
	{
		key: 'plan',
		label: 'Plan',
		does: 'Picks the published procedure and runs it step by step.',
		role: 'procedure',
	},
	{
		key: 'prepare',
		label: 'Prepare',
		does: 'One worker per object family prepares its part.',
		role: 'procedure',
	},
	{ key: 'act', label: 'Act', does: 'Calls the systems through operation routes.', role: 'procedure' },
	{ key: 'check', label: 'Check reply', does: 'Fact-checks the answer before it leaves.', role: 'post_gate' },
];

export function stepOf(a: BusAgent): StepKey {
	if (a.role === 'post_gate') return 'check';
	if (a.kind === 'coordination') return 'plan';
	if (a.kind === 'worker') return 'prepare';
	if (a.kind === 'runner') return 'act';
	return 'understand';
}

/** Seconds since the heartbeat, against the fixture clock; null = never. */
export function heartbeatAge(iso: string | null | undefined, now: string): number | null {
	if (!iso) return null;
	return Math.max(0, Math.round((Date.parse(now) - Date.parse(iso)) / 1000));
}

/** "6 s ago", "4 min ago", "3 d ago". */
export function ago(s: number | null): string {
	if (s == null) return 'never';
	if (s < 90) return `${s} s ago`;
	if (s < 5400) return `${Math.round(s / 60)} min ago`;
	if (s < 172800) return `${Math.round(s / 3600)} h ago`;
	return `${Math.round(s / 86400)} d ago`;
}

/** A heartbeat older than this means the process is not reporting (supervisor restarts after 5 s). */
export const SILENT_AFTER_S = 60;

export type ConfigState = 'current' | 'catching_up' | 'off';

/** Is this copy serving what it should? Test copies compare against the draft revision. */
export function configState(
	s: AgentServing | null | undefined,
	fleet: AgentFleet,
	env: 'prod' | 'test',
): ConfigState {
	if (!s || s.consumers === 0) return 'off';
	if (env === 'prod') {
		const want = fleet.production;
		return s.release === (want.release?.id ?? null) ? 'current' : 'catching_up';
	}
	return (s.rev ?? -1) >= fleet.test.rev ? 'current' : 'catching_up';
}

export type HealthWord = 'ok' | 'warn' | 'err' | 'idle';
export interface AgentHealth {
	word: HealthWord;
	label: string;
	/** Short reasons, in the order a person should act on them. */
	reasons: string[];
	kinds: BehindKind[];
}

/** The four ways an agent can be "behind" (explained on the page). */
export type BehindKind = 'waiting' | 'unacked' | 'config' | 'silent';

export function healthOf(a: BusAgent, fleet: AgentFleet | null): AgentHealth {
	if (a.consumers === 0)
		return { word: 'idle', label: 'Stopped', reasons: ['No instance is running.'], kinds: [] };
	const reasons: string[] = [];
	const kinds: BehindKind[] = [];
	const age = fleet ? heartbeatAge(a.last_heartbeat, fleet.as_of) : 0;
	if (age == null || age > SILENT_AFTER_S) {
		kinds.push('silent');
		reasons.push(`No heartbeat for ${ago(age).replace(' ago', '')}.`);
	}
	if (a.status === 'warn' || a.lag > 5) {
		if (a.lag) {
			kinds.push('waiting');
			reasons.push(`${a.lag} message${a.lag === 1 ? '' : 's'} waiting to be picked up.`);
		}
	}
	if (a.status === 'warn' || a.pending > 3) {
		if (a.pending) {
			kinds.push('unacked');
			const old = a.oldest_pending_s;
			reasons.push(`${a.pending} picked up, not finished${old ? ` (oldest ${old} s)` : ''}.`);
		}
	}
	if (fleet && configState(a.prod, fleet, 'prod') === 'catching_up') {
		kinds.push('config');
		reasons.push(`Still on ${a.prod?.release ?? '—'}.`);
	}
	if (kinds.includes('silent')) return { word: 'err', label: 'Silent', reasons, kinds };
	if (kinds.length) return { word: 'warn', label: 'Behind', reasons, kinds };
	return { word: 'ok', label: 'Keeping up', reasons, kinds };
}

export interface FleetTotals {
	agents: number;
	keeping_up: number;
	instances: number;
	waiting: number;
	unacked: number;
	per_min: number;
	errors_1h: number;
	restarts_24h: number;
}

export function totalsOf(items: BusAgent[], fleet: AgentFleet | null): FleetTotals {
	const sum = (f: (a: BusAgent) => number) => items.reduce((n, a) => n + f(a), 0);
	return {
		agents: items.length,
		keeping_up: items.filter((a) => healthOf(a, fleet).word === 'ok').length,
		instances: sum((a) => a.consumers),
		waiting: sum((a) => a.lag),
		unacked: sum((a) => a.pending),
		per_min: sum((a) => a.per_min ?? 0),
		errors_1h: sum((a) => a.errors_1h ?? 0),
		restarts_24h: sum((a) => a.restarts_24h ?? 0),
	};
}

export interface Verdict {
	tone: 'ok' | 'warn' | 'err' | 'idle';
	title: string;
	detail: string;
	/** Agents that need a look, worst first. */
	attention: BusAgent[];
}

/** One sentence: is this job being served right now, and if not, by whom not. */
export function verdictOf(items: BusAgent[], fleet: AgentFleet | null): Verdict {
	if (items.length && items.every((a) => a.consumers === 0)) {
		const last = items
			.map((a) => a.last_heartbeat ?? '')
			.sort()
			.pop();
		return {
			tone: 'idle',
			title: 'All agents are stopped',
			detail: `Nothing serves this job; customer messages are not answered. Last heartbeat ${fleet ? ago(heartbeatAge(last || null, fleet.as_of)) : '—'}.`,
			attention: [],
		};
	}
	const rank: Record<HealthWord, number> = { err: 0, warn: 1, idle: 2, ok: 3 };
	const attention = items
		.filter((a) => healthOf(a, fleet).word !== 'ok')
		.sort((a, b) => rank[healthOf(a, fleet).word] - rank[healthOf(b, fleet).word] || b.lag - a.lag);
	if (!attention.length)
		return {
			tone: 'ok',
			title: `All ${items.length} agents are keeping up`,
			detail: 'Every message is picked up as it arrives and every agent serves the published release.',
			attention,
		};
	const names = attention.map((a) => a.name);
	const list =
		names.length > 2 ? `${names.slice(0, 2).join(', ')} and ${names.length - 2} more` : names.join(' and ');
	const silent = attention.some((a) => healthOf(a, fleet).word === 'err');
	return {
		tone: silent ? 'err' : 'warn',
		title: `${attention.length} of ${items.length} agents ${attention.length === 1 ? 'is' : 'are'} behind: ${list}`,
		detail: 'The rest keep up. Customers whose request passes through a slow agent wait longer for a reply.',
		attention,
	};
}

/** What each kind of "behind" means and what to do about it. */
export const BEHIND: Record<BehindKind, { label: string; means: string; todo: string; short: string }> = {
	waiting: {
		short: 'Likely a slow system: check Operation calls.',
		label: 'Waiting (lag)',
		means: 'Published to the agent’s stream, not picked up yet. Every instance is busy.',
		todo: 'Usually a slow system further down — check Operation calls for slow or failing routes. The platform adds instances when lag stays up.',
	},
	unacked: {
		short: 'Claim the stuck entries.',
		label: 'Unfinished (pending)',
		means:
			'Picked up, not finished. After 30 s idle another instance may take it over; after 5 tries it goes to dead letters.',
		todo: 'Claim stuck entries to hand them to a healthy instance; read dead letters on Buses & topics.',
	},
	config: {
		short: 'Re-reads within 35 s of a change.',
		label: 'Old config',
		means:
			'Still serving an older release (Production) or draft revision (Test). Agents re-read every 30 s and restart 5 s later.',
		todo: 'Normal for up to 35 s after you publish or save. Longer than that: contact platform support.',
	},
	silent: {
		short: 'Watch the restart count.',
		label: 'Silent',
		means: `No heartbeat for over ${SILENT_AFTER_S} s. The supervisor restarts a crashed instance after 5 s.`,
		todo: 'Watch the restart count. If it keeps restarting or stays silent, the platform team is alerted.',
	},
};

export const plural = (n: number, one: string, many = `${one}s`) => `${n} ${n === 1 ? one : many}`;
