// J18 import + scorecard (B8 T15a, F-51/F-52): the preview's column choice, the upload input, the weight field and
// the refusals as the page words them.
import { ApiError } from '$lib/api/errors';
import type { DiscoveryPreview, DiscoveryUploadInput } from '$lib/api/types/journey';

/** Files the export upload takes (F-52 adds XLSX, first sheet). */
export const IMPORT_ACCEPT = '.csv,.json,.xlsx';

export const DEFAULT_WEIGHT = 100;
export const MAX_WEIGHT = 1000;

/** The column the ticket text is read from, and the optional count column, as the preview suggests them. */
export function initialColumns(p: DiscoveryPreview): { text: string; count: string } {
	const has = (c: string | null) => (c && p.columns.includes(c) ? c : '');
	return { text: has(p.suggested_text_column) || p.columns[0] || '', count: has(p.suggested_count_column) };
}

/** `uploadDiscoveryImport` input for a previewed file: the chosen columns only when they matter. */
export function uploadInput(
	file: File,
	p: DiscoveryPreview,
	cols: { text: string; count: string },
): DiscoveryUploadInput {
	const out: DiscoveryUploadInput = { file, rows: p.rows };
	// A JSON export names its own fields; CSV and XLSX are read by the columns chosen here.
	if (cols.text) out.text_column = cols.text;
	if (cols.count && cols.count !== cols.text) out.count_column = cols.count;
	return out;
}

/** A weight field: a whole number from 0 to 1000 (100 = default) or the reason it is not one. */
export function parseWeight(text: string): { ok: true; weight: number } | { ok: false; problem: string } {
	const t = text.trim();
	if (!/^\d+$/.test(t))
		return {
			ok: false,
			problem: `A weight is a whole number from 0 to ${MAX_WEIGHT} (${DEFAULT_WEIGHT} = default).`,
		};
	const weight = Number(t);
	return weight > MAX_WEIGHT
		? { ok: false, problem: `A weight is at most ${MAX_WEIGHT} (${DEFAULT_WEIGHT} = default).` }
		: { ok: true, weight };
}

/** What the import drawer says when the preview or the upload is refused. */
export function importFileProblem(e: unknown): string {
	if (e instanceof ApiError && e.status === 413)
		return 'The file is too large: an export is at most 20 MiB and 50,000 rows. Split it and upload the parts.';
	if (e instanceof ApiError && e.status === 422 && e.code === 'unsupported_file')
		return 'Upload a .csv, .json or .xlsx export.';
	return e instanceof Error ? e.message : 'The file could not be read.';
}
