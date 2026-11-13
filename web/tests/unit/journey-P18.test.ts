// P18 Discovery generator list view: the pipeline model, run progress / outputs / totals, and the cost estimate.
import { describe, expect, it } from 'vitest';
import { P18_ORDER, P18_TENANT_STEPS, llmStepsFor } from '../../src/lib/journey/P18GenPipeline';

describe('P18 pipeline model', () => {
	it('tenant runs skip business model, operations and harmonise; Input/Validate/Package never call the LLM', () => {
		expect(P18_ORDER).toHaveLength(9);
		expect(P18_ORDER.filter((k) => !P18_TENANT_STEPS.includes(k))).toEqual([
			'business_model',
			'operations',
			'harmonise',
		]);
		expect(llmStepsFor('platform')).toEqual([
			'business_model',
			'journey',
			'objects',
			'operations',
			'harmonise',
			'use_cases',
		]);
		expect(llmStepsFor('tenant')).toEqual(['journey', 'objects', 'use_cases']);
	});
});
