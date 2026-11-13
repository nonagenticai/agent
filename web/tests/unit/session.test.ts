import { describe, expect, it } from 'vitest';
import { Session } from '../../src/lib/auth/session.svelte';
import { ALL_ROLES, meAs } from './me';

const user = { sub: 'u1', name: 'A. Bakker' };

describe('session roles come from /api/me (D88)', () => {
	it('roles are independent; an observer is a member with none', () => {
		const s = new Session();
		s.set(user, meAs([]));
		expect(s.member).toBe(true);
		expect(s.roles).toEqual([]);
		expect(s.has('job_designer')).toBe(false);
		s.set(user, meAs(['support_agent']));
		expect(s.has('support_agent')).toBe(true);
		expect(s.has('support_manager')).toBe(false);
		expect(s.has('tenant_admin')).toBe(false);
		s.set(user, meAs(ALL_ROLES));
		for (const r of ALL_ROLES) expect(s.has(r)).toBe(true);
		expect(s.superAdmin).toBe(false);
	});
	it('checks read the EFFECTIVE roles (view-as narrows); held roles stay visible', () => {
		const s = new Session();
		s.set(user, {
			...meAs(ALL_ROLES),
			effective_roles: ['support_agent'],
			view_as: { roles: ['support_agent'], expires_at: '2026-10-20T13:00:00Z' },
		});
		expect(s.roles).toEqual(ALL_ROLES);
		expect(s.effectiveRoles).toEqual(['support_agent']);
		expect(s.has('support_agent')).toBe(true);
		expect(s.has('job_designer')).toBe(false);
		expect(s.viewAs?.roles).toEqual(['support_agent']);
	});
	it('an unprovisioned caller (null tenant) is no member and has no role', () => {
		const s = new Session();
		s.set(user, meAs(null));
		expect(s.member).toBe(false);
		expect(s.has('tenant_admin')).toBe(false);
	});
	it('a super_admin holds every role only once a tenant is active', () => {
		const s = new Session();
		s.set(user, { ...meAs(null), super_admin: true });
		expect(s.superAdmin).toBe(true);
		expect(s.member).toBe(false);
		s.set(user, { ...meAs(ALL_ROLES), super_admin: true });
		expect(s.has('tenant_admin')).toBe(true);
	});
	it('no me, no role', () => {
		const s = new Session();
		expect(s.member).toBe(false);
		expect(s.has('support_agent')).toBe(false);
	});
});
