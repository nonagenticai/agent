// T5 Usage: the page helpers (pure).
import { describe, expect, it } from 'vitest';
import { delta, driverHeadline, tokensPerTurn } from '../../src/routes/admin/usage/usage';

describe('T5 helpers', () => {
	it('delta, tokensPerTurn, headline', () => {
		expect(delta(120, 100)).toBeCloseTo(0.2);
		expect(delta(5, null)).toBeNull();
		expect(delta(5, 0)).toBeNull();
		expect(tokensPerTurn({ turns: 0, tokens: 10 })).toBe(0);
		const mk = (activity: string, turns: number, tokens: number) => ({
			job: 'j',
			activity,
			label: activity,
			stage: 's',
			turns,
			tokens,
			searches: 0,
		});
		expect(driverHeadline([mk('a:x', 50, 500), mk('a:y', 50, 500)], { turns: 100, tokens: 1000 })).toBeNull();
		expect(
			driverHeadline([mk('a:x', 50, 300), mk('a:y', 50, 700), mk('unqualified', 0, 0)], {
				turns: 100,
				tokens: 1000,
			}),
		).toBe('a:y is the most expensive per turn: 70.0 % of tokens on 50.0 % of turns.');
	});

	it('driverHeadline: a request-type driver (no colon, agent#46) can win the headline', () => {
		const mk = (activity: string, label: string, turns: number, tokens: number) => ({
			job: 'j',
			activity,
			label,
			stage: '',
			turns,
			tokens,
			searches: 0,
		});
		expect(
			driverHeadline(
				[
					mk('support:claim_warranty', 'Claim warranty', 50, 300),
					mk('order_status', 'Order status', 50, 700),
				],
				{
					turns: 100,
					tokens: 1000,
				},
			),
		).toBe('Order status is the most expensive per turn: 70.0 % of tokens on 50.0 % of turns.');
	});

	it('driverHeadline: the unqualified bucket never wins, even when it is the most expensive per turn', () => {
		const mk = (activity: string, label: string, turns: number, tokens: number) => ({
			job: 'j',
			activity,
			label,
			stage: '',
			turns,
			tokens,
			searches: 0,
		});
		const totals = { turns: 100, tokens: 1000 };
		expect(
			driverHeadline(
				[mk('unqualified', 'Not qualified', 10, 900), mk('order_status', 'Order status', 90, 100)],
				totals,
			),
		).toBeNull();
		expect(
			driverHeadline(
				[
					mk('unqualified', 'Not qualified', 10, 600),
					mk('order_status', 'Order status', 30, 400),
					mk('a:x', 'X', 60, 0),
				],
				totals,
			),
		).toBe('Order status is the most expensive per turn: 40.0 % of tokens on 30.0 % of turns.');
	});
});
