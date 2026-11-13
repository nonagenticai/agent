// B1 over HTTP (T18): tenancy, membership, roles + view-as, jobs, provisioning, signups and audit. Verbs and paths
// are the route table of the B1 plan (each one listed in `ROUTES` and in the method's doc comment).
import type { Api } from '../client';
import { seg, type Req } from '../http-client';
import type { Tenant } from '../types/core';
import type { Signup, SignupQueue } from '../types/platform';
import type {
	ApproveResult,
	SignupApproval,
	SignupApprovalPage,
	SignupApprovalStatus,
	SignupsApi,
	EarlyAccessInvite,
	InviteResult,
} from '../types/signup-approvals';
import { stepsOf } from '../types/signup-approvals';
import type { LiveMethod } from './routes';
import type { B7Method } from './b7';
import type { InteractionMethod } from './interaction';
import type { B12Method } from './b12';
import type { B2Method } from './b2';
import type { B3Method } from './b3';
import type { B4Method } from './b4';
import type { B4JourneyMethod } from './b4-journey';
import type { PkMethod } from './pk';
import type { B5Method } from './b5';
import type { B6Method } from './b6';
import type { B8Method } from './b8';
import type { SupportMethod } from './support';
import type { IamMethod } from './iam';
import type { McpTasksMethod } from './mcp-tasks';

/** The answer type of `m`. */
type R<M extends keyof Api> = Awaited<ReturnType<Api[M]>>;

/**
 * The B1 methods (and T21's roles + view-as): every live method but B7's (`http/b7.ts`), the interaction API's
 * (`http/interaction.ts`), B2's (`http/b2.ts`), B3's (`http/b3.ts`), B4's (`http/b4.ts`), B12's (`http/b12.ts`), PK's
 * (`http/pk.ts`), B5's (`http/b5.ts`) and B6's (`http/b6.ts`).
 */
export type B1Method = Exclude<
	LiveMethod,
	| B7Method
	| InteractionMethod
	| B2Method
	| B3Method
	| B4Method
	| B4JourneyMethod
	| B12Method
	| PkMethod
	| B5Method
	| B6Method
	| B8Method
	| SupportMethod
	| IamMethod
	| McpTasksMethod
>;

/** agent-api's signup row (`SignupApproval`) in the platform area's `Signup` shape. */
export function toSignup(a: SignupApproval & { reason?: string | null }): Signup {
	return {
		id: a.id,
		name: a.name,
		email: a.email,
		company: a.company ?? '',
		industry: '',
		message: a.message,
		source: a.source === 'chat' ? 'telegram' : a.source === 'invite' ? 'invite' : 'website',
		status: a.status,
		created_at: a.created_at,
		suggested_tenant_name: a.suggested_tenant_name,
		decided_at: a.decided_at ?? null,
		decided_by: a.decided_by ?? null,
		reason: a.reason ?? null,
		tenant_id: a.tenant_id ?? null,
		signals: a.signals ?? [],
	};
}

/** The invite body without empty optionals (the API refuses unknown fields and validates what is sent). */
export function inviteBody(i: EarlyAccessInvite): EarlyAccessInvite {
	const body: EarlyAccessInvite = { email: i.email.trim(), name: i.name.trim(), residency: i.residency };
	if (i.company?.trim()) body.company = i.company.trim();
	if (i.tenantName?.trim()) body.tenantName = i.tenantName.trim();
	if (i.locale) body.locale = i.locale;
	return body;
}

export function httpB1(req: Req): Pick<Api, B1Method> {
	return {
		// ---- core
		me: () => req<R<'me'>>('GET', '/api/me'),
		licenseStatus: () => req<R<'licenseStatus'>>('GET', '/api/license/status'),
		tenants: (opts = {}) =>
			req<R<'tenants'>>('GET', '/api/tenants', {
				query: { include_archived: opts.include_archived || null },
			}),
		tenant: (id) => req<R<'tenant'>>('GET', `/api/tenants/${seg(id)}`),
		jobs: () => req<R<'jobs'>>('GET', '/api/jobs'),
		job: (key) => req<R<'job'>>('GET', `/api/jobs/${seg(key)}`),
		tenantUsers: (tenantId) => req<R<'tenantUsers'>>('GET', `/api/tenants/${seg(tenantId)}/users`),

		// ---- tenant
		tenantOverview: () => req<R<'tenantOverview'>>('GET', '/api/tenant/overview'),
		retryProvision: (id) => req<R<'retryProvision'>>('POST', `/api/tenant/provisioning/${seg(id)}/retry`),
		cancelProvision: (id) => req<R<'cancelProvision'>>('POST', `/api/tenant/provisioning/${seg(id)}/cancel`),
		tenantHome: () => req<R<'tenantHome'>>('GET', '/api/tenant/home'),
		jobBoard: () => req<R<'jobBoard'>>('GET', '/api/jobs', { query: { view: 'board' } }),
		// The API wraps the catalog: `{"templates": [...]}` (routes/jobs.rs); the client hands pages the array.
		jobTemplateOptions: async () =>
			(await req<{ templates: R<'jobTemplateOptions'> }>('GET', '/api/job-templates')).templates ?? [],
		createJobFromTemplate: (input) => req<R<'createJobFromTemplate'>>('POST', '/api/jobs', { body: input }),
		createJobBlank: (input) => req<R<'createJobBlank'>>('POST', '/api/jobs', { body: input }),
		renameJob: (job, input) => req<R<'renameJob'>>('PATCH', `/api/jobs/${seg(job)}`, { body: input }),
		tenantProfile: () => req<R<'tenantProfile'>>('GET', '/api/tenant/profile'),
		saveTenantProfile: (input) => req<R<'saveTenantProfile'>>('PUT', '/api/tenant/profile', { body: input }),
		jobStaff: (job) => req<R<'jobStaff'>>('GET', `/api/jobs/${seg(job)}/staff`),
		saveJobStaff: (job, input) =>
			req<R<'saveJobStaff'>>('PUT', `/api/jobs/${seg(job)}/staff`, { body: input }),
		requestTemplateUpgrade: (job) =>
			req<R<'requestTemplateUpgrade'>>('POST', `/api/jobs/${seg(job)}/upgrade-request`),
		inviteUser: (input) => req<R<'inviteUser'>>('POST', '/api/tenant/invites', { body: input }),
		addUserBySub: ({ sub, roles }) =>
			req<R<'addUserBySub'>>('PUT', `/api/tenant/users/${seg(sub)}`, { body: { roles } }),
		setUserRole: (sub, roles) =>
			req<R<'setUserRole'>>('PATCH', `/api/tenant/users/${seg(sub)}`, { body: { roles } }),
		disableUser: (sub) => req<R<'disableUser'>>('POST', `/api/tenant/users/${seg(sub)}/disable`),
		enableUser: (sub) => req<R<'enableUser'>>('POST', `/api/tenant/users/${seg(sub)}/enable`),
		memberActivity: () => req<R<'memberActivity'>>('GET', '/api/tenant/users/activity'),
		tenantInvites: (query = {}) =>
			req<R<'tenantInvites'>>('GET', '/api/tenant/invites', { query: { ...query } }),
		resendInvite: (id) =>
			req<R<'resendInvite'>>('POST', `/api/tenant/invites/${seg(id)}/resend`, { body: {} }),
		revokeInvite: (id) =>
			req<R<'revokeInvite'>>('POST', `/api/tenant/invites/${seg(id)}/revoke`, { body: {} }),
		tenantAudit: (query = {}) => req<R<'tenantAudit'>>('GET', '/api/audit', { query: { ...query } }),
		tenantAuditSummary: () => req<R<'tenantAuditSummary'>>('GET', '/api/audit/summary'),

		// ---- roles + view-as (D88, D79)
		roleCatalog: () => req<R<'roleCatalog'>>('GET', '/api/roles'),
		setViewAs: (input) => req<R<'setViewAs'>>('PUT', '/api/me/view-as', { body: input }),
		clearViewAs: () => req<R<'clearViewAs'>>('DELETE', '/api/me/view-as'),

		// ---- platform
		// The API has no archived-only filter: `include_archived` adds archived rows, the client splits them.
		platformTenants: async (opts = {}) => {
			const page = await req<R<'platformTenants'>>('GET', '/api/tenants', {
				query: { include_archived: true },
			});
			const archived = !!opts.archived;
			return { ...page, items: page.items.filter((t) => (t.status === 'archived') === archived) };
		},
		platformTenant: async (id) => {
			type D = R<'platformTenant'>;
			const [tenant, users, jobs, licence, provisioning, offboard] = await Promise.all([
				req<Tenant>('GET', `/api/tenants/${seg(id)}`),
				req<{ items: D['users'] }>('GET', `/api/tenants/${seg(id)}/users`),
				req<{ items: D['jobs'] }>('GET', `/api/tenants/${seg(id)}/jobs`),
				req<Pick<D, 'licence' | 'verdict'>>('GET', `/api/tenants/${seg(id)}/licence`),
				req<D['provisioning']>('GET', `/api/tenants/${seg(id)}/provisioning`),
				// 200 `null` for a tenant never offboarded (B1b T9).
				req<D['offboard']>('GET', `/api/tenants/${seg(id)}/offboard`),
			]);
			return {
				tenant,
				users: users.items,
				jobs: jobs.items,
				licence: licence.licence ?? null,
				verdict: licence.verdict ?? null,
				provisioning,
				offboard,
			};
		},
		platformTenantPortfolio: () =>
			req<R<'platformTenantPortfolio'>>('GET', '/api/tenants', {
				query: { view: 'portfolio', include_archived: true },
			}),
		platformTenantFootprint: (id) =>
			req<R<'platformTenantFootprint'>>('GET', `/api/tenants/${seg(id)}/footprint`),
		createTenant: (input) => req<R<'createTenant'>>('POST', '/api/tenants', { body: input }),
		importTenant: (input) => req<R<'importTenant'>>('POST', '/api/tenants/import', { body: input }),
		setTenantStatus: (id, status) =>
			req<R<'setTenantStatus'>>('PATCH', `/api/tenants/${seg(id)}`, { body: { status } }),
		renameTenant: (id, name) =>
			req<R<'renameTenant'>>('PATCH', `/api/tenants/${seg(id)}`, { body: { name } }),
		setTenantSupportAutoAssign: (id, enabled) =>
			req<R<'setTenantSupportAutoAssign'>>('PATCH', `/api/tenants/${seg(id)}`, {
				body: { support_auto_assign: enabled },
			}),
		setTenantAttachmentQuota: (id, bytes) =>
			req<R<'setTenantAttachmentQuota'>>('PATCH', `/api/tenants/${seg(id)}`, {
				body: { attachment_quota_bytes: bytes },
			}),
		offboardTenant: (id, confirmName) =>
			req<R<'offboardTenant'>>('POST', `/api/tenants/${seg(id)}/offboard`, {
				body: { confirm_name: confirmName },
			}),
		// The API refuses unknown fields: send only what was asked for.
		reissueTenantLicence: (id, input) =>
			req<R<'reissueTenantLicence'>>('POST', `/api/tenants/${seg(id)}/licence/reissue`, {
				body: {
					...(input.tenantLicence ? { tenant_licence: true } : {}),
					...(input.memberSub ? { member_sub: input.memberSub } : {}),
				},
			}),
		// B7 T3b: the leaked-token re-mint; `reason` only when given.
		remintTenantLicence: (id, input) =>
			req<R<'remintTenantLicence'>>('POST', `/api/tenants/${seg(id)}/licence/remint`, {
				body: {
					confirm_name: input.confirmName,
					...(input.reason?.trim() ? { reason: input.reason.trim() } : {}),
				},
			}),
		// B7 T3 licensing ops (bodiless POSTs).
		resealTenantLicence: (id) =>
			req<R<'resealTenantLicence'>>('POST', `/api/tenants/${seg(id)}/licence/reseal`),
		licensingOverview: () => req<R<'licensingOverview'>>('GET', '/api/platform/licensing'),
		resealLicences: () => req<R<'resealLicences'>>('POST', '/api/platform/licence-tokens/reseal'),
		nudgeLicenceVerdicts: () =>
			req<R<'nudgeLicenceVerdicts'>>('POST', '/api/platform/licence-verdicts/refresh'),
		platformRetryProvision: (tenantId, rowId) =>
			req<R<'platformRetryProvision'>>(
				'POST',
				`/api/tenants/${seg(tenantId)}/provisioning/${seg(rowId)}/retry`,
			),
		platformCancelProvision: (tenantId, rowId) =>
			req<R<'platformCancelProvision'>>(
				'POST',
				`/api/tenants/${seg(tenantId)}/provisioning/${seg(rowId)}/cancel`,
			),
		platformJobTemplates: () =>
			req<R<'platformJobTemplates'>>('GET', '/api/templates', { query: { kind: 'job' } }),
		platformCreateJob: (tenantId, input) =>
			req<R<'platformCreateJob'>>('POST', `/api/tenants/${seg(tenantId)}/jobs`, { body: input }),
		platformTenantInvites: (tenantId, query = {}) =>
			req<R<'platformTenantInvites'>>('GET', `/api/tenants/${seg(tenantId)}/invites`, {
				query: { ...query },
			}),
		platformResendInvite: (tenantId, inviteId) =>
			req<R<'platformResendInvite'>>(
				'POST',
				`/api/tenants/${seg(tenantId)}/invites/${seg(inviteId)}/resend`,
				{ body: {} },
			),
		platformRevokeInvite: (tenantId, inviteId) =>
			req<R<'platformRevokeInvite'>>(
				'POST',
				`/api/tenants/${seg(tenantId)}/invites/${seg(inviteId)}/revoke`,
				{ body: {} },
			),
		platformRehomeUser: (sub, targetTenant) =>
			req<R<'platformRehomeUser'>>('POST', `/api/users/${seg(sub)}/rehome`, {
				body: { tenant_id: targetTenant },
			}),
		signupQueue: async (opts = {}) => {
			const q = await req<Omit<SignupQueue, 'items'> & { items: SignupApproval[] }>(
				'GET',
				'/api/signup-approvals',
				{ query: { status: opts.status ?? 'pending' } },
			);
			return { ...q, items: q.items.map(toSignup) };
		},
		approveSignup: async (id, tenantName, residency) => {
			const r = await req<ApproveResult & { signup?: SignupApproval }>(
				'POST',
				`/api/signup-approvals/${seg(id)}/approve`,
				{ body: { tenantName: tenantName.trim(), residency } },
			);
			const signup: Signup = r.signup
				? toSignup(r.signup)
				: ({ id, status: 'approved', tenant_id: r.tenant.id } as Signup);
			return { signup, tenant: r.tenant as Tenant };
		},
		rejectSignup: async (id, reason) =>
			toSignup(
				await req<SignupApproval>('POST', `/api/signup-approvals/${seg(id)}/reject`, {
					body: reason.trim() ? { reason: reason.trim() } : {},
				}),
			),
		retrySignup: (id) => req<R<'retrySignup'>>('POST', `/api/signup-approvals/${seg(id)}/retry`),
		inviteEarlyAccess: async (input) => {
			const r = await req<InviteResult>('POST', '/api/signup-approvals/invite', { body: inviteBody(input) });
			return {
				...r,
				signup: toSignup(r.signup),
				tenant: {
					id: r.tenant.id,
					name: r.tenant.name,
					status: (r.tenant.status ?? 'active') as Tenant['status'],
				},
				steps: stepsOf(r.steps),
			};
		},
		signupLaunch: () => req<R<'signupLaunch'>>('GET', '/api/platform/signup-launch'),
		platformAudit: (opts = {}) =>
			req<R<'platformAudit'>>('GET', '/api/platform/audit', { query: { ...opts } }),
		platformAuditSummary: () => req<R<'platformAuditSummary'>>('GET', '/api/platform/audit/summary'),
	};
}

/** The signups page's calls over HTTP, in agent-api's own row shape (super_admin; the API refuses everyone else). */
export function httpSignups(req: Req): SignupsApi {
	return {
		list: (status: SignupApprovalStatus, before?: string | null) =>
			req<SignupApprovalPage>('GET', '/api/signup-approvals', { query: { status, before } }),
		approve: (id, tenantName, residency) =>
			req<ApproveResult>('POST', `/api/signup-approvals/${seg(id)}/approve`, {
				body: { tenantName: tenantName.trim(), residency },
			}),
		reject: (id) => req<unknown>('POST', `/api/signup-approvals/${seg(id)}/reject`, { body: {} }),
		retry: (id) => req<{ steps: SignupApproval['steps'] }>('POST', `/api/signup-approvals/${seg(id)}/retry`),
		invite: (input) => req<InviteResult>('POST', '/api/signup-approvals/invite', { body: inviteBody(input) }),
	};
}
