import { describe, expect, it } from 'vitest';

import { escalationLabel } from '$lib/mcp-tasks/tasks';

describe('escalationLabel', () => {
	it('names why staff took a task over', () => {
		expect(escalationLabel('no_answer')).toBe('No answer in time');
		expect(escalationLabel('id_field:project_id')).toBe('Needs a reference the agent could not find');
		expect(escalationLabel('failed')).toBe('The task failed');
		expect(escalationLabel('handover')).toBe('The conversation was handed over');
		expect(escalationLabel('refused:decide_in_fr')).toBe('fr needs a person to decide (decide_in_fr)');
	});

	it('shows nothing without a reason and an unknown reason as stored', () => {
		expect(escalationLabel(null)).toBe('');
		expect(escalationLabel('something_else')).toBe('something_else');
	});
});
