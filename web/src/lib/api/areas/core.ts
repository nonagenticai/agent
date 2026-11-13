// The `CoreApi` contract.
import type {
	JobKey,
	JobSummary,
	LicenseStatus,
	Me,
	Paged,
	Tenant,
	TenantId,
	TenantSummary,
	TenantUser,
} from '$lib/api/types/core';

export type CoreApi = {
	/** `GET /api/me` — the caller, their roles (held, effective, view-as) and the tenant they act in. */
	me(): Promise<Me>;
	/**
	 * `GET /api/license/status` — what licence gate 2 says about the caller's tenant (the super_admin's active one),
	 * never a 402: the shell's licence state reads it to explain a block and to retry.
	 */
	licenseStatus(): Promise<LicenseStatus>;
	/** `GET /api/tenants` (super_admin) — live tenants; `?include_archived=true` adds archived ones. */
	tenants(opts?: { include_archived?: boolean }): Promise<Paged<TenantSummary>>;
	/** `GET /api/tenants/{id}` (super_admin, or a member of that tenant). */
	tenant(id: TenantId): Promise<Tenant>;
	/** `GET /api/jobs` — the active tenant's jobs. */
	jobs(): Promise<Paged<JobSummary>>;
	/** `GET /api/jobs/{job}` — one job of the active tenant. */
	job(key: JobKey): Promise<JobSummary>;
	/** `GET /api/tenants/{id}/users` — members, disabled ones included (greyed in the UI). */
	tenantUsers(tenantId: TenantId): Promise<Paged<TenantUser>>;
};
