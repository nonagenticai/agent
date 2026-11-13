// B8 T15a (F-51, F-52): scorecard weight per activity, import preview with column choice, delete (and cancel) an import.
import { ApiError } from '../../src/lib/api/errors';
import type { DiscoveryPreview } from '../../src/lib/api/types/journey';
import { describe, expect, it } from 'vitest';
import {
	importFileProblem,
	initialColumns,
	parseWeight,
	uploadInput,
} from '../../src/routes/admin/jobs/[job]/discovery/import';

const PREVIEW: DiscoveryPreview = {
	format: 'xlsx',
	columns: ['id', 'subject', 'body', 'n'],
	sample_rows: [['1', 'Refund', 'Where is it?', '3']],
	rows: 120,
	suggested_text_column: 'body',
	suggested_count_column: 'n',
};

describe('import helpers', () => {
	it('suggested columns start selected; an unknown suggestion falls back to the first column', () => {
		expect(initialColumns(PREVIEW)).toEqual({ text: 'body', count: 'n' });
		expect(initialColumns({ ...PREVIEW, suggested_text_column: 'zz', suggested_count_column: null })).toEqual(
			{
				text: 'id',
				count: '',
			},
		);
	});
	it('uploadInput sends the chosen columns; no count column when none or the same as the text', () => {
		const f = new File(['x'], 'e.xlsx');
		expect(uploadInput(f, PREVIEW, { text: 'body', count: 'n' })).toMatchObject({
			rows: 120,
			text_column: 'body',
			count_column: 'n',
		});
		const o = uploadInput(f, PREVIEW, { text: 'body', count: '' });
		expect(o.count_column).toBeUndefined();
		expect(uploadInput(f, PREVIEW, { text: 'body', count: 'body' }).count_column).toBeUndefined();
	});
	it('weights are whole numbers 0..1000', () => {
		expect(parseWeight(' 250 ')).toEqual({ ok: true, weight: 250 });
		expect(parseWeight('0')).toEqual({ ok: true, weight: 0 });
		for (const bad of ['', '1.5', '-1', 'abc', '1001']) expect(parseWeight(bad).ok, bad).toBe(false);
	});
	it('file refusals', () => {
		expect(importFileProblem(new ApiError(413, 'too_large', 'x'))).toMatch(/20 MiB/);
		expect(importFileProblem(new ApiError(422, 'unsupported_file', 'x'))).toMatch(/\.xlsx/);
		expect(importFileProblem(new ApiError(422, 'empty_file', 'no rows'))).toBe('no rows');
	});
});
