// The `OntologyApi` contract.
import type { Paged } from '$lib/api/types/core';
import type {
	AgentRoleKey,
	AgentRoleView,
	ContextSpace,
	EffGateQuestion,
	EntityInput,
	Env,
	GateEvalInput,
	GateEvaluation,
	GateOptionRefinement,
	GateQuestion,
	GateQuestionDraft,
	GateQuestionEdit,
	GateRetired,
	GateQuestionInput,
	GateRule,
	GateUsage,
	GateVerdictRow,
	JobGates,
	KnowledgeKinds,
	KnowledgeKindRow,
	Modality,
	OntologyEntity,
	OntologyRelation,
	ProcedureVersionInput,
	ProcedureView,
	RelationInput,
	SideEffect,
	Toolset,
	RolePolicy,
	ToolsetMeta,
	ToolsetOverview,
	VersionStatus,
} from '$lib/api/types/ontology';
import type { KnowledgeKindBind, PlatformPack } from '$lib/api/types/ontology';

export type OntologyApi = {
	// ---- J11 ----
	/** `GET /api/jobs/{job}/ontology/entities?include_hidden=true` — effective entities (base + refinements). */
	ontologyEntities(opts?: { include_hidden?: boolean }): Promise<Paged<OntologyEntity>>;
	/** `GET /api/jobs/{job}/ontology/relations` */
	ontologyRelations(): Promise<Paged<OntologyRelation>>;
	/** `POST /api/jobs/{job}/ontology/entities` — a tenant extension entity (refinement). */
	createOntologyEntity(input: EntityInput): Promise<OntologyEntity>;
	/** `POST /api/jobs/{job}/ontology/entities/{name}/synonyms` */
	addOntologySynonym(name: string, term: string): Promise<OntologyEntity>;
	/** `POST /api/jobs/{job}/ontology/entities/{name}/attributes` — a tenant attribute (refinement, saved to draft). */
	addOntologyAttribute(
		name: string,
		attr: { name: string; data_type: string; pii: boolean; synonyms: string[] },
	): Promise<OntologyEntity>;
	/** `PUT /api/jobs/{job}/ontology/entities/{name}/hidden` — hide (tombstone) or unhide. */
	setOntologyEntityHidden(name: string, hidden: boolean): Promise<OntologyEntity>;
	/** `POST /api/jobs/{job}/ontology/relations` */
	createOntologyRelation(input: RelationInput): Promise<OntologyRelation>;
	/** `DELETE /api/jobs/{job}/ontology/relations` — tenant relations only; base relations go with their entity. */
	deleteOntologyRelation(rel: RelationInput): Promise<void>;

	// ---- J12 ----
	/** `GET /api/jobs/{job}/context-spaces` */
	contextSpaces(): Promise<Paged<ContextSpace>>;
	/** B2's `PUT /api/jobs/{job}/context-spaces/{key}` `{required}` — required slots (refinement, D54 draft kind
	 *  `space_required`); `null` reverts to base. It answers `{draft_rev}`; the row is then read from
	 *  `GET /api/jobs/{job}/context-spaces`. 404 `context_space_not_found`; 422 `invalid_slot`, `duplicate_slot`. */
	saveContextSpaceRequired(key: string, required: string[] | null): Promise<ContextSpace>;
	/** `GET /api/jobs/{job}/procedures` — every version (the draft), with shadowed / effective (live release) /
	 * effective_draft (test env) and the cut-time grant check. */
	procedures(): Promise<Paged<ProcedureView>>;
	/** `GET /api/jobs/{job}/procedures/{name}` — all versions of one procedure, newest first. */
	procedureVersions(name: string): Promise<ProcedureView[]>;
	/** `POST /api/jobs/{job}/procedures` — a new tenant DRAFT version. */
	createProcedureVersion(input: ProcedureVersionInput): Promise<ProcedureView>;
	/** `POST /api/jobs/{job}/procedures/{name}/versions/{v}/approve` — adds the caller as a reviewer (CANARY). */
	approveProcedure(name: string, version: number): Promise<ProcedureView>;
	/** `POST /api/jobs/{job}/procedures/{name}/versions/{v}/status` */
	setProcedureStatus(name: string, version: number, to: VersionStatus): Promise<ProcedureView>;

	// ---- J10 ----
	/** `GET /api/jobs/{job}/knowledge-kinds` — per-stage required kinds, their platform-pack pins (R10: listed,
	 *  unreadable, newer) and the tenant's leaf stores. strata_rag down: the cached pins, `X-Agent-Stale`. */
	knowledgeKinds(): Promise<KnowledgeKinds>;
	/**
	 * `PUT /api/jobs/{job}/knowledge-kinds/{stage}/{kind}` `{store?, packs?}` (job_designer; D54 draft) — `store`
	 * absent keeps, null unbinds the tenant store; `packs` absent keeps, `[]` pins none. A re-pin is this explicit
	 * save. 404 `kind_not_found`; 422 `unknown_store`, `invalid_pack` (not `key@version`, over 16, a key twice),
	 * `unknown_pack` (a NEW pin the tenant's listing lacks); 503 `unavailable` (strata_rag down while `packs` given).
	 */
	bindKnowledgeKind(stage: string, kind: string, input: KnowledgeKindBind): Promise<KnowledgeKindRow>;
	/** `GET /api/knowledge/platform-packs` — the platform packs this tenant may pin (R10); `[]` when platform knowledge
	 *  is off; 503 `unavailable` when strata_rag is down. */
	platformPacks(): Promise<PlatformPack[]>;

	// ---- J7 ----
	/** `GET /api/jobs/{job}/gates` — effective pre/post gates with refinements and 7-day verdict stats. */
	jobGates(): Promise<JobGates>;
	/** `PUT /api/jobs/{job}/gates/{gate}/options/{option}` — relabel / describe / hide (refinement). */
	refineGateOption(gate: string, option: string, r: GateOptionRefinement): Promise<EffGateQuestion>;
	/** `PUT /api/jobs/{job}/gates/{gate}/threshold` — tune (refinement); null reverts to base. */
	setGateThreshold(gate: string, value: number | null): Promise<EffGateQuestion>;
	/** `PUT /api/jobs/{job}/gates/{gate}/rule` — gte / lt / in + min confidence (refinement, D54 draft); null reverts
	 *  to the template rule. Safety gates keep theirs (409). */
	setGateRule(gate: string, rule: GateRule | null): Promise<EffGateQuestion>;
	/** `POST /api/jobs/{job}/gates` — a tenant question for this job (D54 draft). */
	addGateQuestion(input: GateQuestionInput): Promise<EffGateQuestion>;
	/** `DELETE /api/jobs/{job}/gates/{gate}` — tenant-added questions only (platform library questions: 409). */
	deleteGateQuestion(gate: string): Promise<void>;
	/** `PUT /api/jobs/{job}/gates/{gate}/customer-reply` — F-16 (B5 T8a): the text the customer reads when this question
	 *  hands over or blocks (D54 draft, kind `gate_setting`); null = the question's own / the care-bus default.
	 *  404 `gate_not_found`; 422 `invalid_reply` (blank, over 500 characters). */
	setGateCustomerReply(gate: string, text: string | null): Promise<EffGateQuestion>;
	/** `PUT /api/jobs/{job}/gates/{gate}/enabled` — F-17 (B5 T8a): ask this question in this job or not (D54 draft,
	 *  kind `gate_setting`). 404 `gate_not_found`; 409 `safety_gate` (a safety question is always on). */
	setGateEnabled(gate: string, enabled: boolean): Promise<EffGateQuestion>;
	/** `POST /api/jobs/{job}/gates/review` — the tenant read the gate questions and accepts them as they are (D54
	 *  draft, kind `gate_setting`, reserved key `_reviewed`): clears the `gates_unreviewed` gap, changes no question.
	 *  Idempotent; job_designer. Live backend only. */
	reviewGates(): Promise<void>;
	/** `POST /api/jobs/{job}/gates/evaluate` — run a message through one gate point (no side effects); draft or live. */
	evaluateGates(input: GateEvalInput): Promise<GateEvaluation>;
	/** `GET /api/jobs/{job}/gates/verdicts?limit=50` — the newest per-question verdicts of the job's cases. */
	gateVerdicts(job: string, limit?: number): Promise<GateVerdictRow[]>;

	// ---- J8 ----
	/** `GET /api/jobs/{job}/toolsets` — roles, activities and the tenant's narrowings. */
	toolsetMeta(): Promise<ToolsetMeta>;
	/** `POST /api/jobs/{job}/toolsets/{role}/narrowings` — remove one operation for one activity. */
	narrowToolsetGrant(role: AgentRoleKey, activity: string, op: string): Promise<Toolset>;
	/** `DELETE /api/jobs/{job}/toolsets/{role}/narrowings` — undo a narrowing; anything else is a widening (422). */
	restoreToolsetGrant(role: AgentRoleKey, activity: string, op: string): Promise<Toolset>;
	/** `GET /api/jobs/{job}/toolsets/overview?env=` — every role × activity toolset, the effective procedure per
	 * activity (with its steps' operations) and the tenant narrowings with provenance. */
	toolsetOverview(env: Env): Promise<ToolsetOverview>;
	/** `PUT /api/jobs/{job}/toolsets/{role}/enabled` (F-40, B5 T11a) — switch a role for this job (only
	 * `assistant`) and set its tool calls per turn (`maxToolCalls`: absent = keep, null = the default 6). A5: the
	 * same call carries the per-job runtime policy (`policy`: turn budget in seconds 5..600, the assistant's
	 * system prompt up to 4000 characters, one case per thread; absent = keep, null = the default; 422
	 * `invalid_turn_budget_s`, `invalid_system_prompt`). */
	setAgentRoleEnabled(
		role: AgentRoleKey,
		enabled: boolean,
		maxToolCalls?: number | null,
		policy?: RolePolicy,
	): Promise<ToolsetMeta>;

	// ---- P14 ----
	/** `GET /api/platform/gates` (super_admin) — the gate question library. */
	gateLibrary(): Promise<Paged<GateQuestion>>;
	/** `GET /api/platform/gates/usage` (super_admin) — which tenant jobs ask each gate, with refinement counts. */
	gateLibraryUsage(): Promise<GateUsage[]>;
	/** `PATCH /api/platform/gates/{key}` (super_admin) — question text, threshold, generated options. */
	updateGateQuestion(key: string, edit: GateQuestionEdit): Promise<GateQuestion>;
	/** `POST /api/platform/gates/{key}/options` (super_admin) — an authored option. */
	addGateOption(key: string, option: { label: string; description: string }): Promise<GateQuestion>;
	/** F-24 `POST /api/platform/gates` (super_admin) — a new library question in one template layer. 422
	 *  `invalid_question`, `invalid_gate_key`, `invalid_threshold`, `invalid_rule`, `too_many_options`; 404
	 *  `layer_not_found`; 409 `gate_exists`. */
	createGateQuestion(draft: GateQuestionDraft): Promise<GateQuestion>;
	/** F-24 `POST /api/platform/gates/{key}/retire` (super_admin) — tombstone a question. 404 `gate_not_found`; 409
	 *  `safety_gate`, `gate_in_use` (`jobs[]`: job templates that still hold it). */
	retireGateQuestion(key: string): Promise<GateRetired>;
	/** F-24 `DELETE /api/platform/gates/{key}/options/{option}` → 204 (super_admin). 404 `gate_not_found`,
	 *  `option_not_found`; 409 `options_fixed`, `options_generated`, `last_option`. */
	removeGateOption(key: string, option: string): Promise<void>;

	// ---- P15 ----
	/** `GET /api/platform/agent-roles/jobs` (super_admin) — the job templates P15 edits roles for, with role counts. */
	agentRoleJobs(): Promise<{ job: string; roles: number }[]>;
	/** `GET /api/platform/agent-roles?job=` (super_admin) */
	agentRoles(job: string): Promise<Paged<AgentRoleView>>;
	/** `POST /api/platform/agent-roles/{job}/{role}/grants` (super_admin) */
	addAgentRoleGrant(
		job: string,
		role: AgentRoleKey,
		grant: { op: string; variant: string | null; modalities: Modality[] },
	): Promise<AgentRoleView>;
	/** `DELETE /api/platform/agent-roles/{job}/{role}/grants/{object.verb}` (super_admin) */
	removeAgentRoleGrant(
		job: string,
		role: AgentRoleKey,
		op: string,
		variant: string | null,
	): Promise<AgentRoleView>;
	/** `PATCH /api/platform/agent-roles/{job}/{role}` (super_admin) */
	setAgentRoleMaxSideEffect(job: string, role: AgentRoleKey, max: SideEffect): Promise<AgentRoleView>;
};

/** Allowed procedure status moves (plan 05 procedure_management `allowed`). */
export const NEXT_STATUS: Record<VersionStatus, VersionStatus[]> = {
	DRAFT: ['CANARY', 'DISCARDED'],
	CANARY: ['PUBLISHED', 'DISCARDED'],
	PUBLISHED: ['ARCHIVED'],
	ARCHIVED: ['PUBLISHED'],
	DISCARDED: [],
};

export function splitOp(label: string): [string, string | null] {
	const m = /^([^{]+)(?:\{(.+)\})?$/.exec(label);
	return m ? [m[1]!, m[2] ?? null] : [label, null];
}

/** Grant entry `object.verb{variant}` covers step `op{variant}` when the op matches and the grant has no variant
 *  or the same one. */
export function grantCovers(grant: string, step: string): boolean {
	if (grant === step) return true;
	const [gOp, gVar] = splitOp(grant);
	const [sOp] = splitOp(step);
	return gOp === sOp && gVar === null;
}
