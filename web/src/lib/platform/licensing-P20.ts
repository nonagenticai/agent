// P20 Licensing & keys — pure logic (owned by P20). Two independent chains live on this page:
//  1. Gate 2 (C1, K35): licences service → agent-api introspect (cache 300 s) → verdict row (refresh ≤60 s)
//     → edge reads the row only. Missing, not active, or older than 5 min → the edge refuses turns.
//  2. The token keyring (K43): stored `agent` tokens are sealed `v2:<kid>:…`; open by kid, seal with current.
//     Rotation = stage kid → make current → reseal → drop the old kid once 0 tokens remain on it.
import type { LicenceVerdict, LicensingOverview, KeyringKid } from '$lib/api/types/platform';
import { VERDICT_MAX_AGE_S } from '$lib/platform/model';

/** agent-api rewrites every active tenant's verdict row at least this often (K35). */
export const VERDICT_REFRESH_S = 60;
/** Introspection cache per token (04 §4b). */
export const INTROSPECT_CACHE_S = 300;
/** The age bar's full width: a little past the fail-closed line so a stale row reads as "over". */
export const AGE_SCALE_S = 360;

export type EdgeState = 'serving' | 'stale' | 'missing' | 'refused' | 'revoked';

export interface EdgeVerdict {
	state: EdgeState;
	label: string;
	variant: 'ok' | 'err' | 'warn' | 'muted';
	/** One line: why the edge does what it does for this tenant. */
	why: string;
}

/** What the edge does for a tenant, from its verdict row alone (the edge holds no key and no token). */
export function edgeVerdict(v: LicenceVerdict | null): EdgeVerdict {
	if (!v)
		return {
			state: 'missing',
			label: 'no verdict → fails closed',
			variant: 'err',
			why: 'No verdict row yet: the edge refuses until agent-api writes one.',
		};
	if (v.status === 'revoked')
		return {
			state: 'revoked',
			label: 'refused (revoked)',
			variant: 'muted',
			why: 'Licence revoked: the edge refuses by design.',
		};
	if (v.status !== 'active')
		return {
			state: 'refused',
			label: `refused (${v.status})`,
			variant: 'warn',
			why: `Licence ${v.status}: the edge refuses by design${v.reason ? ` (${v.reason})` : ''}.`,
		};
	if (v.age_seconds > VERDICT_MAX_AGE_S)
		return {
			state: 'stale',
			label: 'fails closed (stale)',
			variant: 'err',
			why: `Licence is active, but the row is older than ${VERDICT_MAX_AGE_S / 60} min, so the edge cannot trust it and refuses.`,
		};
	return {
		state: 'serving',
		label: 'serving',
		variant: 'ok',
		why: 'Fresh, active verdict: turns are served.',
	};
}

/** 0..1 position of an age on the age bar (capped). */
export function agePos(seconds: number): number {
	return Math.max(0, Math.min(1, seconds / AGE_SCALE_S));
}

export type KidPhase = 'current' | 'retiring' | 'drop-ready';

/**
 * A kid's place in the rotation lifecycle. `current` seals every new token; `retiring` still opens tokens sealed
 * under it (reseal moves them); `drop-ready` holds 0 tokens and may be removed from LICENSE_TOKEN_ENC_KEYS.
 */
export function kidPhase(k: Pick<KeyringKid, 'current' | 'sealed'>): KidPhase {
	if (k.current) return 'current';
	return k.sealed > 0 ? 'retiring' : 'drop-ready';
}

export interface Summary {
	live: number;
	serving: number;
	/** Active licence but the edge refuses: stale or missing row. The operational problem. */
	failingClosed: string[];
	/** Refused by design (suspended / expired / revoked live tenants). */
	refusedByDesign: string[];
	/** Live tokens not sealed under the current kid. */
	behind: number;
	/** Non-current kids with 0 tokens — remove from SOPS. */
	droppable: string[];
	/** Live tenants with no stored token at all. */
	unsealed: string[];
}

export function summarise(o: LicensingOverview): Summary {
	const live = o.tenants.filter((t) => t.status !== 'archived');
	const states = live.map((t) => ({ t, e: edgeVerdict(t.verdict) }));
	return {
		live: live.length,
		serving: states.filter((s) => s.e.state === 'serving').length,
		failingClosed: states
			.filter((s) => s.e.state === 'stale' || s.e.state === 'missing')
			.map((s) => s.t.name),
		refusedByDesign: states
			.filter((s) => s.e.state === 'refused' || s.e.state === 'revoked')
			.map((s) => s.t.name),
		behind: live.filter((t) => t.licence?.kid && t.licence.kid !== o.current_kid).length,
		droppable: o.keyring.filter((k) => kidPhase(k) === 'drop-ready').map((k) => k.kid),
		unsealed: live.filter((t) => !t.licence?.kid).map((t) => t.name),
	};
}

/** Live tenants whose stored token is sealed under `kid` (what a reseal would move, what a drop would break). */
export function tenantsOnKid(o: LicensingOverview, kid: string): string[] {
	return o.tenants.filter((t) => t.status !== 'archived' && t.licence?.kid === kid).map((t) => t.name);
}
