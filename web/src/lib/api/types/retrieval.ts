// Types for the retrieval area: knowledge search detail (scores, routing, timings — strata_rag `POST /api/query`),
// the golden-question retrieval eval (strata_rag `POST /api/eval`), the model card (the tenant's own bindings, D70) and direct uploads to
// a leaf store. The knowledge query shapes themselves stay in `tenant.ts` (KnowledgeHit / KnowledgeQueryResult).
import type { Iso } from './core';

export type RouteMode = 'embed' | 'laya' | 'hybrid';
export const ROUTE_MODES: RouteMode[] = ['embed', 'laya', 'hybrid'];

/** One hit's scores as strata_rag reports them; null = that stage did not score the passage. */
export interface QueryScores {
	/** Cosine similarity from the tenant's embedding model (its `embed` slot). */
	dense: number | null;
	/** Full-text (lexical) match; only in hybrid mode. */
	lexical: number | null;
	/** Score from the tenant's rerank model (its `rerank` slot); only for the first `rerank_candidates` passages. */
	rerank: number | null;
}

/** How strata_rag routed the question (strata `routing`). */
export interface QueryRouting {
	mode_used: RouteMode;
	/** Leaf store keys searched. */
	leaves: string[];
	laya_consulted: boolean;
	/** Why laya routing fell back to searching every leaf; null = it did not. */
	laya_fallback: string | null;
}

/** Per-stage wall time in ms (strata `timings_ms`). */
export interface QueryTimings {
	embed: number;
	route: number;
	search: number;
	rerank: number;
	total: number;
}

// ---- retrieval eval (strata_rag PR #21: /api/eval/questions, POST /api/eval {source:"tenant"}, /api/eval/runs) -----

/** A mode an eval scores: the three route modes plus `flat` (every leaf, no routing), the verdict's baseline. */
export type EvalMode = RouteMode | 'flat';
export const EVAL_MODES: EvalMode[] = ['embed', 'laya', 'hybrid', 'flat'];
/** strata_rag `EvalOptions::default()`: what `POST /api/eval` scores when the body names no modes. */
export const EVAL_DEFAULT_MODES: EvalMode[] = ['embed', 'hybrid', 'flat'];

/**
 * A golden question as a client writes it (strata_rag `QuestionInput`). A result is a hit when it matches ANY of
 * the expectations, and at least one is required: `expected_document` (title, `source`, or the source's file name),
 * `expected_document_ids`, `expected_chunk_ids`, or `expected_text` (contained in the chunk text, case- and
 * whitespace-insensitive). `expected_leaf` (leaf store id, name path `a/b`, or unique leaf name) only feeds leaf
 * accuracy; whether it exists is checked when the eval runs (409 `eval_not_ready`), not when the question is saved.
 */
export interface GoldenQuestionInput {
	query: string;
	expected_document?: string | null;
	expected_leaf?: string | null;
	expected_document_ids?: string[];
	expected_chunk_ids?: string[];
	expected_text?: string | null;
}

/** One stored question (strata_rag `Question`). Tenant data, not draft config. */
export interface GoldenQuestion {
	id: string;
	query: string;
	expected_document: string | null;
	expected_leaf: string | null;
	expected_document_ids: string[];
	expected_chunk_ids: string[];
	expected_text: string | null;
	/** The `X-Actor-Id` of the request that created it; null when none was sent. */
	created_by: string | null;
	created_at: Iso;
	updated_at: Iso;
}

/**
 * `PATCH /api/eval/questions/{id}` body: a field left out keeps its value, `null` clears an optional field, `[]`
 * clears an id list. At least one field; the result must still be a valid question.
 */
export type GoldenQuestionPatch = Partial<{
	query: string;
	expected_document: string | null;
	expected_leaf: string | null;
	expected_document_ids: string[];
	expected_chunk_ids: string[];
	expected_text: string | null;
}>;

/** strata_rag `ModeReport` (src/eval.rs): one row per mode. Counts, not rates, for laya. */
export interface EvalModeReport {
	mode: EvalMode;
	n: number;
	recall_at_1: number;
	recall_at_5: number;
	recall_at_10: number;
	/** null = no question in the run names a leaf (or flat mode). */
	leaf_accuracy: number | null;
	laya_consulted: number;
	laya_fallbacks: number;
}

/** One evaluated question as the run keeps it (a snapshot); `id` is the stored question's, null for inline. */
export interface EvalRunQuestion {
	id: string | null;
	query: string;
	expected_document: string | null;
	expected_leaf: string | null;
	expected_document_ids: string[];
	expected_chunk_ids: string[];
	expected_text: string | null;
}

/** Per question and mode: 1-based rank of the first hit (null = not in the top 10); leaf_hit null = flat / no leaf. */
export interface EvalResult {
	/** Position in the run's `questions`. */
	index: number;
	question_id: string | null;
	mode: EvalMode;
	rank: number | null;
	leaf_hit: boolean | null;
}

/**
 * A tenant run's lifecycle (strata_rag ASYNC RUNS): `POST /api/eval` answers 202 with the run `running` and a
 * `Location: /api/eval/runs/{id}`; the work continues server-side, independent of the client, and the client polls
 * `GET /api/eval/runs/{id}` until it is `completed` or `failed`. A run left `running` when strata_rag restarts is
 * marked `failed` with error "interrupted". One running run per tenant (409 `eval_running`).
 */
export type EvalRunStatus = 'running' | 'completed' | 'failed';

/** Scores done so far: strata_rag counts question × mode (18 questions in 3 modes = 54). */
export interface EvalProgress {
	done: number;
	total: number;
}

/** What every stored eval run carries, in the runs list and on its own read. */
interface EvalRunHead {
	id: string;
	/** `tenant` = the stored questions; `inline` = questions sent with the request (not saved). */
	source: 'tenant' | 'inline';
	/** The store the eval was scoped to; null = the tenant's whole tree. */
	root_id: string | null;
	modes: EvalMode[];
	beam: number;
	min_score: number;
	status: EvalRunStatus;
	/** Why a `failed` run stopped; null otherwise. */
	error: string | null;
	progress: EvalProgress;
	/** PASS = every routed mode's recall@5 is at least flat's (vacuously PASS without flat). null until completed. */
	verdict: 'PASS' | 'FAIL' | null;
	started_by: string | null;
	started_at: Iso;
	/** null while running. */
	finished_at: Iso | null;
	/** null while running. */
	took_ms: number | null;
	/** null until completed. */
	reports: EvalModeReport[] | null;
}

/**
 * One run as `GET /api/eval/runs` lists it (strata_rag `EvalRunSummary`): no question snapshot (only its size) and
 * no per-question `results` — both are on `GET /api/eval/runs/{id}`. Also the body of the 202 that starts a run.
 */
export interface EvalRunSummary extends EvalRunHead {
	question_count: number;
}

/** One stored eval run with its question snapshot (strata_rag `EvalRun`). */
export interface EvalRun extends EvalRunHead {
	questions: EvalRunQuestion[];
	/** Only on `GET /api/eval/runs/{id}`; [] until completed. */
	results?: EvalResult[];
}

/** `GET /api/eval/runs/{id}`: the snapshot and every question's rank per mode ([] until completed). */
export interface EvalRunDetail extends EvalRun {
	results: EvalResult[];
}

/** A run that finished scoring: verdict, timings and reports are set. */
export type CompletedEvalRun<T extends EvalRunSummary | EvalRun = EvalRunSummary> = T & {
	status: 'completed';
	verdict: 'PASS' | 'FAIL';
	finished_at: Iso;
	took_ms: number;
	reports: EvalModeReport[];
};

export const isCompletedRun = <T extends EvalRunSummary | EvalRun>(r: T): r is CompletedEvalRun<T> =>
	r.status === 'completed' && r.reports != null && r.verdict != null;

/** What the page may choose for a run; omitted = strata_rag defaults (all stored questions, whole tree). */
export interface EvalRunOptions {
	question_ids?: string[];
	/** Inline questions (strata_rag `{questions:[…]}`): scored, kept in the run's snapshot, never saved. Not with
	 *  `question_ids`. */
	questions?: GoldenQuestionInput[];
	root_id?: string | null;
	/** The job whose pinned DRAFT generation is evaluated (T4a); omitted = each store's own binding. */
	job?: string;
	modes?: EvalMode[];
	beam?: number;
	min_score?: number;
}

// ---- model card ------------------------------------------------------------------------------------------------

/**
 * What one search stage runs on for this tenant: the provider + model bound to its slot (D70). Nothing is fixed by
 * the platform: the internal tenant binds mlx_api, every other tenant its own external provider. Read here, changed
 * on Providers (T6), where a change is saved to the draft of every job and goes live per job on publish (D69).
 */
export interface ModelBinding {
	slot: 'embed' | 'rerank';
	/** false = no provider of the tenant's and no platform model: searches that need it fail. */
	configured: boolean;
	/** Provider kind (`mlx_api`, `openai_compatible`, `voyage`, …); null when empty. */
	kind: string | null;
	model: string | null;
	base_url: string | null;
	region: string | null;
	/** The slot version the binding was read from. */
	version: number;
	/** true = mlx_api (nonagentic's own tenant). */
	internal: boolean;
	/** true = the platform's model strata serves this tenant (no provider of its own registered). */
	platform: boolean;
}

/** The models strata_rag searches with for this tenant: its embed and rerank bindings, plus the search defaults. */
export interface RetrievalModels {
	embedding: ModelBinding & { dimensions: number | null };
	reranker: ModelBinding & { candidates: number };
	chunking: { target_tokens: number; overlap_tokens: number };
	query_defaults: { mode: RouteMode; top_k: number; beam: number; min_score: number };
}

// ---- direct upload ---------------------------------------------------------------------------------------------

/** What the upload route needs of a file (a browser `File` satisfies it). */
export interface UploadFile {
	name: string;
	size: number;
	type?: string;
}
