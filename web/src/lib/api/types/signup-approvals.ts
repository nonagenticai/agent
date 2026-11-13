// The early-access signup queue as agent-api serves it (migration 0030_signup_approvals: configurator 0005 shape +
// commonpractice 0121 locale/source/trust, plus the `signup_provisioning_steps` journal). Super_admin only.
// Routes: `GET /api/signup-approvals?status=&before=`, `POST /api/signup-approvals/{id}/approve` `{tenantName, residency}`,
// `POST /api/signup-approvals/{id}/reject`, `POST /api/signup-approvals/{id}/retry` (B1 T14),
// `POST /api/signup-approvals/invite` (CP inviteEarlyAccess).
import type { Residency } from './core';
import type { ProvisionRow, SignupSignal } from './platform';

/** The eight languages the approval email ships in (commonpractice `SIGNUP_LOCALES`); null = not told (English). */
export const SIGNUP_LOCALES = ['ar', 'de', 'en', 'es', 'fr', 'he', 'nl', 'pl'] as const;
export type SignupLocale = (typeof SIGNUP_LOCALES)[number];

export type SignupApprovalStatus = 'pending' | 'approved' | 'rejected';
export type ProvisioningStepStatus = 'ok' | 'failed' | 'pending' | 'skipped';

/** One step of turning an approved signup into a working tenant (Keycloak user, licence, invite email, …). */
export interface ProvisioningStep {
	step: string;
	status: ProvisioningStepStatus;
	code?: string | null;
	message?: string | null;
	attempts?: number | null;
	at?: string | null;
}

export interface SignupApproval {
	id: string;
	email: string;
	name: string;
	company: string | null;
	message: string | null;
	/** `web` | `chat` (who relayed the request) | `invite` (a super_admin invited them; `invited_by` says who). */
	source: string;
	/** Who sent the pre-approved invite (`source = 'invite'`). */
	invited_by?: string | null;
	/** An approve is running right now (its claim has not expired); a second approve gets 409 `approval_in_progress`. */
	approving?: boolean;
	locale: SignupLocale | null;
	/** Which trusted path admitted it (`intake` | `verified_email` | `turnstile`), null = untrusted. */
	trust?: string | null;
	status: SignupApprovalStatus;
	created_at: string;
	decided_at?: string | null;
	decided_by?: string | null;
	tenant_id?: string | null;
	/** The workspace name approval uses when none is given. */
	suggested_tenant_name: string;
	/** The provisioning journal, once approved (an array, or a record keyed by step). */
	steps?: ProvisioningStep[] | Record<string, Omit<ProvisioningStep, 'step'>> | null;
	/** B1b F-03: what intake noticed (always sent by the API; `audit_action` links to the platform audit). */
	signals?: SignupSignal[];
}

export interface SignupApprovalPage {
	items: SignupApproval[];
	next: string | null;
}

export interface ApproveResult {
	tenant: { id: string; name: string };
	steps?: SignupApproval['steps'];
	/** The account was seated as the workspace's tenant admin (false: an existing account got a pending invite). */
	seated?: boolean;
}

/**
 * A step the approval skipped because the address already had a portal account: it is never seated or licensed at
 * approval; a pending member invite holds its tenant_admin seat until that person accepts.
 */
export const EXISTING_IDENTITY_INVITE_PENDING = 'existing_identity_invite_pending';

/** `POST /api/signup-approvals/invite` body (super_admin; unknown fields refused). */
export interface EarlyAccessInvite {
	email: string;
	name: string;
	company?: string;
	/** The workspace name; default from the company / address. */
	tenantName?: string;
	/** The invite email's language; absent = English. */
	locale?: SignupLocale;
	/** D66: required, immutable after. */
	residency: Residency;
}

/** 200 from the invite: the signup row (approved), its workspace and journal. */
export interface InviteResult {
	signup: SignupApproval;
	tenant: { id: string; name: string; status?: string };
	seated: boolean;
	steps: SignupApproval['steps'];
	/** An earlier pending signup for this address was approved instead of a new row. */
	reused_pending: boolean;
}

/** The calls the signups page makes (`httpSignups`: `/api/signup-approvals*`). */
export interface SignupsApi {
	list(status: SignupApprovalStatus, before?: string | null): Promise<SignupApprovalPage>;
	/** D66: residency is required (422 `invalid_residency`) and immutable after. */
	approve(id: string, tenantName: string, residency: Residency): Promise<ApproveResult>;
	reject(id: string): Promise<unknown>;
	/** Re-runs every failed provisioning step of an approved signup; resolves the journal after the run. */
	retry(id: string): Promise<{ steps: SignupApproval['steps'] }>;
	/** Pre-approved invite: a signup row created and approved in one call. */
	invite(input: EarlyAccessInvite): Promise<InviteResult>;
}

/** Provisioning rows of `tenantId` as a signup journal (the in-memory test API builds its journals with it). */
export function stepsFromProvisioning(
	rows: readonly ProvisionRow[],
	tenantId: string | null,
): ProvisioningStep[] {
	if (!tenantId) return [];
	return rows
		.filter((r) => r.tenant_id === tenantId)
		.map((r) => ({
			step: `${r.target} · ${r.op}`,
			status:
				r.status === 'succeeded' ? 'ok' : r.status === 'failed' || r.status === 'dead' ? 'failed' : 'pending',
			code: r.last_error?.code ?? null,
			message: r.last_error?.message ?? null,
			attempts: r.attempts,
			at: r.updated_at,
		}));
}

/** The journal as an ordered array, whichever shape the API sent. */
export function stepsOf(steps: SignupApproval['steps']): ProvisioningStep[] {
	if (!steps) return [];
	if (Array.isArray(steps)) return steps;
	return Object.entries(steps).map(([step, r]) => ({ step, ...r }));
}
