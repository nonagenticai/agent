/**
 * Every refusal the API (and the fixture client) can give: HTTP status + the machine code from the error body
 * `{"error":{"code","message", …extras}}`. Pages branch on `status`/`code`, never on the message text. `details`
 * holds the extras (every field of `error` but `code` and `message`). A role refusal (403 `forbidden`, D88) names
 * the one role it needed in `details.missing_role`, mirrored as `missingRole`.
 */
export class ApiError extends Error {
	/** The §R.1 role key a 403 `forbidden` needed, when the API named one. */
	readonly missingRole: string | null;
	constructor(
		readonly status: number,
		readonly code: string,
		message: string,
		readonly details: Record<string, unknown> = {},
	) {
		super(message);
		this.name = 'ApiError';
		const role = details.missing_role;
		this.missingRole = typeof role === 'string' ? role : null;
	}
}

/**
 * Licence gate 2's 402 codes (agent-api `licensing::gate`, spec 04 §4b): the workspace's licence does not admit
 * tenant work. `required` = no licence yet, `unavailable` = it could not be verified (licences down, or the stored
 * verdict is stale), `unreadable` = the stored token does not open, `expired`, `inactive` (revoked or inactive).
 */
export const LICENCE_402_CODES = [
	'license_required',
	'license_unavailable',
	'license_unreadable',
	'license_expired',
	'license_inactive',
] as const;

/**
 * Licence gate 2's 403 codes: the licence is for another application or another tenant, or licences report the
 * tenant suspended (a verdict). The API's tenant-status check also answers 403 `tenant_suspended` (the shell's
 * suspended state, not a licence block), so a 403 is a licence refusal only when its error object carries the
 * gate's mark `"gate": "licence"` (agent-api `licensing::gate::GATE_FIELD`).
 */
export const LICENCE_403_CODES = [
	'license_app_mismatch',
	'license_tenant_mismatch',
	'tenant_suspended',
] as const;

export const LICENCE_CODES = [...LICENCE_402_CODES, ...LICENCE_403_CODES] as const;
export type LicenceCode = (typeof LICENCE_CODES)[number];

/** The mark licence gate 2 puts on every refusal's error object (`details.gate`). */
export const LICENCE_GATE_MARK = 'licence';

export function isLicenceCode(code: unknown): code is LicenceCode {
	return typeof code === 'string' && (LICENCE_CODES as readonly string[]).includes(code);
}

/**
 * Is this refusal licence gate 2's? A 402 with one of `LICENCE_402_CODES`, or a 403 with one of `LICENCE_403_CODES`
 * whose error object carries the gate's mark (any other 403 `tenant_suspended` is the tenant-status check's).
 */
export function isLicenceRefusal(
	status: number,
	code: unknown,
	details: Record<string, unknown> = {},
): code is LicenceCode {
	if (typeof code !== 'string') return false;
	if (status === 402) return (LICENCE_402_CODES as readonly string[]).includes(code);
	if (status === 403)
		return details.gate === LICENCE_GATE_MARK && (LICENCE_403_CODES as readonly string[]).includes(code);
	return false;
}

/**
 * A licence gate 2 refusal (`isLicenceRefusal`): the shell shows the blocking licence state instead of a page error.
 * Still an `ApiError`, so every existing `instanceof ApiError` branch keeps working.
 */
export class LicenceError extends ApiError {
	constructor(
		readonly licence: LicenceCode,
		message: string,
		details: Record<string, unknown> = {},
		status = 402,
	) {
		super(status, licence, message, details);
		this.name = 'LicenceError';
	}
}
