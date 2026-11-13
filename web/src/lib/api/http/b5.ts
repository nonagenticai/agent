// B5 over HTTP (T13): job design — J11 ontology, J12 context spaces + procedures, J7 gates, J8 toolsets
// (crates/agent-api/src/routes/job_design/*.rs). Verbs, paths and bodies are the B5 plan's route table; each route is
// listed in `ROUTES` and in the method's doc comment in `areas/ontology.ts`. Job-scoped routes act in `job()` (the
// app's active job). `saveContextSpaceRequired` writes through B2's `PUT /api/jobs/{job}/context-spaces/{key}`
// (plan R12; it answers `{draft_rev}`), then reads `contextSpaces` and returns the saved row. Every input body is
// rebuilt field by field: the B5 handlers refuse an unknown field (422 `invalid_body`).
import type { Api } from '../client';
import { ApiError } from '../errors';
import { seg, type Req } from '../http-client';
import type { Paged } from '../types/core';
import type {
	EntityInput,
	GateEvalInput,
	GateOptionRefinement,
	GateQuestionInput,
	ProcedureStep,
	ProcedureVersionInput,
	RelationInput,
} from '../types/ontology';

/** The answer type of `m`. */
type R<M extends keyof Api> = Awaited<ReturnType<Api[M]>>;

/** The 30 B5 methods (plan "Web wiring (summary)"), page by page. */
export const B5_METHODS = [
	// J11 ontology
	'ontologyEntities',
	'ontologyRelations',
	'createOntologyEntity',
	'addOntologySynonym',
	'addOntologyAttribute',
	'setOntologyEntityHidden',
	'createOntologyRelation',
	'deleteOntologyRelation',
	// J12 context spaces + procedures
	'contextSpaces',
	'saveContextSpaceRequired',
	'procedures',
	'procedureVersions',
	'createProcedureVersion',
	'approveProcedure',
	'setProcedureStatus',
	// J7 gates
	'jobGates',
	'refineGateOption',
	'setGateThreshold',
	'setGateRule',
	'addGateQuestion',
	'deleteGateQuestion',
	'setGateCustomerReply',
	'setGateEnabled',
	'reviewGates',
	'evaluateGates',
	'gateVerdicts',
	// J8 toolsets
	'toolsetMeta',
	'narrowToolsetGrant',
	'restoreToolsetGrant',
	'toolsetOverview',
	'setAgentRoleEnabled',
] as const;
export type B5Method = (typeof B5_METHODS)[number];

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

const relationBody = (r: RelationInput) => ({
	subject: r.subject,
	predicate: r.predicate,
	object: r.object,
	cardinality: r.cardinality,
});

const attributeBody = (a: { name: string; data_type: string; pii: boolean; synonyms: string[] }) => ({
	name: a.name,
	data_type: a.data_type,
	pii: a.pii,
	synonyms: a.synonyms,
});

const entityBody = (i: EntityInput) => ({
	name: i.name,
	label: i.label,
	scope: i.scope,
	industry: i.industry,
	department: i.department,
	synonyms: i.synonyms,
	object_key: i.object_key,
	attributes: i.attributes.map(attributeBody),
});

const stepBody = (s: ProcedureStep) =>
	defined({
		step_number: s.step_number,
		phase: s.phase,
		name: s.name,
		operation: s.operation,
		variant: s.variant,
		modality: s.modality,
		side_effect: s.side_effect,
		identity_level: s.identity_level,
		confirm_back: s.confirm_back,
		gate: s.gate,
		args: s.args,
		preconditions: s.preconditions,
		postconditions: s.postconditions,
		performed_by: s.performed_by,
		max_per_case: s.max_per_case,
	});

const procedureBody = (i: ProcedureVersionInput) =>
	defined({
		name: i.name,
		version: i.version,
		context_space: i.context_space,
		description: i.description,
		steps: i.steps.map(stepBody),
		success_criteria: i.success_criteria,
		tool_requirements: i.tool_requirements,
		pddl_domain: i.pddl_domain,
	});

const refinementBody = (r: GateOptionRefinement) =>
	defined({ label: r.label, description: r.description, hidden: r.hidden });

const questionBody = (i: GateQuestionInput) =>
	defined({
		key: i.key,
		point: i.point,
		question: i.question,
		criteria: { true: i.criteria.true, false: i.criteria.false },
		rule: {
			fire_when: i.rule.fire_when,
			threshold: i.rule.threshold,
			fire_values: i.rule.fire_values,
			min_confidence: i.rule.min_confidence,
		},
		action: i.action,
		fail: i.fail,
		tools: i.tools,
		customer_reply: i.customer_reply,
	});

const evalBody = (i: GateEvalInput) =>
	defined({ point: i.point, text: i.text, tool: i.tool, against: i.against });

export function httpB5(req: Req, job: () => string): Pick<Api, B5Method> {
	const j = () => seg(job());
	const contextSpaces = async () =>
		pageOf<R<'contextSpaces'>['items'][number]>(await req<unknown>('GET', `/api/jobs/${j()}/context-spaces`));
	return {
		// ---- J11 ontology
		ontologyEntities: async (opts = {}) =>
			pageOf(
				await req<unknown>('GET', `/api/jobs/${j()}/ontology/entities`, {
					query: { include_hidden: opts.include_hidden ? true : undefined },
				}),
			),
		ontologyRelations: async () => pageOf(await req<unknown>('GET', `/api/jobs/${j()}/ontology/relations`)),
		createOntologyEntity: (input) =>
			req<R<'createOntologyEntity'>>('POST', `/api/jobs/${j()}/ontology/entities`, {
				body: entityBody(input),
			}),
		addOntologySynonym: (name, term) =>
			req<R<'addOntologySynonym'>>('POST', `/api/jobs/${j()}/ontology/entities/${seg(name)}/synonyms`, {
				body: { term },
			}),
		addOntologyAttribute: (name, attr) =>
			req<R<'addOntologyAttribute'>>('POST', `/api/jobs/${j()}/ontology/entities/${seg(name)}/attributes`, {
				body: attributeBody(attr),
			}),
		setOntologyEntityHidden: (name, hidden) =>
			req<R<'setOntologyEntityHidden'>>('PUT', `/api/jobs/${j()}/ontology/entities/${seg(name)}/hidden`, {
				body: { hidden },
			}),
		createOntologyRelation: (input) =>
			req<R<'createOntologyRelation'>>('POST', `/api/jobs/${j()}/ontology/relations`, {
				body: relationBody(input),
			}),
		deleteOntologyRelation: async (rel) => {
			await req<void>('DELETE', `/api/jobs/${j()}/ontology/relations`, { body: relationBody(rel) });
		},

		// ---- J12 context spaces + procedures
		contextSpaces,
		// B2's route (R12): `{required}` only is recorded as `space_required`; it answers `{draft_rev}`, so the row is
		// read back from `contextSpaces`.
		saveContextSpaceRequired: async (key, required) => {
			await req<{ draft_rev: number }>('PUT', `/api/jobs/${j()}/context-spaces/${seg(key)}`, {
				body: { required },
			});
			const row = (await contextSpaces()).items.find((s) => s.key === key);
			if (!row) throw new ApiError(404, 'context_space_not_found', `No context space ${key}.`);
			return row;
		},
		procedures: async () => pageOf(await req<unknown>('GET', `/api/jobs/${j()}/procedures`)),
		procedureVersions: (name) =>
			req<R<'procedureVersions'>>('GET', `/api/jobs/${j()}/procedures/${seg(name)}`),
		createProcedureVersion: (input) =>
			req<R<'createProcedureVersion'>>('POST', `/api/jobs/${j()}/procedures`, { body: procedureBody(input) }),
		approveProcedure: (name, version) =>
			req<R<'approveProcedure'>>(
				'POST',
				`/api/jobs/${j()}/procedures/${seg(name)}/versions/${seg(String(version))}/approve`,
			),
		setProcedureStatus: (name, version, to) =>
			req<R<'setProcedureStatus'>>(
				'POST',
				`/api/jobs/${j()}/procedures/${seg(name)}/versions/${seg(String(version))}/status`,
				{ body: { to } },
			),

		// ---- J7 gates
		jobGates: () => req<R<'jobGates'>>('GET', `/api/jobs/${j()}/gates`),
		refineGateOption: (gate, option, r) =>
			req<R<'refineGateOption'>>('PUT', `/api/jobs/${j()}/gates/${seg(gate)}/options/${seg(option)}`, {
				body: refinementBody(r),
			}),
		setGateThreshold: (gate, value) =>
			req<R<'setGateThreshold'>>('PUT', `/api/jobs/${j()}/gates/${seg(gate)}/threshold`, { body: { value } }),
		setGateRule: (gate, rule) =>
			req<R<'setGateRule'>>('PUT', `/api/jobs/${j()}/gates/${seg(gate)}/rule`, { body: { rule } }),
		addGateQuestion: (input) =>
			req<R<'addGateQuestion'>>('POST', `/api/jobs/${j()}/gates`, { body: questionBody(input) }),
		deleteGateQuestion: async (gate) => {
			await req<void>('DELETE', `/api/jobs/${j()}/gates/${seg(gate)}`);
		},
		setGateCustomerReply: (gate, text) =>
			req<R<'setGateCustomerReply'>>('PUT', `/api/jobs/${j()}/gates/${seg(gate)}/customer-reply`, {
				body: { text },
			}),
		setGateEnabled: (gate, enabled) =>
			req<R<'setGateEnabled'>>('PUT', `/api/jobs/${j()}/gates/${seg(gate)}/enabled`, { body: { enabled } }),
		reviewGates: async () => {
			await req<void>('POST', `/api/jobs/${j()}/gates/review`, { body: {} });
		},
		evaluateGates: (input) =>
			req<R<'evaluateGates'>>('POST', `/api/jobs/${j()}/gates/evaluate`, { body: evalBody(input) }),
		// the page passes its job; the route acts in that job (the active job when none is given)
		gateVerdicts: (jobKey, limit = 50) =>
			req<R<'gateVerdicts'>>('GET', `/api/jobs/${seg(jobKey || job())}/gates/verdicts`, { query: { limit } }),

		// ---- J8 toolsets
		toolsetMeta: () => req<R<'toolsetMeta'>>('GET', `/api/jobs/${j()}/toolsets`),
		narrowToolsetGrant: (role, activity, op) =>
			req<R<'narrowToolsetGrant'>>('POST', `/api/jobs/${j()}/toolsets/${seg(role)}/narrowings`, {
				body: { activity, op },
			}),
		restoreToolsetGrant: (role, activity, op) =>
			req<R<'restoreToolsetGrant'>>('DELETE', `/api/jobs/${j()}/toolsets/${seg(role)}/narrowings`, {
				body: { activity, op },
			}),
		toolsetOverview: (env) =>
			req<R<'toolsetOverview'>>('GET', `/api/jobs/${j()}/toolsets/overview`, { query: { env } }),
		// `max_tool_calls` absent keeps the stored cap, null = the default (agent-api `RoleEnabledInput`)
		setAgentRoleEnabled: (role, enabled, maxToolCalls, policy) =>
			req<R<'setAgentRoleEnabled'>>('PUT', `/api/jobs/${j()}/toolsets/${seg(role)}/enabled`, {
				body: {
					enabled,
					...(maxToolCalls === undefined ? {} : { max_tool_calls: maxToolCalls }),
					...(policy?.turnBudgetS === undefined ? {} : { turn_budget_s: policy.turnBudgetS }),
					...(policy?.systemPrompt === undefined ? {} : { system_prompt: policy.systemPrompt }),
					...(policy?.casePerThread === undefined ? {} : { case_per_thread: policy.casePerThread }),
				},
			}),
	};
}
