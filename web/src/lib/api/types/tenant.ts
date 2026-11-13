// Shapes for the "tenant" area (T1–T9, T13): tenant overview + provisioning, jobs board, users, audit, usage,
// providers, knowledge stores, ingest jobs, sources and channels. Field names follow spec 14 §3 and the wire shapes
// of specs 04/06 (snake_case, as the API will send them).
import type { JobAudience } from './core';
import type { QueryRouting, QueryScores, QueryTimings } from './retrieval';
import type { Provenance } from './grounding';
import type {
	Iso,
	Selection,
	JobKey,
	JobSummary,
	ReleaseRef,
	Sub,
	Tenant,
	TenantId,
	Role,
	TemplatePin,
} from './core';

// ---- T2 tenant overview + provisioning (06 §4.5–§4.6) -----------------------------------------------------------

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

/** One `provision_outbox` row (06 §4.5). `last_error` is redacted text; payloads are never sent to the UI. */
export interface ProvisionRow {
	id: string;
	tenant_id: TenantId;
	target: ProvisionTarget;
	op: ProvisionOp;
	dedupe_key: string;
	status: ProvisionStatus;
	attempts: number;
	max_attempts: number;
	/** When the drainer tries again (pending / failed rows). */
	next_attempt_at: Iso | null;
	last_http_status: number | null;
	last_error_code: string | null;
	last_error: string | null;
	created_by: Sub;
	created_at: Iso;
	updated_at: Iso;
	completed_at: Iso | null;
}

/** The chip on an Integrations row: ready · pending (next retry hh:mm) · failed (code) · dead · cancelled. */
export type IntegrationChip = 'ready' | 'pending' | 'failed' | 'dead' | 'cancelled';

/** `provision_state.drift`: a remote slot version moved by another app (never overwritten, 06 §4.6). */
export interface ProvisionDrift {
	slot: string;
	local_version: number;
	remote_version: number;
	detected_at: Iso;
}

/** One Integrations panel row (Licences / Knowledge / Ingest). */
export interface IntegrationRow {
	target: ProvisionTarget;
	label: 'Licences' | 'Knowledge' | 'Ingest';
	chip: IntegrationChip;
	/** The row that decides the chip (the oldest open one — FIFO per target — else the newest). */
	current: ProvisionRow | null;
	/** Last 20, newest first. */
	history: ProvisionRow[];
	last_success_at: Iso | null;
	drift: ProvisionDrift[];
}

/** Gate-2 verdict (`tenant_licence_verdict`, K35). The token itself is never shown, only its key id. */
export interface LicenceVerdict {
	status: 'active' | 'inactive' | 'revoked' | 'expired' | 'none';
	app: 'agent';
	plan: string;
	kid: string | null;
	licence_id: string | null;
	holder_sub: Sub | null;
	checked_at: Iso;
	reason: string | null;
	platform_owned: boolean;
}

/** The data-plane refusal a page would get right now (06 §4.6): 409 provision_pending / 503 provision_failed. */
export interface ProvisionBlock {
	code: 'provision_pending' | 'provision_failed';
	status: 409 | 503;
	targets: ProvisionTarget[];
}

/** `GET /api/tenant/overview`. */
export interface TenantOverview {
	tenant: Tenant;
	licence: LicenceVerdict;
	integrations: IntegrationRow[];
	blocks: ProvisionBlock[];
	jobs: JobSummary[];
	members: { total: number; admins: number; disabled: number; invited: number };
}

// ---- T2 home: what needs the tenant admin, how each job stands, what the agents did ------------------------------

/** Where an attention item comes from. */
export type AttentionKind =
	| 'licence'
	| 'provision'
	| 'drift'
	| 'release_waiting'
	| 'readiness'
	| 'template_newer'
	| 'ingest_failed'
	| 'agent_alert'
	| 'llm_provider'
	| 'invite'
	// the per-agent items (`readmodel::job_attention`, `GET /api/inbox`)
	| 'dependency_down'
	| 'bus_unreachable'
	| 'release_selection'
	| 'unknown_outcome'
	| 'dead_letters'
	| 'handover_sla'
	| 'handover_unclaimed'
	| 'operation_failing'
	| 'release_base_changed'
	| 'agent_behind';
/** blocking = customers are refused or a call fails now · action = waits on a decision · info = worth knowing. */
export type AttentionSeverity = 'blocking' | 'action' | 'info';

/** One thing that needs the tenant admin, with the page where it is decided. Ids and codes only (K50). */
export interface AttentionItem {
	id: string;
	kind: AttentionKind;
	severity: AttentionSeverity;
	title: string;
	detail: string;
	/** The raw technical text (key, id, error) behind the plain title; shown only with "Show technical details". */
	code?: string | null;
	/** Where the decision is made. */
	href: string;
	/** Verb on the link ("Apply release", "Retry"). */
	action: string;
	job: JobKey | null;
	at: Iso | null;
	/** Who can act on it: a role, or `automatic` (nobody; the system does it). */
	owner_role: OwnerRole;
	/** When it must be done (an SLA deadline); null when there is no deadline. */
	due_at: Iso | null;
}

/** The role that owns an inbox item (`AttentionItem.owner_role`). */
export type OwnerRole = Role | 'automatic';

/** A job's release lane: what serves now, what is approved and waiting, what newer template tags exist (K5). */
export interface JobPulse {
	job: JobSummary;
	current: (ReleaseRef & { released_at: Iso | null }) | null;
	/** Approved but not applied yet (05 §4.7 `approved_waiting`). */
	waiting: (ReleaseRef & { approved_by: Sub | null; approved_at: Iso | null }) | null;
	/** Template tags newer than the job pins: "base changed since your release" (K5). */
	newer: TemplatePin[];
	readiness: { ready: boolean; missing: string[] };
	cases_7d: number;
	containment_7d: number | null;
}

/** What the tenant's agents did in the last 7 days (usage ledger + ai_agent audit rows). */
export interface AgentActivity {
	from: string;
	to: string;
	turns: number;
	tokens: number;
	daily: { day: string; turns: number }[];
	cases: number;
	/** Case-weighted containment over the tenant's jobs; null when no case was handled. */
	containment: number | null;
	/** ai_agent audit rows in the window, per status. */
	outcomes: Record<AuditStatus, number>;
	/** Non-success ai_agent rows in the window, newest first (max 5). */
	notable: AuditEvent[];
	/** False before the licence is on: usage is not attributed to this tenant yet (04 §6a). */
	attributed: boolean;
}

/** `GET /api/tenant/home` — the tenant admin's home: attention list, job lanes, agent activity. */
export interface TenantHome {
	attention: AttentionItem[];
	jobs: JobPulse[];
	activity: AgentActivity;
}

// ---- T1 jobs board -------------------------------------------------------------------------------------------

export interface JobBoardRow {
	job: JobSummary;
	/** Readiness summary for the job's newest approved or current release (K41). */
	readiness: { ready: boolean; missing: string[] };
	/** Template tags newer than what the job pins (K5 badge). */
	newer: TemplatePin[];
	cases_7d: number;
	containment_7d: number | null;
	last_release_at: Iso | null;
	/** A tenant_admin asked the platform admin to move the job onto the newer template. */
	upgrade_requested_at: Iso | null;
	/** The job's selection (`jobs.selection`, K6, D65). */
	selection?: Selection | null;
	/** Newest release that is approved but not yet applied (05 §4.7 `approved_waiting`). */
	approved_waiting?: ReleaseRef | null;
	/** Runtime health right now (null when no agent has ever served the job). */
	live?: JobLive | null;
}

/** Runtime numbers for a job on the board (T1). */
export interface JobLive {
	agents_online: number;
	open_cases: number;
	handovers_7d: number;
	containment_prev_7d: number | null;
	last_case_at: Iso | null;
}

/** A job template a super_admin can create a job from (P6 tags; K11 create is SA-only). */
export interface JobTemplateOption {
	layer: string;
	version: number;
	key: JobKey;
	label: string;
	status: 'tagged' | 'draft';
}

/** D123.2: one tagged job template in the tenant's catalog (`GET /api/job-templates`, any member). */
export interface JobTemplateCatalogEntry {
	/** `<layer>@v<N>`: what `POST /api/jobs` takes as `template`. */
	template: string;
	/** The job key a job created from it gets by default. */
	key: JobKey;
	label: string;
	/** The API sends `null` today (no template description yet). */
	description: string | null;
	industry: string;
	/** The department layer keys the template pins (`industry_dept/<industry>/<dept>`). */
	departments: string[];
	/** The template's industry is the company industry and one of its departments is a company department. */
	recommended: boolean;
}

/** D123.1: `POST /api/jobs` (tenant_admin). */
export interface CreateTenantJobInput {
	template: string;
	/** Defaults to the template key; a taken default becomes `<key>_2`, `<key>_3`; a taken explicit key is 409. */
	key?: string;
	label?: string;
	/** A subset (>=1) of the template's departments. */
	departments?: string[];
	/** D129: who the job serves; defaults to customers. */
	audience?: JobAudience;
	/** The job's region (selects the region sub-layers); absent = none. */
	region?: string | null;
	/** D132.1: a business profile key; absent = the template's / the only fitting profile, else 422 `profile_required`. */
	profile?: string;
	/** D132.4: the languages the job serves; absent = the company languages. */
	languages?: string[];
}

/** D128: `POST /api/jobs` without a template: a job built on the platform's base + industry + department layers. */
export interface CreateBlankJobInput {
	key: string;
	label: string;
	industry: string;
	departments: string[];
	region?: string | null;
	/** D129: who the job serves; defaults to customers. */
	audience?: JobAudience;
	/** D132.1: a business profile key; absent = the only fitting profile, else 422 `profile_required`. */
	profile?: string;
	/** D132.4: the languages the job serves; absent = the company languages. */
	languages?: string[];
}

/** D124: one choice of the company profile's options. */
export interface ProfileChoice {
	key: string;
	label: string;
	/** Departments: the industry they belong to. */
	industry?: string;
}

/** D124.2: `GET /api/tenant/profile` (any member) and the answer of `PUT`. */
export interface TenantProfile {
	/** An industry key, or `unassigned`. */
	industry: string;
	departments: string[];
	region: string | null;
	description: string;
	/** D132.4: the languages the company serves (job-language vocabulary); a new job's default. */
	languages: string[];
	/** The optimistic-lock token to send back as `expected_updated_at`; null until first saved. */
	updated_at: string | null;
	options: {
		industries: ProfileChoice[];
		departments: ProfileChoice[];
		regions: ProfileChoice[];
		/** D132.1: the business profiles a job may be created on (industry null = fits any). */
		profiles?: { key: string; label: string; industry?: string | null }[];
	};
}

/** D124.2: `PUT /api/tenant/profile` (tenant_admin). */
export interface TenantProfileInput {
	industry: string;
	departments: string[];
	region: string | null;
	description: string;
	/** D132.4: absent keeps the stored languages. */
	languages?: string[];
	expected_updated_at: string | null;
}

export type JobRoutingMode = 'inherit' | 'off' | 'round_robin';

/** A staff-team member as the API names them. */
export interface StaffMember {
	sub: Sub;
	name: string | null;
	email: string | null;
}

export interface JobRouting {
	mode: JobRoutingMode;
	reassign_after_secs: number | null;
}

/**
 * D125: `GET`/`PUT /api/jobs/{job}/staff` answer. An empty team = every conversation worker of the tenant.
 * `candidates` = enabled tenant users holding support_agent or support_manager (the picker's options).
 */
export interface JobStaff {
	members: StaffMember[];
	routing: JobRouting;
	candidates: (StaffMember & { roles: Role[] })[];
}

/** D125: the `PUT /api/jobs/{job}/staff` body. `reassign_after_secs` 60..86400, only with `round_robin`. */
export interface JobStaffInput {
	members: Sub[];
	routing: { mode: JobRoutingMode; reassign_after_secs?: number | null };
}

// ---- T3 users ------------------------------------------------------------------------------------------------

export interface InviteInput {
	email: string;
	/** D88 role set (§R.1 keys, no duplicates); empty = observer. */
	roles: Role[];
	display_name?: string;
	/** Explicit names win over splitting `display_name`; with both, the invitee only sets a password. */
	first_name?: string;
	last_name?: string;
}
/** 202 from `POST /api/tenant/invites` (M2, D39): the invite is queued; the row shows "invited". */
export interface InviteAccepted {
	status: 'invite_sent';
	sub: Sub;
	email: string;
}

// ---- B1b member invites (F-01) -------------------------------------------------------------------------------

/** `expired` is computed on read: a pending invite whose `expires_at` has passed. */
export type InviteState = 'pending' | 'accepted' | 'expired' | 'revoked';
export const INVITE_STATES: readonly InviteState[] = ['pending', 'expired', 'accepted', 'revoked'];

/** A `member_invites` row (B1b): a person invited into the tenant with a role set (D88; `[]` = observer). */
export interface MemberInvite {
	id: string;
	/** Lower-case. */
	email: string;
	roles: Role[];
	state: InviteState;
	/** A sub, or `signup:<uuid>` for a signup approval. */
	invited_by: string;
	/** Display name; "Sign-up approval" when `via = signup`. */
	invited_by_name: string | null;
	created_at: Iso;
	expires_at: Iso;
	last_sent_at: Iso | null;
	send_count: number;
	accepted_at: Iso | null;
	revoked_at: Iso | null;
	via: 'invite' | 'signup';
}

/** `GET /api/tenant/invites` / `GET /api/tenants/{id}/invites` query (newest first). */
export interface InviteQuery {
	state?: InviteState;
	before?: string | null;
	limit?: number;
}

// ---- T3 member activity ---------------------------------------------------------------------------------------

/** Where a member was last seen in Agent (they sign in once, through the shared portal realm). */
export type MemberSurface = 'console' | 'chat' | 'admin' | 'support';
/** How someone became a member: first admin at tenant creation, invite by e-mail (M2), or add by sub (M1). */
export type MemberOrigin = 'tenant_create' | 'invite' | 'add_by_sub';
/** `GET /api/tenant/users/activity` row (T3): last activity + who added whom. Keyed by `sub`. */
export interface MemberActivity {
	sub: Sub;
	last_seen_at: Iso | null;
	last_surface: MemberSurface | null;
	sign_ins_30d: number;
	added_by: Sub | null;
	added_by_name: string | null;
	added_via: MemberOrigin | null;
	added_at: Iso | null;
	/** Invited members only: invite links live 7 days (04 §3b, provisioning identity-invite contract). */
	invite_expires_at: Iso | null;
}

// ---- T4 audit ------------------------------------------------------------------------------------------------

export type ActorType = 'human' | 'ai_agent' | 'system';
export type AuditStatus = 'success' | 'failure' | 'denied' | 'error' | 'fallback';

/** `audit_events` row (04 §9b). `details` holds ids and codes only (K50). */
export interface AuditEvent {
	id: string;
	at: Iso;
	actor_id: string;
	actor_type: ActorType;
	action: string;
	resource_type: string;
	resource_id: string;
	job: JobKey | null;
	status: AuditStatus;
	details: Record<string, unknown>;
	/** Read-time: `preferred_username` / display name for a person's sub (04 §9b `details.actor_name`). */
	actor_name?: string | null;
	/** Read-time: the actor is a super_admin acting in this tenant (after `tenant.switch`), not a member. */
	platform_actor?: boolean;
	/** Read-time: display name of the object when it is a person (user rows). */
	resource_label?: string | null;
	/** Read-time, failed rows only: open, or resolved by a later success on the same object. */
	outcome?: AuditOutcome;
}

export interface AuditOutcome {
	state: 'open' | 'resolved';
	by_id: string | null;
	by_action: string | null;
	by_at: Iso | null;
}

/** T4 event families (page grouping); `calls` = agent operation calls (`execute`). */
export type AuditFamily =
	| 'access'
	| 'releases'
	| 'connections'
	| 'knowledge'
	| 'platform'
	| 'calls'
	| 'other';

export interface AuditQuery {
	q?: string;
	actor_type?: ActorType | '';
	action?: string;
	job?: string;
	status?: AuditStatus | '';
	before?: string | null;
	limit?: number;
	/** T4: `changes` = everything but agent operation calls; `calls` = only those. */
	view?: 'changes' | 'calls' | '';
	family?: AuditFamily | '';
	/** Exact actor id (a sub, an agent role, a system client). */
	actor?: string;
}

/** `GET /api/audit/summary` — whole-log counts for the T4 header. */
export interface AuditSummary {
	total: number;
	changes: number;
	calls: { total: number; success: number; denied: number; error: number; fallback: number };
	/** Failed rows no later success resolved, newest first. */
	open: AuditEvent[];
	families: { family: AuditFamily; count: number; open: number }[];
	actors: {
		actor_id: string;
		actor_name: string | null;
		actor_type: ActorType;
		platform: boolean;
		count: number;
	}[];
}

// ---- T5 usage ------------------------------------------------------------------------------------------------

export type Funding = 'local_gpu' | 'platform_external' | 'tenant_byo';
export type UsageGroup = 'job' | 'user' | 'project' | 'provider';
export type UsageWindow = 7 | 30 | 90;

/** One `usage_ledger` day row, already summed per (day, job, user, project, provider, model). */
export interface UsageLedgerRow {
	day: string;
	tenant_id: TenantId;
	job: JobKey;
	/** Staff subject, or `customers:<channel>` for end-customer turns (no Keycloak user). */
	user: string;
	project: string;
	provider: string;
	model: string;
	turns: number;
	tokens: number;
	funding: Funding;
}

export interface UsageBreakdownRow {
	key: string;
	label: string;
	turns: number;
	tokens: number;
	funding: Record<Funding, number>;
}

/** `GET /api/tenant/usage?window=&group=`. No currency: billing is out of scope (K52). */
export interface UsageReport {
	window: UsageWindow;
	group: UsageGroup;
	from: string;
	to: string;
	totals: { turns: number; tokens: number };
	/** Tokens per funding source. */
	funding: Record<Funding, number>;
	rows: UsageBreakdownRow[];
	daily: { day: string; turns: number; tokens: number }[];
	/** False before the licence is on (04 §6a): rows are not attributed to this tenant yet. */
	attributed: boolean;
	/** Totals of the window before this one; null when the ledger does not reach that far back (90 d). */
	previous?: { turns: number; tokens: number } | null;
	/** Staff test-chat turns per day (the rest of `daily[].turns` are end-customer turns). Same order as `daily`. */
	staff_daily?: number[];
	/** What drives the tokens: turns + tokens per qualified activity (the qualifier's verdict, 12 §2). */
	drivers?: UsageDriver[];
	/** Knowledge work outside the LLM ledger (strata_rag searches, strata_ingest jobs) in the window. */
	knowledge?: UsageKnowledge;
	/**
	 * Tenant generator runs started in the window (D56). Not a bill: their tokens are this tenant's usage under the
	 * `generator` activity, on the same funding lanes as turns. `funding`/`totals` stay conversation-turn tokens.
	 */
	generator?: UsageGeneratorRun[];
	/** D56: tokens per source (customer turns, staff test chats, generator), each on its funding lanes. */
	sources?: UsageSource[];
	/** The licence plan the usage runs under. No allowance is defined yet (billing model K52). */
	plan?: { name: string; licence: 'active' | 'inactive' | 'revoked' | 'none'; allowance: null };
}

/** T5: one qualified activity's share of the window's turns and tokens. `unqualified` = small talk / no match. */
export interface UsageDriver {
	job: JobKey;
	activity: string;
	label: string;
	stage: string;
	turns: number;
	tokens: number;
	/** Turns that also ran `knowledge.search` (counted by strata_rag, not in the LLM ledger). */
	searches: number;
}

export interface UsageKnowledge {
	searches: number;
	ingest_jobs: number;
	ingest_failed: number;
	documents: number;
	pages: number;
}

export interface UsageGeneratorRun {
	id: number;
	job: JobKey;
	output: string;
	status: string;
	started_at: Iso;
	started_by: Sub;
	tokens: number;
	/** Funding lane the drafting model ran on (D56). */
	funding: Funding;
	/** Usage-ledger project (the tenant id for this project-less app). */
	project: string;
	activity: 'generator';
}

/** T5 (D56): one source of metered tokens. `turns` is null for the generator (no conversation turns). */
export interface UsageSource {
	key: 'customers' | 'staff' | 'generator';
	label: string;
	turns: number | null;
	tokens: number;
	funding: Record<Funding, number>;
}

// ---- T6 providers (06 §7, D38) -------------------------------------------------------------------------------

/** A write-only secret: whether one is stored, when, and a short fingerprint. Never the value. */
export interface SecretState {
	set: boolean;
	updated_at: Iso | null;
	fingerprint: string | null;
}

/**
 * D70 (owner, 2026-10-19): every GPU service is an external provider. `internal` = nonagentic's own tenant on
 * `mlx_api` (the service key is held by the platform; nothing is stored per tenant). `tenant_key` = any other
 * provider: the tenant enters URL + token, the token is write-only. There is no licence-minted fallback token.
 */
export type TokenMode = 'internal' | 'tenant_key';

/** D70: which side of the line a tenant is on. Only the platform-owned (nonagentic) tenant is `internal`. */
export type GpuHosting = 'internal' | 'external';
export const gpuHostingOf = (t: Pick<Tenant, 'licence'>): GpuHosting =>
	t.licence.platform_owned ? 'internal' : 'external';

/** The provider's API dialect: an OpenAI-compatible endpoint, a Laya System-1 endpoint (gate answers), or a w2v speech
 *  gateway (asr / tts only; B3 T4d, D101). */
export type LlmKind = 'openai' | 'laya' | 'w2v';
export const LLM_KINDS: readonly LlmKind[] = ['openai', 'laya', 'w2v'];
/**
 * What Agent calls a provider for: `qualify` reads each customer turn; `gates` answers the gate questions;
 * `generator` drafts a job's design (D89); `asr` / `tts` are the job's speech (D101/D102). Order = the API's
 * (`providers::llm::Use::ALL`).
 */
export type LlmUse = 'qualify' | 'gates' | 'generator' | 'asr' | 'tts';
export const LLM_USES: readonly LlmUse[] = ['qualify', 'gates', 'generator', 'asr', 'tts'];
export const LLM_KIND_LABEL: Record<LlmKind, string> = {
	openai: 'OpenAI-compatible',
	laya: 'Laya',
	w2v: 'w2v speech',
};
/** A speech use: served by the voice connector, never by the LLM runtime (`Use::is_speech`). */
export const isSpeechUse = (use: LlmUse): boolean => use === 'asr' || use === 'tts';

/**
 * What each use is for and the provider kinds that can serve it (UX reference: care-bus-agents; this app's API refuses
 * any other kind with 422 `provider_kind_mismatch`, or `kind_not_valid_for_use` across speech and LLM). `kind` is the
 * first of `kinds` (the one "Add a … provider" offers). `fallback` is what runs while the use is unbound.
 */
export const LLM_USE_RULES: Record<
	LlmUse,
	{ label: string; kind: LlmKind; kinds: readonly LlmKind[]; purpose: string; fallback: string }
> = {
	qualify: {
		label: 'Turn qualification',
		kind: 'openai',
		kinds: ['openai'],
		purpose: 'Reads each customer turn: intent, tone and what is being asked.',
		fallback: 'None: turns cannot be answered',
	},
	gates: {
		label: 'Laya gates',
		kind: 'laya',
		kinds: ['laya'],
		purpose: 'Answers the gate questions at request, tool and response points (Laya System-1).',
		fallback: 'None: no gate question runs',
	},
	generator: {
		label: 'Generator',
		kind: 'openai',
		kinds: ['openai'],
		purpose: 'Drafts a job design from your documents (the generator, D89).',
		fallback: 'None: the generator cannot draft',
	},
	asr: {
		label: 'Speech to text',
		kind: 'w2v',
		kinds: ['w2v', 'openai'],
		purpose: 'Transcribes what a caller says on a voice channel (D101).',
		fallback: 'None: voice turns are refused',
	},
	tts: {
		label: 'Text to speech',
		kind: 'w2v',
		kinds: ['w2v', 'openai'],
		purpose: 'Speaks the answer on a voice channel, in the voice you pick (D102).',
		fallback: 'None: voice turns are refused',
	},
};

/** Why a provider of `kind` cannot serve `use`, or null when it can. One sentence for the UI and the 422 (mirrors the
 *  API's `Use::refusal`). */
export function llmUseProblem(use: LlmUse, kind: LlmKind): string | null {
	const rule = LLM_USE_RULES[use];
	if (rule.kinds.includes(kind)) return null;
	if (isSpeechUse(use))
		return `${rule.label} needs a w2v or OpenAI-compatible speech provider; this one is ${LLM_KIND_LABEL[kind]}.`;
	if (kind === 'w2v')
		return `A w2v provider serves speech (asr, tts) only; ${rule.label} needs an LLM provider.`;
	return use === 'gates'
		? `Only a Laya provider can serve gates; this one is ${LLM_KIND_LABEL[kind]}.`
		: `${rule.label} needs an ${LLM_KIND_LABEL.openai} provider; this one is ${LLM_KIND_LABEL[kind]}.`;
}
/** The API's refusal code for `llmUseProblem` (`Use::refusal`). */
export const llmUseProblemCode = (use: LlmUse, kind: LlmKind): string =>
	isSpeechUse(use) || kind === 'w2v' ? 'kind_not_valid_for_use' : 'provider_kind_mismatch';
/** A `tts` voice the API accepts (`voice::valid_voice`): 1–64 of letters, digits, `-`, `_`. */
export const validVoice = (v: string): boolean => /^[A-Za-z0-9_-]{1,64}$/.test(v);

export interface LlmProvider {
	id: string;
	name: string;
	kind: LlmKind;
	base_url: string;
	token_mode: TokenMode;
	secret: SecretState;
	models: string[];
	updated_at: Iso;
	updated_by: Sub;
	last_test: { ok: boolean; at: Iso; detail: string } | null;
	/** Where the provider processes data, as declared by the tenant (region pinning); mlx_api = EU. Optional. */
	region?: string | null;
}

export interface LlmProviderUse {
	use: LlmUse;
	label: string;
	provider_id: string | null;
	model: string | null;
	/** `tts` only: the voice the answer is spoken in (null = the provider's default). */
	voice?: string | null;
}

/** A job whose LIVE binding of a use cannot serve (provider deleted, a kind that no longer fits; B3 T3 I1/I3). */
export interface LlmLiveProblem {
	job: JobKey;
	use: LlmUse;
	code: string;
	message: string;
}

export interface LlmProviders {
	/** D70: `internal` = nonagentic's own tenant (mlx_api only); `external` = the tenant brings URL + token. */
	hosting: GpuHosting;
	providers: LlmProvider[];
	uses: LlmProviderUse[];
	/** Set when no use resolves to a usable provider: turns cannot be answered (customers get "the assistant is unavailable"). */
	warning: null | { code: 'no_llm_provider' | 'key_required'; message: string };
	/** Live bindings the runtime refuses, per job (never silent: shown until the job publishes a fix). */
	live_problems: LlmLiveProblem[];
}

export interface LlmProviderInput {
	name: string;
	/** Absent: `openai` on create, the stored kind on update. */
	kind?: LlmKind;
	base_url: string;
	models: string[];
	/** Write-only. Absent or '' keeps the stored key; null clears it. */
	secret?: string | null;
}

/**
 * `POST /api/providers/test`: probe the drawer's values before saving. Nothing is stored — not the provider, not the
 * key, not a `last_test`. Key resolution: a typed `secret` wins; `null` tests without one; absent with
 * `provider_id` uses that provider's stored key, but only for its stored host (422 `secret_required_for_new_host`).
 */
export interface LlmProviderTestInput {
	kind: LlmKind;
	base_url: string;
	secret?: string | null;
	provider_id?: string;
}

export interface LlmProviderTestResult {
	ok: boolean;
	at: Iso;
	detail: string;
	/** What the endpoint's model list (`/models`, Laya `/v1/models`) returned; empty when the probe failed. */
	models: string[];
}

export type SlotKey = 'embed' | 'rerank' | 'chat' | 'ocr' | 'vision' | 'transcribe';

// ---- D70 provider catalogue ------------------------------------------------------------------------------------

/** A GPU capability the Providers page binds: Agent's own LLM uses, or one rag/ingest slot. */
export type GpuCapability = 'llm' | SlotKey;

/**
 * The internal kinds carry no endpoint here: `INTERNAL_MLX_TEXT_URL` / `INTERNAL_MLX_MEDIA_URL` /
 * `INTERNAL_PGX_VLLM_URL` are deployment configuration, and the provider-slot response returns the configured
 * value. A hardcoded copy would go stale the moment an operator moves the host.
 */

/** One provider the tenant can pick for a capability: its default endpoint and the models it offers there. */
export interface GpuProviderKind {
	label: string;
	/** Only nonagentic's own tenant may bind it (mlx_api, pgx vLLM). */
	internal: boolean;
	/** Default endpoint per capability; '' = the tenant must type its own (e.g. an Azure resource URL). */
	base_url: Partial<Record<GpuCapability, string>>;
	/** Models offered per capability. The model select also allows a model not listed here. */
	models: Partial<Record<GpuCapability, string[]>>;
}

export const GPU_PROVIDER_KINDS: Record<string, GpuProviderKind> = {
	mlx_api: {
		label: 'mlx_api (internal)',
		internal: true,
		base_url: {},
		models: {
			embed: ['BAAI/bge-m3'],
			rerank: ['BAAI/bge-reranker-v2-m3'],
			vision: ['Qwen2-VL-2B-Instruct-4bit'],
			ocr: ['PaddleOCR-VL'],
			transcribe: ['whisper-large-v3-turbo'],
		},
	},
	pgx_vllm: {
		label: 'pgx vLLM (internal)',
		internal: true,
		base_url: {},
		models: { llm: ['local-llm'], chat: ['local-llm'] },
	},
	openai_compatible: {
		label: 'OpenAI-compatible',
		internal: false,
		base_url: {
			llm: 'https://api.mistral.ai/v1',
			embed: 'https://api.mistral.ai/v1',
			chat: 'https://api.mistral.ai/v1',
			vision: 'https://api.mistral.ai/v1',
			transcribe: 'https://api.openai.com/v1',
		},
		models: {
			llm: ['mistral-small-latest', 'mistral-large-latest', 'gpt-4.1-mini', 'gpt-4.1'],
			embed: ['mistral-embed', 'text-embedding-3-large', 'text-embedding-3-small'],
			chat: ['mistral-small-latest', 'mistral-large-latest', 'gpt-4.1-mini'],
			vision: ['pixtral-large-latest', 'gpt-4.1-mini'],
			transcribe: ['whisper-1', 'gpt-4o-transcribe'],
		},
	},
	voyage: {
		label: 'Voyage AI',
		internal: false,
		base_url: { embed: 'https://api.voyageai.com/v1', rerank: 'https://api.voyageai.com/v1' },
		models: { embed: ['voyage-3.5', 'voyage-3-large'], rerank: ['rerank-2.5', 'rerank-2.5-lite'] },
	},
	cohere: {
		label: 'Cohere',
		internal: false,
		base_url: { rerank: 'https://api.cohere.com/v2' },
		models: { rerank: ['rerank-v3.5', 'rerank-multilingual-v3.0'] },
	},
	mistral_ocr: {
		label: 'Mistral OCR',
		internal: false,
		base_url: { ocr: 'https://api.mistral.ai/v1' },
		models: { ocr: ['mistral-ocr-latest', 'mistral-ocr-2505'] },
	},
	azure_document_intelligence: {
		label: 'Azure Document Intelligence',
		internal: false,
		base_url: { ocr: '' },
		models: { ocr: ['prebuilt-read', 'prebuilt-layout'] },
	},
};

/** Display name of a provider kind (an unknown kind shows as itself). */
export const gpuKindLabel = (kind: string): string => GPU_PROVIDER_KINDS[kind]?.label ?? kind;
/** The kind nonagentic's own tenant binds for `cap`: pgx vLLM for LLM/chat, mlx_api for everything else. */
export const internalKindFor = (cap: GpuCapability): string =>
	cap === 'llm' || cap === 'chat' ? 'pgx_vllm' : 'mlx_api';
/** True for a kind only nonagentic's own tenant binds (mlx_api, pgx vLLM). */
export const isInternalKind = (kind: string | null | undefined): boolean =>
	!!kind && !!GPU_PROVIDER_KINDS[kind]?.internal;
/** The kind's default endpoint for `cap` ('' = the tenant types it). */
export const kindBaseUrl = (kind: string, cap: GpuCapability): string =>
	GPU_PROVIDER_KINDS[kind]?.base_url[cap] ?? '';
/** The models the kind offers for `cap`. */
export const kindModels = (kind: string, cap: GpuCapability): string[] =>
	GPU_PROVIDER_KINDS[kind]?.models[cap] ?? [];
/** The model strata serves a slot reset to the deployment default (F-08, B3 T4b; agent `Slot::deployment_default`):
 *  the internal tenant's mlx_api / pgx vLLM model; null = this platform deploys none (`transcribe`). */
export const slotDeploymentDefault = (slot: SlotKey): string | null =>
	slot === 'transcribe' ? null : (kindModels(internalKindFor(slot), slot)[0] ?? null);

/** LLM provider presets for the "Add provider" drawer: pick one, then the URL, models and token are filled in. */
export interface LlmPreset {
	key: string;
	label: string;
	kind: LlmKind;
	base_url: string;
	models: string[];
}
export const LLM_PRESETS: LlmPreset[] = [
	{
		key: 'mistral',
		label: 'Mistral (EU)',
		kind: 'openai',
		base_url: 'https://api.mistral.ai/v1',
		models: ['mistral-small-latest', 'mistral-large-latest'],
	},
	{
		key: 'openai',
		label: 'OpenAI',
		kind: 'openai',
		base_url: 'https://api.openai.com/v1',
		models: ['gpt-4.1-mini', 'gpt-4.1'],
	},
	{
		key: 'laya',
		label: 'Laya (nonagentic API)',
		kind: 'laya',
		base_url: 'https://api.nonagentic.ai/laya',
		models: ['jev-latest', 'laya-multilingual'],
	},
	{ key: 'custom', label: 'Other OpenAI-compatible endpoint', kind: 'openai', base_url: '', models: [] },
];

/** A rag/ingest provider slot (A1 manifest + A2 connections). `version` is the expectedVersion for a PUT. */
export interface ProviderSlot {
	slot: SlotKey;
	service: 'strata_rag' | 'strata_ingest';
	label: string;
	purpose: string;
	/** The provider kinds this tenant may pick (D70): the internal kind (mlx_api, or pgx vLLM for chat) for the
	 *  internal tenant, external kinds otherwise. */
	kinds: string[];
	configured: boolean;
	kind: string | null;
	base_url: string | null;
	model: string | null;
	version: number;
	secret: SecretState;
	updated_at: Iso | null;
	/** The remote slot moved (another app wrote it): shown, never overwritten. */
	drift: null | {
		remote_version: number;
		changed_by: string;
		at: Iso;
		/** What the other app wrote (adopting it saves this to the draft, D69). */
		remote?: { kind: string; base_url: string | null; model: string };
	};
	used_by: string[];
	/** Where the slot's provider processes data (region pinning), as declared by the tenant; mlx_api = EU. Optional. */
	region?: string | null;
	/** `default` = reset to the deployment default (F-08, `resetProviderSlot`): configured (deployed), no kind, URL or
	 *  token of the tenant's, `model` = the default's. Absent = `tenant`. */
	source?: 'tenant' | 'default';
	/** The embed slot only: the published model the newest push withheld, because a store a job still reads holds
	 *  content on the old model (`roots`: the strata roots holding it). Rebuild them through their model change and
	 *  publish; `pushProviderSlot` queues it again. Null / absent = nothing withheld. */
	withheld?: SlotWithheld | null;
	/** `pushProviderSlot` answer only: the queued push (outbox row id). */
	push_id?: string;
}

/** The embed model a push withheld (`ProviderSlot.withheld`). */
export interface SlotWithheld {
	model: string | null;
	configured: boolean;
	version: number;
	release: string | null;
	roots: string[];
	at: Iso;
}

/** `pushProviderSlot` answer: the slot, and the push queued again. */
export type ProviderSlotPush = ProviderSlot & { push_id: string };

/** `resetProviderSlot` answer: the slot, and the model change fanned out to the jobs pinning the old model (D93). */
export type ProviderSlotReset = ProviderSlot & { model_change_id?: string | null };

// ---- B3 T4a model changes (D83, D92, D93; plan §5.8) ---------------------------------------------------------------

/** How a change rebuilds a job's stores: `reembed` (embed: new vectors), `copy` (rerank/chat: vectors copied, new
 *  bindings, no GPU embed call), `reingest` (ocr/vision/transcribe: documents re-ingested from their originals). */
export type ModelRebuild = 'reembed' | 'copy' | 'reingest';
export const MODEL_REBUILD_LABEL: Record<ModelRebuild, string> = {
	reembed: 'Re-embed',
	copy: 'Copy',
	reingest: 'Re-ingest',
};
/** One job's part of a change: `pending` → (Re-embed) `reembedding` → `ready` → (Accept) `accepted` (pins in the
 *  draft) → `published` (the job published them). `dismissed` is never offered (the old generation stays). */
export type ModelChangeState = 'pending' | 'reembedding' | 'ready' | 'accepted' | 'published' | 'dismissed';
export const MODEL_CHANGE_STATES: readonly ModelChangeState[] = [
	'pending',
	'reembedding',
	'ready',
	'accepted',
	'published',
	'dismissed',
];
/** The six knowledge capabilities a generation carries a model for (null = the deployment default). */
export type ModelSet = Record<SlotKey, string | null>;

/** A document strata_ingest kept no original of: a re-ingest could not rebuild it (re-upload or re-sync it). */
export interface MissingOriginal {
	document_id: string;
	reason: string;
	title?: string | null;
	source_id?: string | null;
}

/** One strata root per model set of one root store (D83, R7). */
export interface StoreGeneration {
	id: string;
	store_id: string;
	strata_store_id: string | null;
	models: ModelSet;
	build: 'initial' | ModelRebuild;
	state: 'building' | 'ready' | 'failed' | 'retired';
	chunks: number | null;
	missing_originals: MissingOriginal[];
	error: string | null;
	created_at: Iso;
}

/** A tenant slot model change (one row per slot save that moved a model), fanned out to the jobs it affects. */
export interface ModelChangeSummary {
	id: string;
	scope: 'tenant' | 'platform';
	/** The capability whose model changed. */
	slot: SlotKey;
	/** null = the deployment default. */
	from_model: string | null;
	to_model: string | null;
	rebuild: ModelRebuild;
	created_at: Iso;
	created_by: Sub;
	jobs: { job: JobKey; state: ModelChangeState }[];
}

/** One affected store of a job's part. */
export interface ModelChangeStore {
	/** The store's key (null when the store is gone). */
	store: string | null;
	store_id: string;
	from_generation: StoreGeneration | null;
	to_generation: StoreGeneration | null;
	chunks: number | null;
	missing_originals: MissingOriginal[];
}

/** One job's part of a change (`modelChange` detail rows; the answer of re-embed and accept). */
export interface ModelChangeJob {
	model_change_id: string;
	job: JobKey;
	state: ModelChangeState;
	stores: ModelChangeStore[];
	chunks: number | null;
	/** The job's live release id, or null (never published). */
	live_release: string | null;
	draft_rev: number;
	updated_at: Iso;
}

export interface ModelChange extends Omit<ModelChangeSummary, 'jobs'> {
	jobs: ModelChangeJob[];
}

/** `testProviderSlotDraft` input (F-09): the drawer's values; `secret` absent = the stored token, same URL only. */
export interface SlotTestInput {
	kind: string;
	base_url: string;
	model: string;
	secret?: string | null;
}

/** `testProviderSlotDraft` answer: a failing provider is `ok: false` (never a 5xx); `dims` = embed vector size. */
export interface SlotTestResult {
	ok: boolean;
	at: Iso;
	detail: string;
	dims?: number;
}

export interface SlotInput {
	kind: string;
	base_url: string;
	model: string;
	secret?: string | null;
	expected_version: number;
}

// ---- T7 knowledge stores ---------------------------------------------------------------------------------------

export interface KnowledgeStore {
	id: string;
	key: string;
	label: string;
	parent_id: string | null;
	kind: 'group' | 'leaf';
	/** Journey areas / knowledge kinds the store answers for. */
	areas: string[];
	/** Bound strata_rag store; null = not created there yet (K45 `store_ensure`). */
	strata_store_id: string | null;
	/** An open `store_ensure` outbox row. */
	ensuring: boolean;
	documents: number;
	chunks: number;
	tokens: number;
	updated_at: Iso;
	/** strata_rag region the store's data is pinned to (EU / US residency); absent = not pinned yet. */
	region?: 'eu' | 'us';
}

export interface KnowledgeStoreInput {
	key: string;
	label: string;
	parent_id: string | null;
	kind: 'group' | 'leaf';
}

export interface KnowledgeDocument {
	id: string;
	store_id: string;
	title: string;
	source: string;
	tokens: number;
	chunks: number;
	added_at: Iso;
	ingest_job_id: string | null;
	/** D68: the document's language (BCP-47 primary tag, `nl`), set from the source that produced it. */
	language: string;
}

export interface KnowledgeHit {
	store_id: string;
	store_path: string[];
	document: string;
	text: string;
	page_start: number | null;
	page_end: number | null;
	/** The final score the hit is ranked by: rerank blended with the first-stage score when reranked, else that score. */
	score: number;
	/** strata_rag chunk id and its position in the document. */
	chunk_id: string;
	ord: number;
	/** Per-stage scores (strata `scores`). */
	scores: QueryScores;
	/** R10: `tenant` (your stores) or `platform` (a pinned platform pack, `pack` = `key@version`). */
	layer?: 'tenant' | 'platform' | null;
	pack?: string | null;
	/** PK: strata's document metadata the playground passes on; platform documents carry `provenance`. */
	metadata?: { provenance?: Provenance | null };
}

export interface KnowledgeQueryInput {
	query: string;
	mode: 'embed' | 'laya' | 'hybrid';
	top_k: number;
	min_score: number;
	/** Laya routing branches to follow (1–8); ignored in embed mode. */
	beam?: number;
	/** D91: search the tree of this store (any store of it); required (422 `root_required` + `roots`) when the
	 *  tenant has more than one root. */
	store_id?: string;
	/** T4a: search as this job's test environment does (the generation its draft pins). */
	job?: string;
	/** PK (T5): with `job`, search one knowledge kind's leaf set (its tenant store + pinned packs, as the draft
	 *  resolves them) instead of a store tree: no `store_id`, never `root_required`. */
	stage?: string;
	kind?: string;
}

export interface KnowledgeQueryResult {
	query: string;
	mode: 'embed' | 'laya' | 'hybrid';
	searched: string[];
	skipped_unbound: string[];
	hits: KnowledgeHit[];
	/** = `timings_ms.total`. */
	took_ms: number;
	routing: QueryRouting;
	timings_ms: QueryTimings;
	/** PK (T5, kind-scoped searches): pack pins searched without (excluded, retired, unreadable or never listed). */
	degraded?: string[];
}

// ---- T8 ingest jobs ------------------------------------------------------------------------------------------

/** strata_ingest job states (PR strata_ingest#11): `queued` → `running` → `completed` | `failed` | `rejected`; cancel adds
 * `cancelling` (a step was in flight: the worker finishes it, nothing new starts) → `cancelled`. An unfinished `DELETE`
 * leaves `deleting`: marked, some staged blobs may already be gone, rows kept; never claimed, retry and cancel → 409,
 * `DELETE` again resumes and finishes it. Clients must know all eight. */
export type IngestStatus =
	| 'queued'
	| 'running'
	| 'cancelling'
	| 'deleting'
	| 'completed'
	| 'failed'
	| 'rejected'
	| 'cancelled';
/** A job that can still be cancelled. */
export const INGEST_CANCELLABLE: readonly IngestStatus[] = ['queued', 'running'];
/** A job that has ended: only these can be deleted (and a `deleting` one, to resume an unfinished delete). */
export const INGEST_ENDED: readonly IngestStatus[] = ['completed', 'failed', 'rejected', 'cancelled'];

export interface IngestStep {
	kind: string;
	slot: SlotKey | null;
	provider: string | null;
	model: string | null;
	/** `cancelled`: a pending step dropped by a cancel. */
	status: 'completed' | 'failed' | 'running' | 'queued' | 'skipped' | 'cancelled';
	attempt: number;
	error: { code: string; message: string } | null;
	/** T8: the provider-slot version this step ran with (shows which jobs a slot drift affects). */
	slot_version?: number;
}

export interface IngestJob {
	id: string;
	store_id: string;
	source_label: string;
	source_kind: 'file' | 'crawl' | 'sitemap' | 'datatable' | 'api';
	source_id: string | null;
	status: IngestStatus;
	created_at: Iso;
	finished_at: Iso | null;
	created_by: string;
	documents: number;
	pages: { total: number; by_status: Record<string, number> } | null;
	steps: IngestStep[];
	error: { code: string; message: string } | null;
	/** T8: metered provider cost of this job so far (EUR, from the usage ledger); absent = not metered yet. */
	cost_eur?: number;
	/** T8: passages the chunk step produced (absent while not chunked). */
	chunks?: number;
}

/** `DELETE /api/knowledge/ingest-jobs/{id}` → strata_ingest `DELETE /api/ingest/jobs/{id}`: the ended job, its staged
 * blobs (original, figures, page images, audio segments) and a crawl / sitemap parent's child jobs are gone. Documents
 * it already delivered stay indexed in the knowledge store (`documents: 'retained'`, listed in `document_ids`). */
export interface IngestJobDeleted {
	job_id: string;
	/** The status the job had. */
	status: IngestStatus;
	deleted_jobs: number;
	deleted_objects: number;
	document_ids: string[];
	documents: 'retained';
}

/** strata_ingest#11 (@2b04016): the `DELETE` waits up to 5 s; a job still being removed answers 202 with this body.
 * The client polls `GET` (`ingestJob`) until it answers 404 — then the job is gone. */
export interface IngestJobDeleting {
	job_id: string;
	status: 'deleting';
}

/** The 202 answer (still deleting), as opposed to the 200 `IngestJobDeleted` (gone). */
export function isStillDeleting(
	d: IngestJobDeleted | IngestJobDeleting | null | undefined,
): d is IngestJobDeleting {
	return !!d && d.status === 'deleting' && !('deleted_jobs' in d);
}

// ---- T9 sources ----------------------------------------------------------------------------------------------

export type SourceType = 'website' | 'api' | 'file' | 'datatable';
export type RunStatus = 'running' | 'completed' | 'failed' | 'unsupported';

export interface SourceMapping {
	source_field: string;
	entity: string;
	attribute: string;
}

export interface Source {
	id: string;
	job: JobKey;
	name: string;
	type: SourceType;
	config: Record<string, unknown>;
	mapping: SourceMapping[];
	target_store_id: string;
	/** Stored and shown; never run while the platform scheduler is off (D82, B3 T8b). */
	schedule: string | null;
	/** True only when a schedule is set AND the platform scheduler runs (off on this platform: always false). */
	schedule_active?: boolean;
	enabled: boolean;
	has_credentials: boolean;
	credentials_updated_at: Iso | null;
	created_at: Iso;
	last_run: SourceRun | null;
	/** D68: language of the documents this source produces (one of the job's languages; default its first). */
	language: string;
	/** PK-8 (T6): platform topic keys this source's documents replace in searches (sorted, ≤ 32, 1–128 chars). */
	supersedes?: string[];
}

export interface SourceInput {
	name: string;
	type: SourceType;
	config: Record<string, unknown>;
	mapping: SourceMapping[];
	target_store_id: string;
	schedule: string | null;
	enabled: boolean;
	/** Write-only: absent keeps, null clears, `{secret}` replaces. */
	credentials?: { secret: string } | null;
	/** D68: one of the job's languages; absent = the job's first (default) language. */
	language?: string;
	/** PK-8 (T6): absent or null keeps the stored list, `[]` clears it. The form always sends it. */
	supersedes?: string[] | null;
}

/** `{code, message}` as the API sends every refusal and failure. */
export interface RunError {
	code: string;
	message: string;
}

/** One file of a file-source re-sync and what became of it (one ingest job per accepted file). */
export interface SourceUpload {
	file_name: string;
	bytes: number;
	content_type: string | null;
	/** Null when the file was refused before ingest (unsupported type, …). */
	ingest_job_id: string | null;
	error: RunError | null;
}

/** What a completed run did to the documents of the source's earlier runs (only completed runs replace). */
export interface ReplaceReport {
	/** Earlier documents deleted from strata_rag. */
	deleted: number;
	/** Earlier documents that were already gone (deleted by hand). */
	already_gone: number;
	/** Earlier documents kept: unchanged in this run, or shared with another source. */
	kept_shared: number;
	/** Document ids that could not be deleted; retried on the next completed run. */
	failed: string[];
}

export interface SourceRun {
	id: string;
	source_id: string;
	status: RunStatus;
	started_at: Iso;
	finished_at: Iso | null;
	started_by: string;
	documents: number;
	pages: { total: number; by_status: Record<string, number> } | null;
	error: RunError | null;
	/** The strata_ingest jobs of this run: one for a feed, one per accepted file for a file source. */
	ingest_job_ids: string[];
	/** Whether the run used the stored mapping or an edited one. */
	mapping: 'same' | 'edited' | null;
	/** File sources: the uploaded files; null for feeds. */
	uploads: SourceUpload[] | null;
	/** Set once a completed run replaced the earlier runs' documents; null for running/failed runs. */
	replace_report: ReplaceReport | null;
}

/** `GET …/sources/{id}/runs/{run}`: one run with its ingest jobs and the documents it produced. */
export interface SourceRunDetail {
	run: SourceRun;
	ingest_jobs: IngestJob[];
	documents: KnowledgeDocument[];
}

export interface SourceSample {
	ok: boolean;
	status: number | null;
	detail: string;
	fields: string[];
	sample: unknown;
}

export interface ColumnPreview {
	file: string;
	format: 'csv' | 'xlsx';
	row_count: number;
	columns: string[];
	rows: Record<string, string>[];
	/** XLSX: every sheet of the workbook (empty for CSV), and the one read. */
	sheets: string[];
	sheet: string | null;
	/** False: the first row was read as data and the columns are `column_1..N`. */
	header_row: boolean;
}

/** Options of a column preview (the multipart `sheet` / `header_row` fields). */
export interface PreviewOptions {
	/** XLSX sheet to read; absent = the first. */
	sheet?: string;
	/** Default true. */
	header_row?: boolean;
}

// ---- D68 per-language knowledge coverage -----------------------------------------------------------------------

/** One knowledge kind × the job's languages: documents in its bound store per language. */
export interface CoverageRow {
	stage: string;
	kind: string;
	required: boolean;
	/** Bound store path, or null (unbound: every language is missing). */
	store: string | null;
	/** Documents per served language (0 = a gap). */
	documents: Record<string, number>;
	/** Served languages with no document. */
	missing: string[];
}

/** `GET /api/jobs/{job}/knowledge/coverage` (D68): a warning view, never a block. */
export interface KnowledgeCoverage {
	job: JobKey;
	/** The languages the job serves (draft, D59), first = default. */
	languages: string[];
	rows: CoverageRow[];
	/** Missing (kind, language) cells of required kinds. */
	gaps: number;
	/** Documents in a bound store whose language the job does not serve. */
	unserved: Record<string, number>;
}

/** One Entity.attribute a source field may map to (the job's effective ontology). */
export interface MappingTarget {
	entity: string;
	attribute: string;
	type: string;
	pii: boolean;
}

// ---- T13 channels --------------------------------------------------------------------------------------------

/** What agent-edge last reported for a binding (`channel_webhook_status`). The webhook URL is never stored, so never
 *  shown; `pending_update_count` is `null` unless something that knows it reported it. `quiet` = no update for 24 h
 *  (informational); `down` = the edge saw the webhook unset or failing. */
export interface WebhookHealth {
	status: 'ok' | 'degraded' | 'quiet' | 'down';
	last_update_at: Iso | null;
	pending_update_count: number | null;
	/** Why the webhook is `down` (`url_unset` · `url_mismatch` · `webhook_error`); null otherwise. */
	down_reason?: string | null;
	last_error: string | null;
	last_error_at: Iso | null;
	/** When the edge last reported (live only). */
	observed_at?: Iso;
}

/** Identity level a conversation can reach (10 §3.1): 0 public · 1 identified · 2 verified · 3 step-up · S staff. */
export type ChannelIdentityLevel = '0' | '1' | '2' | '3' | 'S';

/** Which job + release answers a channel, resolved from the job's current release (K40). */
export interface ChannelAnsweredBy {
	job: JobKey;
	job_label: string;
	release_id: string | null;
	release_hash: string | null;
	env: 'prod' | 'test';
}

/** One operation of the job a channel serves, with the identity level the runner requires (12 §3 N5). */
export interface ChannelJobOperation {
	op: string;
	side_effect: 'R' | 'W' | 'X' | 'P';
	identity_level: ChannelIdentityLevel;
	/** the level a step-up reaches. */
	raises_to?: ChannelIdentityLevel;
	note?: string;
}

export interface Channel {
	id: string;
	kind: 'telegram' | 'staff_chat' | 'web_chat' | 'voice' | 'helpdesk';
	label: string;
	/** `shared_env_bot` = one bot for the deployment (K40); `per_tenant_bot` later; `job_binding` = a job's own
	 *  channel binding (B12, live); `reserved` not built. */
	binding: 'shared_env_bot' | 'per_tenant_bot' | 'job_binding' | 'staff_console' | 'reserved';
	milestone: 'M1' | 'M2' | 'M3' | 'M4';
	/** `not_ready` = bound but disabled or not verified yet; `available` = built, no job has bound it. */
	state: 'live' | 'other_tenant' | 'not_ready' | 'available' | 'later' | 'reserved';
	bot_username: string | null;
	bound_tenant: string | null;
	bound_job: JobKey | null;
	env: 'prod' | 'test' | null;
	webhook: WebhookHealth | null;
	/** The job's conversations of this kind and env in 7 days (a conversation names no binding). When a job has
	 *  several bindings of one kind + env, the first carries the count and the others are `null` (said in `note`). */
	conversations_7d: number | null;
	note: string;
	/** Level a new conversation starts at on this channel (T13). */
	identity_start?: ChannelIdentityLevel;
	/** Highest level reachable on this channel; operations above it are refused `identity_required`. */
	identity_ceiling?: ChannelIdentityLevel;
	/** How identity is established / raised on this channel. */
	identity_how?: string;
	/** Edge ingress host the customer reaches: for web chat the widget host (`WEBCHAT_PUBLIC_BASE`). `null` for
	 *  Telegram from the API: the edge sets the webhook and the API stores neither its URL nor its host. */
	edge_host?: string | null;
	/** A live (enabled + verified) web chat binding's public widget key `wk_…` (it is in the site's HTML anyway), so
	 *  "Try it" opens `<edge_host>/webchat/demo?key=<widget_key>`; `null` for every other channel. Never a secret. */
	widget_key?: string | null;
	/** Set by the API when the channel delivers to this tenant. */
	answered_by?: ChannelAnsweredBy | null;
	/** Multi-job: the tenant's other jobs this channel can also reach (the staff test chat picks a job; a job with no
	 *  release answers from its draft in the test env). Set by the API. */
	also_answers?: ChannelAnsweredBy[];
	/** D126: which side of its job the binding serves; absent = `customer` (older servers). Worker rows are the
	 *  job's staff-side bindings. */
	side?: 'customer' | 'worker';
	/** D126: the job the binding belongs to (its Channels page is `/admin/jobs/{job_key}/channels`). */
	job_key?: JobKey | null;
}

export interface ChannelsView {
	channels: Channel[];
	/** `tenants.status`; the edge refuses unless it is `active` (03 §2.9). Set by the API. */
	tenant_status?: 'active' | 'suspended' | 'archived';
	/** The operations of the job the customer channel serves, for the identity-level coverage view. */
	job_operations?: ChannelJobOperation[];
	/** The job `job_operations` belongs to. */
	operations_job?: JobKey | null;
	gate: LicenceVerdict;
	/** One neutral text for every refusal code (03 §2.9, K35). */
	refusal_text: string;
	refusals_24h: number;
}

export interface ChannelTestResult {
	delivered: boolean;
	refused: boolean;
	text: string;
	message_id: string | null;
	at: Iso;
}

// ---- PK tenant platform exclusions (T7) -------------------------------------------------------------------------

/** One tenant-wide exclusion of platform knowledge (`GET /api/knowledge/platform-exclusions`): every pack whose key is
 *  `key_prefix` or below it (segment-wise: `industry/retail` covers `industry/retail/hr`, not `industry/retailx`) is
 *  left out of every job's searches, live on save. */
export interface PlatformExclusion {
	key_prefix: string;
	reason: string;
	created_at: Iso;
}

// ---- B4 T16a (F-14): the tenant holiday calendar ------------------------------------------------------------------

/** One closed day of the tenant calendar (`YYYY-MM-DD`; the label is optional text, at most 80 characters). */
export interface TenantHoliday {
	date: string;
	label: string;
	closed: true;
}
/** The tenant's holiday calendar: the timezone people name it in and its closed days (≤ 366, one per date). */
export interface TenantCalendar {
	timezone: string;
	holidays: TenantHoliday[];
}
/** `PUT /api/tenant/calendar` body: `label` and `closed` may be left out (`closed` is always true). */
export interface TenantCalendarInput {
	timezone: string;
	holidays: { date: string; label?: string; closed?: true }[];
}
/**
 * `GET /api/tenant/calendar` — D54/D69: `draft` = the tenant's working row (saved, not live); `live` = the calendar in
 * the active release of the tenant's most recently applied job; `drafted` = the keys of the jobs whose draft still
 * carries an unpublished calendar change (their Production is on an older value until they publish).
 */
export interface TenantCalendarView {
	live: TenantCalendar | null;
	draft: TenantCalendar | null;
	drafted: string[];
}
