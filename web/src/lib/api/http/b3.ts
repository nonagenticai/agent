// B3 over HTTP (T14): LLM providers + uses, rag/ingest provider slots, knowledge stores / documents / query / model
// card / upload, ingest jobs, a job's sources and runs, knowledge kinds + platform packs + coverage, the retrieval
// eval, answer grounding and model changes. Verbs, paths and bodies are the B3 plan's route table (§5) as the Rust
// route tables in `crates/agent-api/src/routes/*.rs` serve them; each route is listed in `ROUTES` and in the method's
// doc comment in `areas/*.ts`. Job-scoped routes act in `job()` (the app's active job) unless the method names a job
// (`knowledgeCoverage`, `knowledgeAnalytics`, the model-change job actions). Multipart bodies (`uploadToStore`, a
// file `resyncSource`, a file `previewSourceColumns`) go through `req.form`; 202 answers (`runEval`, a still-running
// `deleteIngestJob`, a following `resyncSource`) are successes like any 2xx.
import type { Api } from '../client';
import { seg, type Req } from '../http-client';
import type { LiveMethod } from './routes';

/** The answer type of `m`. */
type R<M extends keyof Api> = Awaited<ReturnType<Api[M]>>;

/** The 57 B3 methods (plan §5 + R10 `platformPacks`), in route-table order. */
export const B3_METHODS = [
	// §5.1 providers (T3, T4, T4b, T4c, T4d)
	'llmProviders',
	'createLlmProvider',
	'updateLlmProvider',
	'deleteLlmProvider',
	'testLlmProvider',
	'testLlmProviderDraft',
	'bindLlmUse',
	'providerSlots',
	'saveProviderSlot',
	'acceptSlotDrift',
	'resetProviderSlot',
	'testProviderSlotDraft',
	'pushProviderSlot',
	// §5.2 stores, documents, query, model card, upload (T5, T6)
	'knowledgeStores',
	'knowledgeDocuments',
	'createKnowledgeStore',
	'deleteKnowledgeStore',
	'ensureMissingStores',
	'deleteKnowledgeDocument',
	'knowledgeQuery',
	'retrievalModels',
	'uploadToStore',
	// §5.3 ingest jobs (T6)
	'ingestJobs',
	'ingestJob',
	'retryIngestJob',
	'cancelIngestJob',
	'deleteIngestJob',
	// §5.4 sources + runs + coverage (T7, T8, T10)
	'sources',
	'source',
	'sourceRuns',
	'sourceMappingTargets',
	'createSource',
	'saveSource',
	'deleteSource',
	'testSource',
	'previewSourceColumns',
	'resyncSource',
	'sourceRun',
	'knowledgeCoverage',
	// §5.5 knowledge kinds (T10, R10)
	'knowledgeKinds',
	'bindKnowledgeKind',
	'platformPacks',
	// §5.6 retrieval eval (T9)
	'goldenQuestions',
	'createGoldenQuestion',
	'updateGoldenQuestion',
	'deleteGoldenQuestion',
	'importGoldenQuestions',
	'runEval',
	'evalRuns',
	'evalRun',
	// §5.7 grounding (T11)
	'submitAnswerFeedback',
	'answerFeedback',
	'knowledgeAnalytics',
	// §5.8 model changes (T4a)
	'modelChanges',
	'modelChange',
	'reembedJobStore',
	'acceptModelChange',
] as const satisfies readonly LiveMethod[];

export type B3Method = (typeof B3_METHODS)[number];

/** A file for a multipart part: the page hands real `File`s (a fixture-shaped `{name, size}` cannot be sent). */
function blobOf(f: unknown): Blob {
	if (typeof Blob !== 'undefined' && f instanceof Blob) return f;
	throw new TypeError('A file to upload must be a File (picked in the browser).');
}
const fileName = (f: unknown, fallback: string) =>
	typeof (f as { name?: unknown }).name === 'string' ? (f as { name: string }).name : fallback;

export function httpB3(req: Req, job: () => string): Pick<Api, B3Method> {
	const j = () => seg(job());
	return {
		// ---- §5.1 LLM providers + uses
		llmProviders: () => req<R<'llmProviders'>>('GET', '/api/providers'),
		createLlmProvider: (input) => req<R<'createLlmProvider'>>('POST', '/api/providers', { body: input }),
		updateLlmProvider: (id, input) =>
			req<R<'updateLlmProvider'>>('PUT', `/api/providers/${seg(id)}`, { body: input }),
		deleteLlmProvider: async (id) => {
			await req<unknown>('DELETE', `/api/providers/${seg(id)}`);
		},
		testLlmProvider: (id) => req<R<'testLlmProvider'>>('POST', `/api/providers/${seg(id)}/test`),
		testLlmProviderDraft: (input) =>
			req<R<'testLlmProviderDraft'>>('POST', '/api/providers/test', { body: input }),
		bindLlmUse: (use, provider_id, model, voice) => {
			const body: Record<string, unknown> = { provider_id, model };
			if (voice !== undefined && voice !== null) body.voice = voice;
			return req<R<'bindLlmUse'>>('PUT', `/api/providers/uses/${seg(use)}`, { body });
		},
		// ---- provider slots
		providerSlots: () => req<R<'providerSlots'>>('GET', '/api/providers/slots'),
		saveProviderSlot: (slot, input) =>
			req<R<'saveProviderSlot'>>('PUT', `/api/providers/slots/${seg(slot)}`, { body: input }),
		acceptSlotDrift: (slot) =>
			req<R<'acceptSlotDrift'>>('POST', `/api/providers/slots/${seg(slot)}/adopt-remote`),
		resetProviderSlot: (slot, expected_version) =>
			req<R<'resetProviderSlot'>>('POST', `/api/providers/slots/${seg(slot)}/reset`, {
				body: { expected_version },
			}),
		testProviderSlotDraft: (slot, draft) =>
			req<R<'testProviderSlotDraft'>>('POST', `/api/providers/slots/${seg(slot)}/test`, { body: draft }),
		pushProviderSlot: (slot) => req<R<'pushProviderSlot'>>('POST', `/api/providers/slots/${seg(slot)}/push`),

		// ---- §5.2 stores, documents, query, model card, upload
		knowledgeStores: () => req<R<'knowledgeStores'>>('GET', '/api/knowledge/stores'),
		knowledgeDocuments: (storeId, before) =>
			req<R<'knowledgeDocuments'>>('GET', `/api/knowledge/stores/${seg(storeId)}/documents`, {
				query: { before },
			}),
		createKnowledgeStore: (input) =>
			req<R<'createKnowledgeStore'>>('POST', '/api/knowledge/stores', { body: input }),
		deleteKnowledgeStore: async (id) => {
			await req<unknown>('DELETE', `/api/knowledge/stores/${seg(id)}`);
		},
		ensureMissingStores: () => req<R<'ensureMissingStores'>>('POST', '/api/knowledge/stores/ensure'),
		deleteKnowledgeDocument: async (id) => {
			await req<unknown>('DELETE', `/api/knowledge/documents/${seg(id)}`);
		},
		knowledgeQuery: (input) => req<R<'knowledgeQuery'>>('POST', '/api/knowledge/query', { body: input }),
		retrievalModels: () => req<R<'retrievalModels'>>('GET', '/api/knowledge/models'),
		uploadToStore: async (storeId, files, language, supersedes) => {
			const form = new FormData();
			files.forEach((f, i) => form.append('file', blobOf(f), fileName(f, `file-${i + 1}`)));
			if (language) form.append('language', language);
			// PK-8 (T6): a JSON array string; absent = none
			if (supersedes?.length) form.append('supersedes', JSON.stringify(supersedes));
			return req.form<R<'uploadToStore'>>('POST', `/api/knowledge/stores/${seg(storeId)}/upload`, form);
		},

		// ---- §5.3 ingest jobs
		ingestJobs: (query = {}) =>
			req<R<'ingestJobs'>>('GET', '/api/knowledge/ingest-jobs', {
				query: { status: query.status, store_id: query.store_id },
			}),
		ingestJob: (id) => req<R<'ingestJob'>>('GET', `/api/knowledge/ingest-jobs/${seg(id)}`),
		retryIngestJob: (id) => req<R<'retryIngestJob'>>('POST', `/api/knowledge/ingest-jobs/${seg(id)}/retry`),
		cancelIngestJob: (id) =>
			req<R<'cancelIngestJob'>>('POST', `/api/knowledge/ingest-jobs/${seg(id)}/cancel`),
		deleteIngestJob: (id) => req<R<'deleteIngestJob'>>('DELETE', `/api/knowledge/ingest-jobs/${seg(id)}`),

		// ---- §5.4 sources (active job), runs, coverage
		sources: () => req<R<'sources'>>('GET', `/api/jobs/${j()}/sources`),
		source: (id) => req<R<'source'>>('GET', `/api/jobs/${j()}/sources/${seg(id)}`),
		sourceRuns: (id) => req<R<'sourceRuns'>>('GET', `/api/jobs/${j()}/sources/${seg(id)}/runs`),
		sourceMappingTargets: () =>
			req<R<'sourceMappingTargets'>>('GET', `/api/jobs/${j()}/sources/mapping-targets`),
		createSource: (input) => req<R<'createSource'>>('POST', `/api/jobs/${j()}/sources`, { body: input }),
		saveSource: (id, input) =>
			req<R<'saveSource'>>('PUT', `/api/jobs/${j()}/sources/${seg(id)}`, { body: input }),
		deleteSource: (id, purge) =>
			req<R<'deleteSource'>>('DELETE', `/api/jobs/${j()}/sources/${seg(id)}`, {
				query: { purge: purge ? 'true' : 'false' },
			}),
		testSource: (id, draft) =>
			id === null
				? req<R<'testSource'>>('POST', `/api/jobs/${j()}/sources/test`, { body: draft })
				: req<R<'testSource'>>('POST', `/api/jobs/${j()}/sources/${seg(id)}/test`, { body: draft }),
		previewSourceColumns: async (draft, file, opts = {}) => {
			if (file) {
				const form = new FormData();
				form.append('file', blobOf(file), fileName(file, 'table'));
				form.append('type', draft.type);
				if (opts.sheet) form.append('sheet', opts.sheet);
				if (opts.header_row !== undefined) form.append('header_row', String(opts.header_row));
				return req.form<R<'previewSourceColumns'>>('POST', `/api/jobs/${j()}/sources/preview-columns`, form);
			}
			// T7: JSON = the draft `{type, config, credentials?, sheet?, header_row?}`; the table is fetched server-side.
			const body: Record<string, unknown> = { type: draft.type, config: draft.config };
			if (draft.credentials !== undefined) body.credentials = draft.credentials;
			if (opts.sheet) body.sheet = opts.sheet;
			if (opts.header_row !== undefined) body.header_row = opts.header_row;
			return req<R<'previewSourceColumns'>>('POST', `/api/jobs/${j()}/sources/preview-columns`, { body });
		},
		resyncSource: async (id, opts = {}) => {
			if (opts.files?.length) {
				const form = new FormData();
				opts.files.forEach((f, i) => form.append('file', blobOf(f), fileName(f, `file-${i + 1}`)));
				return req.form<R<'resyncSource'>>('POST', `/api/jobs/${j()}/sources/${seg(id)}/resync`, form);
			}
			const body: Record<string, unknown> = {};
			if (opts.mapping) body.mapping = opts.mapping;
			return req<R<'resyncSource'>>('POST', `/api/jobs/${j()}/sources/${seg(id)}/resync`, { body });
		},
		sourceRun: (sourceId, runId) =>
			req<R<'sourceRun'>>('GET', `/api/jobs/${j()}/sources/${seg(sourceId)}/runs/${seg(runId)}`),
		knowledgeCoverage: (jobKey) =>
			req<R<'knowledgeCoverage'>>('GET', `/api/jobs/${seg(jobKey)}/knowledge/coverage`),

		// ---- §5.5 knowledge kinds + platform packs (R10)
		knowledgeKinds: () => req<R<'knowledgeKinds'>>('GET', `/api/jobs/${j()}/knowledge-kinds`),
		bindKnowledgeKind: (stage, kind, input) => {
			// absent = keep: only the fields the caller gave are sent (`store: null` unbinds, `packs: []` pins none)
			const body: Record<string, unknown> = {};
			if (input.store !== undefined) body.store = input.store;
			if (input.packs !== undefined) body.packs = input.packs;
			return req<R<'bindKnowledgeKind'>>(
				'PUT',
				`/api/jobs/${j()}/knowledge-kinds/${seg(stage)}/${seg(kind)}`,
				{ body },
			);
		},
		platformPacks: () => req<R<'platformPacks'>>('GET', '/api/knowledge/platform-packs'),

		// ---- §5.6 retrieval eval
		goldenQuestions: () => req<R<'goldenQuestions'>>('GET', '/api/knowledge/eval/questions'),
		createGoldenQuestion: (input) =>
			req<R<'createGoldenQuestion'>>('POST', '/api/knowledge/eval/questions', { body: input }),
		updateGoldenQuestion: (id, patch) =>
			req<R<'updateGoldenQuestion'>>('PATCH', `/api/knowledge/eval/questions/${seg(id)}`, { body: patch }),
		deleteGoldenQuestion: async (id) => {
			await req<unknown>('DELETE', `/api/knowledge/eval/questions/${seg(id)}`);
		},
		importGoldenQuestions: (rows) =>
			req<R<'importGoldenQuestions'>>('POST', '/api/knowledge/eval/questions/import', {
				body: { questions: rows },
			}),
		runEval: (opts = {}) => req<R<'runEval'>>('POST', '/api/knowledge/eval/runs', { body: opts }),
		evalRuns: (opts = {}) =>
			req<R<'evalRuns'>>('GET', '/api/knowledge/eval/runs', {
				query: { limit: opts.limit, cursor: opts.cursor },
			}),
		evalRun: (id) => req<R<'evalRun'>>('GET', `/api/knowledge/eval/runs/${seg(id)}`),

		// ---- §5.7 grounding: an answer is B12's assistant `conversation_message`; only the rating is sent (the
		// conversation, case, texts and verdict are read from the message log server-side)
		submitAnswerFeedback: (input) => {
			const body: Record<string, unknown> = {
				surface: input.surface,
				answer_id: input.answer_id,
				vote: input.vote,
			};
			if (input.reason !== undefined) body.reason = input.reason;
			if (input.note !== undefined) body.note = input.note;
			return req<R<'submitAnswerFeedback'>>('POST', `/api/jobs/${j()}/answer-feedback`, { body });
		},
		answerFeedback: (opts = {}) =>
			req<R<'answerFeedback'>>('GET', `/api/jobs/${j()}/answer-feedback`, {
				query: { conversation_id: opts.conversation_id, case_id: opts.case_id },
			}),
		knowledgeAnalytics: (jobKey, days) =>
			req<R<'knowledgeAnalytics'>>('GET', `/api/jobs/${seg(jobKey)}/analytics/knowledge`, {
				query: { days },
			}),

		// ---- §5.8 model changes
		modelChanges: (state) =>
			req<R<'modelChanges'>>('GET', '/api/knowledge/model-changes', { query: { state } }),
		modelChange: (id) => req<R<'modelChange'>>('GET', `/api/knowledge/model-changes/${seg(id)}`),
		reembedJobStore: (id, jobKey) =>
			req<R<'reembedJobStore'>>(
				'POST',
				`/api/knowledge/model-changes/${seg(id)}/jobs/${seg(jobKey)}/reembed`,
			),
		acceptModelChange: (id, jobKey, opts = {}) =>
			req<R<'acceptModelChange'>>(
				'POST',
				`/api/knowledge/model-changes/${seg(id)}/jobs/${seg(jobKey)}/accept`,
				{ query: { acknowledge_missing: opts.acknowledge_missing ? 'true' : null } },
			),
	};
}
