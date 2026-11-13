// B8 over HTTP (phase 2, T16a): the routes phase 1 already serves — P18 generator reads + estimate
// (crates/agent-api/src/routes/generator.rs), the J17 validation grid (routes/validation.rs) and J18 discovery
// (routes/discovery.rs). Verbs, paths and bodies are the routers' own; each route is listed in `ROUTES` and in the
// method's doc comment in `areas/journey.ts`. Job-scoped routes act in `job()` (the app's active job). Every body is
// rebuilt field by field (the handlers refuse an unknown field, 400 `invalid_body`). The uploads (`uploadDiscoveryImport`,
// `previewDiscoveryImport`, `importValidationQuestions`) go through `req.form` with a real `File`; a fixture-style
// file NAME is sent as an empty file of that name, which the server refuses (422). A 413 `too_large` is an `ApiError`.
// The B8 methods whose routes land with later tasks (the journey designer, draft validation, discovery proposals, …)
// stay on the fixture until their task wires them.
import type { Api } from '../client';
import { seg, type Req } from '../http-client';

/** The answer type of `m`. */
type R<M extends keyof Api> = Awaited<ReturnType<Api[M]>>;

/** The B8 methods served by the real API today. */
export const B8_METHODS = [
	'journeyIndustries',
	'industryJourney',
	'updateJourneyStage',
	'setThemeStagePrior',
	'addJourneyActivity',
	'moveJourneyActivity',
	'bindActivityObject',
	'unbindActivityObject',
	// T4a / T4b / T7a template editing
	'updateJourneyActivity',
	'retireJourneyActivity',
	'addJourneyStage',
	'reorderJourneyStages',
	'addJourneyTheme',
	'updateJourneyTheme',
	'setStageKnowledgeKind',
	'removeStageKnowledgeKind',
	'setStageMessages',
	'setJourneyChannelStatus',
	'setActivityOperation',
	'removeActivityOperation',
	'updateUseCaseCard',
	'refineJobUseCaseCard',
	'resetJobUseCaseCard',
	'jobJourney',
	'jobActivity',
	'hideJobActivity',
	'restoreJobActivity',
	'addJobExtensionActivity',
	'removeJobExtensionActivity',
	'relabelJobActivity',
	// P18 generator
	'generatorRuns',
	'generatorRun',
	'estimateGeneratorRun',
	// P18 generator start + drive (T9b / T10 / T10a / T11) and the D97 tenant-driven runs (T10b)
	'startGeneratorRun',
	'draftGeneratorStep',
	'decideGeneratorItem',
	'proposeGeneratorObject',
	'proposeGeneratorOperation',
	'resolveGeneratorProposal',
	'setGeneratorStageScope',
	'advanceGeneratorRun',
	'tagGeneratorRun',
	'uploadGeneratorEvidence',
	'jobGeneratorRun',
	'draftJobGeneratorStep',
	'decideJobGeneratorItem',
	'setJobGeneratorStageScope',
	'advanceJobGeneratorRun',
	'packageJobGeneratorRun',
	// J17 validation grid
	'validationCuts',
	'runValidationCut',
	'setValidationVerdict',
	'addValidationQuestion',
	'releaseValidation',
	'updateValidationQuestion',
	'deleteValidationQuestion',
	'importValidationQuestions',
	'draftValidation',
	'runDraftValidation',
	// J18 discovery
	'discoveryState',
	'uploadDiscoveryImport',
	'previewDiscoveryImport',
	'deleteDiscoveryImport',
	'setScorecardWeight',
	'startShadowMode',
	'stopShadowMode',
	'acceptDiscoveryProposal',
	'rejectDiscoveryProposal',
] as const;

/** The B8 methods served by the real API. */
export type B8Method = (typeof B8_METHODS)[number];

/** A file for a multipart part: a `File`/`Blob` as is, a file name as an empty file of that name. */
function partOf(f: File | string): { blob: Blob; name: string } {
	if (typeof f === 'string') return { blob: new Blob([]), name: f };
	return { blob: f, name: f.name };
}

/** `{…}` without the keys whose value is `undefined` (an absent optional field stays absent on the wire). */
function defined<T extends Record<string, unknown>>(o: T): Partial<T> {
	return Object.fromEntries(Object.entries(o).filter(([, v]) => v !== undefined)) as Partial<T>;
}

export function httpB8(req: Req, job: () => string): Pick<Api, B8Method> {
	const j = () => seg(job());
	const dept = (department?: string) => seg(department ?? '');
	const run = (id: number) => seg(String(id));
	return {
		journeyIndustries: () => req<R<'journeyIndustries'>>('GET', '/api/platform/journeys'),
		industryJourney: (industry, department) =>
			req<R<'industryJourney'>>(
				'GET',
				`/api/platform/journeys/${seg(industry)}?department=${dept(department)}`,
			),
		updateJourneyStage: (industry, stage, patch, department) =>
			req<R<'updateJourneyStage'>>(
				'PATCH',
				`/api/platform/journeys/${seg(industry)}/stages/${seg(stage)}?department=${dept(department)}`,
				{
					body: defined(patch),
				},
			),
		setThemeStagePrior: (industry, theme, stage, weight, department) =>
			req<R<'setThemeStagePrior'>>(
				'PUT',
				`/api/platform/journeys/${seg(industry)}/priors/${seg(theme)}/${seg(stage)}?department=${dept(department)}`,
				{ body: { weight } },
			),
		addJourneyActivity: (industry, input, department) =>
			req<R<'addJourneyActivity'>>(
				'POST',
				`/api/platform/journeys/${seg(industry)}/activities?department=${dept(department)}`,
				{ body: input },
			),
		moveJourneyActivity: (industry, key, stage, department) =>
			req<R<'moveJourneyActivity'>>(
				'POST',
				`/api/platform/journeys/${seg(industry)}/activities/${seg(key)}/move?department=${dept(department)}`,
				{
					body: { stage },
				},
			),
		bindActivityObject: (industry, key, binding, department) => {
			const { object, role, fields, required_inputs } = binding;
			return req<R<'bindActivityObject'>>(
				'PUT',
				`/api/platform/journeys/${seg(industry)}/activities/${seg(key)}/objects/${seg(object)}?department=${dept(department)}`,
				{ body: { role, fields, required_inputs } },
			);
		},
		unbindActivityObject: (industry, key, object, department) =>
			req<R<'unbindActivityObject'>>(
				'DELETE',
				`/api/platform/journeys/${seg(industry)}/activities/${seg(key)}/objects/${seg(object)}?department=${dept(department)}`,
			),
		updateJourneyActivity: (industry, key, patch, department) =>
			req<R<'updateJourneyActivity'>>(
				'PATCH',
				`/api/platform/journeys/${seg(industry)}/activities/${seg(key)}?department=${dept(department)}`,
				{
					body: defined(patch),
				},
			),
		retireJourneyActivity: (industry, key, department) =>
			req<R<'retireJourneyActivity'>>(
				'DELETE',
				`/api/platform/journeys/${seg(industry)}/activities/${seg(key)}?department=${dept(department)}`,
			),
		addJourneyStage: (industry, input, department) =>
			req<R<'addJourneyStage'>>(
				'POST',
				`/api/platform/journeys/${seg(industry)}/stages?department=${dept(department)}`,
				{
					body: defined({
						key: input.key,
						label: input.label,
						description: input.description,
						after: input.after,
					}),
				},
			),
		reorderJourneyStages: (industry, stages, department) =>
			req<R<'reorderJourneyStages'>>(
				'PUT',
				`/api/platform/journeys/${seg(industry)}/stages/order?department=${dept(department)}`,
				{
					body: { stages },
				},
			),
		addJourneyTheme: (industry, input, department) =>
			req<R<'addJourneyTheme'>>(
				'POST',
				`/api/platform/journeys/${seg(industry)}/themes?department=${dept(department)}`,
				{
					body: { key: input.key, label: input.label, escalation_weight: input.escalation_weight },
				},
			),
		updateJourneyTheme: (industry, theme, patch, department) =>
			req<R<'updateJourneyTheme'>>(
				'PATCH',
				`/api/platform/journeys/${seg(industry)}/themes/${seg(theme)}?department=${dept(department)}`,
				{
					body: defined(patch),
				},
			),
		setStageKnowledgeKind: (industry, stage, kind, input, department) =>
			req<R<'setStageKnowledgeKind'>>(
				'PUT',
				`/api/platform/journeys/${seg(industry)}/stages/${seg(stage)}/knowledge-kinds/${seg(kind)}?department=${dept(department)}`,
				{
					body: defined({
						store_key: input.store_key,
						required: input.required,
						tenant_data: input.tenant_data,
						layer: input.layer,
					}),
				},
			),
		removeStageKnowledgeKind: (industry, stage, kind, layer, department) =>
			req<R<'removeStageKnowledgeKind'>>(
				'DELETE',
				`/api/platform/journeys/${seg(industry)}/stages/${seg(stage)}/knowledge-kinds/${seg(kind)}?department=${dept(department)}${layer ? `&layer=${seg(layer)}` : ''}`,
			),
		setStageMessages: (industry, stage, channel, utterances, department) =>
			req<R<'setStageMessages'>>(
				'PUT',
				`/api/platform/journeys/${seg(industry)}/stages/${seg(stage)}/messages/${seg(channel)}?department=${dept(department)}`,
				{ body: { utterances } },
			),
		setJourneyChannelStatus: (industry, channel, status, department) =>
			req<R<'setJourneyChannelStatus'>>(
				'PUT',
				`/api/platform/journeys/${seg(industry)}/channels/${seg(channel)}?department=${dept(department)}`,
				{
					body: { status },
				},
			),
		setActivityOperation: (industry, key, op, expectation, department) =>
			req<R<'setActivityOperation'>>(
				'PUT',
				`/api/platform/journeys/${seg(industry)}/activities/${seg(key)}/operations/${seg(op)}?department=${dept(department)}`,
				{ body: { expectation } },
			),
		removeActivityOperation: (industry, key, op, department) =>
			req<R<'removeActivityOperation'>>(
				'DELETE',
				`/api/platform/journeys/${seg(industry)}/activities/${seg(key)}/operations/${seg(op)}?department=${dept(department)}`,
			),
		updateUseCaseCard: (industry, key, patch, department) =>
			req<R<'updateUseCaseCard'>>(
				'PUT',
				`/api/platform/journeys/${seg(industry)}/activities/${seg(key)}/card?department=${dept(department)}`,
				{
					body: defined(patch),
				},
			),
		jobJourney: () => req<R<'jobJourney'>>('GET', `/api/jobs/${j()}/journey`),
		jobActivity: (key) => req<R<'jobActivity'>>('GET', `/api/jobs/${j()}/journey/activities/${seg(key)}`),
		hideJobActivity: (key) =>
			req<R<'hideJobActivity'>>('POST', `/api/jobs/${j()}/refinements/journey/hide`, { body: { key } }),
		restoreJobActivity: (key) =>
			req<R<'restoreJobActivity'>>('POST', `/api/jobs/${j()}/refinements/journey/restore`, { body: { key } }),
		addJobExtensionActivity: (input) =>
			req<R<'addJobExtensionActivity'>>('POST', `/api/jobs/${j()}/refinements/journey/extensions`, {
				body: input,
			}),
		removeJobExtensionActivity: (key) =>
			req<R<'removeJobExtensionActivity'>>(
				'DELETE',
				`/api/jobs/${j()}/refinements/journey/extensions/${seg(key)}`,
			),
		relabelJobActivity: (key, label) =>
			req<R<'relabelJobActivity'>>('PUT', `/api/jobs/${j()}/refinements/journey/labels/${seg(key)}`, {
				body: { label },
			}),
		refineJobUseCaseCard: (key, patch) =>
			req<R<'refineJobUseCaseCard'>>('PUT', `/api/jobs/${j()}/refinements/journey/cards/${seg(key)}`, {
				body: defined(patch),
			}),
		resetJobUseCaseCard: (key) =>
			req<R<'resetJobUseCaseCard'>>('DELETE', `/api/jobs/${j()}/refinements/journey/cards/${seg(key)}`),
		// ---- P18 generator (super_admin reads; estimate: super_admin or a job_designer's own tenant)
		generatorRuns: () => req<R<'generatorRuns'>>('GET', '/api/platform/generator/runs'),
		generatorRun: (id) => req<R<'generatorRun'>>('GET', `/api/platform/generator/runs/${seg(String(id))}`),
		estimateGeneratorRun: (input) =>
			req<R<'estimateGeneratorRun'>>('POST', '/api/generator/runs/estimate', {
				body: defined({ scope: input.scope, tenant_id: input.tenant_id ?? undefined }),
			}),
		startGeneratorRun: (input) =>
			req<R<'startGeneratorRun'>>('POST', '/api/generator/runs', {
				body: defined({
					scope: input.scope,
					industry: input.industry,
					department: input.department,
					job: input.job,
					description: input.description,
					tenant_id: input.tenant_id ?? undefined,
				}),
			}),
		draftGeneratorStep: (id, hint) =>
			req<R<'draftGeneratorStep'>>('POST', `/api/platform/generator/runs/${run(id)}/draft`, {
				body: defined({ hint: hint?.trim() ? hint : undefined }),
			}),
		decideGeneratorItem: (id, itemId, d) =>
			req<R<'decideGeneratorItem'>>(
				'POST',
				`/api/platform/generator/runs/${run(id)}/items/${seg(itemId)}/decision`,
				{ body: defined({ decision: d.decision, note: d.note }) },
			),
		proposeGeneratorObject: (id, itemId) =>
			req<R<'proposeGeneratorObject'>>(
				'POST',
				`/api/platform/generator/runs/${run(id)}/items/${seg(itemId)}/proposal`,
				{ body: {} },
			),
		proposeGeneratorOperation: (id, itemId, operation) =>
			req<R<'proposeGeneratorOperation'>>(
				'POST',
				`/api/platform/generator/runs/${run(id)}/items/${seg(itemId)}/operation-proposal`,
				{ body: { operation: defined({ ...operation }) } },
			),
		// `{run}:{item}` is ONE path segment, split on its first `:`
		resolveGeneratorProposal: (id, itemId, approve) =>
			req<R<'resolveGeneratorProposal'>>('POST', `/api/platform/object-proposals/${run(id)}:${seg(itemId)}`, {
				body: { approve },
			}),
		setGeneratorStageScope: (id, stage, outOfScope) =>
			req<R<'setGeneratorStageScope'>>(
				'PUT',
				`/api/platform/generator/runs/${run(id)}/stages/${seg(stage)}`,
				{ body: { out_of_scope: outOfScope } },
			),
		advanceGeneratorRun: (id) =>
			req<R<'advanceGeneratorRun'>>('POST', `/api/platform/generator/runs/${run(id)}/advance`),
		tagGeneratorRun: (id) => req<R<'tagGeneratorRun'>>('POST', `/api/platform/generator/runs/${run(id)}/tag`),
		uploadGeneratorEvidence: (id, input) => {
			const { blob, name } = partOf(input.file);
			const form = new FormData();
			form.append('file', blob, name);
			if (input.text_column) form.append('text_column', input.text_column);
			if (input.count_column) form.append('count_column', input.count_column);
			return req.form<R<'uploadGeneratorEvidence'>>(
				'POST',
				`/api/platform/generator/runs/${run(id)}/evidence`,
				form,
			);
		},
		jobGeneratorRun: (id) => req<R<'jobGeneratorRun'>>('GET', `/api/jobs/${j()}/generator/runs/${run(id)}`),
		draftJobGeneratorStep: (id, hint) =>
			req<R<'draftJobGeneratorStep'>>('POST', `/api/jobs/${j()}/generator/runs/${run(id)}/draft`, {
				body: defined({ hint: hint?.trim() ? hint : undefined }),
			}),
		decideJobGeneratorItem: (id, itemId, d) =>
			req<R<'decideJobGeneratorItem'>>(
				'POST',
				`/api/jobs/${j()}/generator/runs/${run(id)}/items/${seg(itemId)}/decision`,
				{ body: defined({ decision: d.decision, note: d.note }) },
			),
		setJobGeneratorStageScope: (id, stage, outOfScope) =>
			req<R<'setJobGeneratorStageScope'>>(
				'PUT',
				`/api/jobs/${j()}/generator/runs/${run(id)}/stages/${seg(stage)}`,
				{ body: { out_of_scope: outOfScope } },
			),
		advanceJobGeneratorRun: (id) =>
			req<R<'advanceJobGeneratorRun'>>('POST', `/api/jobs/${j()}/generator/runs/${run(id)}/advance`),
		packageJobGeneratorRun: (id) =>
			req<R<'packageJobGeneratorRun'>>('POST', `/api/jobs/${j()}/generator/runs/${run(id)}/package`),

		// ---- J17 validation grid
		validationCuts: () => req<R<'validationCuts'>>('GET', `/api/jobs/${j()}/validation`),
		runValidationCut: (releaseId) =>
			req<R<'runValidationCut'>>('POST', `/api/jobs/${j()}/validation/${seg(releaseId)}/run`),
		setValidationVerdict: (releaseId, rowId, verdict, reason) =>
			req<R<'setValidationVerdict'>>(
				'PUT',
				`/api/jobs/${j()}/validation/${seg(releaseId)}/rows/${seg(rowId)}`,
				{ body: defined({ verdict, reason: reason ?? undefined }) },
			),
		addValidationQuestion: (input) =>
			req<R<'addValidationQuestion'>>('POST', `/api/jobs/${j()}/validation/questions`, {
				body: defined({
					question: input.question,
					expected: input.expected,
					reference: input.reference,
					activity: input.activity || undefined,
				}),
			}),
		releaseValidation: (releaseId) =>
			req<R<'releaseValidation'>>('GET', `/api/jobs/${j()}/releases/${seg(releaseId)}/validation`),
		updateValidationQuestion: (id, patch) =>
			req<R<'updateValidationQuestion'>>('PATCH', `/api/jobs/${j()}/validation/questions/${seg(id)}`, {
				body: defined({
					question: patch.question,
					expected: patch.expected,
					reference: patch.reference,
					activity: patch.activity,
				}),
			}),
		deleteValidationQuestion: async (id) => {
			await req<void>('DELETE', `/api/jobs/${j()}/validation/questions/${seg(id)}`);
		},
		draftValidation: () => req<R<'draftValidation'>>('GET', `/api/jobs/${j()}/validation/draft`),
		runDraftValidation: () => req<R<'runDraftValidation'>>('POST', `/api/jobs/${j()}/validation/draft/run`),
		importValidationQuestions: (file) => {
			const { blob, name } = partOf(file);
			const form = new FormData();
			form.append('file', blob, name);
			return req.form<R<'importValidationQuestions'>>(
				'POST',
				`/api/jobs/${j()}/validation/questions/import`,
				form,
			);
		},

		// ---- J18 discovery
		discoveryState: () => req<R<'discoveryState'>>('GET', `/api/jobs/${j()}/discovery`),
		uploadDiscoveryImport: (input) => {
			const { blob, name } = partOf(input.file);
			const form = new FormData();
			form.append('file', blob, name);
			if (input.text_column) form.append('text_column', input.text_column);
			if (input.count_column) form.append('count_column', input.count_column);
			return req.form<R<'uploadDiscoveryImport'>>('POST', `/api/jobs/${j()}/discovery/imports`, form);
		},
		previewDiscoveryImport: (file) => {
			const { blob, name } = partOf(file);
			const form = new FormData();
			form.append('file', blob, name);
			return req.form<R<'previewDiscoveryImport'>>(
				'POST',
				`/api/jobs/${j()}/discovery/imports/preview`,
				form,
			);
		},
		deleteDiscoveryImport: (id) =>
			req<R<'deleteDiscoveryImport'>>('DELETE', `/api/jobs/${j()}/discovery/imports/${seg(id)}`),
		setScorecardWeight: (activity, weight) =>
			req<R<'setScorecardWeight'>>('PUT', `/api/jobs/${j()}/discovery/scorecard/${seg(activity)}`, {
				body: { weight },
			}),
		startShadowMode: (env) =>
			req<R<'startShadowMode'>>('POST', `/api/jobs/${j()}/discovery/shadow`, { body: { env } }),
		stopShadowMode: () => req<R<'stopShadowMode'>>('DELETE', `/api/jobs/${j()}/discovery/shadow`),
		acceptDiscoveryProposal: (id) =>
			req<R<'acceptDiscoveryProposal'>>('POST', `/api/jobs/${j()}/discovery/proposals/${seg(id)}/accept`),
		rejectDiscoveryProposal: (id, reason) =>
			req<R<'rejectDiscoveryProposal'>>('POST', `/api/jobs/${j()}/discovery/proposals/${seg(id)}/reject`, {
				body: { reason },
			}),
	};
}
