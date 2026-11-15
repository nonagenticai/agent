// Shapes for the "journey" area: the journey grid (P8, J4, J5), the discovery generator (P18), the validation grid
// (J17), discovery import (J18) and the handover queue (J21). Field names follow spec 14 §3 and spec 11 §6.2
// (snake_case, as the API will send them).
import type { Iso, JsonObject, JsonValue, JobKey, ReleaseRef, Role, Sub, TenantId } from './core';
import type { Funding } from './tenant';

/** The seven D23 stages, in order. Change and Exit labels are owner-vetoable (K48). */
export type StageKey =
	'orientation' | 'investigation' | 'purchase' | 'delivery' | 'change' | 'support' | 'exit';

export type ThemeKey =
	'need' | 'interest' | 'choice' | 'awaiting' | 'elated' | 'issue' | 'issue_resolved' | 'need_change';

export type Actor = 'customer' | 'company' | 'employee';

/** `journey_theme` (11§6.2): the customer state, compiled into GT options. */
export interface JourneyTheme {
	key: ThemeKey;
	label: string;
	order: number;
	/** 0–1: how strongly the theme raises escalation (Issue / Need Change high). */
	escalation_weight: number;
}

/** `journey_stage` (11§6.2): the capability column, compiled into GS options. */
export interface JourneyStage {
	key: StageKey;
	label: string;
	order: number;
	description: string;
	synonyms: string[];
	/** Marked out of scope for this industry/job (rule 4 then passes without an activity). */
	out_of_scope: boolean;
	/** Which themes the slide draws above this column (`Need → Interest`). */
	themes: ThemeKey[];
}

/** `theme_stage_prior` (11§6.2): weight 0–1, the GS prior. */
export interface ThemeStagePrior {
	theme: ThemeKey;
	stage: StageKey;
	weight: number;
}

/** Slide channels (11§4.3.3): chat is v1; email and contact forms later; phone, visit, signing out of scope. */
export type ChannelKey = 'chat' | 'email' | 'contact_form' | 'phone' | 'visit' | 'signing';
export type ChannelStatus = 'v1' | 'later' | 'out_of_scope';
export interface JourneyChannel {
	key: ChannelKey;
	label: string;
	status: ChannelStatus;
	note: string;
}

/** `activity_object` (11§6.2): an object an activity reads or writes, with its field subset and required slots. */
export interface ActivityObject {
	/** Canonical object key (`order`), optionally with a variant (`case{return}`). */
	object: string;
	role: 'subject' | 'context';
	fields: string[];
	required_inputs: string[];
}

/** `operation_expectation` (11§6.2): `object.verb{variant}` with R(equired) / O(ptional). */
export interface OperationExpectation {
	/** `order.read`, `case.create` */
	op: string;
	variant: string | null;
	required: boolean;
}

/** `activity` (11§6.2): key `stage:activity`. */
export interface JourneyActivity {
	key: string;
	stage: StageKey;
	actor: Actor;
	label: string;
	trigger: string;
	also_in_stages: StageKey[];
	objects: ActivityObject[];
	operations: OperationExpectation[];
	/** The technical form (actor, operations) as authored; carried unchanged through every designer save. */
	summary?: string;
	/** The plain line a customer or employee would recognise ("Change something about an order that has not shipped
	 * yet"): the description of the activity's option in the activity gate. Editable; absent = none. */
	description?: string;
	/** D61: the department area (D60) the activity belongs to — the middle part of its context-space key. Absent on
	 * an activity no area claims yet (a fresh tenant extension). */
	area?: string;
	/** Owner 2026-10-26: the template marks the activity required — a tenant cannot hide it. Absent = false. */
	required?: boolean;
}

/** Per stage × channel: example customer messages (Messages row). */
export interface StageMessage {
	stage: StageKey;
	channel: ChannelKey;
	utterances: string[];
}

/** `stage_knowledge_kind` (11§6.2): what the tenant's strata_rag store must hold for a stage. */
export interface StageKnowledgeKind {
	stage: StageKey;
	kind: string;
	required: boolean;
	description: string;
}

/** F-44: the editable fields of a use case card. A tenant refinement carries only what changed (`null` drops a field
 * back to the template's); the platform template editor also keeps `scenarios` (B8-4), which a tenant never refines. */
export type UseCaseCardPatch = {
	summary?: string | null;
	examples?: string[] | null;
	metric?: { key: string; label: string; target: string; current?: string | null } | null;
	sample_dialogue?: DialogueTurn[] | null;
};

/** F-45: a new stage; `after` places it behind that stage (default: last). */
export interface NewJourneyStage {
	key: string;
	label: string;
	description: string;
	after?: StageKey;
}

/** F-46: a new theme; `escalation_weight` is a whole number 0–100 on the wire (the grid reads it back as 0–1). */
export interface NewJourneyTheme {
	key: string;
	label: string;
	escalation_weight: number;
}

/** F-47: one stage knowledge kind; `layer` is the layer key it is written at (default: the department layer). */
export interface StageKnowledgeKindInput {
	store_key: string;
	required: boolean;
	tenant_data?: boolean;
	layer?: string;
}

/** F-42: R(equired) or O(ptional). */
export type OperationExpectationKind = 'R' | 'O';

export type JourneyStatus = 'tagged' | 'partial' | 'draft';

/** `GET /api/platform/journeys` row. */
export interface IndustryJourneySummary {
	department?: string | null;
	department_label?: string | null;
	dept_tag?: string | null;
	industry: string;
	label: string;
	status: JourneyStatus | 'not_started';
	/** `industry/retail@v2` (null when never tagged). */
	tag: string | null;
	activities: number;
	red: number;
}

/** A validation finding (11§7 rules 1–7). `red` blocks tagging. */
export interface ValidationFinding {
	rule: number;
	/** `info` = information only (rule 7): never blocks. */
	severity: 'red' | 'amber' | 'info';
	message: string;
	/** Stage or activity the finding points at (for the click-through). */
	stage: StageKey | null;
	activity: string | null;
}

/** `GET /api/platform/journeys/{industry}` — the owner's grid for one industry (+ department). */
export interface IndustryJourney {
	industry: string;
	label: string;
	department: string;
	status: JourneyStatus;
	tag: string | null;
	dept_tag: string | null;
	/** Draft revision not yet tagged (journey content edited since the tag). */
	draft_rev: number;
	updated_at: Iso;
	updated_by: Sub;
	themes: JourneyTheme[];
	stages: JourneyStage[];
	priors: ThemeStagePrior[];
	channels: JourneyChannel[];
	activities: JourneyActivity[];
	messages: StageMessage[];
	knowledge_kinds: StageKnowledgeKind[];
	/** Canonical objects of the industry's business model (rule 3 checks each is used). */
	model_objects: string[];
	/** Computed on read. */
	findings: ValidationFinding[];
	/** Where the content came from when it is a generator draft. */
	source_run: number | null;
}

// ---- job journey (J4, J5) ---------------------------------------------------------------------------------------

export type RefinementState = 'base' | 'hidden' | 'extension' | 'relabelled';

export interface JobJourneyActivity extends JourneyActivity {
	refinement: RefinementState;
	/** Label from the template when relabelled. */
	base_label: string | null;
	/** Refinement not yet in a release cut. */
	pending: boolean;
	/** J4: required operations of this activity and those without a route in some env (`case.read · test`). */
	routing?: { required: number; unrouted: string[] };
}

/** J4: live outcome of one activity from the runtime (released config, prod). */
export interface ActivityOutcome {
	activity: string;
	conversations: number;
	/** Resolved by the agent without a person. */
	contained: number;
	/** Handed over to staff. */
	escalated: number;
}

/** A stage's knowledge kind joined with the tenant's bound store. */
export interface JobKnowledgeKind extends StageKnowledgeKind {
	store: string | null;
	/** The store holds documents of this kind. */
	present: boolean;
}

/** `GET /api/jobs/{job}/journey` — the effective grid (template + tenant refinements). */
export interface JobJourney {
	job: JobKey;
	industry: string;
	industry_label: string;
	/** `industry/retail@v2 · industry_dept/retail/customer_service@v2 · job/customer_care_agent@v3` */
	pins: string[];
	release: ReleaseRef | null;
	stages: JourneyStage[];
	themes: JourneyTheme[];
	channels: JourneyChannel[];
	activities: JobJourneyActivity[];
	messages: StageMessage[];
	knowledge_kinds: JobKnowledgeKind[];
	/** Refinement edits since the current release (conflicts are flagged at the next cut). */
	pending: PendingRefinement[];
	findings: ValidationFinding[];
	/** J4: the template's theme → stage priors (the GS gate prior), inherited read-only. */
	priors?: ThemeStagePrior[];
	/** J4: live outcomes per activity over `window`; null when the job has no live traffic yet. */
	outcomes?: { window: string; rows: ActivityOutcome[] } | null;
}

/** A journey refinement saved since the current release. */
export interface PendingRefinement {
	at: Iso;
	by: Sub;
	what: string;
	activity: string | null;
}

export type RouteKind = 'instance' | 'builtin' | 'fallback' | 'deep_link' | 'not_offered' | 'unrouted';

export interface OperationRouteInfo {
	env: 'prod' | 'test';
	kind: RouteKind;
	/** Instance name, `agent_cases`, deep-link URL … (null when unrouted). */
	target: string | null;
	contract_tested: boolean;
}

/** One operation row on the activity detail: catalogue attributes + R/O + its routes per env. */
export interface ActivityOperation extends OperationExpectation {
	modality: 'do' | 'request' | 'observe';
	side_effect: 'R' | 'W' | 'X' | 'P';
	identity_level: string;
	routes: OperationRouteInfo[];
}

export interface DialogueTurn {
	from: 'customer' | 'agent' | 'system';
	text: string;
}

/** `GET /api/jobs/{job}/journey/activities/{key}` — the use case (11§4.1). */
/** J5: a success/error JSON example of an activity's first operation (`outcome: 'none'` = a content activity). */
export interface OperationExample {
	op?: string;
	variant?: string | null;
	outcome: 'ok' | 'error' | 'unknown_outcome' | 'none';
	/** What the operation returned (`outcome: 'ok'`). */
	result?: JsonValue;
	error?: { code: string; message: string };
	/** The read that confirmed a write (`order.read` after `order.cancel`). */
	verify_read?: JsonObject;
	/** What the agent does when the operation finds nothing (`FAQ_FALLBACK → handover`). */
	fallback?: string;
	note?: string;
}

export interface ActivityDetail {
	activity: JobJourneyActivity;
	stage: JourneyStage;
	operations: ActivityOperation[];
	/** D61 `<industry>:<area>:<slug>`; null when the activity belongs to no area yet. */
	context_space: string | null;
	procedure: string;
	sample_dialogue: DialogueTurn[];
	success_example: OperationExample;
	error_example: OperationExample;
	metric: { key: string; label: string; target: string; current: string | null };
	knowledge_kinds: JobKnowledgeKind[];
	/** Required operations without a route in some env (J5 warning). */
	unrouted_required: { op: string; env: 'prod' | 'test' }[];
	/** J5: the use case end to end — qualify → procedure → systems → state change, live outcomes, lineage. */
	use_case?: ActivityUseCase;
}

// ---- J5 use case (additive) -------------------------------------------------------------------------------------

/** One gate on the path to this activity: what it asks and the answer that leads here (11§4.3.4). */
export interface UseCaseGate {
	key: string;
	point: string;
	question: string;
	/** The answer that selects this activity (`Delivery`, `Track a parcel`, `observe`). */
	answer: string;
	threshold: string;
	/** How often the gate gave this answer in the last 7 days (null without stats). */
	verdicts_7d: number | null;
}

/** One procedure step with the system its operation routes to in prod. */
export interface UseCaseStep {
	n: number;
	phase: number;
	name: string;
	/** `delivery.read{by_tracking_ref}` */
	op: string;
	side_effect: 'R' | 'W' | 'X' | 'P';
	identity_level: string;
	confirm_back: boolean;
	/** The case ends as not eligible when this step's outcome is false. */
	gate: boolean;
	prod: OperationRouteInfo;
}

/** A status change on an object this activity's operations may cause (model transitions, 11§1.2). */
export interface UseCaseStateChange {
	object: string;
	verb: string;
	from: string[];
	/** null = the status stays as it is (a guarded write that does not move the state). */
	to: string | null;
	guard: string | null;
}

export interface UseCaseCase {
	case_id: string;
	title: string;
	status: string;
	outcome: string | null;
	identity_level: number;
	env: 'prod' | 'test';
	started_at: Iso;
}

export type UseCaseLayerKind = 'base' | 'industry' | 'department' | 'job' | 'tenant';

/** One layer of where this activity comes from (template pins → tenant refinement). */
export interface UseCaseLayer {
	kind: UseCaseLayerKind;
	/** `industry_dept/retail/customer_service@v2`; null for the tenant row. */
	tag: string | null;
	contributes: string;
	/** F-44, on the `tenant` layer: whether the use case card is the template's or the tenant's own refinement. */
	card?: 'template' | 'refined';
}

export interface ActivityUseCase {
	/** Plain-language: what happens when a customer wants this. */
	summary: string;
	identity: { level: number; label: string; how: string };
	/** Customer phrasings the activity is recognised from. */
	examples: string[];
	gates: UseCaseGate[];
	procedure: {
		name: string;
		version: number;
		origin: 'base' | 'tenant';
		context_space: string;
		description: string;
		runs_7d: number;
		success_rate: number | null;
		steps: UseCaseStep[];
	} | null;
	/** Object state changes it can cause; empty = read-only. */
	state_changes: UseCaseStateChange[];
	/** What the reply is checked against before it is sent (R1 post-gates). */
	checks: { key: string; text: string }[];
	/** Last 30 days in production (null when the activity has had no traffic). */
	live: { cases: number; contained: number; escalated: number; p50_ms: number | null } | null;
	/** Outcome codes of the recent cases, most frequent first. */
	outcomes: { code: string; count: number }[];
	recent_cases: UseCaseCase[];
	lineage: UseCaseLayer[];
	/** Draft refinements of this activity, not yet published by a release (D54: test, then publish). */
	pending: PendingRefinement[];
	/** D62: how customers are offered the activity live (the active release's snapshot) vs in the draft, when the
	 *  draft changes it; null = draft and live agree. */
	live_vs_draft?: { live: string; draft: string } | null;
	/** The job's current draft revision (test-chat cases record it; production cases the release, D63). */
	draft_rev?: number;
}

// ---- generator (P18) ---------------------------------------------------------------------------------------------

export type GeneratorStepKey =
	| 'input'
	| 'business_model'
	| 'journey'
	| 'objects'
	| 'operations'
	| 'harmonise'
	| 'validate'
	| 'use_cases'
	| 'package';

export type DraftDecision = 'pending' | 'accepted' | 'merged' | 'renamed' | 'dropped';

/** One LLM-drafted item of a step: an activity, an activity×object mapping, an operation … */
export interface GeneratorDraftItem {
	id: string;
	step: GeneratorStepKey;
	/** What kind of row it becomes (`activity`, `activity_object`, `operation_expectation`, `profile` …). */
	kind: string;
	title: string;
	detail: string;
	/** Example data (A3–A4) or success JSON (C1–C5). */
	example: JsonObject | null;
	decision: DraftDecision;
	/** New name (renamed) or the item it was merged into. */
	decision_note: string | null;
	/** The draft names an object that is not canonical (rule 1). */
	new_object: string | null;
	/** "new object" / "new operation" proposal sent to the super_admin. */
	proposal: 'none' | 'proposed' | 'approved' | 'rejected';
	/** F-23: the proposed operation row (`{key, fields}`) of an Operations-step item, once proposed. */
	new_operation?: JsonObject | null;
	llm_confidence: number;
}

export interface GeneratorStepState {
	key: GeneratorStepKey;
	status: 'done' | 'current' | 'todo' | 'blocked';
	summary: string;
}

export interface GeneratorRunSummary {
	id: number;
	scope: 'platform' | 'tenant';
	industry: string;
	department: string;
	job: JobKey;
	tenant_id: TenantId | null;
	tenant_name: string | null;
	status: 'running' | 'blocked' | 'packaged' | 'abandoned';
	current_step: GeneratorStepKey;
	started_by: Sub;
	started_at: Iso;
	updated_at: Iso;
	/** What it outputs (template layers for platform runs, refinements for tenant runs, D8). */
	output: string[];
	/**
	 * D56: the run's LLM usage. Not a bill: generator tokens are metered through the same usage pipeline and funding
	 * lane as customer-engagement turns, attributed to the tenant + project + the `generator` activity (tenant runs)
	 * or to the platform's own usage (platform runs, no tenant).
	 */
	cost: GeneratorUsage;
	red: number;
	/** Display name of `started_by` (P18 list). */
	started_by_name?: string;
	/** What Package wrote (`industry/retail@v2`, `refinement rev 12`); null while the run is open. */
	tagged?: string | null;
	/** Provisional `@v0` tags the run has written so far (not immutable, cannot be cut). */
	provisional_tags?: string[];
}

export interface GeneratorRun extends GeneratorRunSummary {
	description: string;
	steps: GeneratorStepState[];
	items: GeneratorDraftItem[];
	/** Stages of the drafted journey (A1–A2), for rule 4. */
	stages: { key: StageKey; label: string; activities: string[]; out_of_scope: boolean }[];
	objects: string[];
	findings: ValidationFinding[];
	tagged: string | null;
	/** P18: one usage row per LLM drafting call — which step, which model, tokens, funding lane (D56). */
	cost_lines?: GeneratorCostLine[];
}

/** D56: whose usage a generator run is — the tenant's (tenant runs) or the platform's own (platform runs). */
export type GeneratorUsageOf = 'platform' | 'tenant';

/** D56: a generator run's metered usage — tokens on a funding lane, attributed like any other usage row. */
export interface GeneratorUsage {
	tokens: number;
	/** Funding lane of the drafting model (same lanes as conversation turns). */
	funding: Funding;
	attributed_to: GeneratorUsageOf;
	/** Usage-ledger project: the tenant id for tenant runs (project-less app), null for platform runs. */
	project: string | null;
	/** Usage-ledger activity the rows are recorded under. */
	activity: 'generator';
}

/** One LLM drafting call of a generator run (one usage-ledger row); the run's `cost.tokens` is the sum. */
export interface GeneratorCostLine {
	step: GeneratorStepKey;
	model: string;
	tokens: number;
	funding: Funding;
	at: Iso;
}

export interface StartGeneratorRunInput {
	scope: 'platform' | 'tenant';
	industry: string;
	department: string;
	job: JobKey;
	description: string;
	/** Tenant runs: the tenant the run refines (a tenant_admin may only name their own). */
	tenant_id?: TenantId | null;
}

/** F-23 `proposeGeneratorOperation` body: the operation as the platform's operation editor takes it (F-22). */
export interface OperationDraft {
	object: string;
	verb: string;
	variant?: string | null;
	modality?: 'do' | 'request' | 'observe';
	identity_level: string;
	raises_identity_level?: string | null;
	side_effect: string;
	confirm_back: boolean;
	verify_read?: string | null;
	idempotency: string;
	input_fields?: string[];
	output_fields?: string[];
	description?: string;
	extra_errors?: string[];
}

/** P18 "New run" usage estimate input: who the run is for decides whose usage it is (D56). */
export interface GeneratorEstimateInput {
	scope: 'platform' | 'tenant';
	tenant_id?: TenantId | null;
}

/** P18 usage estimate before a run starts: a token range from past packaged runs of the same scope (D56: no €). */
export interface GeneratorEstimate {
	scope: 'platform' | 'tenant';
	tokens_low: number;
	tokens_high: number;
	/** Funding lane the drafting model runs on. */
	funding: Funding;
	/** Whose usage the run's tokens are recorded as. */
	attributed_to: GeneratorUsageOf;
	/** Tenant name for tenant runs (usage attributed to it); null for platform runs (platform's own usage). */
	tenant_name: string | null;
	/** The steps this scope runs, in order (tenant runs skip the platform-only steps). */
	steps: GeneratorStepKey[];
	/** The subset of `steps` that call the LLM (and cost tokens). */
	llm_steps: GeneratorStepKey[];
	/** The past runs the range is based on (`#6`). */
	basis: number[];
}

// ---- validation grid (J17) ---------------------------------------------------------------------------------------

export type ValidationVerdict = 'pass' | 'rejected' | 'pending';
export type RejectReason = 'missing_citation' | 'wrong_citation' | 'incorrect' | null;

export interface ValidationRow {
	id: string;
	question: string;
	expected: string;
	/** The reference passage the answer must cite (`policy.returns §2`). */
	reference: string;
	activity: string;
	response: string | null;
	/** Citations the response gave. */
	cited: string[];
	verdict: ValidationVerdict;
	reason: RejectReason;
	/** Who set the verdict (`judge` = LLM-as-judge, else a person). */
	judged_by: string | null;
}

export interface ValidationCut {
	release: ReleaseRef;
	state: 'current' | 'approved' | 'applied_earlier';
	ran_at: Iso | null;
	ran_by: Sub | null;
	rows: ValidationRow[];
}

/** The latest grid run of a release cut (`releaseValidation`); null until one ran. */
export interface ValidationGridRun {
	id: string;
	trigger: string;
	status: 'queued' | 'running' | 'done' | 'failed';
	done: number;
	total: number;
	queued_at: Iso;
	started_at: Iso | null;
	finished_at: Iso | null;
	error_code: string | null;
}

/** `GET /api/jobs/{job}/releases/{id}/validation` — one cut and its latest grid run. */
export type ReleaseValidation = ValidationCut & { grid: ValidationGridRun | null };

/** A question edit (F-50): only the fields present change; all three text fields stay required. */
export interface ValidationQuestionPatch {
	question?: string;
	expected?: string;
	reference?: string;
	activity?: string;
}

/** `POST …/validation/questions/import` answer: all or nothing, one batch. */
export interface ValidationImportResult {
	imported: number;
	import_batch: string;
}

// ---- discovery (J18) ---------------------------------------------------------------------------------------------

export interface ActivityVolume {
	activity: string;
	label: string;
	stage: StageKey;
	tickets: number;
	/** Use-case scorecard (10§8): Valuable = volume × value, Feasible = routing readiness, Usable = live metric. */
	valuable: number;
	feasible: number;
	usable: number | null;
	enabled: boolean;
	/** F-51: the owner's weight, an integer 0..1000 (100 = default); `valuable` is already scaled by it. */
	weight?: number;
}

export interface UncoveredCluster {
	id: string;
	label: string;
	tickets: number;
	examples: string[];
	suggestion: {
		kind: 'enable_activity' | 'tenant_extension';
		activity: string;
		label: string;
		stage: StageKey;
	};
	state: 'open' | 'accepted';
	accepted_at: Iso | null;
	accepted_by: Sub | null;
}

export interface DiscoveryImport {
	id: string;
	file: string;
	rows: number;
	uploaded_at: Iso;
	uploaded_by: Sub;
	status: 'classifying' | 'classified';
	/** 0–1 while classifying. */
	progress: number;
	coverage: number | null;
	long_tail: number;
	volumes: ActivityVolume[];
	clusters: UncoveredCluster[];
}

export interface ShadowMode {
	active: boolean;
	started_at: Iso | null;
	started_by: Sub | null;
	env: 'test' | 'prod';
	conversations: number;
	labelled: number;
}

export interface DiscoveryState {
	job: JobKey;
	/** The job's journey template (a tenant generator run refines it). */
	industry: string | null;
	department: string | null;
	imports: DiscoveryImport[];
	shadow: ShadowMode;
	runs: GeneratorRunSummary[];
	/** J18 (D54): proposals derived from the tenant's real traffic — accept = a draft change, reject = dismissed. */
	proposals?: DiscoveryProposal[];
	/** J18 (D56): the LLM usage discovery recorded for this tenant (metered on Usage, not a bill). */
	usage?: DiscoveryUsage;
	/** J18: the traffic sources discovery reads, with their volumes. */
	signals?: DiscoverySignalSummary[];
}

/** What a discovery proposal changes in the job (each lands as a D54 draft change on its design page). */
export type DiscoveryProposalKind = 'new_activity' | 'enable_activity' | 'knowledge_gap' | 'operation_route';
/** Where the evidence came from: an uploaded export, production conversations, shadow traffic, knowledge searches. */
export type DiscoverySignal = 'import' | 'live' | 'shadow' | 'knowledge_search';
/**
 * Where an accepted proposal is in the D54 lifecycle: `draft` (saved to the draft, runs in Test), `in_release`
 * (bundled in an approved cut), `live` (published), `discarded` (accepted, then its draft change was discarded).
 */
export type DiscoveryProposalStatus = 'open' | 'rejected' | 'draft' | 'in_release' | 'live' | 'discarded';

export interface DiscoveryExample {
	text: string;
	/** Conversation / ticket id (`CNV-4410`, `ZD-88213`). */
	ref: string | null;
	/** What happened to it today (`handed over`, `no passage found`). */
	outcome: string | null;
}

export interface DiscoveryEvidence {
	source: DiscoverySignal;
	count: number;
	unit: 'tickets' | 'conversations' | 'searches';
	/** `zendesk-export-2026-09.csv` / `last 30 days · prod`. */
	window: string;
	/** Share of the source's volume (0–1), when it has one. */
	share: number | null;
	examples: DiscoveryExample[];
	/** One supporting metric line (`40 of 166 refund conversations handed over`). */
	metric: string | null;
}

/** The draft change accepting writes: where (page + ledger kind/key) and the live value next to the draft value. */
export interface DiscoveryChange {
	/** Design page the change lives on (`J4`, `J6`, `J10`). */
	page: string;
	/** Ledger kind(s) and key(s) it records (`activity` / `journey:delivery:invoice_vat_details`). */
	kind: string;
	keys: string[];
	/** Human address (`Delivery · Add VAT details to an invoice`). */
	target: string;
	live: string;
	draft: string;
}

export interface DiscoveryProposal {
	id: string;
	kind: DiscoveryProposalKind;
	title: string;
	/** One sentence: why discovery proposes it. */
	why: string;
	activity: string | null;
	stage: StageKey | null;
	evidence: DiscoveryEvidence;
	change: DiscoveryChange;
	/** Expected effect if published (`~240 tickets/month answered without staff`). */
	estimate: string | null;
	/** The uncovered cluster / import it came from (import proposals). */
	cluster: string | null;
	import_id: string | null;
	status: DiscoveryProposalStatus;
	decided_at: Iso | null;
	decided_by: Sub | null;
	/** Reject reason. */
	reason: string | null;
	/** The release that carries / published it (`in_release`, `live`). */
	release: string | null;
	/** LLM tokens drafting this proposal used (metered as the tenant's usage, D56). */
	drafted: { tokens: number };
}

export interface DiscoveryUsageLine {
	what: string;
	/** `12,480 GA gate calls`, `3 clustering passes`. */
	detail: string;
	tokens: number;
	/** Usage-ledger activity the tokens are recorded under (`generator`, `discovery`). */
	activity: string;
	funding: Funding;
}

/** D56: discovery's LLM usage, attributed to the tenant + project, metered on T5 Usage like turns. */
export interface DiscoveryUsage {
	/** The tenant the usage is attributed to. */
	tenant: string;
	window: string;
	lines: DiscoveryUsageLine[];
	tokens: number;
}

export interface DiscoverySignalSummary {
	source: DiscoverySignal;
	label: string;
	/** Volume read (`12,480 tickets`), 0 = not connected / off. */
	count: number;
	unit: 'tickets' | 'conversations' | 'searches';
	window: string;
	active: boolean;
}

// ---- handover queue (J21, D64) -----------------------------------------------------------------------------------
// D64 (lead, 2026-10-19; fills 14 §5 #4): one handover QUEUE per job. A case enters it when the agent hands over;
// a person in the tenant takes it (assign / take next), answers the customer, and closes it (resolved, or returned
// to the agent). Every transition records actor + time. The SLA POLICY is job config (D54: draft → test → release);
// queue actions are operational and act on live data; D88: Support agent works items, Support manager assigns. Blueprint for the backend's `agent_handover_items` /
// `agent_handover_events` / `agent_handover_policies` tables.

/** Why the agent stopped and handed the case to a person (D64). */
export type HandoverReason =
	| 'identity_failed'
	| 'gate_blocked'
	| 'customer_asked'
	| 'tool_failed'
	| 'unknown_outcome'
	| 'out_of_scope'
	/** 0179 (B4 T6a): a required or routed operation the job does not offer (was recorded `tool_failed`). */
	| 'not_offered'
	/** 0179 (B4 T6a): every step answered ok but the procedure's success criteria do not hold (was `tool_failed`). */
	| 'success_criteria_unmet'
	/** D94 (B4 T9b): the gate service could not answer and the point failed closed. */
	| 'gate_unavailable'
	/** D85: a staff member handed the case over from J20 (`handOverCase`). */
	| 'staff_decision';

/** D64 item lifecycle: `queued → assigned → waiting_customer → resolved` or `returned_to_agent` (both closed). */
export type HandoverState = 'queued' | 'assigned' | 'waiting_customer' | 'resolved' | 'returned_to_agent';

/** The priorities with a configurable SLA target (the policy editor's rows). */
export type HandoverPolicyPriority = 'high' | 'normal' | 'low';
/** An item's priority; `urgent` has no row of its own: its targets are half of `high` (agent-api `Targets::of`). */
export type HandoverPriority = 'urgent' | HandoverPolicyPriority;

export interface HandoverMessage {
	at: Iso;
	from: 'customer' | 'agent' | 'staff' | 'system';
	author: string;
	text: string;
}

/** One row of `agent_handover_events`: a state change or an (re)assignment, with who did it and when. */
export interface HandoverTransition {
	at: Iso;
	/** `agent` for the enqueue; otherwise the person's sub. */
	actor: Sub | 'agent';
	actor_name: string;
	/** `null` = the item entered the queue. */
	from: HandoverState | null;
	to: HandoverState;
	/** Set on an assignment (`to` stays `assigned`/`waiting_customer` on a reassign). */
	assignee?: Sub | null;
	note: string;
}

export interface HandoverItem {
	id: string;
	case_type: 'handover' | 'complaint' | 'return' | 'warranty_claim';
	subject: string;
	customer: string;
	order_id: string | null;
	/** The customer's channel (B12 widens it to every connector). */
	channel: import('./conversations').ConversationChannel;
	reason: HandoverReason;
	/** The rule or signal that fired, in the agent's words (`B4: frustration ≥ 4 on two turns`). */
	trigger: string;
	priority: HandoverPriority;
	state: HandoverState;
	/** Entered the queue (the agent's `case.create{handover}`). */
	queued_at: Iso;
	/** First person's message to the customer (stops the first-response clock). */
	first_response_at: Iso | null;
	/** Resolved or returned to the agent (stops the resolve clock). */
	closed_at: Iso | null;
	/**
	 * Due times, stamped from the job's LIVE SLA policy for `priority` when the item entered the queue (a draft policy
	 * never changes live items). `null` when the job had no policy.
	 */
	first_response_due: Iso | null;
	resolve_due: Iso | null;
	/** One assignee: an active member holding the Support agent role (D88). */
	assignee: Sub | null;
	assignee_name: string | null;
	/** Transitions, oldest first. */
	history: HandoverTransition[];
	messages: HandoverMessage[];
	/** The runtime case (J20) the handover was created from (`cases/{id}`). */
	case_ref: string | null;
	/** The conversation the item belongs to: a queue row opens it in the workspace (`/support/{job}/c/{id}`). */
	conversation_id?: string | null;
	/** Release the agent ran on when it handed over (D54: cases record their release). */
	release_id: string | null;
	/** Staff queue label the `agent_cases` route put it in. */
	queue: string;
	/** The agent's confidence in what the customer wanted, when that was the problem (GA, 0–1). */
	confidence?: number | null;
	/** What the agent already did, in order — so the person does not start over. */
	agent_did: HandoverAgentStep[];
	/** What the agent already collected (verified or as the customer said it). */
	collected: HandoverCollected[];
	/** The agent's suggestion for the person's first step. */
	suggested_next: string;
	/** The design change that would let the agent handle cases like this (fixture: precomputed). */
	proposal_hint: HandoverProposal | null;
	/** The person's verdict after working the case: feedback into the job design. */
	feedback: HandoverFeedback | null;
	/** B13: the customer behind the case (null before identity resolution ran). */
	party?: import('./conversations').PartyRef | null;
	/** B13: the customer's rating of the case once it closed, null while none was given. */
	csat?: import('./conversations').CaseCsat | null;
	/** F-14 (B4 T16a): queued while the job was closed; its SLA clocks start at `clock_starts_at`. */
	outside_hours?: boolean;
	/** F-14: the call-back offered to the customer out of hours (`callback` mode), else null. */
	callback?: { offered_at: Iso; after: Iso } | null;
	/** F-14: when the SLA clocks start (the next open time for an item queued out of hours). */
	clock_starts_at?: Iso | null;
}

export interface HandoverAgentStep {
	/** Operation or gate (`order.read`, `delivery.read`, `post-gate F3`). */
	op: string;
	outcome: 'ok' | 'failed' | 'blocked' | 'not_found';
	detail: string;
}

export interface HandoverCollected {
	label: string;
	value: string;
	/** Checked against a system (identity, order) rather than taken from the customer's words. */
	verified: boolean;
}

/** Who the queue can assign to: an active member holding the Support agent role (D88). */
export interface HandoverAssignee {
	sub: Sub;
	name: string;
	/** Held roles (§R.1); every assignee holds `support_agent`. */
	roles: Role[];
}

/** `GET /api/jobs/{job}/handover` — the job's queue plus who items can be assigned to. */
export interface HandoverQueue {
	items: HandoverItem[];
	next: string | null;
	assignees: HandoverAssignee[];
	/** The clock due times and breach state are read against (fixture: `FIXTURE_NOW`). */
	now: Iso;
}

/** First-response and resolve targets for one priority, in minutes from entering the queue. */
export interface HandoverSlaTarget {
	first_response_min: number;
	resolve_min: number;
}
export type HandoverSlaTargets = Record<HandoverPolicyPriority, HandoverSlaTarget>;

/** F-14 (B4 T16a): the days of a business week, Monday first (agent-api `handover::hours::DAYS`). */
export const WEEKDAYS = ['mon', 'tue', 'wed', 'thu', 'fri', 'sat', 'sun'] as const;
export type Weekday = (typeof WEEKDAYS)[number];
/** One open window of a day, local wall clock `HH:MM`: `[from, to)`, `to` may be `24:00`. */
export interface HoursWindow {
	from: string;
	to: string;
}
/** What a closed job tells the customer: a colleague calls back, or replies, after the next open time. */
export type OutOfHours = 'callback' | 'queue';
/**
 * F-14: a job's business hours (the `hours` of its `handover_policy`; null = always staffed). A holiday of the
 * tenant calendar is closed all day when `use_tenant_calendar`. Open/closed is read on the policy's own timezone.
 */
export interface HandoverHours {
	timezone: string;
	/** Every day present (empty = closed), windows sorted, never overlapping, at most 6 per day. */
	weekly: Record<Weekday, HoursWindow[]>;
	use_tenant_calendar: boolean;
	out_of_hours: OutOfHours;
}

/** One row of `agent_handover_policies` (per tenant + job). Job config: a ledger location (D54). */
export interface HandoverPolicyRow {
	tenant_id: TenantId;
	job: JobKey;
	targets: HandoverSlaTargets;
	/** F-14: business hours; absent or null = always staffed. */
	hours?: HandoverHours | null;
}

/** `GET /api/jobs/{job}/handover/policy` — what Production runs next to the draft. */
export interface HandoverPolicyView {
	job: JobKey;
	/** Published policy: stamps the due times of items that enter the queue. `null` = no SLA. */
	live: HandoverSlaTargets | null;
	/** The draft (= live when nothing is drafted). */
	draft: HandoverSlaTargets | null;
	/** The draft differs from live (an unpublished ledger row exists). */
	drafted: boolean;
	/** F-14 (B4 T16a): business hours, live next to the draft; null = always staffed. */
	hours: { live: HandoverHours | null; draft: HandoverHours | null };
}

export interface HandoverProposal {
	/** Draftable here: `tenant_extension`, `enable_activity` (a journey refinement, D54 draft). Others link out. */
	kind: 'tenant_extension' | 'enable_activity' | 'knowledge_gap' | 'gate_threshold';
	activity: string | null;
	stage: StageKey | null;
	label: string;
	detail: string;
	/** Page where the change is made or reviewed. */
	href: string;
	/** For a `tenant_extension`: the activity's objects and expected operations. */
	objects?: ActivityObject[];
	operations?: OperationExpectation[];
}

export interface HandoverFeedback {
	verdict: 'agent_should_handle' | 'right_to_hand_over';
	note: string;
	by: Sub;
	by_name: string;
	at: Iso;
	/** Only for `agent_should_handle`. */
	proposal: HandoverProposal | null;
	/** Set when the proposal was saved to the job's draft (D54: not live until published). */
	drafted_at: Iso | null;
}

// ---- J17 draft validation (D54 TEST step) --------------------------------------------------------------------------

/** How a test scenario phrases the activity: the plain question, a roundabout real-life phrasing, a Dutch customer. */
export type ScenarioStyle = 'plain' | 'indirect' | 'nl';

/**
 * What a scenario's outcome depends on in the job design (fixture-only stand-in for running the agent): the design
 * location is read from the live view (Production) and from the draft view (Test environment), so a draft change at
 * that location changes the result. `static` = the outcome does not depend on anything the draft can change.
 */
export type ScenarioProbe =
	| { type: 'static'; pass: boolean }
	/** Rule value `key` of rule `rule` lists `item` (e.g. RET-01 excluded categories). */
	| { type: 'rule_includes'; rule: string; key: string; item: string }
	/** The operation is routed (not `not_offered`) for `env`. */
	| { type: 'route_offered'; object: string; verb: string; env: 'prod' | 'test' }
	/** Entity `entity` has attribute `attr` (the agent can capture it). */
	| { type: 'entity_attr'; entity: string; attr: string }
	/** Gate option wording (label + description) does (`match`) / must not (`no_match`) contain one of `words`. */
	| { type: 'gate_wording'; gate: string; option: string; words: string[]; expect: 'match' | 'no_match' };

/** One example customer phrasing of a journey activity, with what a correct reply does. */
export interface TestScenario {
	id: string;
	activity: string;
	activity_label: string;
	stage: StageKey;
	style: ScenarioStyle;
	/** What the customer types. */
	phrasing: string;
	/** What a correct reply does. */
	expected: string;
	probe: ScenarioProbe;
	/** The design place (draft ledger key) the outcome depends on; the real API names it, the fixture reads `probe`. */
	place?: string | null;
	/** Reply when the scenario passes / fails, and the citations a passing reply gives. */
	reply_pass: string;
	reply_fail: string;
	cites: string[];
	/** Why a failing reply fails (shown on the cell). */
	fail_reason: string;
}

export type ScenarioVerdict = 'pass' | 'fail';
/** Draft compared with live: `regression` passes live and fails on the draft; `fixed` the other way round. */
export type ScenarioDiff = 'same' | 'regression' | 'fixed';

export interface ScenarioResult {
	scenario: string;
	live: ScenarioVerdict;
	draft: ScenarioVerdict;
	diff: ScenarioDiff;
	reply_live: string;
	reply_draft: string;
	cited_draft: string[];
	/** The draft change (ledger row) at the design location this scenario depends on, when live and draft differ. */
	cause: { change_id: string; key: string; label: string; page: string } | null;
}

/** One run of every scenario against the draft on the Test environment (D54: test runs record env=test + draft rev). */
export interface DraftTestRun {
	id: string;
	env: 'test';
	/** The job's draft revision (`draft_rev`) the run used. */
	draft_rev: number;
	/** Fingerprint of the draft (every unpublished change and its value) the run used. */
	draft_fp: string;
	/** `key@value` of every unpublished change the run included (to tell what changed since). */
	change_keys: string[];
	/** The release Production ran when the run compared against live. */
	base_release: string;
	at: Iso;
	by: Sub;
	by_name: string;
	results: ScenarioResult[];
	summary: {
		total: number;
		pass: number;
		fail: number;
		regressions: number;
		fixed: number;
		live_pass: number;
	};
}

/** A draft change and what the latest run says it did. */
export interface DraftChangeImpact {
	change_id: string;
	key: string;
	label: string;
	page: string;
	by_name: string;
	at: Iso;
	/** Scenarios whose design location this change touches. 0 = no scenario covers it. */
	covered: number;
	fixed: number;
	regressions: number;
	/** Saved after the latest run: not tested yet. */
	untested: boolean;
}

/** `GET /api/jobs/{job}/validation/draft` — the draft's test status (J17; J14 shows it as "tested"). */
export interface DraftValidation {
	scenarios: TestScenario[];
	/** Newest first; `runs[0]` is the latest. */
	runs: DraftTestRun[];
	current: { draft_rev: number; draft_fp: string; changes: number; base_release: string };
	/** never = no run yet; current = the latest run tested exactly this draft; stale = the draft changed since. */
	state: 'never' | 'current' | 'stale';
	/** Changes saved or discarded since the latest run (labels). */
	changed_since: string[];
	impacts: DraftChangeImpact[];
}

/** `POST …/discovery/imports/preview` (F-52): nothing stored, no model call; sample cells are scrubbed excerpts. */
export interface DiscoveryPreview {
	format: 'csv' | 'json' | 'xlsx';
	columns: string[];
	sample_rows: string[][];
	rows: number;
	suggested_text_column: string | null;
	suggested_count_column: string | null;
}

/** `uploadDiscoveryImport` input: the fixture keeps accepting a file name; HTTP sends the `File`. */
export interface DiscoveryUploadInput {
	file: File | string;
	/** The fixture's row count (the server counts rows itself). */
	rows?: number;
	text_column?: string;
	count_column?: string;
}
