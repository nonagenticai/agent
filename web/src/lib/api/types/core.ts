// Core shapes every area reads: identity, tenants, users, jobs, release basics, paging.
// Owned by the foundation; area agents READ these, they never edit them (add area shapes in types/<area>.ts).
// Field names follow spec 14 §3 (snake_case, as this app's API will send them).

/** ISO-8601 timestamp, UTC (`2026-10-18T09:12:00Z`). */
export type Iso = string;
/** A tenant uuid (`0195f48b-…`). */
export type TenantId = string;
/** A job key (`customer_care_agent`). */
export type JobKey = string;
/** A Keycloak subject. In fixtures a readable slug (`mara-visser`). */
export type Sub = string;

/** Any JSON value (what a payload, example or row content carries on the wire). */
export type JsonValue = string | number | boolean | null | JsonValue[] | { [k: string]: JsonValue };
/** A JSON object. */
export type JsonObject = { [k: string]: JsonValue };

/** D1: every list is `{items, next}`; `next` is the cursor for the next (older) page, null at the end. */
export interface Paged<T> {
	items: T[];
	next: string | null;
}

/**
 * D88 (plan §R.1): the fine-grained tenant roles. Independent — none implies another. A member with no role is an
 * observer (every read, test chat, dry runs, answer feedback). A super_admin acting in a tenant holds all of them.
 */
export type Role =
	| 'tenant_admin'
	| 'job_designer'
	| 'release_manager'
	| 'knowledge_manager'
	| 'integrations_manager'
	| 'support_manager'
	| 'support_agent'
	| 'operator';
/** Every role in §R.1 order (the order of `GET /api/roles` and of every role list the UI shows). */
export const ROLE_KEYS: readonly Role[] = [
	'tenant_admin',
	'job_designer',
	'release_manager',
	'knowledge_manager',
	'integrations_manager',
	'support_manager',
	'support_agent',
	'operator',
];
/** UI labels (§R.1). `GET /api/roles` serves the same labels with a description. */
export const ROLE_LABEL: Record<Role, string> = {
	tenant_admin: 'Tenant admin',
	job_designer: 'Job designer',
	release_manager: 'Release manager',
	knowledge_manager: 'Knowledge manager',
	integrations_manager: 'Integrations manager',
	support_manager: 'Support manager',
	support_agent: 'Support agent',
	operator: 'Operator',
};
/** One row of `GET /api/roles`. */
export interface RoleInfo {
	key: Role;
	label: string;
	description: string;
}
/** D79 view-as: the caller narrowed their own roles until `expires_at` (server-side row, survives reloads). */
export interface ViewAs {
	roles: Role[];
	expires_at: Iso;
}
/** Roles in §R.1 order, without duplicates. */
export function sortRoles(roles: readonly Role[]): Role[] {
	return ROLE_KEYS.filter((r) => roles.includes(r));
}
/** “Job designer, Support agent”, or “Observer” for the empty set (no role = observer, §R.1). */
export function rolesText(roles: readonly Role[]): string {
	return roles.length
		? sortRoles(roles)
				.map((r) => ROLE_LABEL[r])
				.join(', ')
		: 'Observer';
}
export type TenantStatus = 'active' | 'suspended' | 'archived';
export type Milestone = 'M1' | 'M2' | 'M3' | 'M4';

/** `GET /api/me`: who the caller is and in which tenant they act. Roles come only from here (07 §2, T5). */
export interface Me {
	sub: Sub;
	/** What the API records as actor: preferred_username, else sub. */
	actor: string;
	display_name: string;
	super_admin: boolean;
	/** The tenant this request acts in (home tenant; for a super_admin the X-Active-Tenant-Id one). */
	tenant: null | { id: TenantId; name: string; status: TenantStatus };
	active_tenant: TenantId | null;
	/** Roles held in that tenant (§R.1 order; a super_admin acting in a tenant holds all). Empty = observer / no tenant. */
	roles: Role[];
	/** What every role check uses: `roles` ∩ `view_as.roles`, or `roles` without view-as. */
	effective_roles: Role[];
	/** Active view-as (D79), null when none or no active tenant. */
	view_as: ViewAs | null;
}

/**
 * `GET /api/license/status` (never 402): what licence gate 2 says about the caller's tenant. `not_enforced` = the
 * gate is off; `no_workspace` = no tenant to judge (a super_admin in the platform view); `blocked` carries the 402
 * code the tenant routes answer (`license_*`, or a 403 `license_*_mismatch` / `tenant_suspended`) and its message.
 */
export interface LicenseStatus {
	enforced: boolean;
	tenant: TenantId | null;
	state: 'not_enforced' | 'no_workspace' | 'active' | 'blocked';
	code: string | null;
	message?: string;
}

/** Tenant picker / registry row (`GET /api/tenants`). */
export interface TenantSummary {
	id: TenantId;
	name: string;
	status: TenantStatus;
}

/** Licence chip on a tenant (gate 2, spec 04 §4). The token is never shown; only its key id. */
export interface LicenceSummary {
	status: 'active' | 'inactive' | 'revoked' | 'none';
	plan: string;
	kid: string | null;
	/** Platform-owned licence (Care Bus Proof). */
	platform_owned: boolean;
}

/** A tenant with the fields the registry and detail pages show (spec 14 §3 Tenant). */
export interface Tenant extends TenantSummary {
	/** Customer-facing brand (Demo Retail trades as "Lumen & Loom Home"). */
	brand: string | null;
	/** Wave/industry blurb shown under the name. */
	description: string;
	industry: string;
	created_at: Iso;
	suspended_at: Iso | null;
	archived_at: Iso | null;
	licence: LicenceSummary;
	/** D59/D66: data residency, chosen at tenant creation and immutable after (moving it is a support migration).
	 * Pins the tenant's strata stores, LLM providers and logs to one region. A tenant setting, never a job selection
	 * (the job `region` dimension is a placeholder). */
	residency: Residency;
	/** SW-7: platform flag in front of support round-robin (`PATCH /api/tenants/{id}`, super_admin). Off when absent. */
	support_auto_assign?: boolean;
	/** SW-9: the tenant's own attachment storage quota in bytes (super_admin sets it); null/absent = the platform default. */
	attachment_quota_bytes?: number | null;
	/** SW-9: the platform default quota in bytes (single-tenant answers). */
	attachment_quota_default_bytes?: number;
}

/** D59 data residency regions (strata EU/US pinning). */
export type Residency = 'eu' | 'us';
export const RESIDENCY_LABEL: Record<Residency, string> = { eu: 'EU', us: 'US' };

/** A tenant member (`GET /api/tenants/{id}/users`). Disabled is soft (K9): the row stays, greyed. */
export interface TenantUser {
	sub: Sub;
	tenant_id: TenantId;
	display_name: string;
	email: string;
	/** Held roles (§R.1 order); empty = observer. */
	roles: Role[];
	/** Free-text job title ("Support agent"). */
	title: string | null;
	disabled_at: Iso | null;
	/** Invite sent by email, not yet accepted (M2, D39). */
	invited_at: Iso | null;
	created_at: Iso;
}

/** A platform super_admin (never a tenant member, e.g. fr-test). */
export interface SuperAdmin {
	sub: Sub;
	display_name: string;
	email: string;
}

/** Pinned template layer tag, e.g. `{layer:'job/customer_care_agent', version:3}` → `job/customer_care_agent@v3`. */
export interface TemplatePin {
	layer: string;
	version: number;
}

/** Short reference to a release, embedded wherever a release id is shown. */
export interface ReleaseRef {
	/** `cfg-YYYY-MM-DD.N` */
	id: string;
	/** Short bundle hash. */
	hash: string;
}

// ---- selection (D65: the one selection shape — job target, release freeze, job board, compile preview) ----------

/**
 * D65 as amended by D106/D111/D115: what a job is built from (job target, release freeze, job board, compile
 * preview). The business-model `profile`, ONE `industry`, ONE OR MORE `departments` of it (each the
 * `industry_dept/<industry>/<dept>` layer key, extending a base department archetype; ordered, `departments[0]` is
 * the primary, no duplicates), optional `areas` (groupings of context spaces inside those departments — NOT a
 * dimension: empty = every area), the `channel` dimension, and one optional `region` (D111: selects the `…/region/<r>`
 * sub-layers of base, industry and each department; a region outside the platform's `region` dimension values is
 * 422 `unknown_region`). Residency is a tenant setting and languages a job setting — never part of the selection. Only the
 * base-only compile *preview* (no profile, industry, department or job) may leave the hierarchy empty; everything
 * else is refused 422 `hierarchy_incomplete` / `department_industry_mismatch` / `duplicate_department`
 * (`selectionIssues`).
 */
export interface Selection {
	/** Business-model profile key (`retail_ecommerce`); null = no profile rules. */
	profile: string | null;
	/** Industry key (`retail`); '' only in the base-only preview. */
	industry: string;
	/** Department layer keys of `industry` (`industry_dept/retail/customer_service`), primary first. */
	departments: string[];
	/** Area keys inside the chosen departments (`order-support`); empty = all areas. */
	areas: string[];
	dimensions: SelectionDimensions;
	/** D111: one jurisdiction code (`nl`), or null; selects the region sub-layers (none seeded yet). */
	region: string | null;
}

/** The selection's dimension values (D59): `channel` qualifies job layers. */
export interface SelectionDimensions {
	channel: string[];
}

/** Platform dimension/grouping keys a selection stores values under (`area` → `areas`, `channel` → `dimensions`,
 *  `region` → the one `region`). */
export type SelectionValueKey = 'area' | 'channel' | 'region';

/** Every `SelectionValueKey`, in display order (areas, then the dimensions). */
export const SELECTION_VALUE_KEYS: readonly SelectionValueKey[] = ['area', 'channel', 'region'];

/** An empty selection (nothing picked) — the base-only preview until an industry and a department are chosen. */
export function emptySelection(): Selection {
	return {
		profile: null,
		industry: '',
		departments: [],
		areas: [],
		dimensions: { channel: [] },
		region: null,
	};
}

/** The department layer key `industry_dept/<industry>/<dept>`. */
export function departmentKey(industry: string, dept: string): string {
	return `industry_dept/${industry}/${dept}`;
}

/** `[industry, dept]` of a department layer key, or null for anything else. */
export function parseDepartment(key: string): [string, string] | null {
	const parts = key.split('/');
	return parts.length === 3 && parts[0] === 'industry_dept' && parts[1] && parts[2]
		? [parts[1], parts[2]]
		: null;
}

/** The base-only compile preview: no profile, industry, department or job (D106). */
export function isBaseOnlyPreview(s: Selection, job: string | null = null): boolean {
	return !s.profile && !s.industry && !s.departments.length && !job;
}

/** One way a selection breaks D106/D115 (Rust `SelectionIssue`). */
export interface SelectionIssue {
	code:
		| 'hierarchy_incomplete'
		| 'department_industry_mismatch'
		| 'duplicate_department'
		| 'invalid_selection'
		/** D111 (KH T9): `selection.region` is not a platform region (checked by the API against the vocabulary). */
		| 'unknown_region';
	path: string;
	message: string;
}

/** Every way `s` breaks the hierarchy, in path order (Rust `Selection::hierarchy_issues`, same codes and text). */
export function selectionIssues(s: Selection): SelectionIssue[] {
	const out: SelectionIssue[] = [];
	if (!s.industry)
		out.push({
			code: 'hierarchy_incomplete',
			path: 'selection.industry',
			message: 'Choose the industry the job belongs to.',
		});
	if (!s.departments.length)
		out.push({
			code: 'hierarchy_incomplete',
			path: 'selection.departments',
			message: "Choose at least one department of the job's industry.",
		});
	const seen = new Set<string>();
	s.departments.forEach((dept, n) => {
		const path = `selection.departments[${n}]`;
		const parsed = parseDepartment(dept);
		if (!parsed)
			out.push({
				code: 'invalid_selection',
				path,
				message: `“${dept}” is not a department (industry_dept/<industry>/<department>).`,
			});
		else if (s.industry && parsed[0] !== s.industry)
			out.push({
				code: 'department_industry_mismatch',
				path,
				message: `${dept} is a department of ${parsed[0]}, not of ${s.industry}.`,
			});
		else if (seen.has(dept))
			out.push({ code: 'duplicate_department', path, message: `${dept} is selected twice.` });
		else seen.add(dept);
	});
	return out;
}

/** THE refusal code of a list of selection issues (Rust `compile::issues_refusal`): the first D106/D115 code, else
 *  `invalid_selection`; null for none. */
export function refusalCode(issues: { code: string }[]): string | null {
	if (!issues.length) return null;
	const hierarchy = ['hierarchy_incomplete', 'department_industry_mismatch', 'duplicate_department'];
	return issues.find((i) => hierarchy.includes(i.code))?.code ?? 'invalid_selection';
}

/** The values `s` names for a platform dimension/grouping key (`area`, `channel`, `region`); [] for any other key. */
export function selectionValues(s: Selection, key: string): string[] {
	if (key === 'area') return s.areas;
	if (key === 'channel') return s.dimensions.channel;
	if (key === 'region') return s.region ? [s.region] : [];
	return [];
}

/** `s` with the values of one platform dimension/grouping key replaced (a copy; unknown keys change nothing). The
 *  one `region` takes the first value. */
export function withSelectionValues(s: Selection, key: string, values: string[]): Selection {
	if (key === 'area') return { ...s, areas: [...values] };
	if (key === 'channel') return { ...s, dimensions: { ...s.dimensions, channel: [...values] } };
	if (key === 'region') return { ...s, region: values[0] ?? null };
	return s;
}

export type ReleaseState = 'approved' | 'current' | 'applied_earlier';

/** Release basics (K41, spec 05 §4.1). The release area adds selection, provenance, readiness and conflicts. */
export interface ReleaseSummary extends ReleaseRef {
	tenant_id: TenantId;
	job: JobKey;
	state: ReleaseState;
	approved_by: Sub | null;
	approved_at: Iso | null;
	released_by: Sub | null;
	released_at: Iso | null;
}

/** `GET /api/jobs` row (spec 14 §3 Job). */
/** D129: who a job serves: the tenant's customers, or its own signed-in employees. */
export type JobAudience = 'customers' | 'employees';

export interface JobSummary {
	key: JobKey;
	tenant_id: TenantId;
	label: string;
	/** Agents are serving it on the bus. */
	on_bus: boolean;
	settings: { preamble?: string; actor?: string; stance?: string };
	north_star: string;
	pins: TemplatePin[];
	current_release: ReleaseRef | null;
	/** D54/D63: the draft revision (+1 per saved tenant change; test-chat cases record it). Live is a release (its
	 *  stored snapshot): there is no separate live or routing revision — routes ride the release too. */
	draft_rev: number;
	/** D59: languages the job serves (BCP-47 primary tags, first = default). Feeds base gate B6 (language), the reply
	 * language and the knowledge coverage check. A job setting: a change is saved to the draft (D54). */
	languages?: string[];
	/** D129: who the job serves (a tenant_admin setting). */
	audience: JobAudience;
	/** D124.3: the job's region (`job_target.selection.region`), null when it has none. */
	region?: string | null;
}

/** D59: the language tags a job may serve, with their labels. */
export const LANGUAGE_LABEL: Record<string, string> = {
	nl: 'Dutch',
	en: 'English',
	fr: 'French',
	de: 'German',
};

/** Formats a pin as `layer@vN`. */
export function pinTag(p: TemplatePin): string {
	return `${p.layer}@v${p.version}`;
}
