// The shell's licence state (licence gate 2, spec 04 §4b): one honest sentence per licence code (a 402, or a
// gate-marked 403). Pages never show a raw licence refusal; the layout renders `LicenceBlocked.svelte` with this
// text instead of the page.
import type { LicenceCode } from '$lib/api/errors';

/** Every licence refusal has the same headline; the body says why and who can fix it. */
export const LICENCE_TITLE = 'Your workspace licence is not active';

export const LICENCE_TEXT: Record<LicenceCode, string> = {
	license_required:
		'This workspace has no licence yet. Your administrator needs to have one issued; nothing in the workspace is lost meanwhile.',
	license_unavailable:
		'The licence could not be verified just now (the licence service did not answer, or its last check is too old). Access stays closed until it is verified again. Try again in a minute.',
	license_expired: 'The licence of this workspace has expired. Ask your administrator to renew it.',
	license_inactive:
		'The licence of this workspace was revoked or deactivated. Ask your administrator to have it re-issued.',
	license_unreadable:
		'The stored licence of this workspace could not be read. This needs platform support: the licence has to be re-sealed or re-issued.',
	license_app_mismatch:
		'The licence of this workspace was issued for a different application, so it does not cover this one. Ask your administrator to have a licence for this application issued.',
	license_tenant_mismatch:
		'The licence stored for this workspace belongs to a different workspace. This needs platform support: a licence has to be issued for this workspace.',
	tenant_suspended:
		'The licence service reports this workspace as suspended. Ask your administrator to have the suspension lifted; nothing in the workspace is lost meanwhile.',
};

/** Whether trying again can help on its own (an outage clears by itself; the others need an administrator). */
export function retryHelps(code: LicenceCode): boolean {
	return code === 'license_unavailable';
}
