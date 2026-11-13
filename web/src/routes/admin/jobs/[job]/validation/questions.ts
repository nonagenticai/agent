// J17 question management (B8 T12a, F-50): the edit diff and the CSV import's refusals as the page says them.
import { ApiError } from '$lib/api/errors';
import type { ValidationQuestionPatch, ValidationRow } from '$lib/api/types/journey';

export type QuestionDraft = Required<ValidationQuestionPatch>;

/** The editable fields of a row as the edit form starts. */
export const draftOf = (
	r: Pick<ValidationRow, 'question' | 'expected' | 'reference' | 'activity'>,
): QuestionDraft => ({
	question: r.question,
	expected: r.expected,
	reference: r.reference,
	activity: r.activity,
});

/** Question, expected answer and reference citation are all required (422 `fields_required` otherwise). */
export const draftProblem = (d: QuestionDraft): string | null =>
	!d.question.trim() || !d.expected.trim() || !d.reference.trim()
		? 'Question, expected answer and reference citation are all required.'
		: null;

/** Only the fields that changed (the handler refuses nothing it is not sent). Empty = nothing to save. */
export function changedFields(from: QuestionDraft, to: QuestionDraft): ValidationQuestionPatch {
	const out: ValidationQuestionPatch = {};
	for (const k of ['question', 'expected', 'reference', 'activity'] as const) {
		const next = k === 'activity' ? to[k] : to[k].trim();
		if (next !== from[k]) out[k] = next;
	}
	return out;
}

/** The rows an `invalid_rows` refusal names (the file's line numbers, header = line 1). */
export function badRows(e: ApiError): number[] {
	const rows = e.details.rows;
	return Array.isArray(rows) ? rows.filter((n): n is number => typeof n === 'number') : [];
}

/**
 * What the import drawer says when the file is refused. The import is all or nothing: a 413 (file over 4 MiB, or the
 * job would hold more than 500 questions) or a 422 bad row rejects the whole file and nothing is stored.
 */
export function importProblem(e: unknown): string {
	if (!(e instanceof ApiError)) return (e as Error)?.message ?? 'The import failed.';
	if (e.status === 413)
		return e.code === 'too_many_questions'
			? 'Too many questions: a job holds at most 500, so the whole file was rejected. Nothing was imported.'
			: 'The file is larger than 4 MiB, so the whole file was rejected. Nothing was imported.';
	if (e.status === 422 && e.code === 'invalid_rows') {
		const rows = badRows(e);
		const shown = rows.slice(0, 10).join(', ') + (rows.length > 10 ? ` and ${rows.length - 10} more` : '');
		return `${rows.length === 1 ? 'Row' : 'Rows'} ${shown} ${rows.length === 1 ? 'is' : 'are'} not valid, so the whole file was rejected. Nothing was imported: fix ${rows.length === 1 ? 'it' : 'them'} and import the file again.`;
	}
	return e.message;
}
