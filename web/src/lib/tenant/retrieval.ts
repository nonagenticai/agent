// Retrieval logic shared by the knowledge search preview (T7), the retrieval eval page and their fixtures:
//  - `search()`: the fixture stand-in for strata_rag `POST /api/query` (dense / lexical / rerank scores, laya routing
//    with a beam and a fallback, per-stage timings), deterministic so tests and eval runs are repeatable;
//  - `evaluate()`: strata_rag tenant `POST /api/eval` over a golden-question set (recall@1/5/10 per mode, leaf
//    accuracy, laya counts, per-question ranks), plus its question validation and `eval_not_ready` checks;
//  - golden-question CSV import (strata_rag's columns, all or nothing) and the 200-question cap;
//  - `diffRuns()`: two eval runs side by side;
//  - `checkUpload()`: the direct-upload limits (20 files, 50 MB total, known document types).
import type {
	EvalMode,
	EvalModeReport,
	EvalResult,
	EvalRun,
	EvalRunQuestion,
	GoldenQuestionInput,
	GoldenQuestionPatch,
	QueryRouting,
	QueryTimings,
	RouteMode,
	UploadFile,
} from '$lib/api/types/retrieval';
import { EVAL_DEFAULT_MODES, EVAL_MODES } from '$lib/api/types/retrieval';
import type { KnowledgeHit, KnowledgeStore } from '$lib/api/types/tenant';

// ---- limits (strata_rag / strata_ingest) ----------------------------------------------------------------------

/** strata_rag `EVAL_MAX_QUESTIONS`: more gives 422 before any work. */
export const EVAL_MAX_QUESTIONS = 200;
/** The tenant’s rerank model (its rerank slot, D70) re-scores this many candidates. */
export const RERANK_CANDIDATES = 8;
export const BEAM_MAX = 8;
export const UPLOAD_MAX_FILES = 20;
export const UPLOAD_MAX_BYTES = 50 * 1024 * 1024;
export const UPLOAD_TYPES = ['pdf', 'docx', 'txt', 'md', 'html', 'csv', 'xlsx'] as const;
export const UPLOAD_ACCEPT = UPLOAD_TYPES.map((t) => `.${t}`).join(',');

// ---- search -------------------------------------------------------------------------------------------------------

const STOP = new Set([
	'the',
	'and',
	'for',
	'how',
	'what',
	'does',
	'can',
	'are',
	'with',
	'you',
	'your',
	'take',
	'long',
	'when',
	'from',
	'this',
	'that',
	'there',
	'get',
	'have',
]);

/** The meaningful words of a question (3+ letters, no stop words). */
export const terms = (q: string) =>
	q
		.toLowerCase()
		.split(/[^a-z0-9€]+/)
		.filter((t) => t.length >= 3 && !STOP.has(t));

/**
 * What the dense model (and the reranker, and laya) understand that a word match does not: a customer's words for
 * what the documents call something else. Lexical search sees only the literal words.
 */
const SENSE: Record<string, string[]> = {
	money: ['refund'],
	cash: ['refund'],
	broken: ['defect', 'warranty'],
	faulty: ['defect', 'warranty'],
	arrived: ['delivery', 'delivered'],
	package: ['parcel'],
	voucher: ['gift card'],
	shop: ['showroom'],
	wash: ['clean'],
	collect: ['pick-up', 'collected'],
	remove: ['delete'],
	erase: ['delete'],
};

/** FNV-1a, so a passage's "semantic" jitter is stable across runs. */
function hash(s: string): number {
	let h = 0x811c9dc5;
	for (let i = 0; i < s.length; i++) {
		h ^= s.charCodeAt(i);
		h = Math.imul(h, 0x01000193) >>> 0;
	}
	return h >>> 0;
}
/** The chunk id a passage's hits carry (stable: store + document + text). */
export const chunkIdOf = (storeId: string, document: string, text: string) =>
	`ch-${hash(`${storeId}|${document}|${text}`).toString(16).padStart(8, '0')}`;
/** A stable value in [-1, 1). */
const jitter = (s: string) => (hash(s) % 2000) / 1000 - 1;
const r2 = (v: number) => Math.round(v * 100) / 100;
const clamp = (v: number, lo = 0, hi = 1) => Math.min(hi, Math.max(lo, v));

/** A searchable passage (strata_rag chunk stand-in). */
export interface SearchPassage {
	store_id: string;
	document: string;
	text: string;
	page_start: number | null;
	page_end: number | null;
}

export interface SearchInput {
	query: string;
	mode: RouteMode;
	top_k: number;
	min_score: number;
	beam?: number;
}

export interface SearchOutput {
	hits: KnowledgeHit[];
	routing: QueryRouting;
	timings_ms: QueryTimings;
}

/** Laya routes with confidence when the best leaf scores at least this; below it, it falls back to every leaf. */
const ROUTE_CONFIDENCE = 0.5;

function storePath(all: KnowledgeStore[], s: KnowledgeStore): string[] {
	const out = [s.key];
	let p = s.parent_id ? all.find((x) => x.id === s.parent_id) : undefined;
	while (p && out.length < 16) {
		out.unshift(p.key);
		p = p.parent_id ? all.find((x) => x.id === p!.parent_id) : undefined;
	}
	return out;
}

/**
 * strata_rag `POST /api/query` over `passages` in the bound leaves of `stores`. Passages without any query word
 * never match (the "nothing closely related" case). Scores: dense (embedding stand-in: word overlap + stable jitter),
 * lexical (hybrid only), rerank (the first RERANK_CANDIDATES by fused score, blended 60/40 with it into `score`). laya/hybrid route first: the `beam`
 * best leaves, or every leaf when no leaf is confident (`laya_fallback`).
 */
export function search(
	stores: KnowledgeStore[],
	passages: SearchPassage[],
	input: SearchInput,
): SearchOutput {
	const bound = stores.filter((s) => s.kind === 'leaf' && s.strata_store_id);
	const want = terms(input.query);
	const beam = Math.round(clamp(input.beam ?? 3, 1, BEAM_MAX));
	const mine = passages.filter((p) => bound.some((s) => s.id === p.store_id));
	const hayOf = (p: SearchPassage) => `${p.document} ${p.text}`.toLowerCase();
	/** Literal word overlap (lexical search). */
	const lex = (p: SearchPassage) => {
		const hay = hayOf(p);
		return want.length ? want.filter((t) => hay.includes(t)).length / want.length : 0;
	};
	/** Overlap by meaning (dense / rerank / laya): a word or one of its senses. */
	const sem = (p: SearchPassage | string) => {
		const hay = typeof p === 'string' ? p : hayOf(p);
		return want.length
			? want.filter((t) => hay.includes(t) || (SENSE[t] ?? []).some((x) => hay.includes(x))).length /
					want.length
			: 0;
	};

	// Routing.
	const consulted = input.mode !== 'embed';
	let leaves = bound;
	let fallback: string | null = null;
	if (consulted) {
		const rel = bound
			.map((s) => {
				const named = sem(`${s.key} ${s.label}`.toLowerCase()) > 0 ? 0.2 : 0;
				const best = Math.max(0, ...mine.filter((p) => p.store_id === s.id).map((p) => sem(p)));
				return { s, v: best + named };
			})
			.filter((x) => x.v > 0)
			.sort((a, b) => b.v - a.v || a.s.key.localeCompare(b.s.key));
		if (!rel.length || rel[0]!.v < ROUTE_CONFIDENCE) fallback = rel.length ? 'low_confidence' : 'no_route';
		else leaves = rel.slice(0, beam).map((x) => x.s);
	}
	const routed = new Set(leaves.map((s) => s.id));
	const pool = input.mode === 'hybrid' || fallback ? mine : mine.filter((p) => routed.has(p.store_id));

	// Retrieval.
	const ordOf = new Map<string, number>();
	const scored = pool
		.map((p) => {
			const key = `${p.document}|${p.text}`;
			const ord = ordOf.get(p.document) ?? 0;
			ordOf.set(p.document, ord + 1);
			const l = sem(p);
			const dense = r2(clamp(0.2 + 0.6 * l + 0.06 * jitter(`${key}|dense`), 0.05, 0.97));
			const lexical = input.mode === 'hybrid' ? r2(lex(p)) : null;
			const boost = input.mode === 'hybrid' && routed.has(p.store_id) && !fallback ? 0.05 : 0;
			const fused = lexical == null ? dense : 0.6 * dense + 0.4 * lexical + boost;
			return { p, l, dense, lexical, fused, ord, key };
		})
		.filter((x) => x.l > 0)
		.sort((a, b) => b.fused - a.fused || a.key.localeCompare(b.key));
	const reranked = scored.slice(0, RERANK_CANDIDATES).map((x) => {
		const titled = want.some((t) => x.p.document.toLowerCase().includes(t)) ? 0.06 : 0;
		const rerank = r2(clamp(0.12 + 0.78 * x.l + titled + 0.04 * jitter(`${x.key}|rerank`), 0.01, 0.99));
		// The cross-encoder score is blended with the first-stage score, so the mode still shapes the final order.
		return { ...x, rerank: rerank as number | null, score: r2(0.6 * rerank + 0.4 * x.fused) };
	});
	reranked.sort((a, b) => b.score - a.score || a.key.localeCompare(b.key));
	const rest = scored
		.slice(RERANK_CANDIDATES)
		.map((x) => ({ ...x, rerank: null as number | null, score: r2(x.fused) }));
	const hits: KnowledgeHit[] = [...reranked, ...rest]
		.filter((x) => x.score >= input.min_score && x.score > 0)
		.slice(0, Math.max(1, Math.min(50, input.top_k)))
		.map((x) => {
			const s = bound.find((b) => b.id === x.p.store_id)!;
			return {
				store_id: s.id,
				store_path: storePath(stores, s),
				document: x.p.document,
				text: x.p.text,
				page_start: x.p.page_start,
				page_end: x.p.page_end,
				score: x.score,
				chunk_id: chunkIdOf(s.id, x.p.document, x.p.text),
				ord: x.ord,
				scores: { dense: x.dense, lexical: x.lexical, rerank: x.rerank },
			};
		});

	const q = hash(input.query);
	const timings = {
		embed: 9 + (q % 7),
		route: consulted ? 360 + bound.length * 11 + (q % 40) : 0,
		search: 12 + 3 * (fallback || input.mode !== 'laya' ? bound.length : leaves.length),
		rerank: reranked.length ? 40 + reranked.length * 31 : 0,
		total: 0,
	};
	timings.total = timings.embed + timings.route + timings.search + timings.rerank;
	return {
		hits,
		routing: {
			mode_used: input.mode,
			leaves: leaves.map((s) => s.key),
			laya_consulted: consulted,
			laya_fallback: fallback,
		},
		timings_ms: timings,
	};
}

// ---- eval ---------------------------------------------------------------------------------------------------------

/** strata_rag `FIELD_MAX_CHARS`: longest query / expected_document / expected_leaf / expected_text. */
export const FIELD_MAX_CHARS = 2000;
/** strata_rag `MAX_EXPECTED_IDS`: most ids in expected_document_ids / expected_chunk_ids. */
export const MAX_EXPECTED_IDS = 100;
/** strata_rag query defaults a run uses when the body names none. */
export const EVAL_DEFAULT_BEAM = 3;
export const EVAL_DEFAULT_MIN_SCORE = 0.35;
/** Results per question an eval looks at (strata_rag `EVAL_TOP_K`): rank > 10 = miss. */
export const EVAL_TOP_K = 10;
const NIL_UUID = '00000000-0000-0000-0000-000000000000';

/** A question with every field present (strata_rag `QuestionInput::normalized`). */
export type NormalizedQuestion = Omit<EvalRunQuestion, 'id'>;

const cleanOpt = (s: string | null | undefined) => {
	const t = (s ?? '').trim();
	return t ? t : null;
};
const dedup = (ids: string[] | undefined) => [...new Set((ids ?? []).map((x) => x.trim()).filter(Boolean))];

/** Trimmed; blank optional fields become null; repeated ids dropped (first kept). */
export function normalizeQuestion(q: GoldenQuestionInput): NormalizedQuestion {
	return {
		query: (q.query ?? '').trim(),
		expected_document: cleanOpt(q.expected_document),
		expected_leaf: cleanOpt(q.expected_leaf),
		expected_document_ids: dedup(q.expected_document_ids),
		expected_chunk_ids: dedup(q.expected_chunk_ids),
		expected_text: cleanOpt(q.expected_text),
	};
}

export const hasExpectation = (q: NormalizedQuestion) =>
	q.expected_document != null ||
	q.expected_text != null ||
	q.expected_document_ids.length > 0 ||
	q.expected_chunk_ids.length > 0;

/**
 * strata_rag `QuestionInput::problems` over the normalized question: empty query, a field over 2000 characters,
 * more than 100 ids, the nil uuid, no expectation. Empty = valid. The leaf is not checked here (run time only).
 */
export function questionProblems(input: GoldenQuestionInput): string[] {
	const q = normalizeQuestion(input);
	const out: string[] = [];
	if (!q.query) out.push('query is empty');
	for (const [name, v] of [
		['query', q.query],
		['expected_document', q.expected_document],
		['expected_leaf', q.expected_leaf],
		['expected_text', q.expected_text],
	] as const)
		if (v != null && [...v].length > FIELD_MAX_CHARS)
			out.push(`${name} is over ${FIELD_MAX_CHARS} characters`);
	for (const [name, ids] of [
		['expected_document_ids', q.expected_document_ids],
		['expected_chunk_ids', q.expected_chunk_ids],
	] as const) {
		if (ids.length > MAX_EXPECTED_IDS) out.push(`${name} has more than ${MAX_EXPECTED_IDS} ids`);
		if (ids.includes(NIL_UUID)) out.push(`${name} holds the nil uuid`);
	}
	if (!hasExpectation(q))
		out.push(
			'no expectation: give expected_document, expected_document_ids, expected_chunk_ids or expected_text',
		);
	return out;
}

/** Ids as the CSV import and the form take them: separated by `;`, `|` or spaces. */
export const parseIds = (s: string | null | undefined) =>
	dedup(
		(s ?? '')
			.split(/[;|\s]+/)
			.map((x) => x.trim())
			.filter(Boolean),
	);

/**
 * The PATCH body that turns `before` into `after`: changed fields only; a cleared optional field is sent as null,
 * a cleared id list as []. Empty object = nothing changed.
 */
export function questionPatch(before: GoldenQuestionInput, after: GoldenQuestionInput): GoldenQuestionPatch {
	const a = normalizeQuestion(before);
	const b = normalizeQuestion(after);
	const out: GoldenQuestionPatch = {};
	if (a.query !== b.query) out.query = b.query;
	for (const k of ['expected_document', 'expected_leaf', 'expected_text'] as const)
		if (a[k] !== b[k]) out[k] = b[k];
	for (const k of ['expected_document_ids', 'expected_chunk_ids'] as const)
		if (a[k].join(';') !== b[k].join(';')) out[k] = b[k];
	return out;
}

/** A stable key of what a question asks and expects (stale-run detection). */
export const questionKey = (q: GoldenQuestionInput) => JSON.stringify(normalizeQuestion(q));

/** Rank (1-based) of the first hit from `doc`, or null when it is not in the hits. */
export function rankOf(hits: Pick<KnowledgeHit, 'document'>[], doc: string): number | null {
	const i = hits.findIndex((h) => h.document === doc);
	return i < 0 ? null : i + 1;
}

/** Share of questions whose expected document ranks within `k` (unranked = miss). */
export function recallAt(ranks: (number | null)[], k: number): number {
	if (!ranks.length) return 0;
	return r2(ranks.filter((r) => r != null && r <= k).length / ranks.length);
}

/** A leaf store the eval may name: its id, its key and its key path from the top. */
export interface EvalLeaf {
	id: string;
	key: string;
	path: string[];
}
/** A document the eval may name: its id, title and source. */
export interface EvalDocument {
	id: string;
	store_id: string;
	title: string;
	source: string;
}

/** The leaf stores strata_rag holds (bound ones) with their key paths. */
export const leavesOf = (stores: KnowledgeStore[]): EvalLeaf[] =>
	stores
		.filter((s) => s.kind === 'leaf' && s.strata_store_id)
		.map((s) => ({ id: s.id, key: s.key, path: storePath(stores, s) }));

/** strata_rag `resolve_leaf`: a leaf id, a key path `a/b/c`, or a key that is unique among the leaves. */
export function resolveLeaf(spec: string, leaves: EvalLeaf[]): { id: string } | { error: string } {
	const byId = leaves.find((l) => l.id === spec);
	if (byId) return { id: byId.id };
	if (spec.includes('/')) {
		const p = leaves.find((l) => l.path.join('/') === spec);
		return p ? { id: p.id } : { error: 'is not a leaf store in the evaluated tree' };
	}
	const named = leaves.filter((l) => l.key === spec);
	if (named.length === 1) return { id: named[0]!.id };
	return {
		error: named.length
			? 'matches several leaf stores; use its path'
			: 'is not a leaf store in the evaluated tree',
	};
}

const baseName = (s: string) => s.split(/[\\/]/).pop() ?? s;
/** `expected_document` matches a document's title, its `source`, or the file name of its source. */
export const documentMatches = (want: string, d: Pick<EvalDocument, 'title' | 'source'>) =>
	want === d.title || want === d.source || want === baseName(d.source);

/**
 * What strata_rag answers 409 `eval_not_ready` for: no questions, no leaf store, an `expected_leaf` that does not
 * resolve, or a document / document id / chunk id that is not in the evaluated tree. One line per problem,
 * `question N: …`. Empty = runnable.
 */
export function evalReadiness(
	questions: NormalizedQuestion[],
	leaves: EvalLeaf[],
	documents: EvalDocument[],
	chunkIds: Set<string>,
): string[] {
	const out: string[] = [];
	if (!leaves.length) out.push('the evaluated tree has no leaf stores');
	const docIds = new Set(documents.map((d) => d.id));
	questions.forEach((q, i) => {
		const n = i + 1;
		if (q.expected_leaf) {
			const r = resolveLeaf(q.expected_leaf, leaves);
			if ('error' in r) out.push(`question ${n}: expected_leaf \`${q.expected_leaf}\` ${r.error}`);
		}
		const d = q.expected_document;
		if (d && !documents.some((x) => documentMatches(d, x)))
			out.push(`question ${n}: no document \`${d}\` in the evaluated tree`);
		for (const id of q.expected_document_ids)
			if (!docIds.has(id)) out.push(`question ${n}: document ${id} is not in the evaluated tree`);
		for (const id of q.expected_chunk_ids)
			if (!chunkIds.has(id)) out.push(`question ${n}: chunk ${id} is not in the evaluated tree`);
	});
	return out;
}

/** The problem lines of an `eval_not_ready` message (`eval not ready: a\nb` → [a, b]). */
export const notReadyLines = (message: string) =>
	message
		.replace(/^eval not ready:\s*/i, '')
		.split('\n')
		.map((x) => x.trim())
		.filter(Boolean);

const normText = (s: string) => s.toLowerCase().replace(/\s+/g, ' ').trim();

/** strata_rag `Expectation::matches`: a hit matches when ANY expectation does. */
function hitMatches(q: NormalizedQuestion, h: KnowledgeHit, documents: EvalDocument[]): boolean {
	const doc = documents.find((d) => d.store_id === h.store_id && d.title === h.document);
	if (
		q.expected_document &&
		(doc ? documentMatches(q.expected_document, doc) : h.document === q.expected_document)
	)
		return true;
	if (doc && q.expected_document_ids.includes(doc.id)) return true;
	if (q.expected_chunk_ids.includes(h.chunk_id)) return true;
	return q.expected_text != null && normText(h.text).includes(normText(q.expected_text));
}

export interface EvalScore {
	reports: EvalModeReport[];
	results: EvalResult[];
}

/**
 * The fixture stand-in for a tenant `POST /api/eval`: per mode (routed modes in the given order, `flat` last),
 * recall@1/5/10 of the first hit matching any expectation in the top 10 (no score floor), leaf accuracy over the
 * questions that name a leaf (null for flat, or when none does), laya counts; and one result row per question/mode.
 */
export function evaluate(
	stores: KnowledgeStore[],
	passages: SearchPassage[],
	documents: EvalDocument[],
	questions: (NormalizedQuestion & { id?: string | null })[],
	opts: { modes?: EvalMode[]; beam?: number } = {},
): EvalScore {
	const want = opts.modes?.length ? opts.modes : EVAL_DEFAULT_MODES;
	const modes = [
		...new Set([...want.filter((m) => m !== 'flat'), ...(want.includes('flat') ? ['flat' as const] : [])]),
	];
	const leaves = leavesOf(stores);
	const results: EvalResult[] = [];
	const reports = modes.map((mode): EvalModeReport => {
		const ranks: (number | null)[] = [];
		const leafHits: boolean[] = [];
		let consulted = 0;
		let fallbacks = 0;
		questions.forEach((q, index) => {
			// flat: every leaf, no routing (the embed path of the stand-in never consults laya).
			const r = search(stores, passages, {
				query: q.query,
				mode: mode === 'flat' ? 'embed' : mode,
				top_k: EVAL_TOP_K,
				min_score: 0,
				beam: opts.beam ?? EVAL_DEFAULT_BEAM,
			});
			const i = r.hits.findIndex((h) => hitMatches(q, h, documents));
			const rank = i < 0 ? null : i + 1;
			ranks.push(rank);
			let leaf_hit: boolean | null = null;
			if (mode !== 'flat' && q.expected_leaf) {
				const target = resolveLeaf(q.expected_leaf, leaves);
				leaf_hit = 'id' in target && r.hits[0]?.store_id === target.id;
				leafHits.push(leaf_hit);
			}
			if (r.routing.laya_consulted) consulted++;
			if (r.routing.laya_fallback) fallbacks++;
			results.push({ index, question_id: q.id ?? null, mode, rank, leaf_hit });
		});
		return {
			mode,
			n: questions.length,
			recall_at_1: recallAt(ranks, 1),
			recall_at_5: recallAt(ranks, 5),
			recall_at_10: recallAt(ranks, 10),
			leaf_accuracy: leafHits.length ? r2(leafHits.filter(Boolean).length / leafHits.length) : null,
			laya_consulted: consulted,
			laya_fallbacks: fallbacks,
		};
	});
	results.sort((a, b) => a.index - b.index || modes.indexOf(a.mode) - modes.indexOf(b.mode));
	return { reports, results };
}

/** strata_rag `verdict_passes`: every routed mode's recall@5 is at least flat's (PASS without flat). */
export function verdictOf(reports: EvalModeReport[]): 'PASS' | 'FAIL' {
	const flat = reports.find((r) => r.mode === 'flat');
	if (!flat) return 'PASS';
	return reports.filter((r) => r.mode !== 'flat').every((r) => r.recall_at_5 >= flat.recall_at_5)
		? 'PASS'
		: 'FAIL';
}

// ---- golden questions: CSV ----------------------------------------------------------------------------------------

/** RFC 4180-ish: commas, quoted fields with `""` escapes and newlines inside quotes. */
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
		} else if (ch === '"' && cell === '') quoted = true;
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
	return rows;
}

export interface CsvImport {
	rows: NormalizedQuestion[];
	/** Line numbers are 1-based file lines (the header is line 1); line 0 = the whole file. */
	errors: { line: number; message: string }[];
}

/** The columns an import reads (strata_rag `CSV_COLUMNS`); any other column is ignored. */
export const CSV_COLUMNS = [
	'query',
	'expected_document',
	'expected_leaf',
	'expected_document_ids',
	'expected_chunk_ids',
	'expected_text',
] as const;
const EXPECTATION_COLUMNS = CSV_COLUMNS.filter((c) => c !== 'query' && c !== 'expected_leaf');

/**
 * Golden questions from CSV (strata_rag `parse_questions_csv`): a header naming `query` and at least one expectation
 * column (any order, case-insensitive, extra columns ignored); ids separated by `;`, `|` or spaces. Every bad line
 * is reported; strata_rag imports all or nothing, so any error means nothing may be sent. Queries already in the set
 * (409 `question_exists`) and passing the 200 cap (422 `too_many_questions`) are flagged here too.
 */
export function parseGoldenCsv(text: string, opts: { existing?: string[] } = {}): CsvImport {
	const errors: CsvImport['errors'] = [];
	const lines = parseCsv(text.replace(/^﻿/, ''));
	const header = (lines[0] ?? []).map((h) => h.trim().toLowerCase());
	const col = Object.fromEntries(
		CSV_COLUMNS.map((c) => [c, header.indexOf(c)] as const).filter(([, i]) => i >= 0),
	) as Partial<Record<(typeof CSV_COLUMNS)[number], number>>;
	if (col.query == null)
		return { rows: [], errors: [{ line: 1, message: 'The header needs a query column.' }] };
	if (!EXPECTATION_COLUMNS.some((c) => col[c] != null))
		return {
			rows: [],
			errors: [
				{ line: 1, message: `The header needs an expectation column (${EXPECTATION_COLUMNS.join(', ')}).` },
			],
		};
	const rows: NormalizedQuestion[] = [];
	const stored = new Set((opts.existing ?? []).map((q) => q.trim().toLowerCase()));
	const seen = new Set<string>();
	lines.slice(1).forEach((cells, i) => {
		const line = i + 2;
		if (cells.every((c) => !c.trim())) return;
		const get = (c: (typeof CSV_COLUMNS)[number]) => {
			const j = col[c];
			return j == null ? null : (cells[j] ?? null);
		};
		const q = normalizeQuestion({
			query: get('query') ?? '',
			expected_document: get('expected_document'),
			expected_leaf: get('expected_leaf'),
			expected_document_ids: parseIds(get('expected_document_ids')),
			expected_chunk_ids: parseIds(get('expected_chunk_ids')),
			expected_text: get('expected_text'),
		});
		const problems = questionProblems(q);
		const key = q.query.toLowerCase();
		if (!problems.length && seen.has(key)) problems.push('the same query is on an earlier line');
		if (!problems.length && stored.has(key)) problems.push('this question is already in the set');
		if (problems.length) errors.push({ line, message: problems.join('; ') });
		else {
			seen.add(key);
			rows.push(q);
		}
	});
	const total = stored.size + rows.length;
	if (total > EVAL_MAX_QUESTIONS)
		errors.unshift({
			line: 0,
			message: `Importing ${rows.length} would make ${total} questions; the set holds at most ${EVAL_MAX_QUESTIONS}.`,
		});
	if (!rows.length && !errors.length) errors.push({ line: 0, message: 'The file has no question rows.' });
	return { rows, errors };
}

// ---- run diff -----------------------------------------------------------------------------------------------------

export const EVAL_METRICS = [
	['recall_at_1', 'recall@1'],
	['recall_at_5', 'recall@5'],
	['recall_at_10', 'recall@10'],
	['leaf_accuracy', 'leaf accuracy'],
	['laya_consulted', 'laya consulted'],
	['laya_fallbacks', 'fallbacks'],
] as const;
export type EvalMetric = (typeof EVAL_METRICS)[number][0];
/** Counts, not rates: a rise in fallbacks is worse, a change in consulted is neutral. */
export const COUNT_METRICS: EvalMetric[] = ['laya_consulted', 'laya_fallbacks'];

export interface RunDiff {
	modes: {
		mode: EvalMode;
		metrics: {
			key: EvalMetric;
			label: string;
			base: number | null;
			next: number | null;
			delta: number | null;
		}[];
	}[];
	/** Questions only in the newer run / only in the older run (by question text). */
	added: string[];
	removed: string[];
}

/** `next` against `base`: per mode and metric, the change (null when a run lacks that mode or the value). Reads
 * only `reports`, so the runs list's summaries are enough. */
export function diffModes(base: Pick<EvalRun, 'reports'>, next: Pick<EvalRun, 'reports'>): RunDiff['modes'] {
	const ra = base.reports ?? [];
	const rb = next.reports ?? [];
	return EVAL_MODES.filter((m) => ra.some((r) => r.mode === m) || rb.some((r) => r.mode === m)).map(
		(mode) => {
			const a = ra.find((r) => r.mode === mode);
			const b = rb.find((r) => r.mode === mode);
			return {
				mode,
				metrics: EVAL_METRICS.map(([key, label]) => {
					const av = a ? a[key] : null;
					const bv = b ? b[key] : null;
					return {
						key,
						label,
						base: av,
						next: bv,
						delta: av == null || bv == null ? null : COUNT_METRICS.includes(key) ? bv - av : r2(bv - av),
					};
				}),
			};
		},
	);
}

/** `diffModes` plus the questions added and dropped between the two snapshots (`GET /api/eval/runs/{id}`). */
export function diffRuns(base: EvalRun, next: EvalRun): RunDiff {
	const modes = diffModes(base, next);
	const qa = new Set(base.questions.map((q) => q.query));
	const qb = new Set(next.questions.map((q) => q.query));
	return {
		modes,
		added: [...qb].filter((q) => !qa.has(q)),
		removed: [...qa].filter((q) => !qb.has(q)),
	};
}

/** The headline: hybrid recall@5 of a run (else the first mode's); null for none, or a run not completed. */
export function headline(run: Pick<EvalRun, 'reports'> | null | undefined): number | null {
	if (!run?.reports) return null;
	const h = run.reports.find((r) => r.mode === 'hybrid') ?? run.reports[0];
	return h ? h.recall_at_5 : null;
}

/** Poll interval for a running eval (strata_rag suggests every 2 s). */
export const EVAL_POLL_MS = 2000;

/** Consecutive failed polls (network / 5xx) after which the page stops polling and offers "Retry". */
export const EVAL_POLL_MAX_FAILS = 6;

/** Wait before the next poll after `fails` failed ones in a row: doubles from EVAL_POLL_MS, capped at 30 s. */
export const pollDelay = (fails: number) => Math.min(30_000, EVAL_POLL_MS * 2 ** Math.max(0, fails));

/** "6 of 54 scored (18 questions × 3 modes)": strata_rag's progress counts question × mode scores. */
export function progressText(r: {
	progress: { done: number; total: number };
	modes: readonly unknown[];
}): string {
	const m = r.modes.length || 1;
	const q = Math.round(r.progress.total / m);
	return `${r.progress.done} of ${r.progress.total} scored (${q} question${q === 1 ? '' : 's'} × ${m} mode${m === 1 ? '' : 's'})`;
}

// ---- upload limits ------------------------------------------------------------------------------------------------

export interface UploadProblem {
	code: 'no_file' | 'too_many_files' | 'too_large' | 'unsupported_type' | 'empty_file';
	message: string;
	file?: string;
}

export const extOf = (name: string) => {
	const m = /\.([a-z0-9]+)$/i.exec(name.trim());
	return m ? m[1]!.toLowerCase() : '';
};

export function fmtBytes(n: number): string {
	if (n >= 1024 * 1024) return `${(n / (1024 * 1024)).toFixed(1)} MB`;
	if (n >= 1024) return `${Math.round(n / 1024)} KB`;
	return `${n} B`;
}

/** Problems with a set of files for one upload; empty = it may go. */
export function checkUpload(files: UploadFile[]): UploadProblem[] {
	const out: UploadProblem[] = [];
	if (!files.length) return [{ code: 'no_file', message: 'Pick at least one file.' }];
	if (files.length > UPLOAD_MAX_FILES)
		out.push({
			code: 'too_many_files',
			message: `${files.length} files: one upload takes at most ${UPLOAD_MAX_FILES}.`,
		});
	const total = files.reduce((a, f) => a + f.size, 0);
	if (total > UPLOAD_MAX_BYTES)
		out.push({
			code: 'too_large',
			message: `${fmtBytes(total)} in total: one upload takes at most ${fmtBytes(UPLOAD_MAX_BYTES)}.`,
		});
	for (const f of files) {
		if (!(UPLOAD_TYPES as readonly string[]).includes(extOf(f.name)))
			out.push({
				code: 'unsupported_type',
				file: f.name,
				message: `${f.name}: not a supported type (${UPLOAD_TYPES.join(', ')}).`,
			});
		else if (f.size === 0) out.push({ code: 'empty_file', file: f.name, message: `${f.name} is empty.` });
	}
	return out;
}
