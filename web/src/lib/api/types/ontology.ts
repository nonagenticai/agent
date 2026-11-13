// Shapes for the "ontology" area: job ontology (J11), context spaces + procedures (J12), knowledge kinds (J10),
// gates (P14, J7), agent roles (P15) and effective toolsets (J8). Field names follow spec 14 §3 (snake_case).
import type { Iso, JobKey, Sub, TenantId } from './core';

/** Where a row comes from: the pinned template layers (`base`), or this tenant's refinements (`tenant`). */
export type Origin = 'base' | 'tenant';

// ---- J11 ontology ------------------------------------------------------------------------------------------

/** The template layer an entity is authored in: base (generic), industry, or industry × department. No `base_dept`
 *  (D116): entities are not a base-department kind (KC plan §9 S4), and Rust `job_design/ontology.rs` `scope_of`
 *  reads a base_dept layer as the department level. */
export type OntologyScope = 'generic' | 'industry' | 'industry_dept';
export type Cardinality = '1:1' | '1:n' | 'n:1' | 'n:m';

export interface OntologyAttribute {
	name: string;
	data_type: string;
	pii: boolean;
	synonyms: string[];
	origin: Origin;
	/** A refinement whose base parent is gone (05 §4.6 `parent_missing`). */
	conflict?: 'parent_missing' | null;
}

export interface OntologyEntity {
	tenant_id: TenantId;
	job: JobKey;
	/** Key, PascalCase (`Order`). */
	name: string;
	label: string;
	scope: OntologyScope;
	industry: string | null;
	department: string | null;
	synonyms: string[];
	attributes: OntologyAttribute[];
	origin: Origin;
	/** Hidden by a tenant refinement (tombstone); still listed under "Show hidden". */
	hidden: boolean;
	/** The platform object this entity is a view of (`order` → `/platform/objects/order`), owner call #8. */
	object_key: string | null;
	/** Layer tag it was authored in (`job/customer_care_agent@v3`) or `refinement r14`. */
	layer: string;
	created_by: Sub | null;
	created_at: Iso;
}

export interface OntologyRelation {
	tenant_id: TenantId;
	job: JobKey;
	subject: string;
	predicate: string;
	object: string;
	cardinality: Cardinality;
	origin: Origin;
}

export interface EntityInput {
	name: string;
	label: string;
	scope: OntologyScope;
	industry: string | null;
	department: string | null;
	synonyms: string[];
	object_key: string | null;
	attributes: { name: string; data_type: string; pii: boolean; synonyms: string[] }[];
}

export interface RelationInput {
	subject: string;
	predicate: string;
	object: string;
	cardinality: Cardinality;
}

// ---- J12 context spaces + procedures ----------------------------------------------------------------------

export interface ContextSpace {
	tenant_id: TenantId;
	job: JobKey;
	/** `retail:order-support:returns` — also the bus topic prefix. */
	key: string;
	label: string;
	/** Journey stage + activity this space serves (`support:start_return`). */
	stage: string;
	activity: string;
	request_type: string;
	entities: string[];
	/** Required slots the qualifier fills before the procedure runs (effective, after the tenant override). */
	required: string[];
	/** Slots as the base layer declares them. */
	base_required: string[];
	/** J12 (D54): the slots production runs with (the live release); differs from `required` while a draft edit waits
	 *  for the next release. Absent = same as `required`. */
	live_required?: string[];
	origin: Origin;
	/** Tenant changed `required` (refinement). */
	overridden: boolean;
	/** Base removed this space but a tenant row still refers to it (05 §4.6 `base_removed`). */
	conflict: 'base_removed' | 'base_changed' | null;
	/** Procedure name the orchestrator runs for this space. */
	procedure: string;
}

export type VersionStatus = 'DRAFT' | 'CANARY' | 'PUBLISHED' | 'ARCHIVED' | 'DISCARDED';
export type SideEffect = 'R' | 'W' | 'X' | 'P';
export type Modality = 'do' | 'request' | 'observe';

export interface ProcedureStep {
	step_number: number;
	/** 0-based phase; steps in one phase run in parallel. */
	phase: number;
	name: string;
	/** `object.verb` */
	operation: string;
	/** Variant without braces (`return`), or null. */
	variant: string | null;
	modality: Modality;
	side_effect: SideEffect;
	identity_level: string;
	/** Needs a customer confirm-back before it runs (W/X/P). */
	confirm_back: boolean;
	/** The case ends as not eligible when this step's outcome is false. */
	gate: boolean;
	/** Slot bindings `arg ← $slot`. */
	args: Record<string, string>;
	/** T7a (F-20): checked by the runner before the call; reads only steps of EARLIER phases. */
	preconditions?: Condition[];
	/** T7a (F-20): checked after an `ok` answer; may read this step's own output too. */
	postconditions?: Condition[];
	/** T7a: the job's agent role that may run the step; null/absent = the procedure role. */
	performed_by?: string | null;
	/** T7a (F-15): per-case cap of an X/P step, 1–20; null/absent = once per case. Never on R/W steps. */
	max_per_case?: number | null;
}

/** B4 `CondOp`. */
export type CondOp = 'eq' | 'ne' | 'in' | 'exists' | 'absent' | 'gte' | 'lte';

/** B4 `Condition` (`runtime/conditions.rs`): `fact op value`, `fact` one of `FACT_PATHS`
 *  (`$lib/ontology/conditions`). A missing fact makes every condition false except `absent`. */
export interface Condition {
	/** `^[a-z][a-z0-9_]{0,31}$`, unique in its list: what a failure reports. */
	id: string;
	fact: string;
	op: CondOp;
	/** A scalar for eq/ne, a number or text for gte/lte, a non-empty list for `in`, none for exists/absent. */
	value?: string | number | boolean | (string | number | boolean)[] | null;
}

export interface ProcedureHistoryEntry {
	at: Iso;
	actor: Sub;
	action: 'create' | 'approve' | 'transition';
	from: VersionStatus | null;
	to: VersionStatus | null;
	ok: boolean;
	reason: string | null;
}

export interface ProcedureVersion {
	tenant_id: TenantId;
	job: JobKey;
	name: string;
	/** Integer from 1 (B5 R6, `agent_case.procedure_version`); a tenant version is above every version of its name. */
	version: number;
	status: VersionStatus;
	origin: Origin;
	context_space: string;
	description: string;
	reviewers: Sub[];
	steps: ProcedureStep[];
	history: ProcedureHistoryEntry[];
	updated_at: Iso;
	runs_7d: number;
	/** (completed + not_eligible) / runs_7d; null without runs. */
	success_rate: number | null;
	/** J12: the job release this version was materialised in at its last cut/apply (05 §4.10), or null when it was
	 *  never in a release (DRAFT/CANARY, or PUBLISHED after the last cut: live as a refinement, K7). */
	released_in?: string | null;
	/** J12: staff test chats run against this version (env `test`, staff `/chat` runs as `procedure`, K38). */
	test_runs?: ProcedureTestRun[];
	/** T7a (F-20): checked when the last phase finished; unmet → the case is handed over. */
	success_criteria?: Condition[];
	/** T7a: operations (`object.verb{variant}`) the procedure needs routed before its first step. */
	tool_requirements?: string[];
	/** T7a: a PDDL domain, stored verbatim and never interpreted (≤ 64 KiB). */
	pddl_domain?: string | null;
}

/** J12: one staff test chat against a procedure version (env `test`; never a `prod` route, 12 A13). */
export interface ProcedureTestRun {
	at: Iso;
	actor: Sub;
	/** The customer message the tester typed. */
	utterance: string;
	outcome: 'resolved' | 'not_eligible' | 'handed_over' | 'refused';
	/** Last step number that ran (0 = none). */
	reached_step: number;
	note: string;
}

/** A version row as the list and detail pages get it: with the effective flags computed. */
export interface ProcedureView extends ProcedureVersion {
	/** A base PUBLISHED version that a tenant PUBLISHED version of the same name shadows. */
	shadowed: boolean;
	/** The version the orchestrator runs for its context space IN PRODUCTION (D54: follows the live release —
	 *  a version PUBLISHED in the draft is not effective until a release publishes it). */
	effective: boolean;
	/** The version the Test environment (test chat) runs: effective in the draft. */
	effective_draft?: boolean;
	/** Status in the live design (null = the version is not in production at all, e.g. created in the draft). */
	live_status?: VersionStatus | null;
	/** Steps whose operation is outside the derived grant of the space's activity (cut-time check, 12 §2.1). */
	outside_grant: string[];
	/** The activity grant the steps are checked against (`object.verb{variant}`). */
	activity_grant: string[];
}

export interface ProcedureVersionInput {
	name: string;
	version: number;
	context_space: string;
	description: string;
	steps: ProcedureStep[];
	/** T7a: version-level success criteria (may read every step). */
	success_criteria?: Condition[];
	/** T7a: catalogue operations the procedure needs (422 `unknown_operation` otherwise). */
	tool_requirements?: string[];
	/** T7a: stored verbatim (422 `invalid_pddl` over 64 KiB). */
	pddl_domain?: string | null;
}

// ---- J10 knowledge kinds ------------------------------------------------------------------------------------

export interface KnowledgeKindRow {
	tenant_id: TenantId;
	job: JobKey;
	/** A stage, or `*` (every stage, KH T3 / D107). */
	stage: string;
	/** Store key the strata_rag store must hold (`policy.returns`). */
	kind: string;
	required: boolean;
	/** KH T3 (D110): the tenant's own data — frozen platform packs alone do not satisfy it. */
	tenant_data?: boolean;
	description: string;
	/** Bound knowledge store path (`faq-policies/policy.returns`), or null. */
	store: string | null;
	origin: Origin;
	/** R10: the platform packs this kind also searches, pinned `key@version` (empty = none). */
	packs?: KnowledgePackPin[];
	/** `template` = the pins follow the job template until a bind sets them (`binding`). */
	packs_source?: 'binding' | 'template';
}

/** One pinned platform pack of a kind (R10): whether the tenant's listing still has it, and a newer listed version. */
export interface KnowledgePackPin {
	/** `key@version`. */
	pin: string;
	/** In the tenant's current listing (a retired pin stays pinned and readable, but is no longer listed). */
	listed: boolean;
	/** strata_rag could not read the pack the last time a search tried: searches run without it. */
	unreadable: boolean;
	/** `key@version` of the newest listed version above the pin, or null. */
	newer: string | null;
	/** PK (T5): the pack's state in the agent's cache; null/absent = not known yet. */
	state?: PackState | null;
	/** PK-10 (T7/T5): the tenant exclusion prefix that hides this pack (searches run without it), or null. */
	excluded_by?: string | null;
	/** PK (T3/T5): this job hides the template's pin (a `knowledge_pack` tombstone refinement in its draft). */
	tombstoned?: boolean;
	/** PK (T5): how many leaf stores of the pack a search names (0 = nothing to search). */
	leaves?: number;
}

/** A platform pack's lifecycle as strata_rag reports it, `missing` = pinned but unknown to strata (PK). */
export type PackState = 'published' | 'retired' | 'draft' | 'missing';

/** The overlay key of a pack pin on one kind: `stage|kind|pack_key` (object `knowledge_pack`, T3). */
export const packRefinementKey = (stage: string, kind: string, packKey: string) =>
	`${stage}|${kind}|${packKey}`;

/** A platform pack the tenant may pin (`GET /api/knowledge/platform-packs`, R10). */
export interface PlatformPack {
	/** `key@version`. */
	pin: string;
	key: string;
	version: number;
	root_id: string;
	published_at: Iso | null;
}

/** `bindKnowledgeKind` body: `store` absent = keep, null = unbind the tenant store; `packs` absent = keep, [] = none. */
export interface KnowledgeKindBind {
	store?: string | null;
	packs?: string[];
}

/** At most this many pins per kind (`knowledge::kinds::MAX_PINS`). */
export const MAX_PACK_PINS = 16;
/** `key@version` as the API parses it (`kinds::parse_pin`): key = 1–6 `/` segments of a-z 0-9 _ -, version 1–9 digits. */
export function parsePackPin(s: string): { key: string; version: number } | null {
	const at = s.lastIndexOf('@');
	if (at < 0) return null;
	const key = s.slice(0, at);
	const version = s.slice(at + 1);
	const segs = key.split('/');
	if (!/^[1-9][0-9]{0,8}$/.test(version)) return null;
	if (segs.length > 6 || !segs.every((x) => /^[a-z0-9_-]+$/.test(x))) return null;
	return { key, version: Number(version) };
}

// ---- PK platform knowledge curation (super_admin, `/api/platform/knowledge`, T8) ----------------------------

/** A store on the platform plane (strata `StoreView`): routers group, leaves hold documents. */
export interface CuratorStore {
	id: string;
	parent_id: string | null;
	kind: 'router' | 'leaf';
	name: string;
	description?: string;
	chunk_count?: number;
	token_count?: number;
	children?: string[];
}

/** `POST /api/platform/knowledge/stores`. */
export interface CuratorStoreInput {
	name: string;
	parent_id?: string | null;
}

/** One document of a platform store (strata `Document`). */
export interface CuratorDocument {
	id: string;
	store_id: string;
	source: string;
	title: string | null;
	chunk_count?: number;
	token_count?: number;
	source_id?: string | null;
	metadata?: Record<string, unknown>;
	created_at: Iso;
}

/** A page of platform documents (strata's cursor in `next`). */
export interface CuratorDocumentPage {
	items: CuratorDocument[];
	next: string | null;
}

/** `POST /api/platform/knowledge/stores/{id}/documents`: every platform document names its topics and provenance;
 *  strata enforces the licence allow-list (422 passed through). */
export interface CuratorDocumentInput {
	title: string;
	text: string;
	source_id?: string;
	/** 1–32 topic keys (a tenant `supersedes` names these). */
	topic_keys: string[];
	provenance: { source_url: string; license: string; retrieved_at: string };
}

/** Licence ids strata_rag allows on platform documents by default (`DEFAULT_PLATFORM_LICENSES`): hints only; strata
 *  answers 422 for one it does not allow. */
export const PLATFORM_LICENSE_HINTS = [
	'CC0-1.0',
	'PDDL-1.0',
	'public-domain',
	'CC-BY-4.0',
	'CC-BY-SA-4.0',
	'OGL-UK-3.0',
	'ODC-BY-1.0',
] as const;

/** A pack as its owner lists it (`GET /api/platform/knowledge/packs`): drafts and retired packs included. */
export interface CuratorPack {
	key: string;
	version: number;
	root_id: string;
	published_at: Iso | null;
	retired_at: Iso | null;
}

/** One store of a pack's subtree (strata `PackStore`). */
export interface CuratorPackStore {
	id: string;
	parent_id: string | null;
	kind: 'router' | 'leaf';
	name: string;
	/** Store names from the pack root (included) down to this store. */
	path: string[];
	/** `"<model>@<dims>"`: platform leaves are embedded with the pack's own model. */
	embed_model: string;
	chunk_count: number;
}

/** `GET /api/platform/knowledge/packs/{key}/{version}/stores` (strata `PackStores`): root first, by name path. */
export interface CuratorPackStores {
	/** `"<key>@<version>"`. */
	pack: string;
	root_id: string;
	stores: CuratorPackStore[];
}

/** `POST /api/platform/knowledge/packs`: register a draft pack over a platform store tree. */
export interface CuratorPackInput {
	key: string;
	version: number;
	store_id: string;
}

/** Template layer families a pack can be referenced from. */
export type PackLayerFamily = 'base' | 'industry' | 'base_dept' | 'industry_dept' | 'job';

/** `POST /api/platform/knowledge/packs/{key}/{version}/references` and the `DELETE …/references?` query: the layer
 *  (`base`, `industry/retail`, …) and the stage/kind the pin applies to (`*` = every stage / kind). */
export interface PackReferenceInput {
	layer: string;
	stage: string;
	kind: string;
}

/** A reference write: the `knowledge_pack` row written and the tag cut for the layer's family (`industry@v12`). */
export interface PackReferenceResult {
	row: Record<string, unknown>;
	tag: string;
}

export interface KnowledgeStoreOption {
	path: string;
	label: string;
	documents: number;
	updated_at: Iso | null;
}

export interface KnowledgeKinds {
	rows: KnowledgeKindRow[];
	stores: KnowledgeStoreOption[];
	/** KH T3 (D107): template pack references that match none of the job's kinds — listed, never an error. */
	not_applicable?: NotApplicablePack[];
	/** Every platform pack the job's template gives it, with the layer that set it (base → industry → department). */
	inherited?: InheritedPack[];
}

/** A platform pack reference the job inherits from its template (read-only for the tenant). */
export interface InheritedPack {
	/** `stage|kind|pack_key` (stage / kind may be `*`). */
	key: string;
	stage: string;
	kind: string;
	/** `key@version`. */
	pin: string;
	/** The template layer that set it: `base`, `industry/<i>`, `industry_dept/<i>/<d>`, `job/<j>`. */
	layer: string;
	/** False = `pack_not_applicable`: it matches none of the job's knowledge kinds. */
	applies: boolean;
}

/** KH T3 (D107) `pack_not_applicable`: a template pack reference no knowledge kind of the job matches. */
export interface NotApplicablePack {
	/** `stage|kind|pack_key` (stage / kind may be `*`). */
	key: string;
	stage: string;
	kind: string;
	/** `key@version`. */
	pin: string;
	/** The template layer that references it. */
	layer: string;
	code: 'pack_not_applicable';
}

// ---- P14 / J7 gates --------------------------------------------------------------------------------------------

export type GatePoint = 'P1' | 'P2' | 'R1';
export type GateType = 'noul' | 'choice' | 'score';
export type GateFail = 'open' | 'closed';
/** Library grouping: base pre-gate, base safety, post-gate rubric, and the industry/job waves; `tenant` = a question a
 *  tenant added to one job (never in the platform library). */
export type GateGroup = 'base' | 'safety' | 'post' | 'billing' | 'itsd' | 'utilities' | 'tenant';
/** Where in a turn the runtime asks a gate (`point`): P1 = the customer's request, P2 = before a tool
 *  (operation) call opens or changes a case, R1 = the draft response. */
export type GateRunPoint = 'request' | 'tool' | 'response';
/** How a question fires (`fire_when`; UX reference: care-bus-agents gates): `gte` / `lt` compare the answer's value (noul: p(true), choice:
 *  top confidence, score: level) with `threshold`; `in` fires when the chosen option is one of `fire_values`. */
export type GateFireWhen = 'gte' | 'lt' | 'in';
/** What a fired question does: hand the conversation to a person, block (redact / regenerate) the text, or only
 *  annotate the turn with the verdict (routing signal). */
export type GateAction = 'handover' | 'block' | 'annotate';
/** What the gate point decided for the turn. */
export type GateDecision = 'allow' | 'block' | 'handover';

export interface GateRule {
	fire_when: GateFireWhen;
	/** gte / lt: the line (0–1 probability, or a score level). null with `in`. */
	threshold: number | null;
	/** in: the option keys that fire. null with gte / lt. */
	fire_values: string[] | null;
	/** Below this confidence the answer is “unsure”: the question does not fire and its fail policy applies (fail
	 *  closed → a person takes over). null = any confidence. */
	min_confidence: number | null;
}

export interface GateOption {
	key: string;
	label: string;
	description: string;
	/** Produced at the release cut from the job's effective grid; read-only. */
	generated: boolean;
}

export interface GateQuestion {
	key: string;
	group: GateGroup;
	/** Template layer (`base`, `industry/utilities`, `job/it_service_desk_agent`). */
	layer: string;
	point: GatePoint;
	type: GateType;
	question: string;
	options: GateOption[];
	/** Where the generated options come from, shown instead of an editor. */
	generated_from: string | null;
	/** Threshold as written (`top ≥ 0.6`, `any true`). */
	threshold: string;
	/** Numeric part a tenant may tune; null = not tunable (noul / any true). */
	threshold_value: number | null;
	fail: GateFail;
	/** Safety gate: fail closed, never hideable, lock icon (D29). */
	safety: boolean;
	effect: string;
	/** The structured firing rule (gte / lt / in + minimum confidence). */
	rule: GateRule;
	/** What the question does when it fires. */
	action: GateAction;
	/** Tool point only: the tools (`object.verb`) it is asked for; null / empty = every tool. */
	tools: string[] | null;
	/** platform = from the gate library (template layers; cannot be deleted per job); tenant = added to this job. */
	origin: 'platform' | 'tenant';
	updated_by: Sub | null;
	updated_at: Iso;
	/** F-16 (B5 T8a): the question's own customer text when it hands over or blocks; absent / null = the care-bus
	 *  default. */
	customer_reply?: string | null;
}

export interface GateOptionRefinement {
	label?: string | null;
	description?: string | null;
	hidden?: boolean;
}

export interface EffGateOption extends GateOption {
	base_label: string;
	hidden: boolean;
	refined: boolean;
	/** Verdicts with this option as top answer, last 7 days. */
	verdicts: number;
}

export interface EffGateQuestion extends Omit<GateQuestion, 'options'> {
	options: EffGateOption[];
	base_threshold_value: number | null;
	threshold_refined: boolean;
	/** The template rule before the tenant's refinement. */
	base_rule: GateRule;
	/** The tenant changed the rule for this job (D54 draft until published). */
	rule_refined: boolean;
	stats: GateStats;
	/** F-17 (B5 T8a): asked in this job (D54 draft; a safety question is always on). */
	enabled: boolean;
	/** F-16 (B5 T8a): what the customer reads when this question hands over or blocks: the job's setting, else
	 *  `customer_reply_default`; null = the care-bus default. */
	customer_reply: string | null;
	/** The question's own text (library / tenant question); null = none. */
	customer_reply_default: string | null;
}

export interface GateStats {
	verdicts_7d: number;
	/** Fail-open events (gate unavailable, answer let through). */
	fail_open: number;
	/** Fail-closed events (gate unavailable, sent to a human). */
	fail_closed: number;
	below_threshold: number;
	latency_p50_ms: number | null;
	latency_p95_ms: number | null;
	timeouts: number;
	/** J7: top-answer confidence of choice gates in 20 buckets of 0.05 (0–0.05 … 0.95–1), last 7 days; lets the
	 *  page preview how many messages a threshold change would put below the line. Absent without traffic. */
	confidence?: number[];
}

/** J7: where a tenant refinement stands (D54): live = in the applied release; approved = in the approved, not yet
 *  applied release; next_cut = saved to draft, testable in the test chat, published by the next release cut. */
export type GateChangeShips = 'live' | 'approved' | 'next_cut';

/** J7: one tenant refinement of a gate, field by field, with the release it ships in. */
export interface GateChange {
	gate: string;
	/** null = question-level (threshold). */
	option: string | null;
	field: 'label' | 'description' | 'hidden' | 'threshold' | 'rule' | 'question';
	/** Template value (base label, base threshold, `offered`). */
	from: string;
	to: string;
	by: Sub;
	by_name: string;
	at: Iso;
	/** live = in the applied release; approved = in the approved, not yet applied release; next_cut = draft. */
	ships: GateChangeShips;
	release_id: string | null;
}

/** J7: the releases a refinement can be in. */
export interface JobGateReleases {
	live: { id: string; released_at: Iso } | null;
	approved: { id: string; approved_at: Iso } | null;
}

export interface JobGates {
	questions: EffGateQuestion[];
	/** Since when the stats count (`2026-10-11T00:00:00Z`), or null without traffic. */
	window_from: Iso | null;
	/** J7: this job's tenant refinements of its gates, newest first. */
	changes?: GateChange[];
	/** J7: the applied and the approved release of this job. */
	releases?: JobGateReleases;
}

export interface GateQuestionEdit {
	question?: string;
	threshold_value?: number | null;
	generated?: boolean;
	/** P14: the template rule (safety gates keep theirs). */
	rule?: GateRule;
	/** F-16: the question's own customer text; null clears it (the care-bus default). */
	customer_reply?: string | null;
}

/** F-24 `POST /api/platform/gates` — a new library question in one template layer. */
export interface GateQuestionDraft {
	key: string;
	layer: string;
	group: GateGroup;
	point: GatePoint;
	type: GateType;
	question: string;
	options: { key?: string; label: string; description: string }[];
	threshold: string;
	threshold_value?: number | null;
	rule: GateRule;
	fail?: GateFail;
	action?: GateAction;
	effect?: string;
	tools?: string[] | null;
	customer_reply?: string | null;
}

/** F-24 `POST /api/platform/gates/{key}/retire` — the tombstoned question and the jobs still pinned to a tag holding it. */
export interface GateRetired {
	key: string;
	layer: string;
	rev: number;
	retired: true;
	jobs: Record<string, unknown>[];
}

/** `POST /api/jobs/{job}/gates` — a tenant question for one job (D54 draft). noul only: laya reads the two criteria. */
export interface GateQuestionInput {
	key: string;
	point: GatePoint;
	question: string;
	/** The true / false criteria laya answers against. */
	criteria: { true: string; false: string };
	rule: GateRule;
	action: GateAction;
	fail: GateFail;
	tools?: string[] | null;
	/** F-16 (B5 T8a): the question's own customer text, 1–500 characters; absent / null = the care-bus default. */
	customer_reply?: string | null;
}

/** `POST /api/jobs/{job}/gates/evaluate` — try a message against this job's gates. */
export interface GateEvalInput {
	point: GateRunPoint;
	text: string;
	/** Tool point only: the tool the worker would call; only questions covering it are asked. */
	tool?: string;
	/** Which design to run (D54). Default: the draft when it differs from live, else live. */
	against?: 'live' | 'draft';
}

/** One question's answer in an evaluation. */
export interface GateEvalAnswer {
	gate: string;
	question: string;
	type: GateType;
	/** The answer as a label (`true`, `Issue`, `4 · High`). */
	answer: string;
	/** Option key of the answer (`true`, `issue`, `4`). */
	value: string;
	/** Per-option probability, highest first. */
	probabilities: { key: string; label: string; p: number }[];
	confidence: number;
	rule: GateRule;
	fail: GateFail;
	safety: boolean;
	/** The rule fired: `action` applies. */
	fired: boolean;
	/** Confidence below `rule.min_confidence`: the fail policy applied instead. */
	unsure: boolean;
	/** What this answer did to the turn (fired → its action; unsure + fail closed → handover; else null). */
	action: GateAction | null;
}

export interface GateEvaluation {
	point: GateRunPoint;
	gate_point: GatePoint;
	/** The design the evaluation ran (D54). */
	ran_against: 'live' | 'draft';
	/** The draft's gates differ from the live release's. */
	draft_differs: boolean;
	/** The applied release (live), or null. */
	release_id: string | null;
	decision: GateDecision;
	/** Gate keys that decided (fired with handover / block, or failed closed). */
	deciding: string[];
	answers: GateEvalAnswer[];
	/** Questions at this point not asked, with why (tool not covered, options built per case). */
	skipped: { gate: string; reason: string }[];
	latency_ms: number;
}

/** `GET /api/jobs/{job}/gates/verdicts?limit=` — one question's verdict at one gate point of a case turn. */
export interface GateVerdictRow {
	id: string;
	at: Iso;
	point: GatePoint;
	gate: string;
	question: string;
	answer: string;
	confidence: number;
	/** Below the question's line (asked again / fell back). */
	below_threshold: boolean;
	/** What the gate point decided for that turn. */
	action: GateDecision;
	case_id: string;
	conversation_id: string;
}

export interface GateVerdictFilter {
	point?: GatePoint | 'all';
	action?: GateDecision | 'all';
	gate?: string | 'all';
	/** Only verdicts below the line. */
	unsure?: boolean;
	q?: string;
}

/** P14: one job that asks a library gate (`GET /api/platform/gates/usage`). */
export interface GateJobUse {
	tenant_id: string;
	tenant_name: string;
	job: string;
	job_label: string;
	/** Tenant refinements on this gate in this job (relabel / hide / threshold). */
	refinements: number;
	/** Verdicts in the last 7 days, or null without traffic. */
	verdicts_7d: number | null;
}

export interface GateUsage {
	gate: string;
	jobs: GateJobUse[];
}

// ---- P15 agent roles / J8 toolsets ------------------------------------------------------------------------------

export type AgentRoleKey = 'qualifier' | 'procedure' | 'post_gate' | 'assistant';

export interface AgentRoleGrant {
	object: string;
	verb: string;
	variant: string | null;
	modalities: Modality[];
	/** Procedure role: the grant is the activity's operations, never hand-authored. */
	derive_from_activity: boolean;
	/** D119: derived from the job's operation expectations (`deriveRoleGrants`), not a stored grant. */
	derived?: boolean;
}

export interface AgentRole {
	/** Job template the role belongs to (`customer_care_agent`). */
	job: JobKey;
	role: AgentRoleKey;
	label: string;
	used_by: string;
	max_side_effect: SideEffect;
	enabled: boolean;
	grants: AgentRoleGrant[];
	note: string;
	updated_by: Sub | null;
	updated_at: Iso;
}

/** P15 row: the role plus, for `procedure`, the grants it derives per activity of the job template. */
export interface AgentRoleView extends AgentRole {
	derived: { activity: string; label: string; ops: string[] }[];
}

export interface CatalogueOperation {
	/** `object.verb` */
	op: string;
	variant: string | null;
	modality: Modality;
	side_effect: SideEffect;
	identity_level: string;
}

export type RouteTarget =
	| { kind: 'instance'; instance: string }
	| { kind: 'builtin' }
	| { kind: 'fallback'; instance: 'agent_cases' }
	| { kind: 'deep_link'; url: string }
	| { kind: 'not_offered' }
	| { kind: 'unrouted' };

export type Env = 'prod' | 'test';

export interface ToolsetRow {
	op: string;
	variant: string | null;
	side_effect: SideEffect;
	modality: Modality;
	/** N2: the activity expects it, R(equired) or O(ptional); null = the role grants it but the activity does not. */
	expected: 'R' | 'O' | null;
	/** N3: the role grants it (before the tenant narrowing). */
	granted: boolean;
	/** The tenant narrowed it away (refinement). */
	narrowed: boolean;
	/** N4: the route for this env. */
	route: RouteTarget;
	/** N2 ∩ N3 ∩ N4: the agent may call it. */
	effective: boolean;
	/** Why not, when not effective. */
	reason: string | null;
}

export interface ActivityRef {
	key: string;
	stage: string;
	label: string;
	hidden: boolean;
	origin: Origin;
}

export interface Toolset {
	role: AgentRoleKey;
	role_enabled: boolean;
	activity: string;
	env: Env;
	rows: ToolsetRow[];
}

/** J8 role (F-40, B5 T11a): the role at the design's pins plus the job's switch. `enabled` is the effective switch
 *  (`enabled_job` ∧ `enabled_platform`). */
export interface ToolsetRole extends AgentRole {
	/** The job's switch (`role_setting`); a role a job cannot configure is always on here. */
	enabled_job: boolean;
	/** The platform `agent_role.enabled` at the design's pins. */
	enabled_platform: boolean;
	/** Tool calls per assistant turn; null = the runtime default (6). */
	max_tool_calls: number | null;
	/** A5: seconds one assistant turn may run (5..600); null = the runtime default (60). */
	turn_budget_s: number | null;
	/** A5: the job's own assistant prompt; null = the default for the job's audience. */
	system_prompt: string | null;
	/** A5: a turn on a conversation with an open case continues that case. */
	case_per_thread: boolean;
	/** May the job switch this role (only `assistant` today)? */
	configurable: boolean;
}

/** Bounds of `ToolsetRole.max_tool_calls` (the server's 0148 CHECK). */
export const MAX_TOOL_CALLS = 6;
/** Bounds of `ToolsetRole.turn_budget_s` (0233 CHECK) and the length of `system_prompt`. */
export const TURN_BUDGET_S_MIN = 5;
export const TURN_BUDGET_S_MAX = 600;
export const SYSTEM_PROMPT_MAX = 4000;

/** A5 per-job runtime policy of a role; a field left out keeps the stored value, null resets it. */
export interface RolePolicy {
	turnBudgetS?: number | null;
	systemPrompt?: string | null;
	casePerThread?: boolean;
}

export interface ToolsetMeta {
	roles: ToolsetRole[];
	activities: ActivityRef[];
	/** Tenant narrowings: `role|activity|op{variant}`. */
	narrowings: string[];
}

/** J8 overview: one procedure step as the toolset sees it (the op it calls, and the N5 per-call checks). */
export interface ToolsetProcedureStep {
	step_number: number;
	name: string;
	/** `object.verb` */
	operation: string;
	variant: string | null;
	side_effect: SideEffect;
	identity_level: string;
	confirm_back: boolean;
}

/** J8 overview: the procedure the orchestrator runs (as role `procedure`) for one activity. */
export interface ToolsetActivityProcedure {
	activity: string;
	context_space: string;
	name: string;
	version: number;
	origin: Origin;
	steps: ToolsetProcedureStep[];
}

/** J8 overview: one tenant narrowing with its provenance. */
export interface ToolsetNarrowing {
	role: AgentRoleKey;
	activity: string;
	/** `object.verb{variant}` */
	op: string;
	by: Sub;
	at: Iso;
}

/** J8 overview: every role × activity toolset for one env, the procedures that use them, and the narrowings. */
export interface ToolsetOverview {
	env: Env;
	toolsets: Toolset[];
	procedures: ToolsetActivityProcedure[];
	narrowings: ToolsetNarrowing[];
}
