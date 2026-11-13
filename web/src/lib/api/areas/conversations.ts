// The `ConversationsApi` contract.
import type {
	AttachmentQuota,
	ChannelBinding,
	ConsoleConversationPage,
	ConversationEventsOptions,
	CreateChannelBinding,
	CreateJobApiKey,
	CreatedJobApiKey,
	ChannelTestSession,
	IdentitySecret,
	JobApiKey,
	PartyDetail,
	PartySummary,
	UpdateChannelBinding,
} from '$lib/api/types/conversations';
import type { Paged } from '$lib/api/types/core';

export type ConversationsApi = {
	/**
	 * `GET /api/jobs/{job}/console/conversations?before=` — the job's conversations, most recently active first: channel,
	 * customer, case, who answers (the owner record), the open handover item and the last message.
	 */
	consoleConversations(opts?: { before?: string | null }): Promise<ConsoleConversationPage>;
	/**
	 * `GET /api/jobs/{job}/conversations/{id}/events` (SSE) — the conversation's live frames (messages, owner changes,
	 * delivery failures). Resolves when the stream ends or `signal` aborts.
	 */
	conversationEvents(id: string, opts: ConversationEventsOptions): Promise<void>;
	/**
	 * `GET /api/jobs/{job}/events` (SSE) — the job's live frames, every conversation (handover changes, `csat`, …).
	 * Resolves when the stream ends or `signal` aborts.
	 */
	jobEvents(opts: ConversationEventsOptions): Promise<void>;
	/**
	 * `GET /api/jobs/{job}/attachment-quota` (tenant_admin or integrations_manager) — the tenant's attachment storage
	 * and what it holds now. Read-only: a platform super_admin sets the quota.
	 */
	attachmentQuota(): Promise<AttachmentQuota>;
	/** `GET /api/jobs/{job}/channel-bindings` — the job's channel bindings (credentials never returned). */
	channelBindings(): Promise<Paged<ChannelBinding>>;
	/**
	 * `POST /api/jobs/{job}/channel-bindings` `{kind, side, external_id?, label, settings, credential?}`
	 * (integrations_manager) — web chat generates its `wk_` key; a credential is sealed, only `has_credential` +
	 * `credential_fpr` come back; a new binding is verified at creation. 422
	 * `invalid_binding`; 409 `binding_unavailable`.
	 */
	createChannelBinding(input: CreateChannelBinding): Promise<ChannelBinding>;
	/** `PUT /api/jobs/{job}/channel-bindings/{id}` `{label?, settings?, enabled?, credential?}` (integrations_manager). */
	updateChannelBinding(id: string, patch: UpdateChannelBinding): Promise<ChannelBinding>;
	/** `DELETE /api/jobs/{job}/channel-bindings/{id}` (integrations_manager). 404 `binding_not_found`. */
	deleteChannelBinding(id: string): Promise<void>;
	/**
	 * `POST /api/jobs/{job}/channel-bindings/{id}/rotate-key` (integrations_manager, web chat only) — a new `wk_` key;
	 * every open chat of the binding ends (its sessions are blocked, reason `rotated`). Answers the binding. 422
	 * `not_web_chat`.
	 */
	rotateWebChatKey(id: string): Promise<ChannelBinding>;
	/**
	 * D130: `POST /api/jobs/{job}/channel-bindings/{id}/publish` (release_manager; tenant_admin holds it) — the
	 * customer channel goes live (`env` `prod`). 409 `no_release` while the job has no applied release; idempotent.
	 */
	publishBinding(id: string): Promise<ChannelBinding>;
	/** D130: `POST /api/jobs/{job}/channel-bindings/{id}/unpublish` (release_manager) — back to the test env (`env` `test`); idempotent. */
	unpublishBinding(id: string): Promise<ChannelBinding>;
	/**
	 * `POST /api/jobs/{job}/channel-bindings/{id}/identity-secret[?revoke_sessions=true]` (integrations_manager, web
	 * chat in identity mode `hs256`) — a new signing secret, answered once as `{kid, secret}`. The old one keeps
	 * working for 24 h; `revokeSessions` also signs out every open identified chat. 422 `not_web_chat`; 409
	 * `identity_mode_not_hs256`.
	 */
	rotateIdentitySecret(id: string, opts?: { revokeSessions?: boolean }): Promise<IdentitySecret>;
	/**
	 * `POST /api/jobs/{job}/channel-bindings/{id}/test-session` `{customer_ref}` (integrations_manager; a TEST web chat in
	 * identity mode HS256) — core signs a short-lived sign-in proof for that customer, as the tenant's site would, and
	 * answers the hosted test page's URL carrying it. 409 `binding_live`, `identity_not_configured`,
	 * `identity_mode_not_hs256`; 422 `invalid_customer_ref`.
	 */
	createChannelTestSession(id: string, customerRef: string): Promise<ChannelTestSession>;

	// ---- B13 party identity (tenant-wide: one customer talks to every job)
	/** `GET /api/parties?q=&before=` (support_agent) — the tenant's customers, newest first; `q` matches name or ref. */
	parties(opts?: { q?: string; before?: string | null }): Promise<Paged<PartySummary>>;
	/** `GET /api/parties/{id}` (support_agent) — a customer: identities by channel, cases, `merged_into`. */
	party(id: string): Promise<PartyDetail>;
	/** `GET /api/jobs/{job}/cases/{id}/party` — the customer behind a case (null when none is resolved). */
	caseParty(caseId: string): Promise<PartyDetail | null>;
	/**
	 * `POST /api/jobs/{job}/cases/{id}/party` `{party_id}` (support_agent) — link a case to a known customer; the
	 * case's anonymous party is merged into it. 409 `party_merged` (the target was merged).
	 */
	linkCaseParty(caseId: string, partyId: string): Promise<PartyDetail>;

	// ---- B13 per-job API keys (machine callers of the interaction routes; `job` is the key's job)
	/** `GET /api/jobs/{job}/api-keys` (integrations_manager) — the job's keys, newest first, never the secret. */
	jobApiKeys(job: string): Promise<Paged<JobApiKey>>;
	/**
	 * `POST /api/jobs/{job}/api-keys` `{label, expires_at?}` (integrations_manager) — the key and its `ak_…` secret,
	 * shown this once. 422 `label_required` / `invalid_expires_at`.
	 */
	createJobApiKey(job: string, input: CreateJobApiKey): Promise<CreatedJobApiKey>;
	/** `DELETE /api/jobs/{job}/api-keys/{id}` (integrations_manager) — the key answers 401 from now on. */
	revokeJobApiKey(job: string, id: string): Promise<void>;
};
