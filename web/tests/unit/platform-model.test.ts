// Pure model logic behind P4/P5/P12: axis-rule conditions (with value trees), labels and tombstone cascades.
import { describe as group, expect, it } from 'vitest';
import { describe, evaluate } from '../../src/lib/platform/model';

group('evaluate', () => {
	const electricity = {
		order_type: ['continuous'],
		payment_type: ['balancing'],
		'renewal.initiator': ['provider'],
	};
	it('eq / and / in / or / not', () => {
		expect(
			evaluate(
				{ and: [{ eq: ['order_type', 'continuous'] }, { eq: ['payment_type', 'balancing'] }] },
				electricity,
			),
		).toBe(true);
		expect(evaluate({ eq: ['order_type', 'discrete'] }, electricity)).toBe(false);
		expect(evaluate({ in: ['order_type', ['discrete', 'continuous']] }, electricity)).toBe(true);
		expect(
			evaluate(
				{ or: [{ eq: ['order_type', 'discrete'] }, { not: { eq: ['payment_type', 'singleton'] } }] },
				electricity,
			),
		).toBe(true);
	});
	it('a child value satisfies its parent, not the other way round', () => {
		expect(evaluate({ eq: ['payment_type', 'recurring'] }, { payment_type: ['recurring.fixed'] })).toBe(true);
		expect(evaluate({ eq: ['payment_type', 'recurring.fixed'] }, { payment_type: ['recurring'] })).toBe(
			false,
		);
		expect(evaluate({ eq: ['payment_type', 'recurring'] }, { payment_type: ['recurringx'] })).toBe(false);
	});
	it('describes a condition for humans', () => {
		expect(describe({ and: [{ eq: ['a', 'x'] }, { in: ['b', ['y', 'z']] }] })).toBe('a = x ∧ b ∈ {y, z}');
	});
});
