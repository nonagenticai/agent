// P7 Template tag detail: version timeline, composition stack, pin relation + catch-up, contents by family,
// untagged revisions waiting for the next tag.
import { describe, expect, it } from 'vitest';
import { composedOnBy, layerStack, pinRelation } from '../../src/lib/model/logic';

const L = (
	layer_key: string,
	scope: 'base' | 'industry' | 'industry_dept' | 'job',
	industry: string | null,
) => ({
	layer_key,
	scope,
	industry,
});

describe('P7 logic', () => {
	const layers = [
		L('job/customer_care_agent', 'job', 'retail'),
		L('industry/utilities', 'industry', 'utilities'),
		L('base', 'base', null),
		L('industry_dept/retail/customer_service', 'industry_dept', 'retail'),
		L('industry/retail', 'industry', 'retail'),
	];
	it('layerStack composes base → industry → department → job of the same industry', () => {
		expect(layerStack(layers, layers[0]!).map((l) => l.layer_key)).toEqual([
			'base',
			'industry/retail',
			'industry_dept/retail/customer_service',
			'job/customer_care_agent',
		]);
		expect(layerStack(layers, layers[2]!).map((l) => l.layer_key)).toEqual(['base']);
	});
	it('composedOnBy lists the layers that inherit this one', () => {
		expect(composedOnBy(layers, layers[2]!)).toHaveLength(4);
		expect(composedOnBy(layers, layers[4]!).map((l) => l.layer_key)).toEqual([
			'industry_dept/retail/customer_service',
			'job/customer_care_agent',
		]);
		expect(composedOnBy(layers, layers[0]!)).toEqual([]);
	});
	it('pinRelation', () => {
		expect([pinRelation(4, 4), pinRelation(3, 4), pinRelation(4, 3)]).toEqual(['here', 'behind', 'ahead']);
	});
});
