// B8 T12a (F-50): edit / delete a validation question and import questions from a CSV on the J17 page.
import { describe, expect, it } from 'vitest';
import { ApiError } from '../../src/lib/api/errors';
import {
	changedFields,
	draftOf,
	draftProblem,
	importProblem,
} from '../../src/routes/admin/jobs/[job]/validation/questions';

describe('question helpers', () => {
	const row = { question: 'Q', expected: 'E', reference: 'R §1', activity: 'a:b' };
	it('changedFields sends only what changed (trimmed); nothing changed = empty', () => {
		const d = draftOf(row);
		expect(changedFields(d, { ...d })).toEqual({});
		expect(changedFields(d, { ...d, question: '  Q2 ', activity: 'a:c' })).toEqual({
			question: 'Q2',
			activity: 'a:c',
		});
	});
	it('all three text fields stay required', () => {
		expect(draftProblem(draftOf(row))).toBeNull();
		expect(draftProblem({ ...draftOf(row), reference: '  ' })).toMatch(/all required/);
	});
	it('importProblem: 413 and a bad row reject the whole file', () => {
		expect(importProblem(new ApiError(413, 'too_large', 'x'))).toMatch(
			/larger than 4 MiB.*whole file.*Nothing was imported/,
		);
		expect(importProblem(new ApiError(413, 'too_many_questions', 'x'))).toMatch(/at most 500.*whole file/);
		expect(importProblem(new ApiError(422, 'invalid_rows', 'x', { rows: [3, 7] }))).toMatch(
			/Rows 3, 7 are not valid, so the whole file was rejected/,
		);
		expect(importProblem(new ApiError(422, 'invalid_rows', 'x', { rows: [4] }))).toMatch(
			/^Row 4 is not valid/,
		);
		expect(importProblem(new ApiError(422, 'missing_columns', 'needs columns'))).toBe('needs columns');
	});
});
