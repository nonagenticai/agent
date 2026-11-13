// Shapes for the "connectors" area: connector templates (platform P16/P17 + tenant-private T12), connector
// instances with write-only credentials (T10/T11), contract results, and the job's expected operations + route
// matrix (J6). Field names follow spec 14 §3, doc 10 §4.2, doc 11 §6.1 and doc 12 §8 (snake_case, as the API sends).
import type { Iso, JobKey, Paged, Sub, TenantId } from './core';
import type { Idempotency as OperationIdempotency } from './model';
import type { ToolPolicy } from './iam';

export type Transport =
	| 'rest'
	| 'graphql'
	| 'platform'
	| 'stub'
	| 'soap'
	| 'graph'
	| 'ldap'
	| 'mcp'
	| 'table';
/**
 * v1 `auth_kind` (K49); `bundle_token` is the strata_rag platform connector's licence bundle. `mtls` presents the
 * credential's `client_cert_pem` + `client_key_pem` secrets as a TLS client certificate (no header); an optional
 * `ca_pem` is then the only trusted root. Certificate verification can never be turned off.
 */
export type AuthKind =
	| 'none'
	| 'api_key_header'
	| 'basic'
	| 'bearer_static'
	| 'oauth2_client_credentials'
	| 'bundle_token'
	| 'licence_bearer'
	| 'mtls';
export type TemplateStatus = 'draft' | 'active' | 'retired';
export type SideEffect = 'R' | 'W' | 'X' | 'P';
export type Modality = 'observe' | 'do' | 'request';
export type IdentityLevel = '0' | '1' | '2' | '3' | 'S' | '1–2' | '1→2' | '0→2';
export type Env = 'prod' | 'test';
/** Which wave a platform template belongs to (P16 grouping): v1 retail set, "next", ITSD wave (D26). */
export type TemplateWave = 'v1' | 'next' | 'itsd';
export type OpKind = 'standard' | 'hosted_payment_link' | 'builtin';

/** `object.verb{variant}`; `variant = '*'` is the unqualified operation. */
export interface OpRef {
	object: string;
	verb: string;
	variant: string;
}

/** One field of `credential_schema` (JSON Schema): `x-secret` fields → encrypted credential, the rest → config. */
export interface CredentialField {
	name: string;
	label: string;
	secret: boolean;
	hint?: string;
}

export interface RateLimitModel {
	/** Absent when the template declares no rate-limit model (the server sends `{}`). */
	kind?: 'rps' | 'rpm' | 'cost_bucket' | 'none';
	/** Human summary: `100 rps live / 25 sandbox`, `100–2,000 pts/s by plan`. */
	value?: string;
	honour_retry_after?: boolean;
}

/** Per-op idempotency mapping (doc 10 §4.2, doc 12 §3.7). `none` is refused for W/X/P at validation. */
export type Idempotency =
	| { kind: 'nat' }
	| { kind: 'header'; header: string }
	| { kind: 'dup_search'; on: string }
	| { kind: 'none' };

export interface TemplateFixture {
	name: string;
	/** HTTP status (or `200 graphql`) of the recorded, redacted response. */
	status: number;
	/** What the fixture proves: `ok`, `not_found`, `timeout`, `401 → connector_auth_failed`… */
	outcome: string;
	body: unknown;
	/**
	 * Input key (stub transport): without an `x-stub-fixture` header the stub answers with the first 2xx fixture whose
	 * `match` the rendered request satisfies (`path` equal; every `query` pair present with that value), else the first
	 * 2xx fixture without `match`, else — the op has keyed fixtures and none matched — its `not_found` fixture.
	 */
	match?: FixtureMatch;
}

export interface FixtureMatch {
	path?: string;
	query?: Record<string, string>;
}

/** A row of `connector_template_op` (doc 11 §6.1, doc 12 §8.1). */
export interface ConnectorTemplateOp extends OpRef {
	side_effect: SideEffect;
	op_kind: OpKind;
	/** `GET /admin/api/2026-10/graphql.json` or `POST /api/v2/tickets`. */
	method: string;
	path: string;
	/** Request template (GraphQL document or JSON body) with `{{canonical.field}}` / `{{secret.x}}` placeholders. */
	request: string;
	/** Response mapping: system JSONPath → canonical field. */
	field_map: { path: string; field: string; transform?: string }[];
	/** System status → canonical status of the object. */
	status_map: { system: string; canonical: string }[];
	/** Vendor fault → canonical error code. */
	fault_map: { system: string; code: string }[];
	pagination: string | null;
	idempotency: Idempotency;
	/** The read op that confirms an X/P write landed. */
	verify_read: string | null;
	timeout_ms: number;
	fixtures: TemplateFixture[];
}

export interface TierResult {
	status: 'green' | 'red' | 'not_run' | 'drift';
	at: Iso | null;
	note?: string;
}

/** `connector_template@version` (platform, C3) — P16/P17. */
export interface ConnectorTemplate {
	key: string;
	version: string;
	label: string;
	vendor: string;
	system_class: string;
	description: string;
	wave: TemplateWave;
	/** Platform-owned connector (strata_rag, agent_cases, builtin) — provisioned down, never picked. */
	platform_connector: boolean;
	transport: Transport;
	/** A second transport the template also offers (Demo Retail: `rest` + `stub`). */
	alt_transport?: Transport;
	auth_kind: AuthKind;
	/** `token_url_template`, header name, … */
	auth_detail: string;
	/**
	 * Machine-readable auth parameters the runner uses (B7 T6): `header` + `value_template` (api_key_header),
	 * `username_template` + `password_template` (basic), `token_url_template` + `scope_template` (oauth2 client
	 * credentials), `static_headers` (a JSON object of extra request headers). Values may use `{{config.*}}` /
	 * `{{secret.*}}`; `static_headers` values never `{{secret.*}}`, but also the server-side `{{tenant.id}}` (the
	 * calling tenant) and `{{system.env}}` (the env of the call, `test` | `prod`). CSRF prefetch (SAP OData writes): `csrf_fetch_path` (relative, starts with `/`) turns it on,
	 * `csrf_header` (default `x-csrf-token`) names the header — before every non-R op the runner GETs the path with
	 * `<header>: Fetch` and sends the answered token plus the session cookies on that write only.
	 */
	auth_params?: Record<string, string>;
	credential_schema: CredentialField[];
	rate_limit_model: RateLimitModel;
	vendor_api_pin: string;
	/** Inert metadata in v1 (pull only, K49). */
	webhooks: { topic: string; signature: string }[];
	status: TemplateStatus;
	t1: TierResult;
	t2: TierResult;
	published_by: Sub | null;
	published_at: Iso | null;
	/** Vendor version drift found by a T2 run → one re-record PR (doc 10 §4.5). */
	drift: { vendor_version: string; note: string; pr: string } | null;
	ops: ConnectorTemplateOp[];
}

/**
 * Body of `POST /api/platform/connector-templates` (super_admin): a new platform template as a DRAFT. `version`
 * defaults to `1`, `credential_schema` to the default of the auth kind, `ops` to none (add them with
 * `saveConnectorTemplateOp`; a `generic_rest` base needs none).
 */
export interface NewConnectorTemplateInput {
	key: string;
	version?: string;
	label: string;
	transport: Transport;
	auth_kind: AuthKind;
	vendor?: string;
	system_class?: string;
	description?: string;
	wave?: TemplateWave;
	platform_connector?: boolean;
	alt_transport?: Transport;
	auth_detail?: string;
	auth_params?: Record<string, string>;
	credential_schema?: CredentialField[];
	rate_limit_model?: RateLimitModel;
	vendor_api_pin?: string;
	webhooks?: { topic: string; signature: string }[];
	ops?: ConnectorTemplateOp[];
}

/** List row of `GET /api/platform/connector-templates`: the template minus per-op detail, plus implements[]. */
export interface ConnectorTemplateSummary extends Omit<ConnectorTemplate, 'ops'> {
	implements: OpRef[];
	/** Tenant instances pinned to this version. */
	instances: number;
}

/** A tenant-private template (D31): generic (built on generic REST) or custom (the definition of one in-house system). */
export interface TenantConnectorTemplate {
	tenant_id: TenantId;
	key: string;
	version: string;
	label: string;
	kind: 'generic' | 'custom';
	/** Always `generic_rest@2` in v1 — the declarative base (OQ-3). */
	base: string;
	transport: Transport;
	auth_kind: AuthKind;
	/** Server-side auth parameters (same keys as `ConnectorTemplate.auth_params`, incl. the CSRF prefetch). */
	auth_params?: Record<string, string>;
	credential_schema: CredentialField[];
	/** Operation keys (`object.verb`) this template runs as durable MCP tasks; empty for a platform template. */
	task_tools?: string[];
	status: TemplateStatus;
	description: string;
	ops: TenantTemplateOp[];
	validation: { at: Iso; errors: ValidationError[] } | null;
	contract: { at: Iso; status: 'green' | 'red'; passed: number; total: number } | null;
	created_by: Sub;
	updated_at: Iso;
}

/** `POST /api/connections/templates/{key}/{version}/mcp-import` (integrations_manager, draft templates only). */
export interface McpImportResult {
	/** Ops written to the draft from the external server's tools. */
	ops: {
		op: string;
		action: 'created' | 'updated';
		tool: string;
		side_effect: string;
		/** `true` = the tenant's own op (the platform catalogue does not define it; the template op carries `def`). */
		own: boolean;
		inputs: { name: string; type: string; description: string; required: boolean }[];
		outputs: string[];
	}[];
	/** Ops that would not validate: not written. */
	invalid_operations: { key: string; issues: { path?: string; code?: string; message?: string }[] }[];
	/** Tools that map to no operation, as plain sentences. */
	skipped: string[];
}

/** A tenant template op's own definition: absent = the platform catalogue defines the op. */
export interface TenantOpDef {
	side_effect: 'R' | 'W' | 'X' | 'P';
	input_fields: string[];
	output_fields: string[];
	description: string;
}

export interface TenantTemplateOp extends OpRef {
	/** The tenant's own definition of an op the platform catalogue does not know. */
	def?: TenantOpDef;
	method: string;
	path: string;
	request: string;
	field_map: { path: string; field: string; transform: string }[];
	status_map: { system: string; canonical: string }[];
	idempotency: Idempotency;
}

export interface ValidationError {
	/** `ops[0].field_map[2].transform` */
	path: string;
	code: string;
	message: string;
}

/** Write-only credential as the API reports it: never the value (D16/D32). */
export interface CredentialState {
	set: boolean;
	updated_at: Iso | null;
	fingerprint: string | null;
}

/** T3 tenant instance test, per (instance, op, env). */
export interface ContractResult {
	op: string;
	side_effect: SideEffect;
	outcome: 'green' | 'red' | 'write_unverified' | 'needs_ok' | 'not_run';
	code: string | null;
	note: string;
	at: Iso | null;
}

export interface FieldMappingOverride {
	op: string;
	field: string;
	default_path: string;
	path: string;
	updated_by: Sub;
	updated_at: Iso;
}

/** `connector_instance` (tenant) — T10/T11. */
export interface ConnectorInstance {
	id: string;
	tenant_id: TenantId;
	label: string;
	template_key: string;
	template_version: string;
	/** Where the pinned template lives. */
	template_scope: 'platform' | 'tenant';
	/** Platform connectors (strata_rag, agent_cases) serve both envs. */
	env: Env | 'all';
	base_url: string;
	env_ids: Record<string, string>;
	max_rps: number | null;
	stub: boolean;
	platform: boolean;
	/** The auth kind of the pinned template version. */
	auth_kind: AuthKind;
	/** Whether this system needs a stored credential: false for `none` and `licence_bearer` (minted per call). */
	credential_required: boolean;
	credential: CredentialState;
	status: 'ok' | 'auth_failed' | 'invalid_target' | 'draft_template' | 'untested';
	last_error: { code: string; message: string; at: Iso } | null;
	contract: ContractResult[];
	overrides: FieldMappingOverride[];
	/** Response fetched from the sandbox (or pasted) that JSONPath overrides are tried against. */
	sample: { op: string; fetched_at: Iso; body: unknown } | null;
	created_by: Sub;
	created_at: Iso;
}

/** Catalogue row for "Pick system": templates a tenant may instantiate and how many required ops they cover. */
export interface SystemCatalogueRow {
	key: string;
	version: string;
	label: string;
	vendor: string;
	scope: 'platform' | 'tenant';
	status: TemplateStatus;
	transport: Transport;
	auth_kind: AuthKind;
	credential_schema: CredentialField[];
	implements_required: number;
	required_total: number;
	implements_total: number;
	stub_capable: boolean;
	/** Canonical keys (`object.verb{variant}`) this version implements — T10 "fills your gap" (optional, additive). */
	implements?: string[];
}

export interface NewInstanceInput {
	id: string;
	label: string;
	template_key: string;
	template_version: string;
	env: Env;
	base_url: string;
	env_ids: Record<string, string>;
	max_rps: number | null;
	stub: boolean;
}

export type RouteTarget =
	| { kind: 'instance'; instance_id: string; detail?: string }
	| { kind: 'builtin' }
	| { kind: 'agent_cases' }
	| { kind: 'deep_link'; url: string }
	| { kind: 'not_offered' };

/** `operation_route(tenant, job, object, verb, variant, env)`. */
export interface OperationRoute extends OpRef {
	env: Env;
	target: RouteTarget;
	updated_by: Sub;
	updated_at: Iso;
}

/** A row of `eff_job_operations` plus its operation_def attributes. */
export interface ExpectedOperation extends OpRef {
	modality: Modality;
	is_derive: boolean;
	side_effect: SideEffect;
	identity_level: IdentityLevel;
	stages: string[];
	required: boolean;
	note: string;
}

export interface OpStats {
	op: string;
	calls_7d: number;
	error_rate: number;
	p95_ms: number;
}

/** What a cell of the route matrix shows after resolution (variant → `*` fallback, readiness). */
export interface ResolvedRoute {
	route: OperationRoute | null;
	/** The route came from the `*` row of a variant. */
	inherited: boolean;
	/** ok = served; untested = instance route without a green T3; stub = stub instance (never ready); missing. */
	readiness: 'ok' | 'untested' | 'stub' | 'missing' | 'not_offered';
	suggested: RouteTarget | null;
}

/** `GET /api/jobs/{job}/operations` (J6). */
/**
 * D67: whether one required/optional operation can run on one env, decided by the backend (doc 12 §3.5, doc 10
 * §4.5): a route to a healthy, non-stub system whose T3 contract test is green (prod writes after the tenant's OK;
 * X/P ops confirm-back instead), or a fallback. The same decision feeds routing_ready / 409 release_not_ready.
 */
export type CellState =
	| 'ready'
	| 'fallback'
	| 'unverified'
	| 'needs_ok'
	| 'untested'
	| 'red'
	| 'failing'
	| 'stub'
	| 'unrouted'
	| 'not_offered';

/** One op × env of the route matrix, as the backend resolves it. */
export interface RouteCell {
	state: CellState;
	/** The op actually runs for a customer (ready, fallback, or a confirm-back write). */
	runs: boolean;
	target: RouteTarget | null;
	/** The route came from the `*` row of a variant. */
	inherited: boolean;
	/** A suggested route when none exists. */
	suggested: RouteTarget | null;
	/** Instance id the route lands on (null for built-in / fallbacks / unrouted). */
	instance: string | null;
	/** One sentence: why this state, in the router's words. */
	why: string;
}

export interface JobOperations {
	job: JobKey;
	/** The job's draft revision (D63: no separate routing revision — routes ride the release). */
	draft_rev: number;
	ops: ExpectedOperation[];
	routes: OperationRoute[];
	stats: OpStats[];
	instances: Pick<
		ConnectorInstance,
		'id' | 'label' | 'env' | 'stub' | 'platform' | 'template_key' | 'template_version' | 'status'
	>[];
	/** `object.verb{variant}` each instance's template implements, keyed by instance id. */
	implements: Record<string, string[]>;
	/** Instances whose template is a draft: allowed on `test` only. */
	draft_instances: string[];
	/** T3-green ops per instance id. */
	tested: Record<string, string[]>;
	names: Record<Sub, string>;
	/** J6: every T3 result per instance id (red / needs your OK / write unverified, not only green). Optional, additive. */
	contract?: Record<string, Pick<ContractResult, 'op' | 'outcome' | 'code' | 'note' | 'at'>[]>;
	/** D54: `routes` is the draft (what the test env runs). `live_routes` is what Production runs, present only while
	 *  the draft differs from it (unpublished route changes). */
	live_routes?: OperationRoute[];
	/** D67: every op × env resolved by the backend, keyed `object.verb{variant}@env` (`case.create{return}@prod`). */
	cells: Record<string, RouteCell>;
	/** D67 + D54: the same cells resolved on `live_routes` (what Production runs); present with `live_routes`. */
	live_cells?: Record<string, RouteCell>;
}

/** J25: one tool the agent can call in this job — an expected operation with its `operation_def` contract, as the
 *  tenant reads it (purpose, inputs, outputs, the object statuses it changes). Routes, readiness and stats come from
 *  `jobOperations()`; the procedure steps that call it from `procedures()`. */
export interface JobTool extends OpRef {
	/** `order.cancel`, `case.create{return}`. */
	key: string;
	/** What it does, in the operation catalogue's words (falls back to the job's note when the catalogue has none). */
	description: string;
	input_fields: string[];
	output_fields: string[];
	/** The customer confirms before it runs (W/X/P, 12 §3.3 step 4). */
	confirm_back: boolean;
	/** How a retry is made safe (`operation_def.idempotency`); null for reads. */
	idempotency: OperationIdempotency | null;
	/** Status changes it makes on its object (from the object's state machine); empty for reads and derivations. */
	transitions: { object: string; from: string; to: string }[];
	/** A2: the job's draft declaration of this op's inputs/outputs; null when none. */
	policy: ToolPolicy | null;
}

/** `GET /api/connections/templates`: the page plus display names for created_by. */
export type TenantTemplatePage = Paged<TenantConnectorTemplate> & { names: Record<Sub, string> };

/** `GET /api/platform/connector-templates/{key}/{version}` (P17). */
export interface ConnectorTemplateDetail {
	template: ConnectorTemplate;
	/** Every version of this key, newest first. */
	versions: { version: string; status: TemplateStatus }[];
	/** Tenant instances pinned to this version (super_admin view). */
	pinned_by: { tenant: string; instance_id: string; env: Env | 'all' }[];
	/** Display names for the subjects in this response. */
	names: Record<Sub, string>;
}

/** `GET /api/connections/systems/{id}` (T11): the instance plus what its pinned template says. */
export interface SystemDetail {
	instance: ConnectorInstance;
	template: {
		label: string;
		vendor: string;
		status: TemplateStatus;
		transport: Transport;
		auth_kind: AuthKind;
		auth_detail: string;
		credential_schema: CredentialField[];
		ops: {
			op: string;
			side_effect: SideEffect;
			verify_read: string | null;
			field_map: { path: string; field: string }[];
		}[];
	};
	/** A newer active version of the same template key, when one exists. */
	newer_version: string | null;
	/** Jobs × env that route at least one operation to this instance. */
	used_by: { job: JobKey; env: Env; ops: string[] }[];
	/** D67: per template op, whether this system can serve it (routed here + green T3 on a non-stub instance). */
	op_readiness: Record<string, OpReadiness>;
	names: Record<Sub, string>;
}

/** T11 per-op readiness of one system, decided by the backend (doc 10 §4.5, doc 12 §3.5). */
export type OpReadiness = 'ready' | 'blocked' | 'needs_ok' | 'unverified' | 'untested' | 'stub' | 'unrouted';

export interface RouteWriteResult {
	/** The job's draft revision after the write, and before it. */
	draft_rev: number;
	previous_rev: number;
	route: OperationRoute | null;
}

/** One tenant system (connector instance) running any version of a platform template key — P17 "who runs it". */
export interface TemplateInstanceUse {
	tenant_id: TenantId;
	tenant: string;
	instance_id: string;
	label: string;
	env: Env | 'all';
	pinned_version: string;
	status: ConnectorInstance['status'];
	/** Operations at least one job routes to this instance (`object.verb{variant}`), any env. */
	routed_ops: string[];
	/** Tenant JSONPath overrides on this instance (they survive a re-pin only if the field still exists). */
	overrides: { op: string; field: string }[];
}

/** `GET /api/platform/connector-templates/{key}/{version}/usage` (P17): lineage neighbours + every pinning instance. */
export interface ConnectorTemplateUsage {
	/** The next older version of this key (what this version is diffed against), or null for the first one. */
	previous: ConnectorTemplate | null;
	/** The newest active version of this key — the only legal re-pin target (drafts and retired are refused). */
	latest_active: ConnectorTemplate | null;
	/** Instances on ANY version of this key, every tenant (super_admin view). */
	instances: TemplateInstanceUse[];
}

/** How completely one template version maps a system's statuses onto one object's canonical status set (P16). */
export interface StatusMapCoverage {
	object: string;
	/** Canonical states of the object (doc 11 object model, `canonical: true`). */
	canonical: string[];
	/** Canonical states at least one system status maps onto. */
	mapped: string[];
	/** Map targets that are not in the object's canonical set (rename them, or add the state to the object). */
	unknown: string[];
}

/** Per template version: what it covers, what blocks publishing, and which tenants pin it (P16). */
export interface TemplateCoverage {
	key: string;
	version: string;
	/** `generic_rest`: declares ops per tenant template, so it can serve any canonical operation. */
	any: boolean;
	/** Canonical catalogue keys (`object.verb{variant}`) this version implements. */
	implements: string[];
	status_maps: StatusMapCoverage[];
	/** Validation that would refuse publish (e.g. idempotency `none` on a W/X/P op); empty = publishable. */
	publish_blockers: string[];
	pinned_by: { tenant_id: TenantId; tenant: string; instance_id: string; env: Env | 'all' }[];
	/** Tenant-private templates built on this one (D31: generic REST is their base). */
	bases_of: { tenant: string; key: string; version: string; status: TemplateStatus }[];
}

/** A job archetype's expected operations, as the coverage view measures templates against them. */
export interface CoverageJob {
	key: JobKey;
	label: string;
	/** Expected ops (`object.verb{variant}`) with required = R. */
	ops: { key: string; required: boolean }[];
}

/** `GET /api/platform/connector-templates/coverage` — P16's catalogue view in one call. */
export interface ConnectorTemplateOverview {
	templates: ConnectorTemplateSummary[];
	coverage: TemplateCoverage[];
	/** The canonical operation catalogue (doc 11 §4.3.5): every operation a template may implement. */
	catalogue: { key: string; object: string; side_effect: SideEffect | null }[];
	jobs: CoverageJob[];
}

/**
 * `GET /api/connections/templates/usage` (T12): where the tenant's own templates are in use — which systems pin which
 * version, which job routes those systems serve — plus how big the platform catalogue is, so the page can say
 * "use a platform template first; build your own only for what it does not cover".
 */
export interface TenantTemplateUsage {
	/** Systems (connector instances) pinned to a tenant template version. */
	pins: {
		key: string;
		version: string;
		instance_id: string;
		label: string;
		env: Env | 'all';
		status: ConnectorInstance['status'];
	}[];
	/** Job routes (any env) whose target is one of those systems. */
	routes: { instance_id: string; job: JobKey; job_label: string; op: string; env: Env }[];
	/** Active/draft platform templates the tenant can connect instead (non-platform-connector, not retired). */
	platform: { key: string; version: string; label: string; vendor: string }[];
}

/** How the agent handles a canonical error code (doc 10 §3.1): a user outcome gets a reply, a system error escalates. */
export type ErrorHandling = 'reply' | 'escalate' | 'unlisted';

/** T11: one operation of the pinned template as the tenant sees it — how it translates, and how its calls ended. */
export interface SystemOpInsight {
	op: string;
	object: string;
	side_effect: SideEffect;
	/** System status → canonical status of the object, as this pin applies it. */
	status_map: { system: string; canonical: string }[];
	/** Vendor fault → canonical error code, with how the agent handles that code. */
	fault_map: { system: string; code: string; handling: ErrorHandling }[];
	/** `header Idempotency-Key`, `dup_search external_id`, `natural`, `none`. */
	idempotency: string;
	verify_read: string | null;
	timeout_ms: number | null;
	/** Canonical outcome codes of this instance's failed calls, last 30 days (audit `details.outcome`, ids only). */
	errors_30d: { code: string; count: number; handling: ErrorHandling }[];
}

/** T11: what a re-pin to the newest active version would change for THIS system (P17 vocabulary, tenant view). */
export interface SystemUpgrade {
	from: string;
	to: string;
	/** Template-level changes: vendor API pin, auth, credential fields, rate limit, inert webhooks. */
	meta: { label: string; before: string; after: string }[];
	ops: { op: string; change: 'added' | 'removed' | 'changed' | 'same'; details: string[] }[];
	counts: Record<'added' | 'removed' | 'changed' | 'same', number>;
	/** `blocked`: the target drops an op a job routes here; `warnings`: overrides that would be dropped, T3 re-run. */
	verdict:
		| { kind: 'can_upgrade'; to: string; warnings: string[] }
		| { kind: 'blocked'; to: string; reasons: string[] };
}

/** `GET /api/connections/systems/{id}/insight` (T11): translation detail, 30-day outcome mix and the upgrade diff. */
export interface SystemInsight {
	ops: SystemOpInsight[];
	rate_limit: RateLimitModel | null;
	vendor_api_pin: string | null;
	/** Inert in v1 (pull only, K49): listed so the tenant knows what will exist, never delivered. */
	webhooks: { topic: string; signature: string }[];
	upgrade: SystemUpgrade | null;
	/** The canonical error taxonomy (`error_state_def`) as the agent applies it, to read call outcomes against. */
	taxonomy: { code: string; handling: ErrorHandling; description: string }[];
}

/** `GET /api/jobs/{job}/operations/spec` (F-36, T13a): the integration field spec as a file, one row per op × field. */
export type SpecFormat = 'csv' | 'md';
export type SpecOf = 'draft' | 'live';
export interface SpecExport {
	/** `<job_key>-integration-spec-<of>.<csv|md>` (from `Content-Disposition`). */
	filename: string;
	/** `text/csv; charset=utf-8` or `text/markdown; charset=utf-8`. */
	content_type: string;
	body: string;
}

/** A job's own operation (`job_operation` ledger kind): an op added from one of the tenant's own templates. */
export interface JobOp {
	identity_level: '0' | '1' | '2' | '3' | 'S';
	side_effect: 'R' | 'W' | 'X' | 'P';
	input_fields: string[];
	output_fields: string[];
	description: string;
	/** `key@version` of the tenant template it came from. */
	template: string;
	input_types?: Record<string, { type: string; description: string }>;
}

/** One op a tenant template offers the job; `added` = already one of the job's own operations. */
export interface AvailableOwnOperation {
	template: string;
	version: string;
	status: string;
	op: string;
	own: boolean;
	side_effect: 'R' | 'W' | 'X' | 'P';
	input_fields: string[];
	output_fields: string[];
	description: string;
	added: boolean;
}

/** `GET /api/jobs/{job}/own-operations`. */
export interface OwnOperationsView {
	ops: { op: string; value: JobOp }[];
	available: AvailableOwnOperation[];
}

/** `PUT /api/jobs/{job}/own-operations/{op}` body. */
export interface AddJobOwnOperationBody {
	template: string;
	version: string;
	identity_level: JobOp['identity_level'];
}

/** Answer of adding / removing a job's own operation. */
export interface JobOwnOperationWrite {
	op: string;
	value?: JobOp;
	draft_rev: number;
	previous_rev: number;
}
