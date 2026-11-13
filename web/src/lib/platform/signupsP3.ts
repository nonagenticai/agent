// P3 Signups — small pure helpers (owned by P3).
import type { Signup, SignupQueue, SignupSignal } from '$lib/api/types/platform';
import { age } from './ui';

/** `waiting 1 d` from the server clock; empty when the clock is not given. */
export function waiting(created: string, asOf: string | undefined): string {
	if (!asOf) return '';
	const s = Math.max(0, Math.round((Date.parse(asOf) - Date.parse(created)) / 1000));
	return age(s);
}

/** Risk first, then info, then ok — the reader sees the reason to hesitate before the reassurance. */
const RANK: Record<SignupSignal['level'], number> = { risk: 0, info: 1, ok: 2 };
export function orderedSignals(s: { signals?: SignupSignal[] }): SignupSignal[] {
	return [...(s.signals ?? [])].sort((a, b) => RANK[a.level] - RANK[b.level]);
}

export function riskCount(s: Signup): number {
	return (s.signals ?? []).filter((x) => x.level === 'risk').length;
}

/** Glyph + word per level, so a signal never relies on colour alone. */
export const SIGNAL_MARK: Record<SignupSignal['level'], { glyph: string; word: string }> = {
	ok: { glyph: '✓', word: 'OK' },
	info: { glyph: 'i', word: 'Note' },
	risk: { glyph: '!', word: 'Risk' },
};

export interface FunnelStage {
	key: 'intake' | 'queue' | 'approved' | 'rejected';
	label: string;
	count: number | null;
	detail: string;
}

/** The four places a request can be. Counts are null when the queue is unreachable (feature off). */
export function funnel(q: SignupQueue | null): FunnelStage[] {
	const c = q?.counts;
	return [
		{
			key: 'intake',
			label: 'Requested',
			count: c?.received ?? null,
			detail: 'Public form, Turnstile bot check, Telegram notice (ids only).',
		},
		{
			key: 'queue',
			label: 'Waiting for you',
			count: q ? q.pending : null,
			detail: q
				? `Cap ${q.queue_cap}: past it, intake answers 503.`
				: 'Capped; past the cap intake answers 503.',
		},
		{
			key: 'approved',
			label: 'Approved → workspace',
			count: c?.approved ?? null,
			detail: 'Tenant create: licence, requester invited as tenant admin, provisioning queued.',
		},
		{
			key: 'rejected',
			label: 'Rejected',
			count: c?.rejected ?? null,
			detail: 'Nothing created, nothing sent. Reason kept in platform audit.',
		},
	];
}
