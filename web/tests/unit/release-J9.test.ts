// J9 Business rules: the dry-run evaluator (pure).
import { describe, expect, it } from 'vitest';
import { evaluateJ9 } from '../../src/lib/release/J9-evaluate';

describe('J9 evaluator (rule.evaluate, pure)', () => {
	const ret = { window_days: '30', excluded_categories: 'made-to-measure rugs, opened bedding' };
	it('RET-01: inside the window, past it, and an excluded category', () => {
		expect(
			evaluateJ9(
				'RET-01',
				{ 'order.delivered_at': '2026-10-09', 'offering.category': 'textiles' },
				ret,
				'2026-10-18',
			).code,
		).toBe('eligible');
		const late = evaluateJ9(
			'RET-01',
			{ 'order.delivered_at': '2026-08-27', 'offering.category': 'lighting' },
			ret,
			'2026-10-18',
		);
		expect(late.code).toBe('ineligible:RET-01');
		expect(late.reason).toMatch(/closed 22 days ago/);
		expect(
			evaluateJ9(
				'RET-01',
				{ 'order.delivered_at': '2026-10-17', 'offering.category': 'Opened bedding' },
				ret,
				'2026-10-18',
			).code,
		).toBe('ineligible:RET-01');
	});
	it('WAR-01 counts whole months; PAY-04 escalates when pending longer than the drop-off', () => {
		expect(
			evaluateJ9('WAR-01', { 'order.delivered_at': '2024-10-18' }, { period_months: '24' }, '2026-10-18')
				.code,
		).toBe('ineligible:WAR-01');
		expect(
			evaluateJ9('WAR-01', { 'order.delivered_at': '2024-10-19' }, { period_months: '24' }, '2026-10-18')
				.code,
		).toBe('eligible');
		const v = { drop_off_days: '7', customer_text: 'It drops off.' };
		expect(
			evaluateJ9('PAY-04', { 'payment.status': 'pending', 'payment.at': '2026-10-16' }, v, '2026-10-18').code,
		).toBe('ineligible:PAY-04');
		expect(
			evaluateJ9('PAY-04', { 'payment.status': 'pending', 'payment.at': '2026-10-06' }, v, '2026-10-18')
				.outcome,
		).toBe('escalate');
		expect(evaluateJ9('PAY-04', { 'payment.status': 'settled' }, v, '2026-10-18').code).toBe('eligible');
	});
	it('a value not set → placeholder (the agent hands over)', () => {
		const r = evaluateJ9(
			'PAY-04',
			{ 'payment.status': 'pending' },
			{ drop_off_days: null, customer_text: '' },
			'2026-10-18',
		);
		expect(r.outcome).toBe('placeholder');
		expect(r.reason).toMatch(/drop_off_days, customer_text/);
	});
});
