// B7 over HTTP (T18 subset): tenant systems (connector instances, write-only credentials, re-pin, T3 contract tests,
// sample responses), the catalogue a tenant may instantiate from, the platform connector-template reads, and the
// tenant-private templates (T11: create/save/validate/T1/status/next draft + the `fields=io` operation catalogue). Verbs and
// paths are the B7 plan's route table (each one listed in `ROUTES` and in the method's doc comment). Op keys
// (`case.create{return}`) travel raw in JSON bodies, except `saveConnectorTemplateOp`, whose path names the op being
// saved (`…/ops/case.create%7Breturn%7D`, the body's own key, percent-encoded as one segment), and the mapping overrides
// and job routes, whose paths name their op the same way (`…/mappings/case.read%7Bstatus%7D/status`,
// `…/operations/case.create%7Breturn%7D/routes/prod`). Job-scoped methods act in the page's job (`job()`).
import type { Api } from '../client';
import { ApiError } from '../errors';
import type { OpRef } from '../types/connectors';
import { opKey } from '$lib/connectors/ops';
import { seg, type Req } from '../http-client';

/** The answer type of `m`. */
type R<M extends keyof Api> = Awaited<ReturnType<Api[M]>>;

export const B7_METHODS = [
	'connectorInstances',
	'connectorInstance',
	'systemCatalogue',
	'createConnectorInstance',
	'updateConnectorInstance',
	'setConnectorCredential',
	'repinConnectorInstance',
	'runContractTest',
	'fetchSampleResponse',
	'systemInsight',
	'saveMappingOverride',
	'revertMappingOverride',
	'connectorTemplates',
	'connectorTemplate',
	'createConnectorTemplate',
	'connectorTemplateOverview',
	'connectorTemplateUsage',
	'saveConnectorTemplateOp',
	'draftConnectorTemplateVersion',
	'publishConnectorTemplate',
	'retireConnectorTemplate',
	'restoreConnectorTemplate',
	'runTemplateSandbox',
	'tenantConnectorTemplates',
	'tenantTemplateUsage',
	'connectorOpDefs',
	'createTenantConnectorTemplate',
	'saveTenantConnectorTemplate',
	'validateTenantConnectorTemplate',
	'runTenantTemplateContractTests',
	'setTenantTemplateStatus',
	'draftTenantTemplateVersion',
	'importMcpTools',
	'jobOperations',
	'jobOwnOperations',
	'addJobOwnOperation',
	'removeJobOwnOperation',
	'jobTools',
	'setOperationRoute',
	'clearOperationRoute',
	'exportOperationSpec',
] as const;

/** The B7 methods served by the real API. */
export type B7Method = (typeof B7_METHODS)[number];

/** An op's path segment: `case.create{return}` → `case.create%7Breturn%7D` (`*` variant → `case.create`). */
const opSeg = (o: OpRef) => seg(opKey({ object: o.object, verb: o.verb, variant: o.variant || '*' }));

/**
 * `job` is the page's job key, sent as `X-Job` on `systemCatalogue` so the API counts "implements N of your M
 * required ops" against it (no job = no header = 0 of 0), and the `{job}` of the job routes.
 */
export function httpB7(req: Req, job: () => string | null = () => null): Pick<Api, B7Method> {
	/** The page's job as a path segment; no job = nothing to act in (the fixture's `job_not_found`), nothing sent. */
	const jobSeg = () => {
		const j = job();
		if (!j) throw new ApiError(404, 'job_not_found', 'Pick a job first.');
		return seg(j);
	};
	return {
		// ---- tenant systems
		connectorInstances: () => req<R<'connectorInstances'>>('GET', '/api/connections/systems'),
		connectorInstance: (id) => req<R<'connectorInstance'>>('GET', `/api/connections/systems/${seg(id)}`),
		systemCatalogue: () => {
			const j = job();
			return req<R<'systemCatalogue'>>(
				'GET',
				'/api/connections/catalogue',
				j ? { headers: { 'X-Job': j } } : {},
			);
		},
		createConnectorInstance: (input) =>
			req<R<'createConnectorInstance'>>('POST', '/api/connections/systems', { body: input }),
		updateConnectorInstance: (id, patch) =>
			req<R<'updateConnectorInstance'>>('PATCH', `/api/connections/systems/${seg(id)}`, { body: patch }),
		setConnectorCredential: (id, values) =>
			req<R<'setConnectorCredential'>>('PUT', `/api/connections/systems/${seg(id)}/credential`, {
				body: values,
			}),
		repinConnectorInstance: (id, version) =>
			req<R<'repinConnectorInstance'>>('POST', `/api/connections/systems/${seg(id)}/repin`, {
				body: { version },
			}),
		runContractTest: (id, op, opts = {}) =>
			req<R<'runContractTest'>>('POST', `/api/connections/systems/${seg(id)}/contract-tests`, {
				body: opts.prod_write_ok === undefined ? { op } : { op, prod_write_ok: opts.prod_write_ok },
			}),
		fetchSampleResponse: (id, op) =>
			req<R<'fetchSampleResponse'>>('POST', `/api/connections/systems/${seg(id)}/samples`, { body: { op } }),
		systemInsight: (id) => req<R<'systemInsight'>>('GET', `/api/connections/systems/${seg(id)}/insight`),
		saveMappingOverride: (id, { op, field, path }) =>
			req<R<'saveMappingOverride'>>(
				'PUT',
				`/api/connections/systems/${seg(id)}/mappings/${seg(op)}/${seg(field)}`,
				{ body: { path } },
			),
		revertMappingOverride: (id, op, field) =>
			req<R<'revertMappingOverride'>>(
				'DELETE',
				`/api/connections/systems/${seg(id)}/mappings/${seg(op)}/${seg(field)}`,
			),

		// ---- platform connector templates (super_admin)
		connectorTemplates: () => req<R<'connectorTemplates'>>('GET', '/api/platform/connector-templates'),
		connectorTemplate: (key, version) =>
			req<R<'connectorTemplate'>>('GET', `/api/platform/connector-templates/${seg(key)}/${seg(version)}`),
		connectorTemplateOverview: () =>
			req<R<'connectorTemplateOverview'>>('GET', '/api/platform/connector-templates/coverage'),
		connectorTemplateUsage: (key, version) =>
			req<R<'connectorTemplateUsage'>>(
				'GET',
				`/api/platform/connector-templates/${seg(key)}/${seg(version)}/usage`,
			),
		createConnectorTemplate: (input) =>
			req<R<'createConnectorTemplate'>>('POST', '/api/platform/connector-templates', { body: input }),
		saveConnectorTemplateOp: (key, version, op) =>
			req<R<'saveConnectorTemplateOp'>>(
				'PUT',
				`/api/platform/connector-templates/${seg(key)}/${seg(version)}/ops/${seg(opKey(op))}`,
				{ body: op },
			),
		draftConnectorTemplateVersion: (key, fromVersion) =>
			req<R<'draftConnectorTemplateVersion'>>(
				'POST',
				`/api/platform/connector-templates/${seg(key)}/${seg(fromVersion)}/drafts`,
			),
		publishConnectorTemplate: (key, version) =>
			req<R<'publishConnectorTemplate'>>(
				'POST',
				`/api/platform/connector-templates/${seg(key)}/${seg(version)}/publish`,
			),
		retireConnectorTemplate: (key, version) =>
			req<R<'retireConnectorTemplate'>>(
				'POST',
				`/api/platform/connector-templates/${seg(key)}/${seg(version)}/retire`,
			),
		restoreConnectorTemplate: (key, version) =>
			req<R<'restoreConnectorTemplate'>>(
				'POST',
				`/api/platform/connector-templates/${seg(key)}/${seg(version)}/restore`,
			),
		runTemplateSandbox: (key, version) =>
			req<R<'runTemplateSandbox'>>(
				'POST',
				`/api/platform/connector-templates/${seg(key)}/${seg(version)}/t2-runs`,
			),

		// ---- tenant-private templates (T11, D31): a tenant's own REST connector on generic_rest@2
		tenantConnectorTemplates: () => req<R<'tenantConnectorTemplates'>>('GET', '/api/connections/templates'),
		tenantTemplateUsage: () => req<R<'tenantTemplateUsage'>>('GET', '/api/connections/templates/usage'),
		connectorOpDefs: () => req<R<'connectorOpDefs'>>('GET', '/api/operations', { query: { fields: 'io' } }),
		createTenantConnectorTemplate: (input) =>
			req<R<'createTenantConnectorTemplate'>>('POST', '/api/connections/templates', { body: input }),
		saveTenantConnectorTemplate: (key, version, draft) =>
			req<R<'saveTenantConnectorTemplate'>>('PUT', `/api/connections/templates/${seg(key)}/${seg(version)}`, {
				body: draft,
			}),
		validateTenantConnectorTemplate: (key, version) =>
			req<R<'validateTenantConnectorTemplate'>>(
				'POST',
				`/api/connections/templates/${seg(key)}/${seg(version)}/validate`,
			),
		runTenantTemplateContractTests: (key, version) =>
			req<R<'runTenantTemplateContractTests'>>(
				'POST',
				`/api/connections/templates/${seg(key)}/${seg(version)}/contract-tests`,
			),
		setTenantTemplateStatus: (key, version, status) =>
			req<R<'setTenantTemplateStatus'>>(
				'POST',
				`/api/connections/templates/${seg(key)}/${seg(version)}/status`,
				{ body: { status } },
			),
		draftTenantTemplateVersion: (key, fromVersion) =>
			req<R<'draftTenantTemplateVersion'>>(
				'POST',
				`/api/connections/templates/${seg(key)}/${seg(fromVersion)}/drafts`,
			),
		importMcpTools: (key, version, system) =>
			req<R<'importMcpTools'>>('POST', `/api/connections/templates/${seg(key)}/${seg(version)}/mcp-import`, {
				body: { system },
			}),

		// ---- job routes (T13) + the integration field-spec export (T13a)
		jobOperations: async () => req<R<'jobOperations'>>('GET', `/api/jobs/${jobSeg()}/operations`),
		jobOwnOperations: async () => req<R<'jobOwnOperations'>>('GET', `/api/jobs/${jobSeg()}/own-operations`),
		addJobOwnOperation: async (op, body) =>
			req<R<'addJobOwnOperation'>>('PUT', `/api/jobs/${jobSeg()}/own-operations/${seg(op)}`, { body }),
		removeJobOwnOperation: async (op) =>
			req<R<'removeJobOwnOperation'>>('DELETE', `/api/jobs/${jobSeg()}/own-operations/${seg(op)}`),
		jobTools: async () => req<R<'jobTools'>>('GET', `/api/jobs/${jobSeg()}/tools`),
		setOperationRoute: async ({ env, target, ...op }) =>
			req<R<'setOperationRoute'>>('PUT', `/api/jobs/${jobSeg()}/operations/${opSeg(op)}/routes/${seg(env)}`, {
				body: { target },
			}),
		clearOperationRoute: async ({ env, ...op }) =>
			req<R<'clearOperationRoute'>>(
				'DELETE',
				`/api/jobs/${jobSeg()}/operations/${opSeg(op)}/routes/${seg(env)}`,
			),
		exportOperationSpec: async (format, of = 'draft') =>
			req<R<'exportOperationSpec'>>('GET', `/api/jobs/${jobSeg()}/operations/spec`, {
				query: { format, of },
				response: 'text',
			}),
	};
}
