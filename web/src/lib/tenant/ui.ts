// Display helpers shared by the tenant area's pages (T1–T9, T13). Owned by the tenant area.
import type { IntegrationRow, ProvisionRow, SecretState } from '$lib/api/types/tenant';

type Variant = 'ok' | 'warn' | 'err' | 'muted' | 'accent' | 'cust' | 'work' | 'tool' | 'dead';

/** `2026-10-18 09:42` (UTC, the fixture world's clock). */
export const when = (iso: string | null | undefined): string =>
	iso ? `${iso.slice(0, 10)} ${iso.slice(11, 16)}` : '—';
/** `09:42` */
export const hhmm = (iso: string | null | undefined): string => (iso ? iso.slice(11, 16) : '—');
/** `18 Oct` */
export const day = (iso: string): string =>
	new Date(`${iso.slice(0, 10)}T00:00:00Z`).toLocaleDateString('en-GB', {
		day: 'numeric',
		month: 'short',
		timeZone: 'UTC',
	});

/** The Integrations chip text + colour (06 §4.6): ready · pending (next retry hh:mm) · failed (code) · dead. */
export function integrationChip(r: Pick<IntegrationRow, 'chip' | 'current'>): {
	text: string;
	variant: Variant;
} {
	const cur: ProvisionRow | null = r.current;
	switch (r.chip) {
		case 'ready':
			return { text: 'ready', variant: 'ok' };
		case 'pending':
			return {
				text: cur?.next_attempt_at ? `pending (next retry ${hhmm(cur.next_attempt_at)})` : 'pending',
				variant: 'warn',
			};
		case 'failed':
			return { text: `failed (${cur?.last_error_code ?? 'error'})`, variant: 'err' };
		case 'dead':
			return { text: 'dead', variant: 'err' };
		default:
			return { text: 'cancelled', variant: 'muted' };
	}
}

export const TARGET_LABEL: Record<string, string> = {
	licences: 'Licences',
	strata_rag: 'Knowledge (strata_rag)',
	strata_ingest: 'Ingest (strata_ingest)',
};

/** "set · c07e…41d2 · 2026-10-09 11:20" or "not set". */
export function secretText(s: SecretState): string {
	return s.set ? `set · ${s.fingerprint ?? '—'} · ${when(s.updated_at)}` : 'not set';
}

/** `1 204` → `1,204`; percentages with one decimal. */
export const pct = (part: number, whole: number): string =>
	whole <= 0 ? '—' : `${((part / whole) * 100).toFixed(part === whole ? 0 : 1)} %`;

/** An ApiError-ish thrown value as `message (code)`. */
export function errText(e: unknown): string {
	const x = (e ?? {}) as { code?: string; message?: string };
	return x.code ? `${x.message ?? 'Failed'} (${x.code})` : (x.message ?? String(e));
}
