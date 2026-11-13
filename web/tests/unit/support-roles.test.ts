// The one "can work conversations" rule (owner decision 2026-10-26): support_agent OR support_manager — Take, reply,
// Hand back, Resolve, Hand over, Link customer, be assigned. Operator and job_designer keep notes and feedback only.
import { describe, expect, it } from 'vitest';
import { CONVERSATION_WORKERS, WORK_HINT, canWorkConversations, workAttrs } from '../../src/lib/auth/roles';
import type { Role } from '../../src/lib/api/types/core';

const who = (member: boolean, roles: Role[]) => ({
	superAdmin: false,
	member,
	has: (r: Role) => roles.includes(r),
});

describe('canWorkConversations', () => {
	it('is support_agent or support_manager', () => {
		expect([...CONVERSATION_WORKERS]).toEqual(['support_agent', 'support_manager']);
		expect(canWorkConversations(who(true, ['support_agent']))).toBe(true);
		expect(canWorkConversations(who(true, ['support_manager']))).toBe(true);
		expect(canWorkConversations(who(true, ['support_manager', 'operator']))).toBe(true);
	});

	it('is not operator, job_designer, integrations_manager, an observer or a non-member', () => {
		for (const r of ['operator', 'job_designer', 'integrations_manager', 'tenant_admin'] as Role[])
			expect(canWorkConversations(who(true, [r])), r).toBe(false);
		expect(canWorkConversations(who(true, []))).toBe(false);
		expect(canWorkConversations(who(false, ['support_agent']))).toBe(false);
	});

	it('workAttrs disables with the hint for anyone else', () => {
		expect(workAttrs(who(true, ['support_manager']))).toEqual({ disabled: false, title: undefined });
		expect(workAttrs(who(true, ['operator']))).toEqual({ disabled: true, title: WORK_HINT });
		expect(WORK_HINT).toBe('Needs the Support agent or Support manager role');
	});
});
