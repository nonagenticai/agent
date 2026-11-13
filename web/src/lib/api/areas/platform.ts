// The `PlatformApi` contract.
import { ApiError } from '$lib/api/errors';
import type { JobSummary, Paged, Residency, Sub, Tenant, TenantId, TenantUser } from '$lib/api/types/core';
import type {
	Axis,
	AxisCondition,
	AxisInput,
	AxisRule,
	BusinessProfile,
	CompileError,
	CompileOptions,
	CompileReference,
	CompileTarget,
	CreateJobInput,
	CreateTenantInput,
	CreateTenantResult,
	DepartmentArchetype,
	Dimension,
	DimensionInput,
	DimensionUsage,
	EffectiveConfig,
	ImportTenantInput,
	ImportTenantResult,
	JobTemplateTag,
	Layer,
	LayerInput,
	LayerOverride,
	LayerRow,
	LayerRowInput,
	LayerSummary,
	LicensingOverview,
	OffboardResult,
	PlatformAuditEvent,
	PlatformAuditSummary,
	PlatformDepartment,
	PlatformIndustries,
	PlatformIndustry,
	PlatformTenantRow,
	ProvisionRow,
	RenewalPreset,
	ReissueAnswer,
	ResealResult,
	RowKind,
	Signup,
	SignupLaunch,
	SignupQueue,
	TenantDetail,
	TenantPortfolio,
	TenantFootprint,
} from '$lib/api/types/platform';
import { type EarlyAccessInvite, type ProvisioningStep } from '$lib/api/types/signup-approvals';
import type { InviteQuery, MemberInvite } from '$lib/api/types/tenant';

export type PlatformApi = {
	// ---- tenants (P1, P2)
	/** `GET /api/tenants?include_archived=true` (super_admin) — registry rows with counts and provisioning badge. */
	platformTenants(opts?: { archived?: boolean }): Promise<Paged<PlatformTenantRow>>;
	/**
	 * `GET /api/tenants/{id}`, `GET /api/tenants/{id}/users`, `GET /api/tenants/{id}/jobs`,
	 * `GET /api/tenants/{id}/licence`, `GET /api/tenants/{id}/provisioning`, `GET /api/tenants/{id}/offboard`
	 * (super_admin; issued in parallel, `/offboard` 200 `null` = never offboarded) — one tenant end to end.
	 */
	platformTenant(id: TenantId): Promise<TenantDetail>;
	/** `GET /api/tenants?view=portfolio&include_archived=true` (super_admin) — P1: lifecycle stage, provision-down per
	 *  target, owners, members, jobs (release, pins behind, generator run), 30-day usage and attention items. */
	platformTenantPortfolio(): Promise<TenantPortfolio[]>;
	/** `GET /api/tenants/{id}/footprint` (super_admin) — P2: rag/ingest slots, knowledge stores, LLM providers, the P1
	 *  portfolio summary and the tenant's lifecycle history (its audit, tenant/provision/licence/user/job/release). */
	platformTenantFootprint(id: TenantId): Promise<TenantFootprint>;
	/** `POST /api/tenants` — name + first admin email; 502 `licences_unavailable` (nothing created), 409 `user_exists`. */
	createTenant(input: CreateTenantInput): Promise<CreateTenantResult>;
	/** `POST /api/tenants/import` (K30) — keeps the given id; idempotent (`imported:false` when it exists). */
	importTenant(input: ImportTenantInput): Promise<ImportTenantResult>;
	/** `PATCH /api/tenants/{id}` `{status}` — active ↔ suspended (K42). Archived only via offboard. */
	setTenantStatus(id: TenantId, status: 'active' | 'suspended'): Promise<Tenant>;
	/**
	 * `PATCH /api/tenants/{id}` `{name}` (DR, owner 2026-10-23) — rename, any status (an archived tenant keeps its
	 * name until renamed). 422 `invalid_name`; 404 `tenant_not_found`; 409 `tenant_name_taken` (another tenant holds
	 * the name, case-insensitive, archived included). Audited `tenant.rename`; the same name is a no-op.
	 */
	renameTenant(id: TenantId, name: string): Promise<Tenant>;
	/**
	 * `PATCH /api/tenants/{id}` `{support_auto_assign}` (SW-7, owner 2026-10-27; super_admin only) — the platform flag in
	 * front of support round-robin, OFF by default. Off: nothing is auto-assigned whatever the tenant's routing mode
	 * says. 409 `tenant_archived`; the same value is a no-op. Audited `tenant.support_auto_assign`.
	 */
	setTenantSupportAutoAssign(id: TenantId, enabled: boolean): Promise<Tenant>;
	/**
	 * `PATCH /api/tenants/{id}` `{attachment_quota_bytes}` (SW-9, owner 2026-10-28; super_admin only) — the tenant's
	 * attachment storage in bytes (1 B to 1 TiB); `null` returns it to the platform default. 422 `invalid_quota`; 409
	 * `tenant_archived`; the same value is a no-op. Audited `tenant.attachment_quota`. The tenant's admin reads it
	 * with its usage on the job's Channels page (`attachmentQuota`).
	 */
	setTenantAttachmentQuota(id: TenantId, bytes: number | null): Promise<Tenant>;
	/** `POST /api/tenants/{id}/offboard` `{confirm_name}` — revoke, archive, disable users; data retained. */
	offboardTenant(id: TenantId, confirmName: string): Promise<OffboardResult>;
	/** `POST /api/tenants/{id}/provisioning/{row}/retry` — pending now, attempts reset (audited). The path tenant must be
	 *  the row's tenant (else 404 `not_found`). */
	platformRetryProvision(tenantId: TenantId, rowId: string): Promise<ProvisionRow>;
	/** `POST /api/tenants/{id}/provisioning/{row}/cancel` (super_admin). */
	platformCancelProvision(tenantId: TenantId, rowId: string): Promise<ProvisionRow>;
	/** `GET /api/templates?kind=job` — tags a job can be created from. */
	platformJobTemplates(): Promise<JobTemplateTag[]>;
	/** `POST /api/tenants/{id}/jobs` (K11, super_admin) — a job pinned to a template tag (D15). */
	platformCreateJob(tenantId: TenantId, input: CreateJobInput): Promise<JobSummary>;
	/** `POST /api/users/{sub}/rehome` `{tenant_id}` (K43) — a disabled user moves to another tenant. */
	platformRehomeUser(sub: Sub, targetTenant: TenantId): Promise<TenantUser>;
	/**
	 * `GET /api/tenants/{id}/invites?state=&before=&limit=` (super_admin; B1b F-01) — the tenant's member invites,
	 * newest first (`expired` computed on read). 404 `tenant_not_found`; 422 `invalid_state`.
	 */
	platformTenantInvites(tenantId: TenantId, query?: InviteQuery): Promise<Paged<MemberInvite>>;
	/**
	 * `POST /api/tenants/{id}/invites/{invite}/resend` `{}` — 202 re-armed invite; as `resendInvite` plus 404
	 * `tenant_not_found`, 409 `tenant_archived`.
	 */
	platformResendInvite(tenantId: TenantId, inviteId: string): Promise<MemberInvite>;
	/** `POST /api/tenants/{id}/invites/{invite}/revoke` `{}` — as `revokeInvite` plus 404 `tenant_not_found`. */
	platformRevokeInvite(tenantId: TenantId, inviteId: string): Promise<MemberInvite>;
	/** `POST /api/tenants/{id}/licence/reseal` — re-seal this tenant's stored token under the current kid. */
	resealTenantLicence(id: TenantId): Promise<ResealResult>;
	/**
	 * `POST /api/tenants/{id}/licence/reissue` `{tenant_licence?, member_sub?}` (F-31, super_admin) — revoke the
	 * holder's licence and issue a new one in the tenant's residency region; members are never disabled. At least
	 * one of `tenantLicence: true` / `memberSub`. 403 `super_admin_required`; 404 `tenant_not_found`,
	 * `user_not_found`; 409 `tenant_archived`, `tenant_suspended`, `no_licence`, `concurrent_reissue`, `concurrent_change`,
	 * `licence_revoke_refused`, `region_unmapped`; 422 `invalid_input`; 502 `licences_unavailable` (nothing changed).
	 */
	reissueTenantLicence(
		id: TenantId,
		input: { tenantLicence?: boolean; memberSub?: Sub },
	): Promise<ReissueAnswer>;
	/**
	 * `POST /api/tenants/{id}/licence/remint` `{confirm_name, reason?}` (F-32, R1, super_admin) — the leaked-token
	 * path: the tenant licence is replaced and the old one revoked at once (never left for an overlap); members are
	 * untouched. `confirmName` must equal the tenant's name. 403 `super_admin_required`; 404 `tenant_not_found`; 409 as
	 * `reissueTenantLicence`; 422 `confirm_name_mismatch` (nothing changed), `invalid_input` (`reason` over 200
	 * characters or secret-/email-shaped); 502 `licences_unavailable`. `old_revoke: pending` = licences did not confirm
	 * the revoke: the old token stays valid until the retry tick revokes it (the tenant gets a `licence` attention item).
	 */
	remintTenantLicence(id: TenantId, input: { confirmName: string; reason?: string }): Promise<ReissueAnswer>;

	// ---- signups (P3)
	/** `GET /api/signup-approvals?status=` — 404 `not_found` while early access is off (dark feature, K21). */
	signupQueue(opts?: { status?: Signup['status'] | 'decided' }): Promise<SignupQueue>;
	/** `POST /api/signup-approvals/{id}/approve` `{tenant_name}` — runs the tenant create. */
	approveSignup(
		id: string,
		tenantName: string,
		residency: Residency,
	): Promise<{ signup: Signup; tenant: Tenant }>;
	/** `POST /api/signup-approvals/{id}/reject` `{reason}`. */
	rejectSignup(id: string, reason: string): Promise<Signup>;
	/**
	 * `POST /api/signup-approvals/{id}/retry` (B1 T14) — every `failed` provisioning step of an approved signup back
	 * to pending and run again; 409 `signup_not_approved`; no failed step = 200 unchanged.
	 */
	retrySignup(id: string): Promise<{ steps: ProvisioningStep[] }>;
	/**
	 * `POST /api/signup-approvals/invite` `{email, name, company?, tenantName?, locale?, residency}` (super_admin) — a
	 * pre-approved invite: the signup row is created and approved in one call. 422 `invalid_locale`,
	 * `invalid_residency`; 409 `user_belongs_to_another_workspace`, `tenant_name_taken`, `approval_in_progress`,
	 * `signup_not_pending`; 502 `<step code>` with `error.steps` (approve again resumes).
	 */
	inviteEarlyAccess(input: EarlyAccessInvite): Promise<{
		signup: Signup;
		tenant: Pick<Tenant, 'id' | 'name' | 'status'>;
		seated: boolean;
		steps: ProvisioningStep[];
		reused_pending: boolean;
	}>;
	/**
	 * P3 — `GET /api/platform/signup-launch` (B1 T14; always mounted, super_admin): names-only
	 * readiness of the dark signup switch (K21) — which env vars / clients are present, never their values.
	 */
	signupLaunch(): Promise<SignupLaunch>;

	// ---- layers + compile (P4, P5)
	/** `GET /api/layers` — platform layers (no tenant scope). */
	layerCatalog(): Promise<Paged<Layer>>;
	/** `GET /api/layers/{id}/rows?include_retired=` — current rows (plus retired revisions when asked). */
	layerRows(layerId: string, opts?: { include_retired?: boolean }): Promise<Paged<LayerRow>>;
	/** `GET /api/layers/{id}/rows/{kind}/{key}/revisions` — every revision, newest first. */
	layerRowRevisions(layerId: string, kind: RowKind, key: string): Promise<LayerRow[]>;
	/** `GET /api/layer-rows/{row_id}` — a row and its layer, for `?row=` deep links. */
	findLayerRow(rowId: string): Promise<{ layer: Layer; row: LayerRow }>;
	/** `GET /api/layers/summary` — per layer: current rows by op, retired count, authors, last change, untagged. */
	layerSummaries(): Promise<LayerSummary[]>;
	/** `GET /api/layers/{id}/overrides` — for each patch/tombstone row, the earlier layer row it acts on. */
	layerOverrideTargets(layerId: string): Promise<LayerOverride[]>;
	/**
	 * F-21 (T12b) `POST /api/layers` — a new platform layer (D111: `region` = a `…/region/<r>` sub-layer of a base,
	 * industry or department layer, in its parent's family). 422 `invalid_layer_key`, `kind_not_allowed_in_layer`,
	 * `invalid_label`; 404 `parent_layer_not_found`; 409 `layer_exists`.
	 */
	createLayer(input: LayerInput): Promise<Layer>;
	/**
	 * F-21 (T12b) `POST /api/layers/{id}/rows` — one authored revision (`add`, `patch`, or `tombstone` = retire;
	 * patch/tombstone carry `expected_rev`). 404 `layer_not_found`, `row_not_found`; 422 `invalid_row`, `invalid_key`,
	 * `kind_not_allowed_in_layer`; 409 `duplicate_add`, `add_tombstoned`, `stale_revision`.
	 */
	writeLayerRow(layerId: string, input: LayerRowInput): Promise<LayerRow>;
	/**
	 * F-30 (T12f) `GET /api/platform/export?family=&tag=` — the platform seed (§7 format) as a JSON file
	 * (`agent-platform-seed-<ts>.json`); one family / one tag when given. 404 `layer_not_found`, `tag_not_found`.
	 */
	exportPlatformSeed(opts?: { family?: string; tag?: string }): Promise<Blob>;
	/** `GET /api/compile/options` — industries, departments (+ areas), jobs, profiles, dimensions. */
	compileOptions(): Promise<CompileOptions>;
	/** `POST /api/compile` — the effective model, or 422 `compile_failed` with every error. Never writes. F-07
	 *  (T12a): `as_of` compiles those tags instead of the head (404 `tag_not_found`). */
	compilePreview(target: CompileTarget): Promise<EffectiveConfig>;
	/** `GET /api/compile/references` — named selections a preview is compared against (running jobs' releases). */
	compileReferences(): Promise<CompileReference[]>;

	// ---- business models + dimensions (P12, P13)
	/** `GET /api/axes` — axes with value trees (insert-only). */
	businessAxes(): Promise<Axis[]>;
	/** `POST /api/axes/{key}/values` — insert-only; 409 `value_exists`. */
	addAxisValue(axis: string, value: { key: string; label: string; parent?: string | null }): Promise<Axis>;
	/** `GET /api/axis-rules`. */
	axisRules(): Promise<AxisRule[]>;
	/** `POST /api/axis-rules` — a condition and the entity it adds (a new profile layer). D99: `layer` = the rule's
	 *  tagging family (`base` or an industry layer key; absent = `base`); 422 `invalid_rule_layer`. */
	createAxisRule(input: {
		key: string;
		priority: number;
		condition: AxisCondition;
		adds: { key: string; label: string };
		layer?: string;
	}): Promise<AxisRule>;
	/** F-25 `POST /api/axes` — a new axis (insert-only). 422 `invalid_axis_key`, `invalid_label`, `unknown_object`;
	 *  409 `axis_exists`. */
	createAxis(input: AxisInput): Promise<Axis>;
	/** F-26 `POST /api/axes/{key}/values/{value}/retire` — 404 `axis_not_found`, `value_not_found`; 409
	 *  `value_retired`, `value_in_use` (`blockers[]`). */
	retireAxisValue(axis: string, value: string): Promise<Axis>;
	/** F-26 `PATCH /api/axes/{key}` `{cardinality}` — 404 `axis_not_found`; 409 `cardinality_in_use` (`blockers[]`);
	 *  422 `invalid_cardinality`. */
	setAxisCardinality(axis: string, cardinality: 'one' | 'many'): Promise<Axis>;
	/** `GET /api/profiles` — business models. */
	businessProfiles(): Promise<BusinessProfile[]>;
	/** `PUT /api/profiles/{key}` — create or update in place (inlined into release provenance). */
	saveBusinessProfile(
		p: Pick<BusinessProfile, 'key' | 'label' | 'industry' | 'description' | 'selection'>,
	): Promise<BusinessProfile>;
	/** `GET /api/axes/renewal-presets` — the owner's six renewal examples. */
	renewalPresets(): Promise<RenewalPreset[]>;
	/** `GET /api/dimensions` — standard values only (D8). */
	platformDimensions(): Promise<Dimension[]>;
	/** D60 `GET /api/department-archetypes` — base department archetypes and the industry departments extending
	 * each (super_admin). */
	departmentArchetypes(): Promise<DepartmentArchetype[]>;
	/** `POST /api/dimensions/{key}/values` — insert-only; 409 `value_exists`. */
	addDimensionValue(dimension: string, value: { key: string; label: string }): Promise<Dimension>;
	/** F-27 `POST /api/dimensions` — a new dimension (insert-only). 422 `invalid_dimension_key`, `invalid_label`,
	 *  `invalid_kind`; 409 `dimension_exists`. */
	createDimension(input: DimensionInput): Promise<Dimension>;
	/** `GET /api/dimensions/usage` (super_admin) — per value: qualified layers, area context spaces, the job
	 * selections and the releases that name it (P13). */
	platformDimensionUsage(): Promise<DimensionUsage>;

	// ---- industries + departments (plan 2026-10-30 T3; super_admin, no tenant)
	/** `GET /api/platform/industries` — every industry with its departments, their knowledge and the jobs using them. */
	platformIndustries(): Promise<PlatformIndustries>;
	/** `POST /api/platform/industries` — a new `industry/<key>` layer. 422 `invalid_layer_key`, `invalid_label`; 409
	 *  `layer_exists`. */
	createIndustry(input: { industry: string; label: string }): Promise<PlatformIndustry>;
	/** `PATCH /api/platform/industries/{industry}` — the label (the key never changes). 404 `industry_not_found`. */
	renameIndustry(industry: string, label: string): Promise<PlatformIndustry>;
	/** `POST /api/platform/industries/{industry}/retire` — 409 `in_use{jobs}` while a tenant job selects it. */
	retireIndustry(industry: string): Promise<PlatformIndustry>;
	/** `POST /api/platform/industries/{industry}/restore` — offered to tenants again. */
	restoreIndustry(industry: string): Promise<PlatformIndustry>;
	/** `POST /api/platform/industries/{industry}/departments` — an `industry_dept/<industry>/<key>` layer; `extends`
	 *  names a base department archetype (404 `parent_layer_not_found`). 409 `industry_retired`. */
	createDepartment(
		industry: string,
		input: { department: string; label: string; extends?: string | null },
	): Promise<PlatformDepartment>;
	/** `PATCH /api/platform/industries/{industry}/departments/{department}` — the label. */
	renameDepartment(industry: string, department: string, label: string): Promise<PlatformDepartment>;
	/** `POST /api/platform/industries/{industry}/departments/{department}/retire` — 409 `in_use{jobs}`. */
	retireDepartment(industry: string, department: string): Promise<PlatformDepartment>;
	/** `POST /api/platform/industries/{industry}/departments/{department}/restore` */
	restoreDepartment(industry: string, department: string): Promise<PlatformDepartment>;

	// ---- audit + licensing (P19, P20)
	/** `GET /api/platform/audit?action=` (super_admin) — pre-tenant and platform events, newest first. */
	platformAudit(opts?: {
		action?: string;
		status?: 'ok' | 'failed';
		before?: string | null;
		/** P19 family (`signup|creation|keys|catalogue`). */
		family?: string;
		/** Exact `actor_id`. */
		actor?: string;
		/** Case-insensitive match on the subject label or a detail value. */
		q?: string;
		/** B1b F-04: the subject tenant (uuid; 422 `invalid_tenant_id`). */
		tenant?: TenantId;
		/** B1b F-04: `YYYY-MM-DD` or RFC 3339, inclusive, UTC (422 `invalid_since`). */
		since?: string;
	}): Promise<Paged<PlatformAuditEvent>>;
	/** `GET /api/platform/audit/summary` (super_admin) — per-family counts, actors, and the open failures (P19). */
	platformAuditSummary(): Promise<PlatformAuditSummary>;
	/** `GET /api/platform/licensing` — verdict per tenant, keyring kids, last reseal. */
	licensingOverview(): Promise<LicensingOverview>;
	/** `POST /api/platform/licence-tokens/reseal` — every tenant token to the current kid; counts only. */
	resealLicences(): Promise<ResealResult>;
	/** `POST /api/platform/licence-verdicts/refresh` — the `licence:changed` nudge. */
	nudgeLicenceVerdicts(): Promise<LicensingOverview>;
};

/** 422 `compile_failed`: carries every error (the compile never stops at the first). */
export class CompileFailed extends ApiError {
	/** `code` is `compile_failed`, or the D106/D115 refusal (`hierarchy_incomplete`, …) of the first error. */
	constructor(
		readonly errors: CompileError[],
		code = 'compile_failed',
	) {
		super(
			422,
			code,
			code === 'compile_failed'
				? `The selection does not compile: ${errors.length} error${errors.length === 1 ? '' : 's'}.`
				: errors.map((e) => e.message).join(' '),
		);
		this.name = 'CompileFailed';
	}
}
