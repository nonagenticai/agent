// B8 generator UI logic (T10a / T10b, no Svelte): the F-54 draft hint box, the F-23 operation proposal form, the F-53
// evidence upload's refusals and the D89 U-08 "no generator model" handling for tenant-driven runs (D97).
import { ApiError } from '$lib/api/errors';
import type { GeneratorRun, OperationDraft } from '$lib/api/types/journey';

/** F-54: a draft hint takes at most this many characters (the API answers 422 `hint_too_long` beyond it). */
export const HINT_MAX = 2000;

export interface HintState {
	length: number;
	left: number;
	over: boolean;
}

/** The API trims before counting; so does the box. */
export function hintState(hint: string): HintState {
	const length = hint.trim().length;
	return { length, left: HINT_MAX - length, over: length > HINT_MAX };
}

/** What the draft call receives: nothing at all when the box is empty (no hint, never an empty string). */
export const hintArg = (hint: string): string | undefined => (hint.trim() ? hint.trim() : undefined);

// ---- F-23 operation proposal ----

export const IDENTITY_LEVELS = ['0', '1', '2', '3', 'S'] as const;
export const SIDE_EFFECTS = ['R', 'W', 'X', 'P'] as const;
export const IDEMPOTENCY = ['nat', 'key', 'key+dup'] as const;
export const MODALITIES = ['do', 'request', 'observe'] as const;

export interface OperationForm {
	object: string;
	verb: string;
	variant: string;
	modality: (typeof MODALITIES)[number];
	identity_level: string;
	side_effect: string;
	confirm_back: boolean;
	verify_read: string;
	idempotency: string;
	input_fields: string;
	output_fields: string;
	description: string;
}

/** `case.create{incident} R` → object `case`, verb `create`, variant `incident`; null for any other title. */
export function splitOperationTitle(title: string): { object: string; verb: string; variant: string } | null {
	const m = /^([a-z][a-z0-9_]*)\.([a-z][a-z0-9_]*)(?:\{([a-z][a-z0-9_]*)\})?(?:\s|$)/.exec(title.trim());
	return m ? { object: m[1]!, verb: m[2]!, variant: m[3] ?? '' } : null;
}

/** The form an Operations-step item opens with: key from the draft's title, the rest read from its detail line. */
export function formFromItem(title: string, detail: string): OperationForm {
	const key = splitOperationTitle(title);
	const modality = (MODALITIES.find((m) => new RegExp(`\\b${m}\\b`).test(detail)) ??
		'do') as OperationForm['modality'];
	const se = /\b([RWXP]) ([0-3S])\b/.exec(detail);
	const verify = /verify_read ([a-z_.]+)/.exec(detail);
	return {
		object: key?.object ?? '',
		verb: key?.verb ?? '',
		variant: key?.variant ?? '',
		modality,
		identity_level: se?.[2] ?? '1',
		side_effect: se?.[1] ?? 'R',
		confirm_back: /confirm-back/.test(detail),
		verify_read: verify?.[1] ?? '',
		idempotency: 'nat',
		input_fields: '',
		output_fields: '',
		description: '',
	};
}

export const splitList = (s: string): string[] =>
	s
		.split(/[\s,]+/)
		.map((x) => x.trim())
		.filter(Boolean);

const SNAKE = /^[a-z][a-z0-9_]*$/;

/** Field → message; empty when the form can be sent. The server's operation validator stays the authority (422). */
export function operationErrors(f: OperationForm): Record<string, string> {
	const e: Record<string, string> = {};
	if (!SNAKE.test(f.object)) e.object = 'An object name is lower_snake_case.';
	if (!SNAKE.test(f.verb)) e.verb = 'A verb is lower_snake_case.';
	if (f.variant && !SNAKE.test(f.variant)) e.variant = 'A variant is lower_snake_case.';
	if (f.side_effect === 'X' && !f.confirm_back)
		e.confirm_back = 'An irreversible (X) operation confirms back.';
	return e;
}

export function toOperationDraft(f: OperationForm): OperationDraft {
	return {
		object: f.object.trim(),
		verb: f.verb.trim(),
		variant: f.variant.trim() || null,
		modality: f.modality,
		identity_level: f.identity_level,
		side_effect: f.side_effect,
		confirm_back: f.confirm_back,
		verify_read: f.verify_read.trim() || null,
		idempotency: f.idempotency,
		input_fields: splitList(f.input_fields),
		output_fields: splitList(f.output_fields),
		description: f.description.trim(),
	};
}

/** Reads of the refusals the operation proposal can answer (F-23). */
export function operationRefusal(e: unknown): string {
	if (e instanceof ApiError) {
		if (e.code === 'operation_exists')
			return 'That operation already exists in the catalogue — pick another key.';
		if (e.code === 'operation_invalid') return `The operation validator refused it: ${e.message}`;
		if (e.code === 'already_proposed') return 'This draft already carries a proposal.';
		return `${e.message} (${e.code})`;
	}
	return e instanceof Error ? e.message : String(e);
}

// ---- F-53 evidence upload ----

/** Upload refusals as one line the panel shows next to the file (413 too large, 422 unreadable, 409 no gates model). */
export function uploadRefusal(e: unknown): string {
	if (e instanceof ApiError) {
		if (e.status === 413) return 'The file is too large (413). Split the export and upload the parts.';
		if (e.status === 422) return `The file could not be read (422 ${e.code}): ${e.message}`;
		if (e.code === 'gates_provider_missing')
			return 'The platform tenant has no gates model bound yet — bind one in Providers, then upload again.';
		return `${e.message} (${e.code})`;
	}
	return e instanceof Error ? e.message : String(e);
}

/** Rows of a CSV / TSV / JSON-lines export, the CSV header excluded (the fixture client counts rows this way). */
export function countRows(text: string, name: string): number {
	const lines = text.split(/\r?\n/).filter((l) => l.trim()).length;
	return Math.max(0, lines - (name.toLowerCase().endsWith('.csv') ? 1 : 0));
}

// ---- D97 / D89 U-08 tenant-driven runs ----

export const PROVIDERS_LINK = '/admin/providers#uses';

/** 409 `generator_provider_missing`: the tenant has no `generator` model bound — drafting cannot run. */
export const providerMissing = (e: unknown): boolean =>
	e instanceof ApiError && e.status === 409 && e.code === 'generator_provider_missing';

/** D56 for the tenant run: BYO funding means the tenant's own key pays. */
export function fundingNote(r: Pick<GeneratorRun, 'cost' | 'tenant_name'>): string | null {
	return r.cost.funding === 'tenant_byo'
		? `Funded by ${r.tenant_name ?? 'your'} own provider key (BYO) — the tokens never touch the platform's lanes.`
		: null;
}

/** The tenant run lives under its job's discovery page. */
export const tenantRunHref = (job: string, id: number) =>
	`/admin/jobs/${encodeURIComponent(job)}/discovery/runs/${id}`;
