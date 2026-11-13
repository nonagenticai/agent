// The `RetrievalApi` contract.
import type { Paged } from '$lib/api/types/core';
import type { IngestJob } from '$lib/api/types/tenant';
import type {
	EvalRunDetail,
	EvalRunOptions,
	EvalRunSummary,
	GoldenQuestion,
	GoldenQuestionInput,
	GoldenQuestionPatch,
	RetrievalModels,
	UploadFile,
} from '$lib/api/types/retrieval';

export type RetrievalApi = {
	/** `GET /api/knowledge/models` — the embedding and rerank models strata_rag searches with: this tenant's slot
	 *  bindings (D70; mlx_api for the internal tenant), never platform-fixed. */
	retrievalModels(): Promise<RetrievalModels>;
	/** `GET /api/knowledge/eval/questions` → strata_rag `GET /api/eval/questions`: all (at most 200), oldest first,
	 *  one page (`next: null`). */
	goldenQuestions(): Promise<Paged<GoldenQuestion>>;
	/**
	 * `POST /api/knowledge/eval/questions` (knowledge_manager) → strata_rag `POST /api/eval/questions`: 201 with the row.
	 * 422 `invalid_input` (empty query, no expectation, a field too long), 409 `question_exists` (case-insensitive),
	 * 422 `too_many_questions` at 200. The leaf is not checked until a run.
	 */
	createGoldenQuestion(input: GoldenQuestionInput): Promise<GoldenQuestion>;
	/**
	 * `PATCH /api/knowledge/eval/questions/{id}` (knowledge_manager) → strata_rag `PATCH /api/eval/questions/{id}`: a field
	 * left out keeps its value, `null` clears it (`[]` an id list). 422 `invalid_input` for an empty patch or an
	 * invalid result, 409 `question_exists`, 404 `not_found`.
	 */
	updateGoldenQuestion(id: string, patch: GoldenQuestionPatch): Promise<GoldenQuestion>;
	/** `DELETE /api/knowledge/eval/questions/{id}` (knowledge_manager) → 204; stored runs keep their snapshot. 404 `not_found`. */
	deleteGoldenQuestion(id: string): Promise<void>;
	/**
	 * `POST /api/knowledge/eval/questions/import` (knowledge_manager) → strata_rag `POST /api/eval/questions/import`
	 * `{questions:[…]}`: all or nothing, 201 `{added, total}`. 422 `invalid_input` for any bad row (or none),
	 * 409 `question_exists` for a query already stored, 422 `too_many_questions` past 200.
	 */
	importGoldenQuestions(rows: GoldenQuestionInput[]): Promise<{ added: number; total: number }>;
	/**
	 * `POST /api/knowledge/eval/runs` (knowledge_manager) → strata_rag `POST /api/eval {source:"tenant", question_ids? |
	 * questions?, root_id?, modes?, beam?, min_score?}`: 202 Accepted with the run summary `status: "running"`
	 * (progress 0 of question × mode scores) and `Location: /api/eval/runs/{id}`; the scoring continues server-side —
	 * poll `evalRun` every 2 s until it is `completed` or `failed` (`error`; `finished_at` / `took_ms` set). Inline
	 * `questions` are scored but never saved (`source: "inline"`, snapshot ids null). Every refusal comes from this
	 * POST, in strata_rag's order: 409 `eval_running` first (single-flight), then 422 `invalid_input` (questions AND
	 * question_ids, a bad inline question `question N: …`, an empty set), 422 `too_many_questions`, 404 `not_found`
	 * (a question_ids entry or root_id that is not the tenant's — nothing stored), 409 `eval_not_ready` (no stored
	 * questions, or a leaf / document / chunk that is not in the tree — one per line). Tenant runs carry no
	 * `contaminated` / `foreign_stores` (strata_rag's built-in eval only).
	 */
	runEval(opts?: EvalRunOptions): Promise<EvalRunSummary>;
	/** `GET /api/knowledge/eval/runs?limit=&cursor=` → strata_rag `GET /api/eval/runs`: newest first, summaries
	 *  (`question_count`, no question snapshot, no `results`); limit 20 by default, at most 100. */
	evalRuns(opts?: { limit?: number; cursor?: string | null }): Promise<Paged<EvalRunSummary>>;
	/** `GET /api/knowledge/eval/runs/{id}` → strata_rag `GET /api/eval/runs/{id}`: one run with its question
	 *  snapshot, `status` / `progress` / `error`, and `results` ([] until completed). The poll target of a run. */
	evalRun(id: string): Promise<EvalRunDetail>;
	/**
	 * `POST /api/knowledge/stores/{id}/upload` (knowledge_manager, multipart `file` parts + optional `language`) — a
	 * one-off strata_ingest job per file without a source (`source_kind: 'file'`); answers the first job. At most 20
	 * files and 50 MB per upload; pdf, docx, txt, md, html, csv, xlsx. 404 `store_not_found`; 422 `not_a_leaf`,
	 * `no_file`, `too_many_files`, `invalid_language`; 409 `store_not_bound`; 413 `too_large`; 415 `unsupported_type`.
	 * PK-8 (T6): an optional `supersedes` part (a JSON array of platform topic keys these documents replace in
	 * searches; ≤ 32, 1–128 characters) → 422 `invalid_supersedes`.
	 */
	uploadToStore(
		storeId: string,
		files: UploadFile[],
		language?: string,
		supersedes?: string[],
	): Promise<IngestJob>;
};
