// J22 Operation calls — pure helpers. Every call the agent made on a system ends one of five ways: ok; denied by
// the runner before anything was sent (entitlement, identity level, confirm-back); a user outcome the agent answers
// the customer with (not_found, invalid_state, ineligible …); a system error it escalates (upstream_*, timeout,
// rate_limited, connector_*); or an unknown outcome — a write was sent and no answer came back, so a person must
// check the system before anything is retried (12 §3.3 steps 2–10, doc 10 §3.1 common errors).
import type { ErrorStateDef } from '$lib/api/types/model';
import type { CallHandling, OperationCall, OperationStat } from '$lib/api/types/runtime';

/** `invalid_state:in_transit` → reply (user outcome) | escalate (system error). Codes not in the taxonomy escalate. */
export function handlingOf(code: string, taxonomy: ErrorStateDef[]): CallHandling {
	const base = code.split(':')[0]!.trim();
	const def = taxonomy.find((d) => d.code === base);
	return def?.user_facing ? 'reply' : 'escalate';
}

/** Handling of one call: null when it succeeded or is still pending. */
export function callHandling(
	c: Pick<OperationCall, 'state' | 'outcome'>,
	taxonomy: ErrorStateDef[],
): CallHandling | null {
	if (c.state === 'ok' || c.state === 'pending') return null;
	if (c.state === 'unknown_outcome') return 'escalate';
	return handlingOf(c.outcome, taxonomy);
}

export type Ending = 'ok' | 'denied' | 'replied' | 'escalated' | 'unknown';

export const ENDING: Record<Ending, { label: string; hint: string; cls: string }> = {
	ok: { label: 'OK', hint: 'The system answered and the step continued.', cls: 'e-ok' },
	denied: {
		label: 'Denied',
		hint: 'Refused by the runner before anything was sent: not in the toolset, identity too low, no confirm-back.',
		cls: 'e-denied',
	},
	replied: {
		label: 'Answered',
		hint: 'A user outcome (not found, wrong state, a rule says no): the agent tells the customer.',
		cls: 'e-replied',
	},
	escalated: {
		label: 'Escalated',
		hint: 'A system error (down, timeout, throttled, bad credential): the case goes to staff.',
		cls: 'e-escalated',
	},
	unknown: {
		label: 'Unknown outcome',
		hint: 'A write was sent and no answer came back. Never retried: a person checks the system.',
		cls: 'e-unknown',
	},
};
export const ENDINGS: Ending[] = ['ok', 'denied', 'replied', 'escalated', 'unknown'];

export function endingOf(c: Pick<OperationCall, 'state' | 'handling'>): Ending | 'pending' {
	if (c.state === 'pending') return 'pending';
	if (c.state === 'ok') return 'ok';
	if (c.state === 'denied') return 'denied';
	if (c.state === 'unknown_outcome') return 'unknown';
	return c.handling === 'reply' ? 'replied' : 'escalated';
}

export const SIDE_EFFECT: Record<'R' | 'W' | 'X' | 'P', { label: string; hint: string }> = {
	R: { label: 'R', hint: 'Read — safe to repeat' },
	W: { label: 'W', hint: 'Reversible write — needs the customer’s confirm-back' },
	X: { label: 'X', hint: 'Irreversible write — confirm-back, then verified with a read' },
	P: { label: 'P', hint: 'Payment — hosted link only, confirm-back, verified with a read' },
};

export const IDEMPOTENCY_LABEL: Record<string, string> = {
	nat: 'safe to repeat',
	key: 'idempotency key',
	dup: 'checked for duplicates',
	'key+dup': 'key + duplicate check',
};

/** Share of each ending in one op × system's 30-day calls (counts, not percentages). */
/** One stat row is an op on one route to one system: the same op appears once per route (instance, not_offered, ...). */
export function statKey(s: Pick<OperationStat, 'op' | 'instance' | 'route'>): string {
	return `${s.op}|${s.instance ?? ''}|${s.route}`;
}

export function mixOf(s: OperationStat): Record<Ending, number> {
	const codes = s.error_codes ?? [];
	const unknown = s.unknown ?? 0;
	const known = codes.filter((c) => c.code !== 'unknown_outcome');
	const replied = known.filter((c) => c.handling === 'reply').reduce((a, c) => a + c.count, 0);
	// Errors with no code breakdown count as escalated (system errors fail closed).
	const escalated = Math.max(0, s.errors - unknown - replied);
	return {
		ok: Math.max(0, s.calls - s.errors - s.denied),
		denied: s.denied,
		replied,
		escalated,
		unknown,
	};
}

export interface Pattern {
	stat: OperationStat;
	/** Failed calls / calls (denials are counted separately: nothing was sent). */
	rate: number;
	mix: Record<Ending, number>;
	/** What the failures mostly are, for the one-line reading. */
	kind: 'system' | 'user' | 'mixed' | 'unknown';
	codes: { code: string; count: number; handling: CallHandling }[];
}

/** A pattern worth a look: ≥ 2 % failing on ≥ 10 calls, or any unknown outcome. */
export const PATTERN_THRESHOLD = 0.02;

export function failurePatterns(stats: OperationStat[]): { patterns: Pattern[]; healthy: OperationStat[] } {
	const patterns: Pattern[] = [];
	const healthy: OperationStat[] = [];
	for (const s of stats) {
		const rate = s.calls ? s.errors / s.calls : 0;
		if ((rate >= PATTERN_THRESHOLD && s.calls >= 10) || (s.unknown ?? 0) > 0) {
			const mix = mixOf(s);
			const failed = mix.replied + mix.escalated + mix.unknown;
			const kind: Pattern['kind'] =
				mix.unknown > 0
					? 'unknown'
					: mix.replied >= failed * 0.75
						? 'user'
						: mix.escalated >= failed * 0.75
							? 'system'
							: 'mixed';
			const codes = [...(s.error_codes ?? [])].sort((a, b) => b.count - a.count);
			patterns.push({ stat: s, rate, mix, kind, codes });
		} else healthy.push(s);
	}
	patterns.sort((a, b) => b.rate - a.rate || b.stat.calls - a.stat.calls);
	return { patterns, healthy };
}

/** The one-line reading of a pattern: where to look to fix it. */
export function readingOf(p: Pattern): string {
	const sys = p.stat.instance ?? 'the system';
	switch (p.kind) {
		case 'unknown':
			return `Writes to ${sys} went unanswered — each one needs a person to check ${sys} before anything is retried.`;
		case 'user':
			return `Mostly user outcomes the agent answers: the customer asks for something ${sys} cannot do. Check the procedure or the rule that offers it.`;
		case 'system':
			return `Mostly system errors that escalate: ${sys} is down, slow or throttling. Check the system and its rate limit.`;
		default:
			return `A mix of answered user outcomes and escalated system errors on ${sys}.`;
	}
}

/** `/admin/connections/systems/{id}` for a tenant instance; null for builtin, fallback and deep links. */
export function systemHref(instance: string | null): string | null {
	if (!instance || instance === 'builtin' || instance === 'agent_cases' || instance === 'strata_rag')
		return null;
	return `/admin/connections/systems/${encodeURIComponent(instance)}`;
}
