// Shapes for the "model" area: template tags (P6/P7), the canonical object model (P9/P10) and the operations
// catalogue (P11). Field names follow spec 14 §3 and spec 11 §6.1 (`template_tag`, `object_def`, `object_field`,
// `object_relation`, `object_state`, `object_transition`, `operation_def`, `error_state_def`), snake_case as the
// API will send them.
import type { Iso, JobKey, Sub, TenantId, TenantStatus } from './core';

// ---------------------------------------------------------------------------------------------------------------
// Templates (D15, 11 §4.1, §6.1 template_tag)

export type TemplateScope = 'base' | 'industry' | 'base_dept' | 'industry_dept' | 'job';

/** Template rollout order (D26): customer care first, then IT service desk, then the other job areas. */
export type RolloutWave = 1 | 2 | 3;

/** What a tag holds, one row per authored item. `kind` is the overlay kind (05 §2.1, 11 §6.2, 12 §8.2). */
export type TagItemKind =
	// ontology + job content (§3: compiled into the effective model / `job_content`)
	| 'entity'
	| 'attribute'
	| 'relation'
	| 'job'
	| 'context_space'
	| 'procedure'
	| 'connector_slot'
	| 'knowledge_store'
	// catalogue (§3: the object model's operations and error taxonomy) + department archetypes (D60/D107)
	| 'operation'
	| 'error_state'
	| 'department_archetype'
	| 'activity_object'
	| 'object'
	| 'profile'
	| 'axis_rule'
	| 'journey_theme'
	| 'journey_stage'
	| 'theme_stage_prior'
	| 'stage_knowledge_kind'
	| 'activity'
	| 'operation_expectation'
	| 'gate_question'
	| 'agent_role'
	| 'job_setting'
	/** PK-1: a template layer's reference to a strata platform pack (key `<stage>|<kind>|<pack_key>`, `{version}`). */
	| 'knowledge_pack'
	/** KH T4 (D108): one prompt slot (key `identity|policy|tone|safety|escalation|format`, `{text, language?, mode?}`). */
	| 'instruction'
	/** B8 T1: a channel's status (key = channel; base/industry). */
	| 'journey_channel'
	/** B8 T1: a stage out of scope for a department (key = stage). */
	| 'stage_scope'
	/** B8 T1: an activity's use-case card and scenarios (key `stage:activity`; never base). */
	| 'use_case_card';

export interface TagItem {
	kind: TagItemKind;
	key: string;
	label: string;
	/** The item's authored content in one line (what a diff compares). */
	detail: string;
	/** D111: the region of the region sub-layer the item comes from (`nl`); its own item, never the parent's. */
	region?: string;
}

/** An untagged overlay revision waiting for the next `@vN` (the generator's Package step, or a hand edit). */
export interface DraftRevision {
	rev: number;
	op: 'add' | 'patch' | 'tombstone';
	item: TagItem;
	by: Sub;
	by_name: string;
	at: Iso;
}

/** One `template_tag` row: immutable once written (trigger); `v0` is a generator's provisional tag. */
export interface TemplateTag {
	layer_key: string;
	version: number;
	/** `sha256:` of the tag's resolved content (first 12 hex shown). */
	content_hash: string;
	tagged_by: Sub;
	tagged_by_name: string;
	tagged_at: Iso;
	note: string;
	/** v0 tags written by a generator run that has not passed validation yet: not immutable, cannot be cut. */
	provisional: boolean;
	item_count: number;
}

/** A job's pin on a layer (`job_template_pin`), joined with its tenant. */
export interface TemplatePinRow {
	tenant_id: TenantId;
	tenant_name: string;
	tenant_status: TenantStatus;
	job: JobKey;
	job_label: string;
	version: number;
	/** Latest tag of the layer minus the pinned version (0 = on the latest). Feeds the K5 base-changed badge. */
	behind: number;
}

/** P6 row: one template layer with its tags, draft revisions and pins. */
export interface TemplateLayerSummary {
	layer_key: string;
	scope: TemplateScope;
	label: string;
	industry: string | null;
	rollout_wave: RolloutWave | null;
	rollout_label: string | null;
	latest: TemplateTag | null;
	tags: TemplateTag[];
	draft_revisions: DraftRevision[];
	/** Set while a generator run is producing this layer (Northwind run #7). */
	generator: { run: string; step: string; validation_red: string[] } | null;
	pins: TemplatePinRow[];
	pinned_jobs: number;
	behind_jobs: number;
}

/** "Newer tag than pin" per tenant (P6 card; the same count drives a tenant's K5 badge). */
export interface TenantPinLag {
	tenant_id: TenantId;
	tenant_name: string;
	tenant_status: TenantStatus;
	behind: { job: JobKey; layer_key: string; pinned: number; latest: number }[];
}

export interface TemplateCatalog {
	layers: TemplateLayerSummary[];
	tenant_lag: TenantPinLag[];
}

export type DiffChange = 'added' | 'changed' | 'removed';

export interface TagDiffRow {
	kind: TagItemKind;
	key: string;
	label: string;
	change: DiffChange;
	before: string | null;
	after: string | null;
	/** D111: the region of a region sub-layer item (Rust `TagDiffRow::region`). */
	region?: string;
}

/** P7: one tag, its contents, what changed since the previous tag, who pins the layer. */
/** Result of retiring / restoring a job template tag. */
export interface TemplateRetireResult {
	layer_key: string;
	version: number;
	retired: boolean;
}

export interface TemplateTagDetail {
	layer: Omit<TemplateLayerSummary, 'tags' | 'pins' | 'draft_revisions'>;
	tag: TemplateTag;
	/** Every tag of the layer, oldest first (compare picker, prev/next). */
	versions: number[];
	previous: number | null;
	items: TagItem[];
	diff_vs_previous: TagDiffRow[];
	pins: TemplatePinRow[];
	pinned_on_this_tag: number;
	/** P7: every tag of the layer, oldest first, with note / date / hash (the version timeline). */
	tags?: TemplateTag[];
	/** P7: untagged revisions waiting for the next `@vN` (the Package step on P6). */
	draft_revisions?: DraftRevision[];
	/** P7: the layers a job composes in order base → industry → department → job, this one marked `here`. */
	stack?: TagStackLayer[];
	/** P7: layer keys that compose on top of this one (they inherit what this tag contributes). */
	composed_on_by?: string[];
	/** P7: per pinned version, how much changes when that pin moves to the latest tag. */
	catch_up?: TagCatchUp[];
	/** Versions of this layer a super_admin retired (hidden from new jobs). */
	retired_versions?: number[];
}

/** P7: one layer of the composition stack a tag sits in. */
export interface TagStackLayer {
	layer_key: string;
	scope: TemplateScope;
	label: string;
	latest: number | null;
	here: boolean;
}

/** P7: the diff a pin on `from` takes when it is re-pointed to `to` (the latest tag) and cut. */
export interface TagCatchUp {
	from: number;
	to: number;
	added: number;
	changed: number;
	removed: number;
}

// ---------------------------------------------------------------------------------------------------------------
// Objects (D19–D21, D25, 11 §1–§2, §6.1)

export type ObjectTier = 'base' | 'industry' | 'optional';

export type FieldType =
	| 'id'
	| 'string'
	| 'text'
	| 'enum'
	| 'money'
	| 'int'
	| 'decimal'
	| 'bool'
	| 'date'
	| 'datetime'
	| 'address'
	| 'ref'
	| 'list'
	| 'json'
	| 'url';

export interface ObjectField {
	field: string;
	data_type: FieldType;
	/** Personal data: masked in logs and transcripts, never in a post-gate quote. */
	pii: boolean;
	/** Allowed values for `enum` fields (or the enum ref name for axis-backed fields). */
	enum_values: string[];
	/** `ref` / `list` target, e.g. `party` or `order_line`. */
	ref: string | null;
	required: boolean;
	description: string;
	/** F-29: retired (kept for history and older tags; no new operation may name it). */
	retired?: boolean;
}

export interface ObjectRelation {
	subject: string;
	predicate: string;
	object: string;
	cardinality: '1:1' | '1:0..1' | '1:n' | 'n:1' | 'n:m';
}

export interface ObjectState {
	status: string;
	label: string;
	/** No way out. */
	terminal: boolean;
	/** Part of the canonical status set connectors map onto (not a connector-only extra). */
	canonical: boolean;
	initial: boolean;
}

export interface ObjectTransition {
	/** `order.cancel`; backend events are written `backend:<event>` and are not operations. */
	verb: string;
	from_status: string;
	to_status: string;
	guard_rule: string | null;
}

export interface ObjectDef {
	object: string;
	label: string;
	tier: ObjectTier;
	/** D20: `arrangement` is stored as a Contract specialisation. */
	storage_shape_of: string | null;
	/** Spec 11 §2.1 candidate label (M1–M12) or the owner file it comes from. */
	origin: string;
	description: string;
	/** One of the 14 keys the retail customer-care job uses (11 §4.3.5). */
	in_retail_job: boolean;
	fields: ObjectField[];
	relations: ObjectRelation[];
	states: ObjectState[];
	transitions: ObjectTransition[];
}

export interface ObjectSummary {
	object: string;
	label: string;
	tier: ObjectTier;
	storage_shape_of: string | null;
	origin: string;
	description: string;
	in_retail_job: boolean;
	field_count: number;
	pii_count: number;
	state_count: number;
	transition_count: number;
	operation_count: number;
}

/** A concept that is deliberately NOT an object (11 §2.1 "Folded"). */
export interface FoldedConcept {
	concept: string;
	becomes: string;
	object: string;
	cite: string;
}

export interface ObjectCatalog {
	objects: ObjectSummary[];
	folded: FoldedConcept[];
}

/** Derived per verb (11 §3.2, §6.1): every state with no transition row for the verb yields `invalid_state:<s>`. */
export interface VerbStates {
	verb: string;
	allowed_from: string[];
	invalid_states: string[];
}

/** An edit to the platform model not yet carried by a tag/cut (banner on P10). */
export interface PendingModelEdit {
	object: string;
	what: string;
	by: Sub;
	by_name: string;
	at: Iso;
}

export interface ObjectDetail extends ObjectDef {
	operations: OperationRow[];
	verbs: VerbStates[];
	pending_edits: PendingModelEdit[];
}

// ---------------------------------------------------------------------------------------------------------------
// Operations (D22, D27, K48, K49, 11 §3.2, 12 §8.1)

export type Modality = 'observe' | 'do' | 'request';
/** 0 public · 1 identified · 2 verified · 3 step-up · S staff. */
export type IdentityLevel = '0' | '1' | '2' | '3' | 'S';
/** R read · W reversible · X irreversible/financial/identity · P payment. */
export type SideEffect = 'R' | 'W' | 'X' | 'P';
export type Idempotency = 'nat' | 'key' | 'key+dup';
export type OpKind = 'standard' | 'hosted_payment_link' | 'builtin';

/** The columns a variant row may override (NULL = inherit from `*`). */
export interface OperationAttrs {
	modality: Modality;
	is_derive: boolean;
	identity_level: IdentityLevel;
	side_effect: SideEffect;
	idempotency: Idempotency;
	input_fields: string[];
	output_fields: string[];
	confirm_back: boolean;
	verify_read: string | null;
	builtin: boolean;
	op_kind: OpKind;
	guard_rules: string[];
	/** Spec 17 F-22: the level a verify-type operation raises the case to (absent/null = raises nothing). */
	raises_identity_level?: IdentityLevel | null;
}

/** Stored `operation_def` row: `*` holds every column; a variant row only what it overrides. */
export interface OperationDefRow {
	object: string;
	verb: string;
	variant: string;
	discriminator: { field: string; value: string } | null;
	attrs: Partial<OperationAttrs>;
	description: string;
	/** The retail customer-care job's expectation (11 §4.3.5): R required, O optional, null not expected. */
	retail: 'R' | 'O' | null;
	stages: string[];
	/** Errors the generic rules cannot know (e.g. `outside_hours` on a handover). */
	extra_errors: string[];
}

/** Resolved row as the release cut copies it (variant merged with `*`). */
export interface OperationRow extends OperationAttrs {
	/** `order.cancel`, `case.create{return}`. */
	key: string;
	object: string;
	verb: string;
	variant: string;
	discriminator: { field: string; value: string } | null;
	/** Columns this variant takes from `*` (empty for a `*` row). */
	inherited: (keyof OperationAttrs)[];
	/** Columns this variant sets itself. */
	overridden: (keyof OperationAttrs)[];
	description: string;
	retail: 'R' | 'O' | null;
	stages: string[];
	/** Generated: common taxonomy + `invalid_state:<s>` from the state machine + `ineligible:<rule>`. */
	error_states: string[];
}

export interface OperationFilter {
	object?: string;
	modality?: Modality;
	side_effect?: SideEffect;
	identity_level?: IdentityLevel;
	builtin?: boolean;
	retail_only?: boolean;
	q?: string;
}

export interface ErrorStateDef {
	code: string;
	user_facing: boolean;
	retryable: boolean;
	escalate: boolean;
	description: string;
}

// ---------------------------------------------------------------------------------------------------------------
// Writes

/** Start an object in a template layer; add fields and lifecycle in its detail editor. */
export interface NewObjectInput {
	object: string;
	label: string;
	tier: ObjectTier;
	description: string;
	layer_key: string;
}

export type CreatedObject = ObjectDef & { layer_key: string; rev: number };

/** A new connector-routed operation. The server enforces contract I/O and identity floors. */
export interface NewOperationInput extends OperationEdit {
	object: string;
	verb: string;
	variant?: string;
	layer_key: string;
	modality: Modality;
	op_kind: Exclude<OpKind, 'builtin'>;
	discriminator?: { field: string; value: string };
}

export interface FieldInput {
	field: string;
	data_type: FieldType;
	pii: boolean;
	enum_values: string[];
	ref: string | null;
	required: boolean;
	description: string;
}

/** F-22 `PUT /api/platform/operations/{object}/{verb}?variant=` — the editable columns of one operation row. */
export interface OperationEdit {
	identity_level: IdentityLevel;
	raises_identity_level?: IdentityLevel | null;
	side_effect: SideEffect;
	confirm_back: boolean;
	verify_read: string | null;
	idempotency: Idempotency;
	input_fields: string[];
	output_fields: string[];
	description: string;
	extra_errors: string[];
}

export interface StateInput {
	status: string;
	label: string;
	terminal: boolean;
	canonical: boolean;
}

export interface TransitionInput {
	verb: string;
	from_status: string;
	to_status: string;
	guard_rule: string | null;
}

// ---------------------------------------------------------------------------------------------------------------
// P9 object map (11 §1.3 relations, §2.1 tiers, §3.1 modalities)

/** A template layer that carries an object item (the object enters a job's model through this layer). */
export interface ObjectLayerRef {
	layer_key: string;
	scope: TemplateScope;
	label: string;
	/** Tag version the item first appears in. */
	since: number;
}

/** One node of the P9 object map: identity, lifecycle and what operations can do to it. */
export interface ObjectMapNode {
	object: string;
	label: string;
	tier: ObjectTier;
	origin: string;
	description: string;
	in_retail_job: boolean;
	storage_shape_of: string | null;
	/** Ordered as the state machine declares them. */
	states: Pick<ObjectState, 'status' | 'initial' | 'terminal'>[];
	transition_count: number;
	/** Operation keys on this object (resolved rows, variants included). */
	operations: { key: string; modality: Modality; side_effect: SideEffect }[];
	/** Layers carrying this object, broadest scope first (base → industry → department → job). */
	layers: ObjectLayerRef[];
	pii_count: number;
	field_count: number;
}

/** `GET /api/platform/objects/map` — every object with its lifecycle and operations, plus all relations. */
export interface ObjectModelMap {
	nodes: ObjectMapNode[];
	relations: ObjectRelation[];
}

// ---------------------------------------------------------------------------------------------------------------
// P10 object context: where one object lives outside its own rows (layers, connector templates, journey activities)

/** A template-layer item that defines the object (`object` row) or references it (`order.cancel`, `object delivery`). */
export interface ObjectLayerTouch {
	layer_key: string;
	scope: TemplateScope;
	layer_label: string;
	latest_tag: number | null;
	role: 'defines' | 'references';
	kind: TagItemKind;
	key: string;
	label: string;
	/** The item's detail at the latest tag (or the draft's, when pending). */
	detail: string;
	/** First tag carrying the item. */
	since: number;
	/** Tags at which its detail changed. */
	changed_in: number[];
	/** Last tag carrying the item, when it was removed. */
	removed_after: number | null;
	/** An untagged draft revision touches it. */
	pending: boolean;
}

/** A connector template (newest version) implementing operations on the object, with its status mapping. */
export interface ObjectSystemHolder {
	template_key: string;
	version: string;
	label: string;
	vendor: string;
	system_class: string;
	platform_connector: boolean;
	/** `order.read`, `case.create{return}`. */
	ops: string[];
	/** System status → canonical status (one row per distinct system status). */
	status_map: { system: string; canonical: string }[];
	/** Tenant instances of the template. */
	instances: {
		id: string;
		label: string;
		tenant_id: TenantId;
		tenant_name: string;
		env: string;
		stub: boolean;
	}[];
}

/** A journey activity that binds the object or expects an operation on it (11 §6.2 `activity_object`). */
export interface ObjectActivityTouch {
	industry: string;
	industry_label: string;
	stage: string;
	key: string;
	label: string;
	actor: string;
	role: 'subject' | 'context';
	/** `{return}` when the activity binds a variant. */
	variant: string | null;
	fields: string[];
	operations: { op: string; required: boolean }[];
}

export interface ObjectContext {
	object: string;
	layers: ObjectLayerTouch[];
	systems: ObjectSystemHolder[];
	activities: ObjectActivityTouch[];
}
