import type { Me, Role } from '../../src/lib/api/types/core';
import { ROLE_KEYS } from '../../src/lib/api/types/core';

export const BOOT_TENANT = '0195f48b-d000-7000-8000-000000000099';
/** Every §R.1 role (a tenant's first member, or a super_admin acting in a tenant). */
export const ALL_ROLES: Role[] = [...ROLE_KEYS];

/** A `GET /api/me` body for a person in the bootstrap tenant holding `roles` (`[]` = observer); `null` = not provisioned. */
export function meAs(roles: Role[] | null, sub = 'u1'): Me {
	return {
		sub,
		actor: sub,
		display_name: sub,
		super_admin: false,
		tenant: roles ? { id: BOOT_TENANT, name: 'Bootstrap', status: 'active' } : null,
		active_tenant: roles ? BOOT_TENANT : null,
		roles: roles ?? [],
		effective_roles: roles ?? [],
		view_as: null,
	};
}
