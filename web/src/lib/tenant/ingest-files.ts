// T9 file sources and column previews: the upload limits a re-sync enforces (20 files,
// 50 MB in all; previews 10 MB), the CSV reader and table preview the fixture serves, and the replace-report text.
// Pure: the run history checks the limits before sending, the fixture repeats them as the server would.
import type { ColumnPreview, PreviewOptions, ReplaceReport, RunError } from '$lib/api/types/tenant';

export const MAX_UPLOAD_FILES = 20;
export const MAX_UPLOAD_BYTES = 50 * 1024 * 1024;
export const MAX_PREVIEW_BYTES = 10 * 1024 * 1024;
/** File types strata_ingest takes for a file source. */
export const INGESTIBLE = ['pdf', 'docx', 'pptx', 'odt', 'rtf', 'html', 'htm', 'txt', 'md'];
/** What a column preview reads. */
export const PREVIEWABLE = ['csv', 'xlsx'];

export const extOf = (name: string) => {
	const i = name.lastIndexOf('.');
	return i < 0 ? '' : name.slice(i + 1).toLowerCase();
};

/** "820 KB", "12.4 MB". */
export function fileSize(bytes: number): string {
	if (bytes < 1024) return `${bytes} B`;
	if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`;
	return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

export interface UploadCheck {
	/** Problems that block sending (too many files, too large in all, empty). */
	errors: string[];
	/** Files strata_ingest will refuse (unsupported type); the rest of the upload still runs. */
	unsupported: string[];
	total: number;
}

/** Client-side check before a file-source re-sync; the server repeats the hard limits (too_many_files, too_large). */
export function checkUploads(files: { name: string; size: number }[]): UploadCheck {
	const total = files.reduce((n, f) => n + f.size, 0);
	const errors: string[] = [];
	if (files.length > MAX_UPLOAD_FILES)
		errors.push(`A re-sync takes at most ${MAX_UPLOAD_FILES} files; ${files.length} are chosen.`);
	if (total > MAX_UPLOAD_BYTES)
		errors.push(`A re-sync takes up to 50 MB of files; these are ${fileSize(total)}.`);
	const empty = files.filter((f) => f.size === 0).map((f) => f.name);
	if (empty.length) errors.push(`Empty file${empty.length === 1 ? '' : 's'}: ${empty.join(', ')}.`);
	const unsupported = files.filter((f) => !INGESTIBLE.includes(extOf(f.name))).map((f) => f.name);
	return { errors, unsupported, total };
}

/** Add dropped files to the chosen ones; a file with the same name replaces the earlier one. */
export function mergeFiles<F extends { name: string }>(chosen: F[], added: F[]): F[] {
	const names = new Set(added.map((f) => f.name));
	return [...chosen.filter((f) => !names.has(f.name)), ...added];
}

/** RFC 4180-ish CSV: quoted fields, doubled quotes, CRLF; blank lines skipped. */
export function parseCsv(text: string): string[][] {
	const rows: string[][] = [];
	let row: string[] = [];
	let cell = '';
	let quoted = false;
	for (let i = 0; i < text.length; i++) {
		const ch = text[i]!;
		if (quoted) {
			if (ch === '"' && text[i + 1] === '"') {
				cell += '"';
				i++;
			} else if (ch === '"') quoted = false;
			else cell += ch;
		} else if (ch === '"') quoted = true;
		else if (ch === ',') {
			row.push(cell);
			cell = '';
		} else if (ch === '\n' || ch === '\r') {
			if (ch === '\r' && text[i + 1] === '\n') i++;
			row.push(cell);
			rows.push(row);
			row = [];
			cell = '';
		} else cell += ch;
	}
	if (cell !== '' || row.length) {
		row.push(cell);
		rows.push(row);
	}
	return rows.filter((r) => r.some((c) => c.trim() !== ''));
}

/** At most this many sample rows come back. */
export const SAMPLE_ROWS = 5;

/**
 * Columns, sample rows and row count of one table. With `header_row` (default) the first row names the columns;
 * without it every row is data and the columns are `column_1..N`, N the widest row.
 */
export function tablePreview(
	file: string,
	format: 'csv' | 'xlsx',
	sheets: { name: string; rows: string[][] }[],
	opts: PreviewOptions = {},
): ColumnPreview | { error: RunError } {
	const headerRow = opts.header_row ?? true;
	const sheet =
		format === 'xlsx' ? (opts.sheet ? sheets.find((s) => s.name === opts.sheet) : sheets[0]) : sheets[0];
	if (!sheet)
		return {
			error: { code: 'sheet_not_found', message: `The workbook has no sheet named "${opts.sheet}".` },
		};
	const rows = sheet.rows.filter((r) => r.some((c) => c.trim() !== ''));
	if (!rows.length) return { error: { code: 'unreadable_table', message: 'The file has no header row.' } };
	const width = Math.max(...rows.map((r) => r.length));
	const columns = headerRow
		? rows[0]!.map((c, i) => c.trim() || `column_${i + 1}`)
		: Array.from({ length: width }, (_, i) => `column_${i + 1}`);
	const data = headerRow ? rows.slice(1) : rows;
	return {
		file,
		format,
		row_count: data.length,
		columns,
		rows: data
			.slice(0, SAMPLE_ROWS)
			.map((r) => Object.fromEntries(columns.map((c, i) => [c, (r[i] ?? '').trim()]))),
		sheets: format === 'xlsx' ? sheets.map((s) => s.name) : [],
		sheet: format === 'xlsx' ? sheet.name : null,
		header_row: headerRow,
	};
}

export const REPLACE_PARTS: { key: keyof ReplaceReport; label: string; hint: string }[] = [
	{
		key: 'deleted',
		label: 'Deleted',
		hint: 'Earlier documents this run no longer has, removed from the store.',
	},
	{ key: 'already_gone', label: 'Already gone', hint: 'Earlier documents someone had already deleted.' },
	{
		key: 'kept_shared',
		label: 'Kept (unchanged or shared)',
		hint: 'Earlier documents this run produced again, or that another source also holds.',
	},
	{
		key: 'failed',
		label: 'Could not delete',
		hint: 'strata_rag refused the delete; retried on the next completed run.',
	},
];

export const replaceCount = (r: ReplaceReport, key: keyof ReplaceReport) =>
	key === 'failed' ? r.failed.length : r[key];

/** "Replaced 1 older document · 1 kept · 1 could not be deleted (retried on the next completed run)". */
export function replaceSummary(r: ReplaceReport | null): string {
	if (!r) return '';
	const parts = [`Replaced ${r.deleted} older document${r.deleted === 1 ? '' : 's'}`];
	if (r.kept_shared) parts.push(`${r.kept_shared} kept`);
	if (r.already_gone) parts.push(`${r.already_gone} already gone`);
	if (r.failed.length)
		parts.push(`${r.failed.length} could not be deleted (retried on the next completed run)`);
	return parts.join(' · ');
}

/** `code: message`, the way a run's error reads everywhere. */
export const errorText = (e: RunError | null | undefined) => (e ? `${e.code}: ${e.message}` : '');
