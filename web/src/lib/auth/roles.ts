// Role helpers every page uses to show or disable controls (07 §4.2, spec 14 §1, D88 §R.1). Pages never read
// `session.me` for roles directly. A write the caller's EFFECTIVE roles do not cover stays VISIBLE but disabled,
// with `writeHint(role)` as the tooltip/hint, so the role difference is reviewable (and view-as can preview it).
import { ROLE_LABEL, type Role } from '$lib/api/types/core';
import type { Session } from './session.svelte';

/** The hint next to (and title of) every write control the caller sees disabled for lack of `role`. */
export function writeHint(role: Role): string {
	return `Needs the ${ROLE_LABEL[role]} role`;
}
/** Shown instead of a super_admin-only control's action to tenant members. */
export const SUPER_ADMIN_HINT = 'Super-admin only';

type Who = Pick<Session, 'superAdmin' | 'member' | 'has'>;

/** May make the writes §R.1 gives `role` in the active tenant (effective roles: held ∩ view-as). */
export function canWrite(s: Who, role: Role): boolean {
	return s.member && s.has(role);
}

/**
 * The roles that work a conversation (Take, Send reply, Hand back, Resolve, Hand over case, Link customer, be
 * assigned): a support_manager can do everything a support_agent can. Mirrors `CONVERSATION_WORKERS` in
 * crates/agent-api/src/auth.rs. Notes stay with the wider staff roles.
 */
export const CONVERSATION_WORKERS = ['support_agent', 'support_manager'] as const satisfies readonly Role[];

/** The staff roles that work a conversation in the wider sense (notes, handover feedback). Mirrors `STAFF_ROLES`. */
export const STAFF_ROLES = [
	'support_agent',
	'support_manager',
	'operator',
	'job_designer',
] as const satisfies readonly Role[];

/** The hint of a conversation write the caller may not make (the server's refusal names `support_agent`). */
export const WORK_HINT = `Needs the ${ROLE_LABEL.support_agent} or ${ROLE_LABEL.support_manager} role`;

/** Holds one of the effective roles that work conversations (the server's `require_conversation_worker`). */
export function canWorkConversations(s: Who): boolean {
	return s.member && CONVERSATION_WORKERS.some((r) => s.has(r));
}

/** Props for a conversation-work button: disabled with [`WORK_HINT`] as its title for anyone who cannot work one. */
export function workAttrs(s: Who): { disabled: boolean; title: string | undefined } {
	const ok = canWorkConversations(s);
	return { disabled: !ok, title: ok ? undefined : WORK_HINT };
}

/** May read tenant/job pages: any member of the active tenant, observers included. */
export function canRead(s: Who): boolean {
	return s.member;
}

/** May create jobs, rename them and edit the company profile (D123/D124): a tenant_admin; a super_admin in the tenant holds it too. */
export function canTenantAdmin(s: Who): boolean {
	return canWrite(s, 'tenant_admin');
}

/** May edit a job's staff team (D125.5): a tenant_admin or a support_manager. */
export function canEditStaffTeam(s: Who): boolean {
	return s.member && (s.has('tenant_admin') || s.has('support_manager'));
}

/** The hint of a channel write the caller may not make (D126). */
export const CHANNELS_HINT = `Needs the ${ROLE_LABEL.tenant_admin} or ${ROLE_LABEL.integrations_manager} role`;

/** May write a job's channel bindings (D126.1): a tenant_admin or an integrations_manager. */
export function canEditChannels(s: Who): boolean {
	return s.member && (s.has('tenant_admin') || s.has('integrations_manager'));
}

/** A super_admin (platform pages; SA-only controls such as create job, suspend, fault injection). */
export function isSuperAdmin(s: Pick<Session, 'superAdmin'>): boolean {
	return s.superAdmin;
}

/** A super_admin acting inside a tenant: may use the SA-only controls on tenant pages. */
export function canSuperAdminInTenant(s: Pick<Session, 'superAdmin' | 'member'>): boolean {
	return s.superAdmin && s.member;
}

/**
 * Props for a write button: `<button class="btn" {...writeAttrs(session, 'job_designer')}>Save</button>`.
 * Disabled with the role hint as its title for anyone without that effective role.
 */
export function writeAttrs(s: Who, role: Role): { disabled: boolean; title: string | undefined } {
	const ok = canWrite(s, role);
	return { disabled: !ok, title: ok ? undefined : writeHint(role) };
}

/**
 * Props for a super_admin-only control: `<button class="btn" {...superAdminAttrs(session)}>Publish</button>`.
 * Enabled for any super_admin, with or without an active tenant (platform writes run as agent_platform and
 * need no tenant role). Disabled — never hidden — with SUPER_ADMIN_HINT as its title for everyone else.
 */
export function superAdminAttrs(s: Pick<Session, 'superAdmin'>): {
	disabled: boolean;
	title: string | undefined;
} {
	return { disabled: !s.superAdmin, title: s.superAdmin ? undefined : SUPER_ADMIN_HINT };
}

/**
 * `writeAttrs` for a control a super_admin may also use without being a member of any tenant (the platform generator:
 * `POST /api/generator/runs` with scope platform is super_admin only).
 */
export function writeAttrsOrSuperAdmin(s: Who, role: Role): { disabled: boolean; title: string | undefined } {
	return s.superAdmin ? { disabled: false, title: undefined } : writeAttrs(s, role);
}
