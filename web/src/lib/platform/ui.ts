// Small helpers shared by the platform pages (owned by the platform area).
import type { ProvisionStatus, Signup } from '$lib/api/types/platform';
import type { TenantStatus } from '$lib/api/types/core';

/** The same, for a write on a tenant that is archived (read-only for everyone). */
export const ARCHIVED_HINT = 'Archived tenants are read-only';

export const STATUS_CHIP: Record<TenantStatus, 'ok' | 'warn' | 'muted'> = {
	active: 'ok',
	suspended: 'warn',
	archived: 'muted',
};

export const PROVISION_CHIP: Record<ProvisionStatus, 'ok' | 'warn' | 'err' | 'muted' | 'dead'> = {
	succeeded: 'ok',
	pending: 'warn',
	in_flight: 'warn',
	failed: 'err',
	dead: 'dead',
	superseded: 'muted',
	cancelled: 'muted',
};

export const TARGET_LABEL = {
	licences: 'Licences',
	strata_rag: 'Knowledge',
	strata_ingest: 'Ingest',
} as const;

/** `femke.brouwer@brightside-bikes.example` → `f•••@brightside-bikes.example` (PII in lists). */
export function maskEmail(email: Signup['email']): string {
	const [local, domain] = email.split('@');
	if (!local || !domain) return '•••';
	return `${local[0]}•••@${domain}`;
}

/** `2026-10-18 09:12` in UTC — fixture times are UTC and must read the same for every reviewer. */
export function utc(iso: string | null | undefined): string {
	if (!iso) return '—';
	return `${iso.slice(0, 10)} ${iso.slice(11, 16)}`;
}

/** `hh:mm` UTC. */
export function hhmm(iso: string | null | undefined): string {
	return iso ? iso.slice(11, 16) : '—';
}

/** `40 s`, `7 min`, `18 d`. */
export function age(seconds: number): string {
	if (seconds < 90) return `${seconds} s`;
	if (seconds < 5400) return `${Math.round(seconds / 60)} min`;
	if (seconds < 172800) return `${Math.round(seconds / 3600)} h`;
	return `${Math.round(seconds / 86400)} d`;
}
