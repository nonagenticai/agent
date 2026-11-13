// Shapes for the "platform" area (spec 14 §3; strata_configurator `web/src/lib/api.ts` types for layers, rows,
// origins and the effective config, minus Target/PushStep/Drift). Snake_case as the API will send them.
import type {
	Iso,
	JobKey,
	JobSummary,
	JsonObject,
	Residency,
	Selection,
	Sub,
	Tenant,
	TenantId,
	TenantUser,
	TemplatePin,
} from './core';
import type { TokenMode } from './tenant';

// ---- tenants (P1, P2) -------------------------------------------------------------------------------------------

/** Provisioning targets (06 §4.5): the licences service, strata_rag ("Knowledge") and strata_ingest ("Ingest"). */
export type ProvisionTarget = 'licences' | 'strata_rag' | 'strata_ingest';
export type ProvisionOp = 'licence_ensure' | 'licence_revoke' | 'connections_put' | 'store_ensure';
export type ProvisionStatus =
	| 'pending'
	| 'in_flight'
	| 'succeeded'
	| 'failed'
	| 'dead'
	| 'superseded'
	| 'cancelled';

/** One `provision_outbox` row joined with its target's `provision_state` (K22, K45). */
export interface ProvisionRow {
	id: string;
	tenant_id: TenantId;
	target: ProvisionTarget;
	op: ProvisionOp;
	status: ProvisionStatus;
	attempts: number;
	next_attempt_at: Iso | null;
	last_error: { code: string; message: string } | null;
	last_success_at: Iso | null;
	/** A remote version moved by another app (never overwritten, only shown). */
	drift: string | null;
	updated_at: Iso;
}

/** Worst provisioning state of a tenant, for the registry badge. */
export type ProvisionBadge = 'ok' | 'pending' | 'failed' | 'dead';

/** `GET /api/tenants` row as the platform registry shows it. */
export interface PlatformTenantRow extends Tenant {
	jobs: number;
	users: number;
	provisioning: ProvisionBadge;
	/** Rows in the worst state (for "2 dead"). */
	provisioning_count: number;
}

/** The tenant's agent licence (gate 2). The sealed token is never sent; only its key id. */
export interface TenantLicence {
	licence_id: string;
	subject_id: string;
	holder_sub: Sub;
	app: 'agent';
	plan: string;
	/** `v2:<kid>:` — the key the stored token is sealed with. */
	kid: string | null;
	introspect_status: 'active' | 'inactive' | 'revoked';
	issued_at: Iso;
}

/** What an offboard did (04 §7b). rag/ingest keep the tenant's data, so the report is never complete. */
export interface OffboardResult {
	tenant_id: TenantId;
	at: Iso;
	by: Sub;
	licence_revoked: boolean;
	archived: boolean;
	users_disabled: number;
	outbox: { target: ProvisionTarget; op: ProvisionOp; status: ProvisionStatus }[];
	data_retained: { app: 'strata_rag' | 'strata_ingest'; what: string }[];
	complete: false;
}

/** `GET /api/tenants/{id}` (platform view): one tenant end to end. */
export interface TenantDetail {
	tenant: Tenant;
	users: TenantUser[];
	jobs: JobSummary[];
	licence: TenantLicence | null;
	verdict: LicenceVerdict | null;
	provisioning: ProvisionRow[];
	offboard: OffboardResult | null;
}

export interface CreateTenantInput {
	name: string;
	first_admin_email: string;
	/** Both REQUIRED (422 `first_admin_name_required`): the portal profile needs them, so the invitee only sets a password. */
	first_admin_first_name: string;
	first_admin_last_name: string;
	/** D59/D66: data residency, chosen here and immutable after. Pins strata stores, LLM providers and logs. */
	residency: Residency;
}

export interface CreateTenantResult {
	tenant: Tenant;
	/** null when the licences service issued none yet (the licence tick heals it); `kid` null until sealed. */
	licence: { id: string; kid: string | null } | null;
	provisioning: ProvisionRow[];
	/** The first admin was not in Keycloak yet: an invite went out. */
	invited: boolean;
}

export interface ImportTenantInput {
	id: string;
	name: string;
	/** An email (invite) or a Keycloak subject. */
	first_admin: string;
	/** Required when `first_admin` is an email (as on create); ignored for a subject. */
	first_admin_first_name?: string;
	first_admin_last_name?: string;
	/** D66: data residency, chosen at import (creation) and immutable after. */
	residency: Residency;
}

export interface ImportTenantResult {
	tenant: Tenant;
	/** false = that id was imported already (idempotent; nothing changed). */
	imported: boolean;
}

/** A job template tag a super_admin can create a job from (D15 pins). */
export interface JobTemplateTag {
	layer: string;
	version: number;
	label: string;
	industry: string | null;
	/** Pins a job created from it gets (base, industry, department, job). */
	pins: TemplatePin[];
	status: 'tagged' | 'draft';
}

export interface CreateJobInput {
	template: string;
	key: string;
	label: string;
}

// ---- signups (P3) -----------------------------------------------------------------------------------------------

export type SignupStatus = 'pending' | 'approved' | 'rejected';

/** An early-access signup (`signup_approvals`, 04 §8). The email is PII: lists show it masked. */
export interface Signup {
	id: string;
	name: string;
	email: string;
	company: string;
	industry: string;
	message: string | null;
	source: 'website' | 'telegram' | 'referral' | 'invite';
	status: SignupStatus;
	created_at: Iso;
	suggested_tenant_name: string;
	decided_at: Iso | null;
	decided_by: Sub | null;
	reason: string | null;
	/** Set once approved: the workspace it created. */
	tenant_id: TenantId | null;
	/** P3: what intake noticed about the request (bot check, domain, duplicates, earlier attempts). */
	signals?: SignupSignal[];
}

/** P3: one intake observation shown next to a request. `risk` = a reason to reject, `info` = context. */
export interface SignupSignal {
	key: 'turnstile' | 'domain' | 'duplicate' | 'history' | 'detail';
	level: 'ok' | 'info' | 'risk';
	label: string;
	detail: string;
	/** The platform audit action this signal is based on (links to Platform audit filtered by it). */
	audit_action?: string;
}

/** P3: one prerequisite of turning the dark signup feature on (names only, never values). */
export interface SignupLaunchItem {
	key: string;
	label: string;
	/** Env var / Keycloak client name it hinges on. */
	name: string;
	status: 'set' | 'missing';
	detail: string;
}

/** P3: the dark-launch switch (K21) — what is in place and what flipping it mounts. */
export interface SignupLaunch {
	/** `EARLY_ACCESS_SIGNUP_ENABLED` in the deployment. False everywhere today (K21). */
	enabled: boolean;
	items: SignupLaunchItem[];
	/** Routes that exist only while the flag is on. */
	routes: { method: string; path: string; guard: string }[];
}

export interface SignupQueue {
	items: Signup[];
	next: string | null;
	/** Intake refuses (503) once this many are pending. */
	queue_cap: number;
	pending: number;
	/** P3 funnel: totals over every request, whichever view is listed. */
	counts?: { received: number; approved: number; rejected: number };
	/** Server clock, for "waiting 2 d". */
	as_of?: Iso;
}

// ---- layers + compile (P4, P5) ----------------------------------------------------------------------------------

export type LayerScope = 'base' | 'profile' | 'industry' | 'base_dept' | 'industry_dept' | 'job';
export type RowKind =
	| 'entity'
	| 'attribute'
	| 'relation'
	| 'job'
	| 'context_space'
	| 'procedure'
	| 'connector_slot'
	| 'knowledge_store';
export type RowOp = 'add' | 'patch' | 'tombstone';

/** A layer (05 §2.1). No `tenant` scope: tenant changes are refinements (D8). */
export interface Layer {
	id: string;
	scope: LayerScope;
	industry?: string;
	department?: string;
	job?: string;
	/** Profile layers hold the rows one axis rule adds. */
	rule?: string;
	dimension?: [string, string];
	/** D111: the region of a region sub-layer (`industry/retail/region/nl`): a base, industry or industry_dept layer's
	 *  add-on, in its parent's tag family, folded right after its parent when the selection names the region. */
	region?: string;
	/** D60: an `industry_dept` layer specialises one base department archetype (`customer_service`); D116: its
	 *  `base_dept/<extends>` layer, when present, folds right before the department. */
	extends?: string;
	/** Template tag the current revisions belong to (`industry/retail@v2`), when tagged. */
	tag: string | null;
	/** When `tag` was cut (P4): current revisions created after it are drafts not in the tag yet. */
	tagged_at?: Iso;
}

/** Per-layer rollup for the P4 navigator: what the layer does, who wrote it, how much is not tagged yet. */
export interface LayerSummary {
	layer_id: string;
	/** Current revisions by op. */
	adds: number;
	patches: number;
	tombstones: number;
	/** Retired (superseded) revisions kept for history. */
	retired: number;
	/** Distinct authors of current revisions, most recent first. */
	authors: Sub[];
	last_change_at: Iso | null;
	/** Current revisions created after `tagged_at` (all of them when the layer has no tag). */
	untagged: number;
}

/** What a `patch`/`tombstone` row acts on: the nearest earlier layer whose current row `add`s that key. */
export interface LayerOverride {
	row_id: string;
	target: Layer | null;
	target_row_id: string | null;
}

/** One overlay row revision. Append-only: a change retires the current revision and adds a new one. */
export interface LayerRow {
	id: string;
	layer_id: string;
	kind: RowKind;
	key: string;
	op: RowOp;
	rev: number;
	fields: JsonObject;
	is_current: boolean;
	author: Sub;
	created_at: Iso;
}

/** Where an effective item came from: the row that built it, in layer order. */
export interface Origin {
	row_id: string;
	layer: Layer;
	op: RowOp;
	/** The row's fields as that layer wrote them (what an `add` set, what a `patch` replaced) — P5 override view. */
	fields?: JsonObject;
}

/** P5: what a compile preview compiles — a selection (D65), plus the job layer on top (null = no job layer). */
export interface CompileTarget {
	selection: Selection;
	job: JobKey | null;
	/** F-07 (T12a): compile these tags instead of the head (absent = the head). */
	as_of?: TemplatePin[];
}

/** F-21 (T12b) `POST /api/layers` — a new platform layer (D111: `region` makes a `…/region/<r>` sub-layer). */
export interface LayerInput {
	scope: LayerScope;
	industry?: string;
	department?: string;
	job?: string;
	dimension?: [string, string];
	region?: string;
	label: string;
	extends?: string;
	areas?: string[];
}

/** F-21 (T12b) `POST /api/layers/{id}/rows` — one authored revision; `patch` / `tombstone` (retire) need `expected_rev`. */
export interface LayerRowInput {
	kind: RowKind;
	key: string;
	op: RowOp;
	fields: JsonObject;
	expected_rev?: number;
}

export type Items = Record<string, JsonObject>;

export interface EffectiveConfig {
	hash: string;
	entities: Items;
	attributes: Items;
	relations: Items;
	job: JsonObject | null;
	job_content: {
		context_spaces: Items;
		procedures: Items;
		connectors: Items;
		knowledge_stores: Items;
	};
	provenance: Record<string, Origin[]>;
	warnings: { code: string; key: string; message: string }[];
	/** Layers that contributed, in order. */
	layers: string[];
	/** Axis rules that fired for the profile. */
	rules_fired: string[];
}

export interface CompileError {
	/** A {@link CompileErrorCode}, or another compile code (`duplicate_add`, `invalid_row`, …). */
	code: string;
	key: string;
	row_id: string | null;
	message: string;
	/** KH T3: the layers the error names as data (a `department_conflict`'s sibling layers, in fold order). */
	layers?: string[];
}

/**
 * KH T3 compile codes (D107, D112, D115), as the pinned compile and the cut's 422 `compile_failed` report them:
 * - `department_conflict` — two sibling departments (or their archetypes) set one key differently, or one removes
 *   what the other sets; `key` = `<kind>:<key>`, the message names the layers; a job-layer row of the key decides;
 * - `kind_required_downgrade` — a lower layer makes a required knowledge kind optional (only false → true);
 * - `pack_version_conflict` — two departments pin one pack key at two versions (a release freezes one);
 * - `pack_not_applicable` — a WARNING, never an error: a pack reference that matches none of the job's kinds.
 * `pack_kind_undeclared` is the 422 of a `knowledge_pack` row write naming an undeclared `stage|kind`.
 */
export type CompileErrorCode =
	| 'department_conflict'
	| 'kind_required_downgrade'
	| 'pack_version_conflict'
	| 'pack_not_applicable'
	| 'pack_kind_undeclared';

export interface CompileOptions {
	industries: { key: string; label: string }[];
	departments: {
		industry: string;
		key: string;
		label: string;
		areas: { key: string; label: string }[];
		/** D60: the base department archetype this industry department specialises. */
		extends?: string;
	}[];
	jobs: { key: string; label: string }[];
	profiles: { key: string; label: string; industry: string | null }[];
	dimensions: { key: string; label: string; values: { key: string; label: string }[] }[];
}

/**
 * A named selection P5 compares against (`GET /api/compile/references`): a running job's release selection, or a
 * template starting point. Compiled on today's rows — compile-as-of a template tag arrives with D15 (M4).
 */
export interface CompileReference extends CompileTarget {
	key: string;
	label: string;
	/** One line: what this selection is and why you would compare against it. */
	note: string;
	/** The release this selection was cut into, when it belongs to a running job. */
	release?: { id: string; tenant_id: TenantId; tenant: string; job: string; status: 'current' | 'approved' };
}

// ---- business models + dimensions (P12, P13) --------------------------------------------------------------------

export interface AxisValue {
	key: string;
	label: string;
	parent: string | null;
	added_at: Iso;
	/** Who added the value (axis values are insert-only, B6-8). */
	added_by: Sub;
	/** F-26: when the value was retired (kept for history and older tags; a new selection may not name it). */
	retired_at?: Iso;
	retired_by?: Sub;
}

/** F-25 `POST /api/axes` — a new business axis (insert-only vocabulary). */
export interface AxisInput {
	key: string;
	label: string;
	description: string;
	cardinality: 'one' | 'many';
	object: string;
	milestone?: 'M1' | 'M4';
}

export interface Axis {
	key: string;
	label: string;
	description: string;
	cardinality: 'one' | 'many';
	/** Which object the axis classifies (Party, Offering, Order, Renewal, Delivery, Payment, Contract). */
	object: string;
	values: AxisValue[];
	/** Milestone the axis arrives in (offering split is M4). */
	milestone: 'M1' | 'M4';
}

export type AxisCondition =
	| { eq: [string, string] }
	| { in: [string, string[]] }
	| { and: AxisCondition[] }
	| { or: AxisCondition[] }
	| { not: AxisCondition };

export interface AxisRule {
	key: string;
	priority: number;
	condition: AxisCondition;
	/** Objects (entities) the rule adds, and how many rows its profile layer holds. */
	adds: string[];
	rows: number;
	layer_id: string;
	created_by: Sub;
	created_at: Iso;
}

/** A business model (configurator profile): axis values per axis. */
export interface BusinessProfile {
	key: string;
	label: string;
	industry: string | null;
	description: string;
	selection: Record<string, string[]>;
	/** Extra notes shown with the profile (party roles, freemium offerings). */
	notes: string[];
	updated_at: Iso;
	updated_by: Sub;
}

export interface RenewalPreset {
	key: string;
	label: string;
	example: string;
	order_type: string;
	initiator: string;
	mode: string | null;
}

/**
 * D59: what a dimension does. A dimension "selects or parametrises content in the layer stack":
 * `dimension` = a real one (channel switches job × channel layers on), `grouping` = areas (groupings of context
 * spaces inside a department, D60), `placeholder` = recorded in the release, selects nothing (region).
 */
export type DimensionKind = 'dimension' | 'grouping' | 'placeholder';

/** F-27 `POST /api/dimensions` — a new dimension (insert-only vocabulary; `kind` absent = `dimension`). */
export interface DimensionInput {
	key: string;
	label: string;
	description: string;
	kind?: DimensionKind;
}

/** D59: one of the four concerns the old "region" dimension mixed up, and where each now lives. */
export interface RegionConcern {
	key: 'residency' | 'jurisdiction' | 'languages' | 'market_operations';
	label: string;
	/** Who owns it now. */
	owner: 'tenant' | 'job' | 'jurisdiction_pack' | 'tenant_wiring';
	/** `live` = modelled in the UI today; `future` = a concept only; `note` = wiring, no UI. */
	status: 'live' | 'future' | 'note';
	text: string;
	examples: string[];
	/** Where it is set (route + label), when it has a page. */
	where?: { href: string; label: string };
}

export interface Dimension {
	key: string;
	label: string;
	description: string;
	values: { key: string; label: string; added_at: Iso; added_by: Sub; note?: string }[];
	/** D59 (absent = `dimension`). */
	kind?: DimensionKind;
	/** D59: region only — the four concerns it was split into. */
	concerns?: RegionConcern[];
}

/** The six prompt slots (D108) an archetype instruction may fill. */
export type ArchetypeInstructionSlot = 'identity' | 'policy' | 'tone' | 'safety' | 'escalation' | 'format';

/** An archetype's instruction for one slot: the fields of an `instruction` row (`mode` absent = `replace`). */
export interface ArchetypeInstruction {
	text: string;
	language?: string;
	mode?: 'replace' | 'append';
	summary?: string;
}

/**
 * D60: a BASE department archetype (base layer content). Generic work every department of that kind does in any
 * industry; an `industry_dept/<industry>/<dept>` layer `extends` it with industry specifics ("any marketing
 * department does marketing work; marketing for Amazon vs Tesla differs").
 */
export interface DepartmentArchetype {
	key: string;
	label: string;
	description: string;
	/** Generic activities (journey stages the department works). */
	activities: string[];
	/** Request taxonomy: the kinds of request it handles. */
	request_taxonomy: string[];
	knowledge_kinds: string[];
	metrics: string[];
	/** Approval-rule slots an industry/tenant fills (thresholds, four-eyes…). */
	approval_rule_slots: string[];
	/**
	 * Owner ruling 2026-10-23: prompt slots the archetype carries once for every department that extends it — an
	 * `instruction` row's fields per slot, folded at the archetype's step (after the industry, before the
	 * department). Absent = none.
	 */
	instructions?: Partial<Record<ArchetypeInstructionSlot, ArchetypeInstruction>>;
	/** The base layer that carries it. */
	layer_id: string;
	/** Industry departments (`industry_dept` layers) that extend it. */
	specialised_by: {
		industry: string;
		key: string;
		label: string;
		layer_id: string | null;
		/** The areas (groupings of its context spaces) inside this department. */
		areas: string[];
	}[];
}

/** P13: one place a standard dimension value is referenced. */
export interface DimensionValueUsage {
	dimension: string;
	value: string;
	/** Job layers qualified by this value (`job/customer_care_agent × channel=email`) and the row keys they add. */
	layers: { id: string; path: string; job: string | null; row_id: string | null; adds: string[] }[];
	/** Context spaces grouped under this area (`retail:order-support:*`, D60). */
	context_spaces: string[];
	/** Job selections (tenant · job) that name the value today. */
	selections: { tenant_id: TenantId; tenant_name: string; job: string; job_label: string }[];
	/** Releases whose frozen selection names the value (re-cutting them must find it again). */
	releases: {
		id: string;
		tenant_id: TenantId;
		tenant_name: string;
		job: string;
		state: string;
		at: Iso | null;
	}[];
}

/** P13: one job selection, reduced to the dimension values it names. */
export interface DimensionSelectionRow {
	tenant_id: TenantId;
	tenant_name: string;
	job: string;
	job_label: string;
	selection: Selection;
	/** Platform dimension key → chosen values. */
	values: Record<string, string[]>;
	updated_at: Iso | null;
}

/** `GET /api/dimensions/usage` (super_admin) — where every standard value is used (P13). */
export interface DimensionUsage {
	/** Platform dimension key → the key a job selection stores it under (identity since D60: `area` → `area`). */
	selection_keys: Record<string, string>;
	values: DimensionValueUsage[];
	selections: DimensionSelectionRow[];
	/** Values a selection names that are not standard. Never expected (D8); shown as an error when present. */
	unknown: { dimension: string; value: string; tenant_name: string; job: string }[];
}

// ---- platform audit + licensing (P19, P20) ----------------------------------------------------------------------

export interface PlatformAuditEvent {
	id: string;
	at: Iso;
	actor_id: string;
	actor_type: 'user' | 'system';
	action: string;
	/** The tenant the event is about; null for pre-create failures and signups. */
	subject_tenant: TenantId | null;
	subject_label: string;
	status: 'ok' | 'failed';
	/** Ids and codes only, never secrets. A null value is a reset to the platform default (`tenant.attachment_quota`). */
	details: Record<string, string | number | boolean | null>;
	/** Display name of a `user` actor (`preferred_username`, 04 §9b), resolved at read time. */
	actor_name?: string | null;
	/** Failed events only: was a later success on the same subject recorded (P19)? */
	outcome?: {
		state: 'resolved' | 'open';
		by_id: string | null;
		by_action: string | null;
		by_at: Iso | null;
	};
}

/** P19 grouping of the platform audit vocabulary. */
export type PlatformAuditFamily = 'signup' | 'creation' | 'keys' | 'catalogue' | 'other';

/** `GET /api/platform/audit/summary` (P19): counts over the whole platform log, independent of filters. */
export interface PlatformAuditSummary {
	total: number;
	failed: number;
	/** Failed events no later success has resolved — what a super_admin still has to act on. */
	open: PlatformAuditEvent[];
	families: { family: PlatformAuditFamily; count: number; failed: number; open: number }[];
	actors: { actor_id: string; actor_name: string | null; actor_type: 'user' | 'system'; count: number }[];
	actions: string[];
}

/** `tenant_licence_verdict` row (K35): what the edge reads. Older than 5 minutes = the edge fails closed. */
export interface LicenceVerdict {
	tenant_id: TenantId;
	status: 'active' | 'inactive' | 'revoked' | 'expired';
	checked_at: Iso;
	reason: string | null;
	/** Seconds since `checked_at`, against the fixture clock. */
	age_seconds: number;
	/** The edge refuses turns: status not active or row stale (> 300 s). */
	fails_closed: boolean;
}

export interface KeyringKid {
	kid: string;
	current: boolean;
	/** `null` live: agent-api does not record when a kid was added. */
	added_at: Iso | null;
	/** Tenant tokens still sealed with this kid. 0 = safe to drop from LICENSE_TOKEN_ENC_KEYS. */
	sealed: number;
	/** Live only: `false` = a stored token names a kid LICENSE_TOKEN_ENC_KEYS does not hold (it does not open). */
	configured?: boolean;
}

/** R2: the newest re-issue / re-mint of a tenant (`licence_reissue`). Never a token. */
export interface LastReissue {
	at: Iso;
	kind: 'reissue' | 'remint';
	reason: 'expired' | 'revoked' | 'inactive' | 'unreadable' | 'leaked' | 'requested';
	actor: string;
	holder: 'tenant' | 'member';
	member_sub: Sub | null;
	old_licence_id: string | null;
	new_licence_id: string | null;
	old_revoke: 'revoked' | 'pending' | 'none';
	status: 'reissued' | 'pending';
	/** The old licence's revoke is still parked: a leaked token stays valid until the retry tick revokes it. */
	old_still_valid: boolean;
}

export interface LicensingOverview {
	now: Iso;
	current_kid: string;
	keyring: KeyringKid[];
	tenants: {
		tenant_id: TenantId;
		name: string;
		status: Tenant['status'];
		licence: TenantLicence | null;
		verdict: LicenceVerdict | null;
		/** R2: the newest re-issue / re-mint (`null` = never). */
		last_reissue?: LastReissue | null;
		/** Live only: legacy `v1:` tokens still stored (the boot reseal could not open them). */
		v1_tokens?: number;
	}[];
	last_reseal: { at: Iso; resealed: number; remaining: number; kid: string } | null;
	/** Live only: `LICENSE_INTROSPECT_URL` is set (else no verdict row is ever written). */
	verdicts_configured?: boolean;
	/** `nudgeLicenceVerdicts` only: what the refresh pass did. */
	refreshed?: { tenants: number; written: number; unavailable: number; failed: number; repaired: number };
	/** P20: the `agent-strata` licence bundle every tenant holds (04 §5e). Optional: older callers ignore it. */
	bundle?: LicenceBundle;
	/** P20: `METERING_EMIT_ENABLED` — the ledger is always written; the billing outbox only when this is on (04 §6). */
	metering_emit_enabled?: boolean;
}

/** One app in the `agent-strata` bundle and how its per-tenant token is held (04 §5e, K28/K43/K53/K60). */
export interface LicenceBundleMember {
	app: 'agent' | 'vllm' | 'strata-rag' | 'strata-ingest';
	/**
	 * `sealed` = stored per tenant in `tenant_license` under a kid (gate 2); `minted` = minted on demand by
	 * `agent-bundle-minter`, memory only; `static` = interim static token until W5 lands (K28).
	 */
	holding: 'sealed' | 'minted' | 'static';
	/** What the token is used for, one line. */
	purpose: string;
	/** Minted tokens: TTL in minutes (refreshed at 80 %). */
	ttl_min?: number;
	/** The decision/call this row rests on. */
	ref: string;
}

export interface LicenceBundle {
	key: string;
	/** Client that may only mint bundle tokens (never issue licences). */
	minter: string;
	members: LicenceBundleMember[];
}

export interface ResealResult {
	kid: string;
	resealed: number;
	remaining: number;
	tenants: TenantId[];
}

/**
 * One holder's licence re-issue (F-31): the old licence is revoked, a new one minted in the tenant's residency region.
 * `pending` = the old revoke did not confirm in time (parked for the retry tick) or the new mint failed after the old
 * was revoked; the provisioning tick finishes it and `new_licence_id` is null until then.
 */
export interface ReissueResult {
	holder: 'tenant' | 'member';
	sub: Sub | null;
	old_licence_id: string;
	new_licence_id: string | null;
	old_revoke: 'revoked' | 'pending';
	status: 'reissued' | 'pending';
	code: string | null;
}

/** `POST /api/tenants/{id}/licence/reissue` answer. Members are never disabled by a re-issue. */
export interface ReissueAnswer {
	tenant_id: TenantId;
	/** `remint` = the leaked-token path (`remintTenantLicence`). */
	kind?: 'reissue' | 'remint';
	residency: Residency;
	members_unchanged: boolean;
	results: ReissueResult[];
}

// ---- P1 tenant portfolio ----------------------------------------------------------------------------------------

/** Where a tenant sits in its lifecycle (04 §7b). `provisioning` = created, provision-down rows still open (06 §4.6). */
export type TenantLifecycleStage = 'provisioning' | 'active' | 'suspended' | 'archived';

/** One thing a super_admin should look at on a tenant, and where it is fixed. */
export interface TenantAttention {
	kind: 'provisioning' | 'licence' | 'pins_behind' | 'generator' | 'not_live' | 'readiness';
	severity: 'err' | 'warn' | 'info';
	text: string;
	href: string;
}

/** One job of a tenant as the portfolio shows it: serving?, release, pins behind the latest tag, a generator run. */
export interface PortfolioJob {
	key: string;
	label: string;
	on_bus: boolean;
	release: { id: string; hash: string } | null;
	behind: { layer: string; pinned: number; latest: number }[];
	generator: { run: number; status: 'running' | 'blocked' | 'packaged' | 'abandoned'; step: string } | null;
	cases_7d: number | null;
	containment_7d: number | null;
}

/** `GET /api/tenants?view=portfolio` (super_admin) — P1: per tenant the owner, jobs, seats in use, 30-day usage
 *  and what needs attention. Derived from the registry, jobs, usage ledger, outbox and generator runs. */
export interface TenantPortfolio {
	tenant_id: TenantId;
	stage: TenantLifecycleStage;
	/** Provision-down per target (Licences, Knowledge, Ingest): the latest outbox row's status, null = never queued. */
	provision: { target: ProvisionTarget; status: ProvisionStatus | null; error: string | null }[];
	owners: { sub: Sub; display_name: string; email: string }[];
	members: { active: number; admins: number; disabled: number; invited: number };
	jobs: PortfolioJob[];
	usage_30d: { turns: number; tokens: number; last_day: string | null };
	attention: TenantAttention[];
}

// ---- P2 tenant footprint ----------------------------------------------------------------------------------------

/** One rag/ingest provider slot as P2 shows it: what provision-down pushed (06 §4.2). Keys never leave the API. */
export interface FootprintSlot {
	slot: string;
	service: 'strata_rag' | 'strata_ingest';
	label: string;
	configured: boolean;
	/** `platform` (internal model) or the tenant's own provider kind. */
	kind: string | null;
	model: string | null;
	version: number;
	/** The remote slot moved (another app wrote it): shown, never overwritten (K45). */
	drift: boolean;
}

/** One lifecycle event of the tenant (its own audit log, filtered to tenant/provision/licence/user/job/release). */
export interface FootprintEvent {
	at: Iso;
	actor_id: string;
	actor_type: 'human' | 'ai_agent' | 'system';
	action: string;
	resource_id: string;
	ok: boolean;
	job: string | null;
}

/** `GET /api/tenants/{id}/footprint` (super_admin) — P2: what one tenant holds in every downstream system (rag/ingest
 *  slots, knowledge stores, LLM providers), its portfolio summary (P1 shape) and its lifecycle history (06 §4.2, 04 §9). */
export interface TenantFootprint {
	tenant_id: TenantId;
	portfolio: TenantPortfolio;
	slots: FootprintSlot[];
	stores: { leaves: number; bound: number; unbound: string[]; documents: number };
	llm_providers: { name: string; token_mode: TokenMode; test_ok: boolean | null }[];
	/** The licence bundle `licence_ensure` pushes (04 §5e): its key and the per-app licences it yields. */
	bundle: { key: string; apps: string[] } | null;
	history: FootprintEvent[];
}

// ---- Industries + departments (plan 2026-10-30-super-admin-platform-fixes T3) -----------------------------------

/** A knowledge pack a layer references: the `knowledge_pack` row `<stage>|<kind>|<pack>` (`*` = every stage/kind). */
export interface LayerPackRef {
	pack: string;
	stage: string;
	kind: string;
	version: number | null;
}

/** A tenant job whose saved selection names the industry / department (retire is refused while there is one). */
export interface IndustryJobUse {
	tenant_id: TenantId;
	tenant_name: string;
	job: string;
	job_label: string;
}

/** An `industry_dept/<industry>/<department>` layer. */
export interface PlatformDepartment {
	department: string;
	key: string;
	label: string;
	/** The base department archetype it extends (D107); null = none. */
	extends: string | null;
	/** `<family>@v<n>`; null until first published — a tenant cannot choose it before. */
	tag: string | null;
	retired_at: Iso | null;
	knowledge: LayerPackRef[];
	jobs: IndustryJobUse[];
}

/** An `industry/<industry>` layer with its departments. */
export interface PlatformIndustry {
	industry: string;
	key: string;
	label: string;
	tag: string | null;
	retired_at: Iso | null;
	knowledge: LayerPackRef[];
	jobs: IndustryJobUse[];
	departments: PlatformDepartment[];
}

/** `GET /api/platform/industries`. */
export interface PlatformIndustries {
	/** The packs the `base` layer references (every tenant job inherits them). */
	base_knowledge: LayerPackRef[];
	items: PlatformIndustry[];
}
