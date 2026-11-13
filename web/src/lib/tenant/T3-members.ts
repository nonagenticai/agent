// T3 Users: pure helpers for the membership page (state, role capabilities, last-activity wording).
import type { TenantUser } from '$lib/api/types/core';
import type { MemberActivity, MemberOrigin, MemberSurface } from '$lib/api/types/tenant';

export type MemberState = 'invited' | 'active' | 'disabled';

/** invited (not accepted yet) → active (can sign in) → disabled (soft, history kept). */
export const memberState = (u: Pick<TenantUser, 'disabled_at' | 'invited_at'>): MemberState =>
	u.disabled_at ? 'disabled' : u.invited_at ? 'invited' : 'active';

/** A member who can sign in but has not for this many days is flagged "quiet" (a disable candidate). */
export const QUIET_DAYS = 21;

const DAY = 86_400_000;

/** Whole days between two instants (b − a), floored, never negative. */
export const daysBetween = (a: string, b: number): number =>
	Math.max(0, Math.floor((b - Date.parse(a)) / DAY));

/** "today 09:31", "yesterday", "24 d ago", "never". */
export function ago(iso: string | null, now: number): string {
	if (!iso) return 'never';
	const d = daysBetween(iso, now);
	if (d === 0) return `today ${iso.slice(11, 16)}`;
	if (d === 1) return 'yesterday';
	return `${d} d ago`;
}

/** Active, and no sign-in for QUIET_DAYS or more (or never). */
export const isQuiet = (state: MemberState, a: MemberActivity | undefined, now: number): boolean =>
	state === 'active' && (!a?.last_seen_at || daysBetween(a.last_seen_at, now) >= QUIET_DAYS);

/** "18 Oct" (UTC). */
export const shortDate = (iso: string | null): string =>
	iso
		? new Date(`${iso.slice(0, 10)}T00:00:00Z`).toLocaleDateString('en-GB', {
				day: 'numeric',
				month: 'short',
				timeZone: 'UTC',
			})
		: '—';

export const SURFACE_LABEL: Record<MemberSurface, string> = {
	console: 'Console',
	chat: 'Test chat',
	admin: 'Admin',
	support: 'Support',
};

export const ORIGIN_LABEL: Record<MemberOrigin, string> = {
	tenant_create: 'first admin',
	invite: 'invited',
	add_by_sub: 'added by ID',
};

export interface MemberSummary {
	/** Active members holding the Tenant admin role. */
	admins: number;
	/** Every other active member (other roles, or observers). */
	users: number;
	invited: number;
	disabled: number;
	quiet: number;
}

export function summarize(
	users: TenantUser[],
	activity: Map<string, MemberActivity>,
	now: number,
): MemberSummary {
	const s: MemberSummary = { admins: 0, users: 0, invited: 0, disabled: 0, quiet: 0 };
	for (const u of users) {
		const st = memberState(u);
		if (st === 'invited') s.invited++;
		else if (st === 'disabled') s.disabled++;
		else {
			if (u.roles.includes('tenant_admin')) s.admins++;
			else s.users++;
			if (isQuiet(st, activity.get(u.sub), now)) s.quiet++;
		}
	}
	return s;
}
