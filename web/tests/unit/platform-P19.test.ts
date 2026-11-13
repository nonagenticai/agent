// P19 Platform audit: routing picture, open failures (failed with no later success on the same subject),
// family / actor / subject filters, catalogue writes audited, object links. pure helpers.
import { describe, expect, it } from 'vitest';
import type { PlatformAuditEvent } from '../../src/lib/api/types/platform';
import { byDay, familyOf, objectLink, outcomeOf, sentence, threadKey } from '../../src/lib/platform/auditP19';

const ev = (p: Partial<PlatformAuditEvent>): PlatformAuditEvent => ({
	id: 'x',
	at: '2026-10-01T00:00:00Z',
	actor_id: 'owner',
	actor_type: 'user',
	action: 'tenant.import',
	subject_tenant: null,
	subject_label: 'Acme',
	status: 'ok',
	details: {},
	...p,
});

describe('auditP19 helpers', () => {
	it('classifies actions into families', () => {
		expect(familyOf('signup.intake')).toBe('signup');
		expect(familyOf('license.issue_failed')).toBe('creation');
		expect(familyOf('license.reseal')).toBe('keys');
		expect(familyOf('dimension.value_add')).toBe('catalogue');
		expect(familyOf('licence.gate_refused')).toBe('other');
	});

	it('threads by subject name, ignoring the parenthetical; counts are not threads', () => {
		expect(threadKey(ev({ subject_label: 'Northwind IT Services (first attempt)' }))).toBe(
			'northwind it services',
		);
		expect(threadKey(ev({ action: 'license.reseal', subject_label: '3 tenants' }))).toBeNull();
	});

	it('a failure is resolved only by a LATER creating success on the same subject', () => {
		const fail = ev({ id: 'f', status: 'failed', at: '2026-10-06T09:58:00Z', subject_label: 'Acme (first)' });
		const later = ev({ id: 'ok', at: '2026-10-06T10:00:00Z' });
		const earlier = ev({ id: 'old', at: '2026-10-01T10:00:00Z' });
		expect(outcomeOf(fail, [fail, later])).toMatchObject({ state: 'resolved', by_id: 'ok' });
		expect(outcomeOf(fail, [fail, earlier])).toMatchObject({ state: 'open', by_id: null });
		// a later signup.intake is not a resolution
		expect(outcomeOf(fail, [fail, ev({ action: 'signup.intake', at: '2026-10-07T00:00:00Z' })])?.state).toBe(
			'open',
		);
		expect(outcomeOf(later, [fail, later])).toBeUndefined();
	});

	it('links each event to the object it changed', () => {
		expect(objectLink(ev({ subject_tenant: 't1', subject_label: 'Acme' }))).toEqual({
			href: '/platform/tenants/t1',
			label: 'Acme',
		});
		expect(objectLink(ev({ action: 'signup.reject' })).href).toBe('/platform/signups');
		expect(objectLink(ev({ action: 'license.reseal' })).href).toBe('/platform/licensing');
		expect(objectLink(ev({ action: 'dimension.value_add' })).href).toBe('/platform/dimensions');
		expect(objectLink(ev({ action: 'layer.tag' })).href).toBe('/platform/layers');
	});

	it('writes a sentence from ids and codes and groups by UTC day', () => {
		expect(
			sentence(ev({ action: 'license.reseal', details: { kid: 'k2', resealed: 3, remaining: 0 } })),
		).toBe('Re-sealed 3 tenant tokens to key k2; 0 left on older keys.');
		const g = byDay([
			ev({ id: 'a', at: '2026-10-17T10:00:00Z' }),
			ev({ id: 'b', at: '2026-10-17T07:00:00Z' }),
			ev({ id: 'c', at: '2026-10-16T07:00:00Z' }),
		]);
		expect(g.map((d) => [d.day, d.events.length])).toEqual([
			['2026-10-17', 2],
			['2026-10-16', 1],
		]);
	});
});
