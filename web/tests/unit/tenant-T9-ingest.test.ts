// Tenant ingest files: client-side upload limits and the column preview helpers (pure).
import { describe, expect, it } from 'vitest';
import { checkUploads, mergeFiles, parseCsv, tablePreview } from '../../src/lib/tenant/ingest-files';

/** A File with real content, or a declared size (the limits read `size` only). */
function file(name: string, content = 'x', size?: number, type = ''): File {
	const f = new File([content], name, { type });
	if (size !== undefined) Object.defineProperty(f, 'size', { value: size });
	return f;
}
const MB = 1024 * 1024;

describe('upload limits (client side)', () => {
	it('20 files and 50 MB in all; empty files block; unsupported types only warn', () => {
		const ok = checkUploads([file('a.pdf', 'x', 10 * MB), file('b.docx', 'x', 40 * MB)]);
		expect(ok).toEqual({ errors: [], unsupported: [], total: 50 * MB });
		const big = checkUploads([file('a.pdf', 'x', 30 * MB), file('b.pdf', 'x', 21 * MB)]);
		expect(big.errors).toEqual(['A re-sync takes up to 50 MB of files; these are 51.0 MB.']);
		const many = checkUploads(Array.from({ length: 21 }, (_, i) => file(`f${i}.pdf`, 'x', 10)));
		expect(many.errors[0]).toBe('A re-sync takes at most 20 files; 21 are chosen.');
		expect(checkUploads(Array.from({ length: 20 }, (_, i) => file(`f${i}.pdf`, 'x', 10))).errors).toEqual([]);
		expect(checkUploads([file('e.pdf', '', 0)]).errors).toEqual(['Empty file: e.pdf.']);
		expect(checkUploads([file('x.pages', 'x', 5), file('y.PDF', 'x', 5)]).unsupported).toEqual(['x.pages']);
	});

	it('a dropped file with the same name replaces the chosen one', () => {
		const a1 = file('a.pdf', 'one');
		const a2 = file('a.pdf', 'two!');
		const b = file('b.pdf');
		expect(mergeFiles([a1, b], [a2])).toEqual([b, a2]);
	});
});

describe('column preview helpers', () => {
	it('parses quoted CSV and reads the first row as header or as data', () => {
		const rows = parseCsv('sku,title\r\n"LL-1","Sofa, ""Oslo"""\n\nLL-2,Lamp\n');
		expect(rows).toEqual([
			['sku', 'title'],
			['LL-1', 'Sofa, "Oslo"'],
			['LL-2', 'Lamp'],
		]);
		const withHeader = tablePreview('t.csv', 'csv', [{ name: 't.csv', rows }]);
		expect(withHeader).toMatchObject({ columns: ['sku', 'title'], row_count: 2, sheets: [], sheet: null });
		const noHeader = tablePreview('t.csv', 'csv', [{ name: 't.csv', rows }], { header_row: false });
		expect(noHeader).toMatchObject({ columns: ['column_1', 'column_2'], row_count: 3, header_row: false });
		expect(tablePreview('w.xlsx', 'xlsx', [{ name: 'A', rows }], { sheet: 'B' })).toEqual({
			error: { code: 'sheet_not_found', message: 'The workbook has no sheet named "B".' },
		});
	});
});
