// B2 over HTTP (T17): drafts, releases, refinements, settings, languages, readiness, business rules and the design
// reads. Verbs, paths and bodies are the 37-method route table of the B2 plan (§4); each route is listed in `ROUTES`
// and in the method's doc comment in `areas/release.ts`. Job-scoped routes act in `job()` (the app's active job).
import type { Api } from '../client';
import { ApiError } from '../errors';
import { seg, type Req } from '../http-client';
import { ReleaseError } from '../areas/release';
import type { OverlayLayers } from '../types/release';
import type { LiveMethod } from './routes';

/** The answer type of `m`. */
type R<M extends keyof Api> = Awaited<ReturnType<Api[M]>>;

/** The 37 B2 methods (plan §4), in route-table order. */
export const B2_METHODS = [
	'releaseTarget',
	'saveReleaseTarget',
	'releaseList',
	'releaseDetail',
	'releaseCompare',
	'releaseStatus',
	'approveRelease',
	'applyRelease',
	'releaseConflicts',
	'resolveConflict',
	'conflictBoard',
	'refinementLayers',
	'refine',
	'jobSettingsView',
	'saveJobSettingOverride',
	'revertJobSettingOverride',
	'jobLanguages',
	'saveJobLanguages',
	'saveContextSpaceOverride',
	'saveInstruction',
	'releaseReadiness',
	'readinessBoard',
	'businessRules',
	'createBusinessRule',
	'saveRuleDefinition',
	'deleteBusinessRule',
	'rulePassages',
	'saveBusinessRule',
	'linkRulePassage',
	'evaluateRule',
	'draftSummary',
	'draftDiff',
	'liveDesign',
	'draftDesign',
	'releaseDesign',
	'designDiff',
	'tenantDesignDiff',
	'discardDraft',
	'publishedChanges',
	'rollbackPreview',
	'restoreRolledBack',
] as const satisfies readonly LiveMethod[];

export type B2Method = (typeof B2_METHODS)[number];

/** A refusal whose body carries `blocking` / `violations` / `errors` becomes a `ReleaseError` (the pages list them). */
async function releaseOp<T>(p: Promise<T>): Promise<T> {
	try {
		return await p;
	} catch (e) {
		throw e instanceof ApiError ? ReleaseError.fromApiError(e) : e;
	}
}

/** One layer of `GET /{object}?layer=…`: the full `OverlayLayers` shape, or a bare list of that layer's rows. */
function layerRows<K extends 'base' | 'tenant'>(answer: unknown, k: K): OverlayLayers[K] {
	if (Array.isArray(answer)) return answer as OverlayLayers[K];
	const rows = (answer as Partial<OverlayLayers> | null | undefined)?.[k];
	return (Array.isArray(rows) ? rows : []) as OverlayLayers[K];
}

export function httpB2(req: Req, job: () => string): Pick<Api, B2Method> {
	const j = () => seg(job());
	return {
		// ---- target + releases
		releaseTarget: () => req<R<'releaseTarget'>>('GET', `/api/jobs/${j()}/target`),
		saveReleaseTarget: (selection, expected_updated_at) =>
			req<R<'saveReleaseTarget'>>('PUT', `/api/jobs/${j()}/target`, {
				body: { selection, expected_updated_at },
			}),
		releaseList: () => req<R<'releaseList'>>('GET', `/api/jobs/${j()}/releases`),
		releaseDetail: (id) => req<R<'releaseDetail'>>('GET', `/api/jobs/${j()}/releases/${seg(id)}`),
		releaseCompare: (id) =>
			req<R<'releaseCompare'>>('GET', `/api/jobs/${j()}/releases/${seg(id)}/compare`, {
				query: { with: 'current' },
			}),
		releaseStatus: () => req<R<'releaseStatus'>>('GET', `/api/jobs/${j()}/releases/status`),
		approveRelease: (input) => {
			const body: Record<string, unknown> = {
				apply: input.apply,
				selection_seen_at: input.selection_seen_at,
			};
			if (input.adopt_pins !== undefined) body.adopt_pins = input.adopt_pins;
			return releaseOp(req<R<'approveRelease'>>('POST', `/api/jobs/${j()}/releases`, { body }));
		},
		applyRelease: (id) =>
			releaseOp(req<R<'applyRelease'>>('POST', `/api/jobs/${j()}/releases/${seg(id)}/apply`)),

		// ---- conflicts + refinements
		releaseConflicts: (status) =>
			req<R<'releaseConflicts'>>('GET', `/api/jobs/${j()}/release-conflicts`, { query: { status } }),
		resolveConflict: (id, op) =>
			req<R<'resolveConflict'>>('POST', `/api/jobs/${j()}/refinements/${seg(op)}`, {
				body: { conflict: id },
			}),
		conflictBoard: (status) =>
			req<R<'conflictBoard'>>('GET', `/api/jobs/${j()}/release-conflicts/board`, { query: { status } }),
		refinementLayers: async (object) => {
			const [base, tenant] = await Promise.all([
				req<unknown>('GET', `/api/jobs/${j()}/${seg(object)}`, { query: { layer: 'base' } }),
				req<unknown>('GET', `/api/jobs/${j()}/${seg(object)}`, { query: { layer: 'tenant' } }),
			]);
			const meta = (
				Array.isArray(tenant) ? (Array.isArray(base) ? {} : base) : tenant
			) as Partial<OverlayLayers> | null;
			return {
				object,
				draft_rev: meta?.draft_rev ?? 0,
				entities: meta?.entities ?? [],
				base: layerRows(base, 'base'),
				tenant: layerRows(tenant, 'tenant'),
			};
		},
		refine: (op, object, key) =>
			req<R<'refine'>>('POST', `/api/jobs/${j()}/refinements/${seg(op)}`, { body: { object, key } }),

		// ---- settings, languages, context spaces
		jobSettingsView: () => req<R<'jobSettingsView'>>('GET', `/api/jobs/${j()}/settings`),
		saveJobSettingOverride: (patch) =>
			req<R<'saveJobSettingOverride'>>('PUT', `/api/jobs/${j()}/settings`, { body: patch }),
		revertJobSettingOverride: (key) =>
			req<R<'revertJobSettingOverride'>>('DELETE', `/api/jobs/${j()}/settings/${seg(key)}`),
		jobLanguages: () => req<R<'jobLanguages'>>('GET', `/api/jobs/${j()}/languages`),
		saveJobLanguages: (languages) =>
			req<R<'saveJobLanguages'>>('PUT', `/api/jobs/${j()}/languages`, { body: { languages } }),
		saveContextSpaceOverride: (key, patch) =>
			req<R<'saveContextSpaceOverride'>>('PUT', `/api/jobs/${j()}/context-spaces/${seg(key)}`, {
				body: patch,
			}),
		saveInstruction: (slot, patch) =>
			req<R<'saveInstruction'>>('PUT', `/api/jobs/${j()}/instructions/${seg(slot)}`, { body: patch }),

		// ---- readiness (D67, J3)
		releaseReadiness: (key, opts = {}) =>
			req<R<'releaseReadiness'>>('GET', `/api/jobs/${seg(key ?? job())}/release-readiness`, {
				query: { of: opts.of },
			}),
		readinessBoard: (opts = {}) =>
			req<R<'readinessBoard'>>('GET', `/api/jobs/${j()}/readiness/board`, { query: { of: opts.of } }),

		// ---- business rules (J9)
		businessRules: () => req<R<'businessRules'>>('GET', `/api/jobs/${j()}/rules`),
		createBusinessRule: (body) => req<R<'createBusinessRule'>>('POST', `/api/jobs/${j()}/rules`, { body }),
		saveRuleDefinition: (id, body) =>
			req<R<'saveRuleDefinition'>>('PUT', `/api/jobs/${j()}/rules/${seg(id)}/definition`, { body }),
		deleteBusinessRule: (id) => req<R<'deleteBusinessRule'>>('DELETE', `/api/jobs/${j()}/rules/${seg(id)}`),
		rulePassages: () => req<R<'rulePassages'>>('GET', `/api/jobs/${j()}/rules/passages`),
		saveBusinessRule: (id, values) =>
			req<R<'saveBusinessRule'>>('PUT', `/api/jobs/${j()}/rules/${seg(id)}`, { body: { values } }),
		linkRulePassage: (id, passage_id) =>
			req<R<'linkRulePassage'>>('PUT', `/api/jobs/${j()}/rules/${seg(id)}/passage`, {
				body: { passage_id },
			}),
		evaluateRule: (id, facts, values, as_of) => {
			const body: Record<string, unknown> = { facts };
			if (values !== undefined) body.values = values;
			if (as_of !== undefined) body.as_of = as_of;
			return req<R<'evaluateRule'>>('POST', `/api/jobs/${j()}/rules/${seg(id)}/evaluate`, { body });
		},

		// ---- draft + design (D54, D62, D69)
		draftSummary: () => req<R<'draftSummary'>>('GET', `/api/jobs/${j()}/draft`),
		draftDiff: () => req<R<'draftDiff'>>('GET', `/api/jobs/${j()}/draft/diff`),
		liveDesign: () => req<R<'liveDesign'>>('GET', `/api/jobs/${j()}/design/live`),
		draftDesign: () => req<R<'draftDesign'>>('GET', `/api/jobs/${j()}/design/draft`),
		releaseDesign: (id) => req<R<'releaseDesign'>>('GET', `/api/jobs/${j()}/releases/${seg(id)}/design`),
		designDiff: () => req<R<'designDiff'>>('GET', `/api/jobs/${j()}/design/diff`),
		tenantDesignDiff: (kinds) =>
			req<R<'tenantDesignDiff'>>('GET', '/api/design/tenant-diff', {
				query: { kinds: kinds?.length ? kinds.join(',') : null },
			}),
		discardDraft: (id) =>
			id === 'all'
				? req<R<'discardDraft'>>('DELETE', `/api/jobs/${j()}/draft`)
				: req<R<'discardDraft'>>('DELETE', `/api/jobs/${j()}/draft/${seg(id)}`),
		publishedChanges: () => req<R<'publishedChanges'>>('GET', `/api/jobs/${j()}/draft/published`),
		rollbackPreview: (id) =>
			req<R<'rollbackPreview'>>('GET', `/api/jobs/${j()}/releases/${seg(id)}/rollback-preview`),
		restoreRolledBack: (ids) =>
			req<R<'restoreRolledBack'>>('POST', `/api/jobs/${j()}/draft/restore`, { body: { ids } }),
	};
}
