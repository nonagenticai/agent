import type { TenantSummary, TenantUser } from './api/types/core';

/** Tenants whose name or id contains `q` (case-insensitive); all of them for a blank query. */
export function filterTenants(list: TenantSummary[], q: string): TenantSummary[] {
	const needle = q.trim().toLowerCase();
	if (!needle) return list;
	return list.filter((t) => t.name.toLowerCase().includes(needle) || t.id.toLowerCase().includes(needle));
}

/** The tenant's name when the list knows it, else its id; null for no tenant. */
export function tenantName(list: TenantSummary[], id: string | null): string | null {
	if (!id) return null;
	return list.find((t) => t.id === id)?.name ?? id;
}

/**
 * `sub` is the tenant's only holder of the Tenant admin role: nobody may remove that role from it or disable it
 * (409 last_tenant_admin). Counts HELD roles, never a view-as.
 */
export function isLastAdmin(users: Pick<TenantUser, 'sub' | 'roles'>[], sub: string): boolean {
	const admins = users.filter((u) => u.roles.includes('tenant_admin'));
	return admins.length === 1 && admins[0]!.sub === sub;
}
