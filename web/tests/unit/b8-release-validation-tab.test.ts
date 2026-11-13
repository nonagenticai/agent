// B8 T12a (A12): the release detail's Validation tab — grid rows, the latest grid run, polling while it runs.
import { describe, expect, it } from 'vitest';
import type { ValidationGridRun } from '../../src/lib/api/types/journey';
import {
	gridActive,
	gridProgress,
	gridStatusLine,
	releaseValidationUi,
	triggerLabel,
} from '../../src/routes/admin/jobs/[job]/release/[id]/validation-tab';

const run = (o: Partial<ValidationGridRun>): ValidationGridRun => ({
	id: 'g1',
	trigger: 'approve',
	status: 'queued',
	done: 0,
	total: 4,
	queued_at: '2026-10-26T10:00:00Z',
	started_at: null,
	finished_at: null,
	error_code: null,
	...o,
});

describe('grid run wording', () => {
	it('says queued on approve, progress, finished and failed', () => {
		expect(gridStatusLine(null)).toBe('No validation run yet');
		expect(gridStatusLine(run({}))).toBe('Queued on approve');
		expect(gridStatusLine(run({ status: 'running', done: 1 }))).toBe('Running · 1 of 4 questions');
		expect(gridStatusLine(run({ status: 'done', done: 4 }))).toBe('Finished · 4 of 4 questions');
		expect(gridStatusLine(run({ status: 'failed', error_code: 'unavailable' }))).toContain(
			'Failed (unavailable)',
		);
		expect(gridProgress(run({ status: 'running', done: 1 }))).toBe(0.25);
		expect(gridActive(run({ status: 'running' }))).toBe(true);
		expect(gridActive(run({ status: 'done' }))).toBe(false);
		expect(triggerLabel('manual')).toBe('run by a person');
	});
});
