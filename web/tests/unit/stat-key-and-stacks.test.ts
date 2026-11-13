import { describe, expect, it } from 'vitest';
import { statKey } from '../../src/lib/runtime/J22-calls';
import { STACK_ORDER } from '../../src/lib/model/templatesP6';

describe('statKey', () => {
	it('tells apart the same op on different routes to the same system', () => {
		const a = statKey({ op: 'payment.refund', instance: null, route: 'instance' });
		const b = statKey({ op: 'payment.refund', instance: null, route: 'not_offered' });
		const c = statKey({ op: 'payment.refund', instance: 'shop-test', route: 'instance' });
		expect(new Set([a, b, c]).size).toBe(3);
	});
});

describe('STACK_ORDER', () => {
	it('has a slot for every template scope, in composition order', () => {
		expect(STACK_ORDER).toEqual(['base', 'industry', 'base_dept', 'industry_dept', 'job']);
	});
});
