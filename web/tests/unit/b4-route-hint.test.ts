// B4 T9b web (F-19, D94): the console Inspector's "Likely request type" row reads the `route_hint` a qualified
// request carries (agent-api QualifiedRequest.route_hint, null until B5's hinter), and the J21 reason taxonomy
// lists D94's `gate_unavailable` and D85's `staff_decision`.
import { afterEach, describe, expect, it } from 'vitest';
import { cleanup, render, screen } from '@testing-library/svelte';
import Inspector from '../../src/routes/console/Inspector.svelte';
import type { BusEvent } from '../../src/lib/api/types/live';
import { REASON, REASONS } from '../../src/lib/journey/J21Handover';
import { toHandoverItem } from '../../src/lib/api/http/b12';

afterEach(() => cleanup());

function qualified(
	payload: Record<string, unknown>,
	topic = 'order-support.returns.request.qualified',
): BusEvent {
	return {
		stream: 'bus:customer:order-support',
		entry_id: '1-0',
		consumer_group: 'orchestrator',
		acked: true,
		envelope: {
			id: 'e1',
			topic,
			from: 'interaction',
			correlation_id: 'CS-1',
			published_at_ms: 1_760_000_000_000,
			payload,
			tenant_id: 't',
			job: 'demo-retail',
		},
	};
}

describe('Inspector route hint (T9b)', () => {
	it('shows the hinted request type and confidence', () => {
		render(Inspector, {
			event: qualified({
				case_id: 'CS-1',
				route_hint: { request_type: 'return', activity: 'returns:start', confidence: 0.82 },
			}),
			agent: null,
			ack: 'acked',
		});
		const row = screen.getByTestId('inspector-route-hint');
		expect(row.textContent).toContain('Likely request type');
		expect(row.textContent).toContain('return');
		expect(row.textContent).toContain('82%');
		expect(row.textContent).toContain('returns:start');
	});

	it('a null hint (no B5 hinter yet) says so; an entry without the field has no row', () => {
		render(Inspector, { event: qualified({ case_id: 'CS-1', route_hint: null }), agent: null, ack: null });
		expect(screen.getByTestId('inspector-route-hint').textContent).toContain('none');
		cleanup();
		render(Inspector, {
			event: qualified({ case_id: 'CS-1' }, 'order-support.returns.request.resolved'),
			agent: null,
			ack: null,
		});
		expect(screen.queryByTestId('inspector-route-hint')).toBeNull();
	});
});

describe('handover reasons (D85, D94)', () => {
	it('J21 lists 10 reasons: gate_unavailable, staff_decision and the two from 0179 labelled', () => {
		expect(REASONS).toHaveLength(10);
		expect(REASON.gate_unavailable.label).toBe('Gate service unavailable');
		expect(REASON.staff_decision.label).toBe('Staff decision');
		expect(REASON.not_offered.label).toBe('Required action not available');
		expect(REASON.success_criteria_unmet.label).toBe('Success criteria not met');
	});

	it('the live queue keeps both reasons instead of reading them as out of scope', () => {
		expect(toHandoverItem({ id: 'HO-1', reason: 'gate_unavailable' } as never).reason).toBe(
			'gate_unavailable',
		);
		expect(toHandoverItem({ id: 'HO-2', reason: 'staff_decision' } as never).reason).toBe('staff_decision');
		expect(toHandoverItem({ id: 'HO-4', reason: 'not_offered' } as never).reason).toBe('not_offered');
		expect(toHandoverItem({ id: 'HO-5', reason: 'success_criteria_unmet' } as never).reason).toBe(
			'success_criteria_unmet',
		);
		expect(toHandoverItem({ id: 'HO-3', reason: 'made_up' } as never).reason).toBe('out_of_scope');
	});
});
