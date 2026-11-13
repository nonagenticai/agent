// The `TenantApi` contract.
import type {
	JobAudience,
	JobKey,
	JobSummary,
	Me,
	Paged,
	Role,
	RoleInfo,
	Sub,
	TenantUser,
} from '$lib/api/types/core';
import type {
	AuditEvent,
	AuditQuery,
	AuditSummary,
	ChannelTestResult,
	ChannelsView,
	ColumnPreview,
	IngestJob,
	IngestJobDeleted,
	IngestJobDeleting,
	IngestStatus,
	InviteAccepted,
	InviteInput,
	InviteQuery,
	MemberInvite,
	JobBoardRow,
	MemberActivity,
	CreateBlankJobInput,
	CreateTenantJobInput,
	JobStaff,
	JobStaffInput,
	JobTemplateCatalogEntry,
	TenantProfile,
	TenantProfileInput,
	KnowledgeCoverage,
	KnowledgeDocument,
	KnowledgeQueryInput,
	KnowledgeQueryResult,
	KnowledgeStore,
	KnowledgeStoreInput,
	LlmProvider,
	LlmProviderInput,
	LlmProviders,
	LlmProviderTestInput,
	LlmProviderTestResult,
	LlmUse,
	MappingTarget,
	PreviewOptions,
	ProvisionRow,
	ProviderSlot,
	ProviderSlotPush,
	ProviderSlotReset,
	SlotInput,
	SlotTestInput,
	SlotTestResult,
	SlotKey,
	Source,
	SourceInput,
	SourceMapping,
	SourceRun,
	SourceRunDetail,
	SourceSample,
	TenantOverview,
	TenantHome,
	UsageGroup,
	UsageReport,
	UsageWindow,
	ModelChange,
	ModelChangeJob,
	ModelChangeState,
	ModelChangeSummary,
	TenantCalendarInput,
	TenantCalendarView,
} from '$lib/api/types/tenant';

export type TenantApi = {
	// T2
	/** `GET /api/tenant/overview` — licence verdict, Integrations panel (provision_outbox/state), jobs, members. */
	tenantOverview(): Promise<TenantOverview>;
	/** `POST /api/tenant/provisioning/{id}/retry` (tenant_admin) — back to pending, attempts 0, now. */
	retryProvision(id: string): Promise<ProvisionRow>;
	/** `POST /api/tenant/provisioning/{id}/cancel` (tenant_admin, D88 §R.1). */
	cancelProvision(id: string): Promise<ProvisionRow>;
	/** `GET /api/tenant/home` — what needs the tenant admin, each job's release lane, 7-day agent activity. */
	tenantHome(): Promise<TenantHome>;
	// T1
	/** `GET /api/jobs?view=board` — jobs with readiness, newer template tags, 7-day cases. */
	jobBoard(): Promise<Paged<JobBoardRow>>;
	/**
	 * `GET /api/job-templates` (any member, D123.2) — every tagged job template with its industry and departments;
	 * `recommended` = the template fits the company profile (D124). Recommended first.
	 */
	jobTemplateOptions(): Promise<JobTemplateCatalogEntry[]>;
	/**
	 * `POST /api/jobs` (tenant_admin, D123.1) `{template, key?, label?, departments?}` — create a job in the active
	 * tenant from a tagged template. A taken default key becomes `<key>_2`…; 409 `job_exists` (explicit key), 422
	 * `industry_mismatch`, 422 `departments_not_in_template`.
	 */
	createJobFromTemplate(input: CreateTenantJobInput): Promise<JobSummary>;
	/**
	 * `POST /api/jobs` (tenant_admin, D128) `{key, label, industry, departments[], region?}` — a job with no template,
	 * pinned to base + the industry + each department at their newest tags. 422 `layer_not_published` (names the
	 * missing family, or says no industry/department is published yet), `hierarchy_incomplete`,
	 * `industry_mismatch`, `unknown_region`, `invalid_job_key`; 409 `job_exists`.
	 */
	createJobBlank(input: CreateBlankJobInput): Promise<JobSummary>;
	/** `PATCH /api/jobs/{job}` `{label?, audience?}` (tenant_admin, D123.4/D129) — rename a job or set who it serves; audited `job.update`. */
	renameJob(
		job: JobKey,
		input: { label?: string; description?: string; audience?: JobAudience },
	): Promise<JobSummary>;
	/** `GET /api/tenant/profile` (any member, D124.2) — the company profile and the options it may take. */
	tenantProfile(): Promise<TenantProfile>;
	/**
	 * `PUT /api/tenant/profile` (tenant_admin, D124.2) — 409 `profile_changed` (stale `expected_updated_at`), 409
	 * `industry_in_use` (a job has another industry), 422 `invalid_profile`.
	 */
	saveTenantProfile(input: TenantProfileInput): Promise<TenantProfile>;
	/** `GET /api/jobs/{job}/staff` (any staff role, D125.5) — the job's staff team and routing. */
	jobStaff(job: JobKey): Promise<JobStaff>;
	/**
	 * `PUT /api/jobs/{job}/staff` (tenant_admin or support_manager, D125.5) — 422 `not_a_worker`, 422
	 * `invalid_staff` (bad members or routing).
	 */
	saveJobStaff(job: JobKey, input: JobStaffInput): Promise<JobStaff>;
	/** `POST /api/jobs/{job}/upgrade-request` (tenant_admin) — ask the platform admin to re-pin onto a newer tag. */
	requestTemplateUpgrade(job: JobKey): Promise<JobBoardRow>;
	// T3
	/** `POST /api/tenant/invites` (tenant_admin, M2) — 202: Keycloak sends the invite e-mail. */
	inviteUser(input: InviteInput): Promise<InviteAccepted>;
	/**
	 * `GET /api/tenant/invites?state=pending|accepted|expired|revoked&before=&limit=` (any member; B1b F-01) — newest
	 * first; `expired` is computed on read; 422 `invalid_state`.
	 */
	tenantInvites(query?: InviteQuery): Promise<Paged<MemberInvite>>;
	/**
	 * `POST /api/tenant/invites/{id}/resend` `{}` (tenant_admin) — 202, re-armed (`expires_at` +7 d, `send_count`+1).
	 * 404 `invite_not_found`; 409 `invite_not_pending`, `invite_superseded`; 429 `invite_resend_cooldown` with
	 * `error.retry_after_s`; 502 `keycloak_error`.
	 */
	resendInvite(id: string): Promise<MemberInvite>;
	/**
	 * `POST /api/tenant/invites/{id}/revoke` `{}` (tenant_admin) — `state: revoked` (idempotent); the never-signed-in
	 * member is disabled. 404 `invite_not_found`; 409 `invite_not_pending` (accepted).
	 */
	revokeInvite(id: string): Promise<MemberInvite>;
	/** `PUT /api/tenant/users/{sub}` `{roles}` (tenant_admin, M1) — add an existing portal user by subject. */
	addUserBySub(input: { sub: Sub; roles: Role[] }): Promise<TenantUser>;
	/**
	 * `PATCH /api/tenant/users/{sub}` `{roles}` (tenant_admin) — replace the member's whole role set (D88; empty =
	 * observer); 422 `invalid_role` / `duplicate_role`; 409 `last_tenant_admin` for every caller, SA included.
	 */
	setUserRole(sub: Sub, roles: Role[]): Promise<TenantUser>;
	/** `GET /api/roles` (any member) — the §R.1 role catalog, in R.1 order. */
	roleCatalog(): Promise<RoleInfo[]>;
	/**
	 * `PUT /api/me/view-as` `{roles, ttl_seconds?}` (any member, SA in a tenant included; D79) — narrow the caller's
	 * own effective roles until it expires (60..28800 s, default 3600). 422 `invalid_role`, `duplicate_role`,
	 * `view_as_not_narrowing` (a role not held), `invalid_ttl`; 409 `active_tenant_required`.
	 */
	setViewAs(input: { roles: Role[]; ttl_seconds?: number }): Promise<Me>;
	/** `DELETE /api/me/view-as` — end view-as (idempotent, always allowed). */
	clearViewAs(): Promise<Me>;
	/** `POST /api/tenant/users/{sub}/disable` — soft disable (K9); the row stays, greyed. */
	disableUser(sub: Sub): Promise<TenantUser>;
	/** `POST /api/tenant/users/{sub}/enable` — undo a soft disable. */
	enableUser(sub: Sub): Promise<TenantUser>;
	/** `GET /api/tenant/users/activity` — per member: last sign-in + surface, sign-ins (30 d), who added them and how. */
	memberActivity(): Promise<MemberActivity[]>;
	// T4
	/** `GET /api/audit?q=&actor_type=&action=&job=&status=&before=` — tenant audit events, newest first. */
	tenantAudit(query?: AuditQuery): Promise<Paged<AuditEvent>>;
	/** `GET /api/audit/summary` — whole-log counts for T4: open failures, families, actors, agent-call mix. */
	tenantAuditSummary(): Promise<AuditSummary>;
	// T5
	/** `GET /api/tenant/usage?window=&group=` — turns and tokens per job/user/project/provider; no currency. */
	tenantUsage(opts: { window: UsageWindow; group: UsageGroup }): Promise<UsageReport>;
	// T6
	/** `GET /api/providers` — LLM providers + their uses (D38). Keys never come back, only `{set, fingerprint}`. */
	llmProviders(): Promise<LlmProviders>;
	/** `POST /api/providers` (integrations_manager). */
	createLlmProvider(input: LlmProviderInput): Promise<LlmProvider>;
	/** `PUT /api/providers/{id}` (integrations_manager). `secret` absent/'' keeps, null clears. */
	updateLlmProvider(id: string, input: LlmProviderInput): Promise<LlmProvider>;
	/** `DELETE /api/providers/{id}` (integrations_manager) — 409 `provider_in_use` while a use is bound to it. */
	deleteLlmProvider(id: string): Promise<void>;
	/** `POST /api/providers/{id}/test` (integrations_manager) — authenticated probe with the resolved token. */
	testLlmProvider(id: string): Promise<{ ok: boolean; at: string; detail: string }>;
	/**
	 * `POST /api/providers/test` (integrations_manager) — probe unsaved drawer values. Stores nothing (no provider, key or
	 * `last_test`); 422 `secret_required_for_new_host` when a stored key would go to another host.
	 */
	testLlmProviderDraft(input: LlmProviderTestInput): Promise<LlmProviderTestResult>;
	/**
	 * `PUT /api/providers/uses/{use}` `{provider_id, model, voice?}` (integrations_manager) — bind a use to a provider +
	 * model (null = the use's fallback). 422 `provider_kind_mismatch` when the provider's kind cannot serve the use
	 * (gates ← laya only, qualify / generator ← openai), `kind_not_valid_for_use` across speech and LLM (asr / tts ←
	 * w2v or openai), `voice_only_for_tts`, `invalid_voice`, `unknown_model`. D69: design — saved to the draft of
	 * every job (ledger `llm_use`), live per job when it publishes.
	 */
	bindLlmUse(
		use: LlmUse,
		providerId: string | null,
		model: string | null,
		voice?: string | null,
	): Promise<LlmProviders>;
	/** `GET /api/providers/slots` — rag/ingest slots (A1 manifest + A2 connections) with versions and drift. */
	providerSlots(): Promise<ProviderSlot[]>;
	/** `PUT /api/providers/slots/{slot}` (integrations_manager) — live-tested; 409 `VERSION_CONFLICT`. D69: saved to the draft of
	 * every job (ledger `provider_slot`), written into the slot when a job publishes. */
	saveProviderSlot(slot: SlotKey, input: SlotInput): Promise<ProviderSlot>;
	/** `POST /api/providers/slots/{slot}/adopt-remote` (integrations_manager) — take the version another app wrote (D69: into
	 * the draft). */
	acceptSlotDrift(slot: SlotKey): Promise<ProviderSlot>;
	/**
	 * `POST /api/providers/slots/{slot}/reset` (integrations_manager; F-08 "Use default", B3 T4b) — the slot uses the
	 * deployment default: kind, URL, model and token cleared (`source: 'default'`, still configured), pushed to strata
	 * at once without any key, and every job pinning the old model gets a model change (D93, `model_change_id`).
	 * 409 `VERSION_CONFLICT` (stale version or drift), `already_default`, `no_deployment_default`; 422
	 * `internal_uses_mlx_api`; 404 `slot_not_found`.
	 */
	resetProviderSlot(slot: SlotKey, expectedVersion: number): Promise<ProviderSlotReset>;
	/**
	 * `POST /api/providers/slots/{slot}/test` (integrations_manager; F-09, B3 T4c) — probe the drawer's values through
	 * the slot's service without saving anything. The token is the one typed, else the stored one for the same URL
	 * (422 `secret_required_for_new_host`). 422 as `saveProviderSlot`; 503 `slot_test_unavailable` (the service cannot
	 * test yet). A failing provider is `ok: false`, never an error.
	 */
	testProviderSlotDraft(slot: SlotKey, draft: SlotTestInput): Promise<SlotTestResult>;
	/**
	 * `POST /api/providers/slots/{slot}/push` (integrations_manager; B3 review I4) — queue again every published entry
	 * of the slot's service strata has not confirmed (a failed, dead or withheld push, e.g. `withheld` on the embed
	 * slot); answers the slot and `push_id`. 409 `nothing_to_push`; 422 `internal_uses_mlx_api`; 404 `slot_not_found`.
	 */
	pushProviderSlot(slot: SlotKey): Promise<ProviderSlotPush>;
	// B3 T4a model changes (D83, D92, D93): a slot model change waits per job until a person rebuilds and accepts it.
	/** `GET /api/knowledge/model-changes?state=` — the tenant's model changes, newest first, each with its jobs'
	 *  states; `state` keeps the changes with a job in that state. 422 `invalid_state`. */
	modelChanges(state?: ModelChangeState | ''): Promise<ModelChangeSummary[]>;
	/** `GET /api/knowledge/model-changes/{id}` — the change with every affected job's part: stores with their
	 *  from / to generations, chunks, `missing_originals`, `live_release`, `draft_rev`. 404 `model_change_not_found`. */
	modelChange(id: string): Promise<ModelChange>;
	/**
	 * `POST /api/knowledge/model-changes/{id}/jobs/{job}/reembed` (knowledge_manager) — start the job's rebuild (Re-embed,
	 * Copy or Re-ingest per the change's `rebuild`; a person's click is the only way one starts). Answers the job's
	 * part. 404; 409 `already_reembedding`, `already_accepted`, `provision_pending`; 503 `unavailable`.
	 */
	reembedJobStore(id: string, job: JobKey): Promise<ModelChangeJob>;
	/**
	 * `POST /api/knowledge/model-changes/{id}/jobs/{job}/accept` (knowledge_manager; D54 draft `embedding_pin`) — pin
	 * the rebuilt generations in the job's draft. 409 `generation_not_ready` + `stores`, `originals_not_retained` +
	 * `documents` (unless `acknowledge_missing`), `already_accepted`; 404.
	 */
	acceptModelChange(
		id: string,
		job: JobKey,
		opts?: { acknowledge_missing?: boolean },
	): Promise<ModelChangeJob>;
	// T7
	/** `GET /api/knowledge/stores` — the tenant's store tree with strata_rag binding. */
	knowledgeStores(): Promise<Paged<KnowledgeStore>>;
	/** `GET /api/knowledge/stores/{id}/documents?before=` — 25 per page. */
	knowledgeDocuments(storeId: string, before?: string | null): Promise<Paged<KnowledgeDocument>>;
	/** `POST /api/knowledge/stores` (knowledge_manager) — enqueues `store_ensure` (K45). */
	createKnowledgeStore(input: KnowledgeStoreInput): Promise<KnowledgeStore>;
	/** `DELETE /api/knowledge/stores/{id}` (knowledge_manager). */
	deleteKnowledgeStore(id: string): Promise<void>;
	/** `POST /api/knowledge/stores/ensure` (knowledge_manager) — "Create missing stores": one `store_ensure` per unbound. */
	ensureMissingStores(): Promise<{ enqueued: number; stores: string[] }>;
	/** `DELETE /api/knowledge/documents/{id}` (knowledge_manager). */
	deleteKnowledgeDocument(id: string): Promise<void>;
	/** `POST /api/knowledge/query` — query playground over bound stores (→ strata_rag `POST /api/query`): per-hit
	 * dense/lexical/rerank scores, the laya routing taken (`beam` branches) and per-stage `timings_ms`. */
	knowledgeQuery(input: KnowledgeQueryInput): Promise<KnowledgeQueryResult>;
	// T8
	/** `GET /api/knowledge/ingest-jobs?status=&store_id=` — strata_ingest jobs. */
	ingestJobs(query?: { status?: IngestStatus | ''; store_id?: string }): Promise<Paged<IngestJob>>;
	/** `GET /api/knowledge/ingest-jobs/{id}`. */
	ingestJob(id: string): Promise<IngestJob>;
	/**
	 * `POST /api/knowledge/ingest-jobs/{id}/retry` (knowledge_manager) → strata_ingest `db::jobs::retry`: failed jobs only.
	 * Every refusal is 409 `conflict` (strata_ingest has no distinct code): a `deleting` job, any status but `failed`
	 * ("only a failed job can be retried; job is {status}"), a failed job with no failed step. Plus this app's own
	 * 409 `slot_not_configured` when the failed step's slot is still empty.
	 */
	retryIngestJob(id: string): Promise<IngestJob>;
	/** `POST /api/knowledge/ingest-jobs/{id}/cancel` (knowledge_manager) → strata_ingest `POST /api/ingest/jobs/{id}/cancel`,
	 * 200 + the job. `queued` (or no step in flight) → `cancelled` at once; a step in flight → `cancelling` until the
	 * worker finishes it (poll `ingestJob`). `cancelling` / `cancelled` → 200 unchanged; `completed` / `failed` /
	 * `rejected` / `deleting` → 409 `conflict`. */
	cancelIngestJob(id: string): Promise<IngestJob>;
	/** `DELETE /api/knowledge/ingest-jobs/{id}` (knowledge_manager) → strata_ingest `DELETE /api/ingest/jobs/{id}`: ended
	 * jobs only (`completed` / `failed` / `rejected` / `cancelled`; else 409 `conflict`); removes the job and its staged
	 * files, its delivered documents stay indexed. Three phases: mark `deleting` → delete blobs → delete rows. 502
	 * `upstream_unavailable` = a blob could not be deleted: the job stays `deleting` (rows kept, some blobs may be gone,
	 * never retryable); `DELETE` again resumes and finishes. The answer's `status` is the one it had before.
	 * strata_ingest#11 (@2b04016): the `DELETE` waits up to 5 s; still working → 202 `{job_id, status: 'deleting'}`
	 * (`IngestJobDeleting`) and the client polls `ingestJob` until 404. 200 and 502 are unchanged. */
	deleteIngestJob(id: string): Promise<IngestJobDeleted | IngestJobDeleting>;
	// T9
	/** `GET /api/jobs/{job}/sources`. */
	sources(): Promise<Paged<Source>>;
	/** `GET /api/jobs/{job}/sources/{id}`. */
	source(id: string): Promise<Source>;
	/** `GET /api/jobs/{job}/sources/{id}/runs`. */
	sourceRuns(id: string): Promise<Paged<SourceRun>>;
	/** `GET /api/jobs/{job}/sources/mapping-targets` — Entity.attribute targets of the job's effective ontology. */
	sourceMappingTargets(): Promise<MappingTarget[]>;
	/** `POST /api/jobs/{job}/sources` (knowledge_manager). */
	createSource(input: SourceInput): Promise<Source>;
	/** `PUT /api/jobs/{job}/sources/{id}` (knowledge_manager). */
	saveSource(id: string, input: SourceInput): Promise<Source>;
	/** `DELETE /api/jobs/{job}/sources/{id}?purge=` (knowledge_manager). */
	deleteSource(
		id: string,
		purge: boolean,
	): Promise<{ purged: boolean; documents_deleted: number; documents_kept: number }>;
	/** `POST /api/jobs/{job}/sources/test` (a draft) or `POST /api/jobs/{job}/sources/{id}/test` (a saved source)
	 *  (knowledge_manager) — nothing is saved. */
	testSource(id: string | null, draft: SourceInput): Promise<SourceSample>;
	/**
	 * `POST /api/jobs/{job}/sources/preview-columns` (knowledge_manager) — columns, up to 5 sample rows and the row count
	 * of the draft's table URL, or (multipart `file` + optional `sheet` / `header_row`) of a local CSV/XLSX of at
	 * most 10 MB. Nothing is stored. 422 sheet_not_found / unreadable_table, 413 too_large, 415 unsupported_type.
	 */
	previewSourceColumns(draft: SourceInput, file?: File, opts?: PreviewOptions): Promise<ColumnPreview>;
	/**
	 * `POST /api/jobs/{job}/sources/{id}/resync` (knowledge_manager) — with the stored or an edited mapping. A file source
	 * takes its files here (multipart `file` fields, at most 20 files / 50 MB in all: 422 too_many_files, 413
	 * too_large, 422 no_file); each accepted file becomes one ingest job, an unsupported type is reported per file.
	 */
	resyncSource(id: string, opts?: { mapping?: SourceMapping[]; files?: File[] }): Promise<SourceRun>;
	/** `GET /api/jobs/{job}/sources/{id}/runs/{run}` — one run with its ingest jobs and the documents it produced. */
	sourceRun(sourceId: string, runId: string): Promise<SourceRunDetail>;
	/**
	 * `GET /api/jobs/{job}/knowledge/coverage` (D68) — documents per knowledge kind × language the job serves. Any
	 * member. A gap is a warning, never a block.
	 */
	knowledgeCoverage(job: JobKey): Promise<KnowledgeCoverage>;
	// T13
	/**
	 * `GET /api/tenant/channels` — any member: every job's customer-side channel bindings (webhook health from the
	 * edge's reports), the staff test chat, the kinds not bound yet, the gate-2 verdict and the refusal text.
	 */
	tenantChannels(): Promise<ChannelsView>;
	/**
	 * `POST /api/jobs/{job}/channel-bindings/{id}/test-message` (integrations_manager, B7 T15) — one fixed text to a
	 * Telegram binding's test chat (`settings.test_chat_id`, else its first allowed user), sent by the edge with the
	 * binding's own bot. 409 `channel_not_bound` / `test_chat_not_configured`, 502 `channel_send_failed`.
	 */
	sendChannelTestMessage(channelId: string, job?: JobKey): Promise<ChannelTestResult>;
	// B4 T16a (F-14): the tenant holiday calendar (the admin home's Calendar card)
	/**
	 * `GET /api/tenant/calendar` — any member: `{live, draft, drafted}`. `draft` = the saved tenant calendar, `live` =
	 * the one in the active release of the most recently applied job, `drafted` = the jobs whose draft still carries
	 * an unpublished calendar change.
	 */
	tenantCalendar(): Promise<TenantCalendarView>;
	/**
	 * `PUT /api/tenant/calendar` `{timezone, holidays}` (support_manager) — D54/D69: a tenant-wide design change saved
	 * to the draft of every job (ledger `tenant_calendar`), live per job when it publishes. 422 `invalid_calendar`
	 * (unknown timezone, bad or duplicate date, more than 366 holidays, label over 80 characters).
	 */
	saveTenantCalendar(input: TenantCalendarInput): Promise<TenantCalendarView>;
};
