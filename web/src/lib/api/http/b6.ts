// B6b over HTTP (plan 2026-10-20 T13): the super_admin platform model — P4 layers (+ T12b authoring), P5 compile
// (+ T12a `as_of`), P12 business models, P13 dimensions, P6/P7 templates, P9–P11 objects + operations, P14 gate
// library, P15 agent roles and the T12f seed export (crates/agent-api/src/platform/routes/*.rs). Verbs and paths
// are the plan's §5 route table (the server wins where they differ); each one is listed in `ROUTES` and in the
// method's doc comment in `areas/{platform,model,ontology}.ts`. Platform routes never send a tenant (07 §4.5) and
// never act in a job. Every input body is rebuilt field by field: the B6b handlers refuse an unknown field (400
// `invalid_body`). A 422 that carries `error.errors[]` (`compile_failed`, the D106 hierarchy refusals) becomes a
// `CompileFailed`; a DELETE answered 204 resolves `void`; `exportPlatformSeed` resolves the file as a `Blob`.
import type { Api } from '../client';
import { ApiError } from '../errors';
import { seg, type Req, type TextFile } from '../http-client';
import { CompileFailed } from '../areas/platform';
import type { Paged } from '../types/core';
import type { CompileError, CompileTarget } from '../types/platform';

/** The answer type of `m`. */
type R<M extends keyof Api> = Awaited<ReturnType<Api[M]>>;

/** Platform model methods, including authoring and the owner-ruling methods (T12a–T12f). */
export const B6_METHODS = [
	// P4 layers (T9a) + authoring (T12b)
	'layerCatalog',
	'layerRows',
	'layerRowRevisions',
	'findLayerRow',
	'layerSummaries',
	'layerOverrideTargets',
	'createLayer',
	'writeLayerRow',
	// P5 compile (T9b, T12a)
	'compileOptions',
	'compilePreview',
	'compileReferences',
	// P12 business models (T10, T12e, T12g)
	'businessAxes',
	'addAxisValue',
	'createAxis',
	'retireAxisValue',
	'setAxisCardinality',
	'axisRules',
	'createAxisRule',
	'businessProfiles',
	'saveBusinessProfile',
	'renewalPresets',
	// P13 dimensions (T10, T12e)
	'platformDimensions',
	'departmentArchetypes',
	'addDimensionValue',
	'createDimension',
	'platformDimensionUsage',
	// industries + departments (plan 2026-10-30 T3)
	'platformIndustries',
	'createIndustry',
	'renameIndustry',
	'retireIndustry',
	'restoreIndustry',
	'createDepartment',
	'renameDepartment',
	'retireDepartment',
	'restoreDepartment',
	// P6/P7 templates (T11)
	'templateCatalog',
	'templateTagDetail',
	'tagLayerRevisions',
	'retireTemplateTag',
	'restoreTemplateTag',
	// P9/P10 objects (T11, T12f)
	'objectCatalog',
	'createObject',
	'objectModelMap',
	'objectDetail',
	'objectContext',
	'saveObjectField',
	'retireObjectField',
	'saveObjectState',
	'saveObjectTransition',
	'deleteObjectTransition',
	'saveObjectRelation',
	'deleteObjectRelation',
	// P11 operations (T11, T12c)
	'catalogOperations',
	'createOperation',
	'saveOperation',
	'errorStateDefs',
	// P14 gate library (T12, T12d)
	'gateLibrary',
	'gateLibraryUsage',
	'updateGateQuestion',
	'addGateOption',
	'createGateQuestion',
	'retireGateQuestion',
	'removeGateOption',
	// P15 agent roles (T12)
	'agentRoleJobs',
	'agentRoles',
	'addAgentRoleGrant',
	'removeAgentRoleGrant',
	'setAgentRoleMaxSideEffect',
	// T12f
	'exportPlatformSeed',
] as const;

/** The B6 methods served by the real API. */
export type B6Method = (typeof B6_METHODS)[number];

/** A list answer as a page: `[…]` or `{items, next}`. */
function pageOf<T>(raw: unknown): Paged<T> {
	if (Array.isArray(raw)) return { items: raw as T[], next: null };
	const r = (raw ?? {}) as { items?: T[]; next?: string | null };
	return { items: r.items ?? [], next: r.next ?? null };
}

/** `{…}` without the keys whose value is `undefined` (an absent optional field stays absent on the wire). */
function defined<T extends Record<string, unknown>>(o: T): Partial<T> {
	return Object.fromEntries(Object.entries(o).filter(([, v]) => v !== undefined)) as Partial<T>;
}

/** A template layer key as its URL slug: `industry_dept/retail/customer_service` → `industry_dept.retail.customer_service`. */
const layerSlug = (layer: string) => seg(layer.replace(/\//g, '.'));

/** The compile body (`CompileTarget` + F-07 `as_of`), the KH `SelectionIn` field by field. */
function compileBody(t: CompileTarget) {
	const s = t.selection;
	return defined({
		selection: {
			profile: s.profile,
			industry: s.industry,
			departments: [...s.departments],
			areas: [...s.areas],
			dimensions: { channel: [...s.dimensions.channel] },
			region: s.region,
		},
		job: t.job,
		as_of: t.as_of?.map((p) => ({ layer: p.layer, version: p.version })),
	});
}

/** A 422 carrying `error.errors[]` (`compile_failed`, `hierarchy_incomplete`, …) → `CompileFailed` with every error. */
async function compiling<T>(call: () => Promise<T>): Promise<T> {
	try {
		return await call();
	} catch (e) {
		if (e instanceof ApiError && e.status === 422 && Array.isArray(e.details.errors))
			throw new CompileFailed(e.details.errors as CompileError[], e.code);
		throw e;
	}
}

export function httpB6(req: Req): Pick<Api, B6Method> {
	return {
		// ---- P4 layers + T12b authoring
		layerCatalog: async () => pageOf(await req('GET', '/api/layers')),
		layerRows: async (layerId, opts = {}) =>
			pageOf(
				await req('GET', `/api/layers/${seg(layerId)}/rows`, {
					query: { include_retired: opts.include_retired ? 'true' : undefined },
				}),
			),
		layerRowRevisions: (layerId, kind, key) =>
			req<R<'layerRowRevisions'>>(
				'GET',
				`/api/layers/${seg(layerId)}/rows/${seg(kind)}/${seg(key)}/revisions`,
			),
		findLayerRow: (rowId) => req<R<'findLayerRow'>>('GET', `/api/layer-rows/${seg(rowId)}`),
		layerSummaries: () => req<R<'layerSummaries'>>('GET', '/api/layers/summary'),
		layerOverrideTargets: (layerId) =>
			req<R<'layerOverrideTargets'>>('GET', `/api/layers/${seg(layerId)}/overrides`),
		createLayer: (input) =>
			req<R<'createLayer'>>('POST', '/api/layers', {
				body: defined({
					scope: input.scope,
					industry: input.industry,
					department: input.department,
					job: input.job,
					dimension: input.dimension ? [...input.dimension] : undefined,
					region: input.region,
					label: input.label,
					extends: input.extends,
					areas: input.areas ? [...input.areas] : undefined,
				}),
			}),
		writeLayerRow: (layerId, input) =>
			req<R<'writeLayerRow'>>('POST', `/api/layers/${seg(layerId)}/rows`, {
				body: defined({
					kind: input.kind,
					key: input.key,
					op: input.op,
					fields: input.fields,
					expected_rev: input.expected_rev,
				}),
			}),

		// ---- P5 compile (never writes)
		compileOptions: () => req<R<'compileOptions'>>('GET', '/api/compile/options'),
		compilePreview: (target) =>
			compiling(() => req<R<'compilePreview'>>('POST', '/api/compile', { body: compileBody(target) })),
		compileReferences: () => req<R<'compileReferences'>>('GET', '/api/compile/references'),

		// ---- P12 business models
		businessAxes: () => req<R<'businessAxes'>>('GET', '/api/axes'),
		addAxisValue: (axis, v) =>
			req<R<'addAxisValue'>>('POST', `/api/axes/${seg(axis)}/values`, {
				body: defined({ key: v.key, label: v.label, parent: v.parent ?? undefined }),
			}),
		createAxis: (input) =>
			req<R<'createAxis'>>('POST', '/api/axes', {
				body: defined({
					key: input.key,
					label: input.label,
					description: input.description,
					cardinality: input.cardinality,
					object: input.object,
					milestone: input.milestone,
				}),
			}),
		retireAxisValue: (axis, value) =>
			req<R<'retireAxisValue'>>('POST', `/api/axes/${seg(axis)}/values/${seg(value)}/retire`),
		setAxisCardinality: (axis, cardinality) =>
			req<R<'setAxisCardinality'>>('PATCH', `/api/axes/${seg(axis)}`, { body: { cardinality } }),
		axisRules: () => req<R<'axisRules'>>('GET', '/api/axis-rules'),
		createAxisRule: (input) =>
			req<R<'createAxisRule'>>('POST', '/api/axis-rules', {
				body: defined({
					key: input.key,
					priority: input.priority,
					condition: input.condition,
					adds: { key: input.adds.key, label: input.adds.label },
					layer: input.layer,
				}),
			}),
		businessProfiles: () => req<R<'businessProfiles'>>('GET', '/api/profiles'),
		saveBusinessProfile: (p) =>
			req<R<'saveBusinessProfile'>>('PUT', `/api/profiles/${seg(p.key)}`, {
				body: {
					key: p.key,
					label: p.label,
					industry: p.industry,
					description: p.description,
					selection: p.selection,
				},
			}),
		renewalPresets: () => req<R<'renewalPresets'>>('GET', '/api/axes/renewal-presets'),

		// ---- P13 dimensions
		platformDimensions: () => req<R<'platformDimensions'>>('GET', '/api/dimensions'),
		departmentArchetypes: () => req<R<'departmentArchetypes'>>('GET', '/api/department-archetypes'),
		platformIndustries: () => req<R<'platformIndustries'>>('GET', '/api/platform/industries'),
		createIndustry: (input) =>
			req<R<'createIndustry'>>('POST', '/api/platform/industries', {
				body: { industry: input.industry, label: input.label },
			}),
		renameIndustry: (industry, label) =>
			req<R<'renameIndustry'>>('PATCH', `/api/platform/industries/${seg(industry)}`, { body: { label } }),
		retireIndustry: (industry) =>
			req<R<'retireIndustry'>>('POST', `/api/platform/industries/${seg(industry)}/retire`, { body: {} }),
		restoreIndustry: (industry) =>
			req<R<'restoreIndustry'>>('POST', `/api/platform/industries/${seg(industry)}/restore`, { body: {} }),
		createDepartment: (industry, input) =>
			req<R<'createDepartment'>>('POST', `/api/platform/industries/${seg(industry)}/departments`, {
				body: defined({
					department: input.department,
					label: input.label,
					extends: input.extends || undefined,
				}),
			}),
		renameDepartment: (industry, department, label) =>
			req<R<'renameDepartment'>>(
				'PATCH',
				`/api/platform/industries/${seg(industry)}/departments/${seg(department)}`,
				{ body: { label } },
			),
		retireDepartment: (industry, department) =>
			req<R<'retireDepartment'>>(
				'POST',
				`/api/platform/industries/${seg(industry)}/departments/${seg(department)}/retire`,
				{ body: {} },
			),
		restoreDepartment: (industry, department) =>
			req<R<'restoreDepartment'>>(
				'POST',
				`/api/platform/industries/${seg(industry)}/departments/${seg(department)}/restore`,
				{ body: {} },
			),
		addDimensionValue: (dimension, v) =>
			req<R<'addDimensionValue'>>('POST', `/api/dimensions/${seg(dimension)}/values`, {
				body: { key: v.key, label: v.label },
			}),
		createDimension: (input) =>
			req<R<'createDimension'>>('POST', '/api/dimensions', {
				body: defined({
					key: input.key,
					label: input.label,
					description: input.description,
					kind: input.kind,
				}),
			}),
		platformDimensionUsage: () => req<R<'platformDimensionUsage'>>('GET', '/api/dimensions/usage'),

		// ---- P6/P7 templates
		templateCatalog: () => req<R<'templateCatalog'>>('GET', '/api/platform/templates'),
		templateTagDetail: (layer, version) =>
			req<R<'templateTagDetail'>>('GET', `/api/platform/templates/${layerSlug(layer)}/v${version}`),
		tagLayerRevisions: (layer, note) =>
			req<R<'tagLayerRevisions'>>('POST', `/api/platform/templates/${layerSlug(layer)}/tags`, {
				body: { note },
			}),
		retireTemplateTag: (layer, version) =>
			req<R<'retireTemplateTag'>>(
				'POST',
				`/api/platform/templates/${layerSlug(layer)}/tags/v${version}/retire`,
			),
		restoreTemplateTag: (layer, version) =>
			req<R<'restoreTemplateTag'>>(
				'POST',
				`/api/platform/templates/${layerSlug(layer)}/tags/v${version}/restore`,
			),

		// ---- P9/P10 objects
		objectCatalog: () => req<R<'objectCatalog'>>('GET', '/api/platform/objects'),
		createObject: (o) =>
			req<R<'createObject'>>('POST', '/api/platform/objects', {
				body: {
					object: o.object,
					label: o.label,
					tier: o.tier,
					description: o.description,
					layer_key: o.layer_key,
				},
			}),
		objectModelMap: () => req<R<'objectModelMap'>>('GET', '/api/platform/objects/map'),
		objectDetail: (object) => req<R<'objectDetail'>>('GET', `/api/platform/objects/${seg(object)}`),
		objectContext: (object) => req<R<'objectContext'>>('GET', `/api/platform/objects/${seg(object)}/context`),
		saveObjectField: (object, f) =>
			req<R<'saveObjectField'>>('PUT', `/api/platform/objects/${seg(object)}/fields/${seg(f.field)}`, {
				body: {
					field: f.field,
					data_type: f.data_type,
					pii: f.pii,
					enum_values: [...f.enum_values],
					ref: f.ref,
					required: f.required,
					description: f.description,
				},
			}),
		retireObjectField: (object, field) =>
			req<R<'retireObjectField'>>('POST', `/api/platform/objects/${seg(object)}/fields/${seg(field)}/retire`),
		saveObjectState: (object, s) =>
			req<R<'saveObjectState'>>('PUT', `/api/platform/objects/${seg(object)}/states/${seg(s.status)}`, {
				body: { status: s.status, label: s.label, terminal: s.terminal, canonical: s.canonical },
			}),
		saveObjectTransition: (object, t) =>
			req<R<'saveObjectTransition'>>(
				'PUT',
				`/api/platform/objects/${seg(object)}/transitions/${seg(t.verb)}/${seg(t.from_status)}`,
				{
					body: {
						verb: t.verb,
						from_status: t.from_status,
						to_status: t.to_status,
						guard_rule: t.guard_rule,
					},
				},
			),
		deleteObjectTransition: async (object, verb, from) => {
			await req<void>('DELETE', `/api/platform/objects/${seg(object)}/transitions/${seg(verb)}/${seg(from)}`);
		},
		saveObjectRelation: (object, predicate, target, cardinality) =>
			req<R<'saveObjectRelation'>>(
				'PUT',
				`/api/platform/objects/${seg(object)}/relations/${seg(predicate)}/${seg(target)}`,
				{ body: { cardinality } },
			),
		deleteObjectRelation: async (object, predicate, target) => {
			await req<void>(
				'DELETE',
				`/api/platform/objects/${seg(object)}/relations/${seg(predicate)}/${seg(target)}`,
			);
		},

		// ---- P11 operations
		createOperation: (o) =>
			req<R<'createOperation'>>('POST', '/api/platform/operations', {
				body: defined({
					object: o.object,
					verb: o.verb,
					variant: o.variant,
					layer_key: o.layer_key,
					modality: o.modality,
					op_kind: o.op_kind,
					discriminator: o.discriminator,
					identity_level: o.identity_level,
					raises_identity_level: o.raises_identity_level,
					side_effect: o.side_effect,
					confirm_back: o.confirm_back,
					verify_read: o.verify_read,
					idempotency: o.idempotency,
					input_fields: [...o.input_fields],
					output_fields: [...o.output_fields],
					description: o.description,
					extra_errors: [...o.extra_errors],
				}),
			}),
		catalogOperations: async (f = {}) =>
			pageOf(
				await req('GET', '/api/platform/operations', {
					query: {
						object: f.object,
						modality: f.modality,
						side_effect: f.side_effect,
						identity_level: f.identity_level,
						builtin: f.builtin,
						retail_only: f.retail_only || undefined,
						q: f.q,
					},
				}),
			),
		saveOperation: (object, verb, variant, e) =>
			req<R<'saveOperation'>>('PUT', `/api/platform/operations/${seg(object)}/${seg(verb)}`, {
				query: { variant: variant && variant !== '*' ? variant : undefined },
				body: defined({
					identity_level: e.identity_level,
					raises_identity_level: e.raises_identity_level,
					side_effect: e.side_effect,
					confirm_back: e.confirm_back,
					verify_read: e.verify_read,
					idempotency: e.idempotency,
					input_fields: [...e.input_fields],
					output_fields: [...e.output_fields],
					description: e.description,
					extra_errors: [...e.extra_errors],
				}),
			}),
		errorStateDefs: () => req<R<'errorStateDefs'>>('GET', '/api/platform/error-states'),

		// ---- P14 gate library
		gateLibrary: async () => pageOf(await req('GET', '/api/platform/gates')),
		gateLibraryUsage: () => req<R<'gateLibraryUsage'>>('GET', '/api/platform/gates/usage'),
		updateGateQuestion: (key, e) =>
			req<R<'updateGateQuestion'>>('PATCH', `/api/platform/gates/${seg(key)}`, {
				body: defined({
					question: e.question,
					threshold_value: e.threshold_value,
					generated: e.generated,
					rule: e.rule,
					customer_reply: e.customer_reply,
				}),
			}),
		addGateOption: (key, o) =>
			req<R<'addGateOption'>>('POST', `/api/platform/gates/${seg(key)}/options`, {
				body: { label: o.label, description: o.description },
			}),
		createGateQuestion: (d) =>
			req<R<'createGateQuestion'>>('POST', '/api/platform/gates', {
				body: defined({
					key: d.key,
					layer: d.layer,
					group: d.group,
					point: d.point,
					type: d.type,
					question: d.question,
					options: d.options.map((o) => defined({ key: o.key, label: o.label, description: o.description })),
					threshold: d.threshold,
					threshold_value: d.threshold_value,
					rule: d.rule,
					fail: d.fail,
					action: d.action,
					effect: d.effect,
					tools: d.tools,
					customer_reply: d.customer_reply,
				}),
			}),
		retireGateQuestion: (key) =>
			req<R<'retireGateQuestion'>>('POST', `/api/platform/gates/${seg(key)}/retire`),
		removeGateOption: async (key, option) => {
			await req<void>('DELETE', `/api/platform/gates/${seg(key)}/options/${seg(option)}`);
		},

		// ---- P15 agent roles
		agentRoleJobs: () => req<R<'agentRoleJobs'>>('GET', '/api/platform/agent-roles/jobs'),
		agentRoles: async (job) => pageOf(await req('GET', '/api/platform/agent-roles', { query: { job } })),
		addAgentRoleGrant: (job, role, g) =>
			req<R<'addAgentRoleGrant'>>('POST', `/api/platform/agent-roles/${seg(job)}/${seg(role)}/grants`, {
				body: defined({ op: g.op, variant: g.variant ?? undefined, modalities: [...g.modalities] }),
			}),
		removeAgentRoleGrant: (job, role, op, variant) =>
			req<R<'removeAgentRoleGrant'>>(
				'DELETE',
				`/api/platform/agent-roles/${seg(job)}/${seg(role)}/grants/${seg(op)}`,
				{ query: { variant: variant ?? undefined } },
			),
		setAgentRoleMaxSideEffect: (job, role, max) =>
			req<R<'setAgentRoleMaxSideEffect'>>('PATCH', `/api/platform/agent-roles/${seg(job)}/${seg(role)}`, {
				body: { max_side_effect: max },
			}),

		// ---- T12f seed export (a JSON attachment)
		exportPlatformSeed: async (opts = {}) => {
			const f = await req<TextFile>('GET', '/api/platform/export', {
				query: { family: opts.family, tag: opts.tag },
				response: 'text',
			});
			return new Blob([f.body], { type: f.content_type || 'application/json' });
		},
	};
}
