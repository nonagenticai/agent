// Shapes for the "release" area: job selection (K6), releases (K41, 05 §4.1), release status (05 §4.7), conflicts
// (05 §4.6), refinements (D8, 05 §2.5), readiness (10 §5.2, K25) and business rules (10 §5.2 step 7).
// Field names follow spec 14 §3, snake_case as this app's API sends them.
import type { Iso, JsonObject, JobKey, ReleaseSummary, Selection, Sub, TemplatePin } from './core';

// ---- selection (jobs.selection, `GET/PUT /api/jobs/{job}/target`) ---------------------------------------------

export interface SelectionChoice {
	key: string;
	label: string;
	/** Short note under the option (what it adds to the bundle). */
	note?: string;
	/** D60 (departments): the base department archetype this industry department specialises. */
	extends?: string;
	/** The industry this option belongs to (profiles, industry departments, areas). */
	industry?: string;
	/** Areas only: the department layer key (`industry_dept/<industry>/<dept>`) that groups this area (D60). */
	industry_dept?: string;
}

export interface SelectionDimension {
	key: string;
	label: string;
	values: SelectionChoice[];
	/** D59: `dimension` (selects layers), `placeholder` (recorded only). */
	kind?: 'dimension' | 'placeholder';
	/** One line under the group: what picking a value does. */
	note?: string;
}

/**
 * The options a selection may name (D65): platform templates only, shaped like `Selection`. Industry departments
 * and areas carry their `industry` (areas also their `industry_dept`), so a page offers only the chosen industry's.
 */
export interface SelectionOptions {
	profiles: SelectionChoice[];
	industries: SelectionChoice[];
	/** D60/D115: the departments; `key` is the `industry_dept/<industry>/<dept>` layer key, `extends` names the base
	 *  department archetype each specialises. */
	departments: SelectionChoice[];
	/** Areas inside the industry departments (groupings of context spaces). */
	areas: SelectionChoice[];
	/** D59: `channel` selects layers; D111: `region` offers the values of the selection's one `region` (recorded only). */
	dimensions: Record<'channel' | 'region', SelectionDimension>;
}

/** One `job.selection_update` audit row (old → new). */
export interface SelectionAudit {
	at: Iso;
	actor: Sub;
	actor_name?: string;
	action: 'job.selection_update';
	/** Human summary of what changed ("channel + telegram"). */
	change: string;
	old: Selection | null;
	new: Selection;
}

/** `GET /api/jobs/{job}/target`. */
export interface JobTarget {
	job: JobKey;
	selection: Selection | null;
	selection_updated_by: Sub | null;
	selection_updated_by_name?: string;
	selection_updated_at: Iso | null;
	/** Set when a profile or dimension value the selection names no longer exists (05 §4.1). */
	selection_error: string | null;
	options: SelectionOptions;
	history: SelectionAudit[];
}

// ---- releases --------------------------------------------------------------------------------------------------

/** A selection as frozen in a release: profile values inlined (profiles mutate in place). */
export interface ResolvedSelection extends Selection {
	profile_values: Record<string, string>;
}

export interface OverlayRev {
	/** `overlay_row.id` (uuid). */
	id: string;
	scope: 'base' | 'profile' | 'industry' | 'base_dept' | 'industry_dept' | 'job';
	kind: string;
	key: string;
	rev: number;
}

export interface ReleaseProvenance {
	overlay_revs: OverlayRev[];
	seed_versions: number[];
	/** Template tags the cut compiled (D15 pins). */
	pins: TemplatePin[];
}

/** One missing dependency of a release (K25 W5 slot, or 12 §3.5 routing_ready). */
export interface MissingItem {
	dependency: string;
	slot: string;
	/** Readable reason. */
	detail: string;
	/** Where to fix it. */
	href: string;
}

/** K25 readiness recorded at an apply. */
export interface ReadinessResult {
	ready: boolean;
	checked_at: Iso;
	missing: MissingItem[];
}

export interface BundleSection {
	key: string;
	label: string;
	items: string[];
}

/** `GET /api/jobs/{job}/releases` row: the core summary plus the mutable apply columns. */
export interface ReleaseRow extends ReleaseSummary {
	applied_by: Sub | null;
	last_applied_at: Iso | null;
	counts: Record<string, number>;
	/** Display names of approved_by / released_by / applied_by. */
	names: Record<string, string>;
	/** D54: tenant draft changes the release bundled at its cut. */
	draft_count?: number;
	/** D62: design places the release's stored snapshot holds (what Production runs while it is current); null = no
	 *  snapshot stored (it cannot be applied). */
	snapshot_places?: number | null;
	/** D15: template tags this cut moved forward against the release before it (`job/x` v3 → v4). */
	upgrades?: { layer: string; from: number; to: number }[];
	/** The last rollback that took Production away from this release (it is no longer live), or null. */
	rolled_back?: { at: Iso; by: Sub; by_name: string; to: string; undone: number } | null;
}

/** `GET /api/jobs/{job}/releases/{id}`: immutable content + provenance. */
export interface ReleaseDetail extends ReleaseRow {
	selection: ResolvedSelection;
	provenance: ReleaseProvenance;
	readiness: ReadinessResult | null;
	bundle: BundleSection[];
	/** What the release needs from the tenant's environment (drives readiness). `routes` = Production routes apply
	 * checks (12 §3.5); `test_routes` = Test-env routes its test runs need (block testing, never publishing). */
	requires: { slots: string[]; routes: string[]; test_routes?: string[] };
	/** D54: the tenant's draft changes this release bundled at its cut (published when it is applied). */
	drafts?: DraftChange[];
	/** Rollbacks that moved Production away from this release or back to it, newest first. */
	rollbacks?: RollbackEvent[];
}

export interface CompareSection {
	key: string;
	label: string;
	added: string[];
	removed: string[];
}

/** This release vs the current one (`GET /api/jobs/{job}/releases/{id}/compare?with=current`). */
export interface ReleaseCompare {
	id: string;
	current: string | null;
	same_hash: boolean;
	sections: CompareSection[];
}

/** `GET /api/jobs/{job}/releases/status` (05 §4.7). */
export interface ReleaseStatus {
	base_changed: boolean;
	approved_waiting: string | null;
	selection_error: string | null;
	missing_slots: MissingItem[];
	/** The fresh compile's short hash (draft). */
	draft_hash: string | null;
	/** Newer platform template tags than the job's pins (why base changed). `in_release`: an approved cut that
	 *  already adopts it (D15, opt-in at the cut). */
	newer_pins: { layer: string; pinned: number; latest: number; in_release?: string | null }[];
	/** D54: unpublished draft changes (ledger rows) the next cut would bundle. */
	draft_count?: number;
}

export interface ApproveInput {
	apply: boolean;
	/** `selection_updated_at` the page read; differs → 409 selection_changed. */
	selection_seen_at: Iso | null;
	/** D15 (opt-in): template layers whose newest tag this cut adopts (`job/customer_care_agent`). Omitted = the
	 *  cut keeps the job's pins. Conflicts the upgrade raises against the tenant's refinements open at the cut. */
	adopt_pins?: string[];
}

export interface ApproveResult {
	release: ReleaseRow;
	/** false = same hash as the newest release: no row, no audit (05 §4.2 step 7). */
	created: boolean;
	applied: boolean;
}

export interface ApplyResult {
	release: ReleaseRow;
	/** false = already current. */
	applied: boolean;
	previous: string | null;
	/** D54: draft changes this apply made live (stamped `published_in` = this release). */
	published?: number;
	conflicts_open: number;
	/** rollback = an older release made current again · reapply = a newer, earlier-applied one · apply = a cut. */
	direction?: RollbackDirection;
	/** Published design changes this apply took out of Production (rollback) / put back (re-apply). */
	rolled_back?: number;
	reapplied?: number;
}

/** One `{path, message}` of a 422 bundle_incomplete / bundle_invalid / compile_failed. */
export interface BundleViolation {
	path: string;
	message: string;
}

// ---- conflicts (05 §4.6) -----------------------------------------------------------------------------------------

export type ConflictKind =
	| 'base_changed'
	| 'base_removed'
	| 'base_added'
	| 'parent_missing'
	| 'base_shadowed'
	| 'job_setting'
	/** KH T3 (D112): informational — a wildcard pack tombstone of the job also excludes a pack the release adds. */
	| 'excluded_by_rule'
	/** KH T8 (D110): informational — the upgrade makes a knowledge kind required for a department (object
	 *  `knowledge_kind`, key `<department>:<stage>|<kind>`, `tenant_row: 'none'`); readiness blocks the release's
	 *  apply until the kind resolves. */
	| 'required_kind_introduced';
export type ConflictStatus = 'open' | 'superseded' | 'resolved';
export type RefinableObject =
	| 'activity'
	| 'context_space'
	| 'ontology_attribute'
	| 'gate_option'
	| 'knowledge_store'
	/** PK (T3): a template's pack pin on one kind, key `stage|kind|pack_key`; tombstone only (hide / unhide). */
	| 'knowledge_pack'
	/** KH T4 (D108): a folded prompt slot; a tenant refines `tone`, `escalation`, `format` only (else 422
	 *  `instruction_not_refinable`). */
	| 'instruction'
	| 'job_setting';

export interface ReleaseConflict {
	id: number;
	kind: ConflictKind;
	object: RefinableObject | 'knowledge_kind';
	key: string;
	/** Tenant row kind the conflict is about (`none`: a finding about no tenant row, `required_kind_introduced`). */
	tenant_row: 'override' | 'extension' | 'tombstone' | 'none';
	/** One line on what changed underneath. */
	detail: string;
	base_hash_before: string | null;
	base_hash_after: string | null;
	release_id: string;
	detected_at: Iso;
	status: ConflictStatus;
	resolution: 'reverted' | 'rebased' | null;
	resolved_by: Sub | null;
	resolved_by_name?: string;
	resolved_at: Iso | null;
	/** J16 (D54): the tenant's choice, saved to the draft. The conflict stays `open` (decided in draft) until the
	 *  release carrying `draft_key` is applied; then it is `resolved` with `resolution` = `choice`. Discarding the
	 *  draft change clears it (the conflict is undecided again) unless an approved cut already carries it. */
	decision?: ConflictDecision | null;
	/** D15: raised at the cut of this approved release (a template upgrade it adopts), before it is applied. */
	raised_by_cut?: string | null;
}

export interface ConflictDecision {
	choice: 'reverted' | 'rebased';
	/** Ledger key of the draft change that carries it (`overlay:context_space:…`, `setting:preamble`). */
	draft_key: string;
	/** Ledger row id while the change is unpublished (`d-7`), else null. */
	draft_id: string | null;
	by: Sub;
	by_name?: string;
	at: Iso;
	/** The release whose apply published it (set when the conflict becomes `resolved`). */
	published_in?: string | null;
}

/** J16: what moved the base under a refinement — a template upgrade (D15, new pinned tag) or a new cut of the same pins. */
export interface ConflictCause {
	kind: 'template_upgrade' | 'new_cut';
	/** `job/customer_care_agent` (template layer), when a tag moved. */
	layer?: string;
	from?: string;
	to?: string;
}

/** J16: what customers get while a conflict is open, and how many conversations touched the location (30 days). */
export interface ConflictImpact {
	conversations_30d: number;
	/** One line: what Production does with the refinement as it stands. */
	effect: string;
}

/** J16 (D54): one conflict as the Conflicts tab shows it — base before → after, the tenant's refinement, the cause,
 *  the customer impact, and where its resolution is in draft → publish. `GET /api/jobs/{job}/release-conflicts/board`. */
export interface ConflictCard extends ReleaseConflict {
	/** What the base rows are: the row itself, or its parent (`parent_missing`). */
	base_subject: string;
	base_before: JsonObject | null;
	base_after: JsonObject | null;
	/** The tenant's row content (null for a hide = tombstone). */
	mine: JsonObject | null;
	cause: ConflictCause | null;
	impact: ConflictImpact | null;
	/** Decided in draft (D54): the unpublished draft change carrying the choice (`id` null when only an approved
	 *  cut still carries it, i.e. the draft row was discarded after the cut), or null. */
	draft: { id: string | null; label: string; in_release: string | null } | null;
	/** The release that published the resolution, when it has been published. */
	published_in: string | null;
	/** Design page where the refinement can be edited instead (`/refinements`). */
	edit_path: string;
}

export interface ConflictBoard {
	items: ConflictCard[];
	/** Open and undecided. */
	open: number;
	/** Open, a choice saved to draft and not published yet (D54). */
	decided_in_draft: number;
	/** 05 §4.3: apply never checks conflicts — open ones are superseded and recomputed against the new base. */
	blocks_publish: false;
	/** The current release the open conflicts were detected against. */
	current_release: string | null;
	/** Unpublished changes in the whole job draft (the publish they ride with). */
	draft_count: number;
}

// ---- refinements (05 §2.5) -----------------------------------------------------------------------------------------

export type RefinementOp = 'hide' | 'unhide' | 'revert' | 'rebase';
export type RowState = 'base' | 'override' | 'extension' | 'tombstone';
export type OverlayItem = Record<string, unknown> & { key: string };

/** A tenant row: copy-on-write over a base row (override), a new row (extension) or a hide (tombstone). */
export interface TenantRow {
	key: string;
	kind: 'override' | 'extension' | 'tombstone';
	/** The tenant's values (override/extension). */
	item: OverlayItem | null;
	base_hash: string | null;
	content_hash: string;
	updated_by: Sub;
	updated_by_name?: string;
	updated_at: Iso;
}

/** `GET /api/jobs/{job}/{object}?layer=base|tenant`, both sides at once. */
export interface OverlayLayers {
	object: RefinableObject;
	base: OverlayItem[];
	tenant: TenantRow[];
	draft_rev: number;
	/** Entity names a context space may reference (the editor's choices). */
	entities: string[];
}

/** `GET /api/jobs/{job}/settings`: `override[k] = null` hides the base value. */
export interface JobSettingsView {
	base: Record<string, string | null>;
	override: Record<string, string | null>;
	effective: Record<string, string | null>;
	draft_rev: number;
}

/** D59: `GET /api/jobs/{job}/languages` — the languages the job serves, live next to draft (D54). */
export interface JobLanguagesView {
	/** What Production serves (the current release + changes published since). */
	live: string[];
	/** What the Test environment serves (the draft); equal to `live` when nothing is unpublished. */
	draft: string[];
	/** Every language a job may serve, with what the knowledge base covers for it. */
	options: { key: string; label: string; knowledge: 'full' | 'partial' | 'none' }[];
	/** True when the draft differs from live (an unpublished change). */
	unpublished: boolean;
}

export interface ContextSpacePatch {
	label?: string;
	required?: string[];
	entities?: string[];
}

/** `PUT /api/jobs/{job}/instructions/{slot}` — the tenant's own text for a refinable prompt slot (KH T4, D108). */
export const INSTRUCTION_SLOTS = ['tone', 'escalation', 'format'] as const;
export type InstructionSlot = (typeof INSTRUCTION_SLOTS)[number];
/** The server trims `text`, refuses blank (422 `invalid_body`), > 4000 UTF-16 units and a line that starts with a
 * prompt section marker; `language` is an optional language tag. */
export const MAX_INSTRUCTION_CHARS = 4000;
export interface InstructionPatch {
	text: string;
	language?: string;
}

export interface RefineResult {
	draft_rev: number;
}

// ---- readiness (J3) and business rules (J9) ---------------------------------------------------------------------

export type ReadinessStatus = 'done' | 'current' | 'todo' | 'missing' | 'placeholder' | 'blocked';

export interface ReadinessStep {
	key: string;
	label: string;
	status: ReadinessStatus;
	detail: string;
	href: string;
}

export interface ReadinessChecklist {
	job: JobKey;
	/** The release the go-live check is for (newest approved, else current). */
	release_id: string | null;
	steps: ReadinessStep[];
	definition_of_ready: ReadinessStep[];
	/** The same list a 409 release_not_ready would carry for that release (Production routes, W5 slots, the selection
	 *  hierarchy and required knowledge per department). */
	missing: MissingItem[];
	placeholders: string[];
	/** 12 §3.5: required Test-env routes with no tested route. They block testing (test chat, regression runs on
	 *  the draft), never publishing — not part of `missing`. */
	test_env?: MissingItem[];
}

export interface RuleValue {
	key: string;
	label: string;
	unit: string;
	value: string | null;
	hint: string;
	/** What the template suggests (J9). Never applied by itself: the business confirms its own number. */
	suggested?: string | null;
}

export interface PolicyPassage {
	id: string;
	store: string;
	kind: string;
	title: string;
	excerpt: string;
}

export type RuleComparison = 'eq' | 'ne' | 'lt' | 'lte' | 'gt' | 'gte' | 'in' | 'not_in';
export type RuleOperand =
	| { kind: 'fact' | 'value' | 'days_since' | 'months_since'; key: string }
	| { kind: 'literal'; value: string };
export type RuleCondition =
	| { kind: 'all' | 'any'; conditions: RuleCondition[] }
	| { kind: 'compare'; left: RuleOperand; op: RuleComparison; right: RuleOperand };
export interface RuleProgram {
	condition: RuleCondition;
	eligible_reason: string;
	ineligible_reason: string;
}
export interface RuleDefinition {
	label: string;
	applies_to: string[];
	inputs: string[];
	outcome: string;
	values: Omit<RuleValue, 'value'>[];
	facts: RuleFact[];
	evaluation: RuleProgram;
	statement?: string;
	uses?: RuleUse[];
	outcomes?: RuleOutcomeDef[];
	examples?: RuleExample[];
}

export interface BusinessRule {
	id: string;
	label: string;
	evaluation?: RuleProgram;
	tenant_authored?: boolean;
	/** `object.verb{variant}` it guards. */
	applies_to: string[];
	inputs: string[];
	outcome: string;
	values: RuleValue[];
	/** Any value unset. */
	placeholder: boolean;
	passage: PolicyPassage | null;
	updated_by: Sub | null;
	updated_by_name?: string;
	updated_at: Iso | null;
	// J9 additions (optional; older rows may lack them)
	/** The rule in one sentence, `{key}` = a value slot (`Returns are accepted within {window_days} days…`). */
	statement?: string;
	/** The template layer that defines the slot (`industry/retail@v2`). Slot shape is read-only for the tenant. */
	slot_layer?: string;
	/** The applied release that carries the slot (derived on read). */
	slot_release?: string | null;
	/** D54: the published (Production) values while a saved draft differs from them; absent when the draft equals
	 *  what customers get. `values` above is the draft the test chat runs. */
	live_values?: Record<string, string | null>;
	uses?: RuleUse[];
	outcomes?: RuleOutcomeDef[];
	facts?: RuleFact[];
	examples?: RuleExample[];
	history?: RuleChange[];
	decisions?: RuleDecision[];
}

// ---- J9 business rules: the slot's anatomy, where it is evaluated, its history and a dry-run evaluation ---------

/** Where a rule is evaluated: a journey activity, and the procedure step that calls `rule.evaluate{id}` (if any). */
export interface RuleUse {
	activity: string;
	activity_label: string;
	procedure: string | null;
	step: string;
}

/** One outcome the template defines for the rule, and what the agent does with it. */
export interface RuleOutcomeDef {
	/** `eligible` | `ineligible:RET-01` | `escalate`. */
	code: string;
	label: string;
	agent_does: string;
}

/** A fact the rule reads (an input object field), with how the J9 test form asks for it. */
export interface RuleFact {
	key: string;
	label: string;
	kind: 'date' | 'text' | 'choice' | 'number' | 'bool';
	options?: string[];
}

/** A ready-made set of facts for the test form ("lamp, delivered 52 days ago"). */
export interface RuleExample {
	label: string;
	facts: Record<string, string>;
}

/** One change to the rule's values or passage. */
export interface RuleChange {
	at: Iso;
	by: Sub;
	by_name?: string;
	change: string;
}

/** A recent `rule.evaluate` in a real case: the facts it saw and the outcome it returned. */
export interface RuleDecision {
	case_id: string;
	at: Iso;
	code: string;
	detail: string;
	facts: Record<string, string>;
}

/** `POST /api/jobs/{job}/rules/{id}/evaluate` — a dry run of the built-in `rule.evaluate` (stores nothing). */
export interface RuleEvaluation {
	rule: string;
	outcome: 'eligible' | 'ineligible' | 'escalate' | 'placeholder';
	/** `eligible`, `ineligible:RET-01`, `escalate`, `placeholder`. */
	code: string;
	reason: string;
	/** Each comparison the rule made, in order. */
	trace: string[];
	values_used: Record<string, string | null>;
}

// ---- J3 readiness board (`GET /api/jobs/{job}/readiness/board`) ---------------------------------------------------

/** What a gap stops: `apply` = the 409 release_not_ready list; `go_live` = apply passes, the agent runs with a hole;
 *  `testing` = a Test-env gap (12 §3.5: only Production routes block apply) — the draft cannot be fully tested. */
export type ReadinessBlocks = 'apply' | 'go_live' | 'testing';
/** Who can fix a gap: the tenant's own admin, or the platform owner (template / provisioning side). */
export type ReadinessFixer = 'tenant_admin' | 'super_admin';
export type ReadinessAreaKey =
	'providers' | 'routes' | 'tests' | 'knowledge' | 'gates' | 'toolsets' | 'procedures' | 'rules';

/** One thing the next release needs that this tenant's environment does not have yet. */
export interface ReadinessGap {
	key: string;
	area: ReadinessAreaKey;
	/** The exact item (`strata_ingest.vision`, `case.read@test`, `PAY-04`). */
	subject: string;
	title: string;
	/** Why it blocks, in the customer's terms. */
	why: string;
	/** Which part of the release introduced the need (`channel Telegram`), or null when the template always needs it. */
	cause: string | null;
	blocks: ReadinessBlocks;
	fixer: ReadinessFixer;
	fix_label: string;
	href: string;
	/** Build milestone from which apply enforces this check (M1 = W5 slots only). */
	enforced_from: 'M1' | 'M4';
}

/** One kind of thing the job needs, with how much of it is in place. */
export interface ReadinessArea {
	key: ReadinessAreaKey;
	label: string;
	/** The question this check answers. */
	question: string;
	/** What the template layers ask for (inherited, read-only here). */
	template_part: string;
	/** What the tenant supplies. */
	tenant_part: string;
	ready: number;
	total: number;
	status: 'ok' | 'gap' | 'warn' | 'todo';
	summary: string;
	enforced_from: 'M1' | 'M4';
	/** Page id + label where it is fixed (`J6 Operations & routes`). */
	page: string;
	href: string;
}

export interface ReadinessBoard {
	job: JobKey;
	/** The release checked: newest approved, else current. */
	release: {
		id: string;
		hash: string;
		state: string;
		approved_by_name: string | null;
		approved_at: Iso | null;
	} | null;
	current_id: string | null;
	/** blocked = apply refused · go_live_gaps = apply passes, gaps remain · ready · no_release = nothing cut yet. */
	verdict: 'blocked' | 'go_live_gaps' | 'ready' | 'no_release';
	gaps: ReadinessGap[];
	areas: ReadinessArea[];
	/** 10 §5.2 steps 1–10. */
	steps: ReadinessStep[];
}

/**
 * D67: why a release can or cannot go out, as the backend decides it. `blocking` is exactly what a publish (approve
 * with apply, or apply) refuses with — 409 `release_not_ready` carries the same items; `warnings` never block it.
 */
export type ReadinessCode =
	/** KH T1 (D106/D115): the selection has no industry / department, a department of another industry or one twice,
	 *  or there is no selection at all (`hierarchy_incomplete:selection`); `subject` = `<code>:<path>`. Blocking. */
	| 'hierarchy_incomplete'
	/** K25: a W5 slot the release uses has no provider deployment. Blocking. */
	| 'slot_not_deployed'
	/** 12 §3.5: a required operation has no ready Production route. Blocking (routing_ready = false). */
	| 'route_not_ready'
	/** 12 §3.5: a required operation has no ready Test-env route — blocks testing the draft, never publishing. */
	| 'test_route_not_ready'
	| 'contract_tests_pending'
	/** KH T8 (D110/D115): a department's required knowledge kind has nothing to resolve (no store of yours, no frozen
	 *  platform pack; a tenant-data kind needs your own source), or nothing in the job's primary language;
	 *  `subject` = `<department|job>:<stage>|<kind>[:<lang>]`. Blocking. Also the code of the knowledge warnings
	 *  (store not created, pack unavailable, another language uncovered). */
	| 'knowledge_unbound'
	| 'rule_placeholder'
	| 'gates_unreviewed'
	| 'procedure_unpublished';

/** One readiness finding: a machine code, the exact item, a sentence for people and where it is fixed. */
export interface ReadinessItem {
	code: ReadinessCode;
	/** The exact item (`strata_ingest.vision`, `case.read@prod`, `PAY-04`). */
	subject: string;
	message: string;
	/** Link target of the page that fixes it. */
	href: string;
}

/** `GET /api/jobs/{job}/release-readiness?of=release|draft` (D67). */
export interface ReleaseReadiness {
	job: JobKey;
	/** The release checked (newest approved, else current); null when nothing was cut yet. */
	release_id: string | null;
	/** Every required operation has a ready Production route (12 §3.5). */
	routing_ready: boolean;
	/** Publishing (apply) refuses while this is non-empty. */
	blocking: ReadinessItem[];
	warnings: ReadinessItem[];
}

// ---- D54 draft → test → publish ----------------------------------------------------------------------------------

/** D54: one unpublished tenant change. Every tenant write is saved to the job's draft; the draft runs in the Test
 *  environment (test chat + test-env routes) and reaches Production only through a release cut → approve → apply. */
export interface DraftChange {
	/** Ledger row id (`d-12`): pass it to `discardDraft`. */
	id: string;
	/** Ledger target kind (`gate`, `rule_values`, `entity_attributes`, `route`, `procedure`, `journey`, …). */
	kind: string;
	/** Design area the change belongs to (`gates`, `ontology`, `toolsets`, `knowledge`, `rules`, `routes`, …). */
	area: string;
	/** Page id that owns the change (`J7`, `J11`, …), for "N here" counts and links. */
	page: string;
	/** One line a reader understands: `GM · “request” relabelled “Arrange it for me”`. */
	label: string;
	/** What was changed (`gate:GM:request`, `narrow:procedure:act:op`, `rule:RET-01`, …) so a page can mark the row. */
	key: string;
	at: Iso;
	by: Sub;
	by_name: string;
	/** What Production runs at this location (the live value), and the draft the test chat runs — one line each. */
	live: string | null;
	draft: string | null;
	/** The approved, not yet applied release that already carries this change (it ships when that release is
	 *  applied), or null: it waits for the next cut. */
	in_release: string | null;
	/** A rollback moved the live value under this unpublished change (the draft itself is untouched). */
	live_moved?: { release: string; at: Iso; was: string | null; matches_live: boolean } | null;
	/**
	 * The live release whose snapshot does not store this change's kind (a kind newer than that release): Production
	 * reads the saved value until it is covered, and publishing makes this draft value part of the release. Null when
	 * covered (owner ruling 2026-10-22: the operator sees this before publishing).
	 */
	uncovered?: string | null;
}

/** D54: the job's unpublished changes, newest first. `count` = ledger rows = `changes.length`. */
export interface DraftSummary {
	count: number;
	changes: DraftChange[];
	/** D80: ledger kinds whose owning plan has not registered a release target yet (agent-api `design::PENDING`):
	 *  changes of these kinds are sample data and are not included in releases. The live API sends it; the fixture
	 *  world (everything is sample data there) leaves it out. */
	pending_kinds?: string[];
}

/** D54: one change a release made live (`GET /api/jobs/{job}/draft/published`). */
export interface PublishedChange extends DraftChange {
	published_in: string;
	published_at: Iso;
	published_by: Sub;
	/** A rollback took this change out of Production (to the release `to`); null/absent = still live or superseded
	 *  by a later change at the same place. */
	rolled_back?: { to: string; at: Iso; by: Sub; by_name: string } | null;
	/** "Restore to draft" put it back in the draft after the rollback (it ships again with the next release). */
	restored_to_draft?: { at: Iso; by: Sub; by_name: string } | null;
}

// ---- D62: releases snapshot the live design ------------------------------------------------------------------------

/** One design place and its value, one line (`rule:RET-01` → `window_days = 30 · …`). */
export interface DesignValue {
	/** Place key (`gate:GM:request`, `rule:RET-01`, `entity:Order`, `route:delivery.redirect:prod`, …). */
	key: string;
	/** Snapshot kind (`gate`, `rule_values`, `entity`, `route`, `activity`, …). */
	kind: string;
	ref: Record<string, string | null>;
	text: string | null;
}

/** D62: a job's design at every place — `live` = the active release's stored snapshot, `draft` = the working rows,
 *  `release` = the snapshot one release stores (immutable once cut). */
export interface DesignState {
	of: 'live' | 'draft' | 'release';
	/** The release whose snapshot this is (`live`: the active one; null = the job was never released). */
	release_id: string | null;
	/** The job's draft revision (`draft` only). */
	draft_rev: number | null;
	values: DesignValue[];
}

/** One place where the draft differs from live (the active release's snapshot). */
export interface DesignDiffRow {
	key: string;
	kind: string;
	ref: Record<string, string | null>;
	live: string | null;
	draft: string | null;
	/** The unpublished changes saved at this place (change history: who changed what), oldest first. */
	changes: { id: string; key: string; label: string; at: Iso; by: Sub; by_name: string }[];
}

/** D62: draft vs live, computed from the stored snapshot of the active release (never from the change history). */
export interface DesignDiff {
	release_id: string | null;
	draft_rev: number;
	rows: DesignDiffRow[];
}

/** D69: one job whose Production still runs another value at a tenant-wide design place. */
export interface TenantDesignJob {
	job: string;
	job_label: string;
	/** What this job's Production runs (its active release's snapshot). */
	live: string | null;
	/** Unpublished saves recorded in this job's ledger at that place, oldest first. */
	changes: DesignDiffRow['changes'];
}
/** D69: a tenant-wide design place (provider binding/slot, mapping override, tenant template) whose draft differs
 *  from the live value of at least one job. */
export interface TenantDesignRow {
	key: string;
	kind: string;
	ref: Record<string, string | null>;
	draft: string | null;
	/** Only the jobs whose live value differs from the draft. */
	jobs: TenantDesignJob[];
}
export interface TenantDesignDiff {
	rows: TenantDesignRow[];
}

// ---- Rollback (05 §4.4 + D54: a release carries the full design state it put live) -------------------------------

export type RollbackDirection = 'rollback' | 'reapply' | 'apply';

/** One published tenant design change a rollback takes out of Production (or a re-apply puts back). */
export interface RollbackChange {
	/** Published row id (`pub-3`) — pass it to `restoreRolledBack`. */
	id: string;
	key: string;
	kind: string;
	area: string;
	page: string;
	label: string;
	published_in: string;
	published_at: Iso;
	/** What Production runs at that place now, and what it runs after the rollback. */
	now: string | null;
	after: string | null;
}

/** An unpublished draft change whose live value the rollback moves (the draft value itself stays). */
export interface RollbackDraftEffect {
	id: string;
	key: string;
	label: string;
	live_now: string | null;
	live_after: string | null;
	draft: string | null;
	/** After the rollback the draft equals live at this place: publishing it changes nothing there. */
	matches_live: boolean;
}

/** `GET /api/jobs/{job}/releases/{id}/rollback-preview` — what making `target` current would do, before it is done. */
export interface RollbackPreview {
	target: string;
	current: string | null;
	direction: RollbackDirection;
	/** Bundles of the current and the target release, for the customer-visible impact (J15 `customerImpact`). */
	bundle_now: BundleSection[];
	bundle_after: BundleSection[];
	/** `channel − telegram · region − de` (selection now → target), or null when the same. */
	selection_change: string | null;
	pins: { layer: string; from: number | null; to: number }[];
	/** Published design changes customers lose (rollback) and changes they get back (re-apply). */
	undone: RollbackChange[];
	reapplied: RollbackChange[];
	/** Other design places whose live value moves (no published row names them, e.g. seeded state). */
	draft: RollbackDraftEffect[];
	/** Apply would refuse with 409 release_not_ready for these (the rollback re-validates). */
	missing: MissingItem[];
	/** Number of design places whose live value changes. */
	design_changes: number;
}

/** One rollback (or re-apply) as the history and J15 show it. */
export interface RollbackEvent {
	id: string;
	at: Iso;
	by: Sub;
	by_name: string;
	direction: RollbackDirection;
	/** The release that was current, and the one made current. */
	from: string | null;
	to: string;
	undone: { id: string; key: string; label: string; published_in: string }[];
	reapplied: { id: string; key: string; label: string; published_in: string }[];
	/** Unpublished draft changes whose live value moved. */
	draft_moved: number;
}
