// The `SupportApi` contract.
import type { ConversationEventsOptions } from '$lib/api/types/conversations';
import type { HandoverItem, HandoverQueue, HandoverState } from '$lib/api/types/journey';
import type { PartyDetail } from '$lib/api/types/conversations';
import type {
	AttachmentRef,
	CannedReply,
	CustomerContext,
	Inbox,
	InboxQuery,
	MessageView,
	NewCannedReply,
	OwnerKey,
	OwnerView,
	PresenceState,
	Resolve,
	SendMessage,
	SupportRouting,
	CsatQuery,
	CsatReport,
	SupportRoutingInput,
	SupportTranscript,
} from '$lib/api/types/support';

export type SupportApi = {
	/**
	 * `GET /api/support/inbox?tab=&reason=&channel=&priority=` — the caller's conversations across every job they work:
	 * `tab` is `mine` (owner is me), `unassigned` (owner `queue`), `open` (owner is not the agent) or `resolved` (closed
	 * in the last 7 days). Sorted by the sooner SLA due time, then by how long it has waited. `counts` ignore the tab.
	 */
	supportInbox(opts?: InboxQuery): Promise<Inbox>;
	/**
	 * `GET /api/support/events` (SSE) — one stream for the whole tenant: frames `inbox_row`, `message`, `owner`,
	 * `handover`, `delivery_failed`, `presence`. 503 `stream_unavailable` without Redis: the page polls every 10 s.
	 * Resolves when the stream ends or `signal` aborts.
	 */
	events(opts: ConversationEventsOptions): Promise<void>;
	/**
	 * `POST /api/jobs/{job}/conversations/{id}/take` `{expected_owner}` (support_agent or support_manager) — the caller becomes the owner.
	 * 409 `owner_changed` (`details.owner` is the owner now) when `expected_owner` is stale.
	 */
	take(job: string, id: string, expected: OwnerKey): Promise<OwnerView>;
	/**
	 * `POST /api/jobs/{job}/conversations/{id}/assign` `{user_sub, expected_owner}` (support_manager) — give it to an
	 * active Support agent. 409 `owner_changed`; 422 `not_assignable`.
	 */
	assign(job: string, id: string, user_sub: string, expected: OwnerKey): Promise<OwnerView>;
	/**
	 * `POST /api/jobs/{job}/conversations/{id}/hand-back` `{note}` (support_agent, the owner) — the agent answers the
	 * next message. 403 `not_owner`; 422 `note_required`.
	 */
	handBack(job: string, id: string, note: string): Promise<OwnerView>;
	/**
	 * `POST /api/jobs/{job}/conversations/{id}/messages` `{kind, text, client_msg_id}` — a reply to the customer
	 * (owner only: 403 `not_owner`) or an internal note (never delivered). The same `client_msg_id` returns the same
	 * message. 422 `message_too_long` (`details.limit`).
	 */
	send(job: string, id: string, input: SendMessage): Promise<MessageView>;
	/**
	 * `POST /api/jobs/{job}/conversations/{id}/attachments` multipart `file` (support_agent) — one photo or PDF, stored
	 * unlinked until a reply carries its id. 422 `attachment_refused` (`details.reason`: unsupported_type,
	 * too_many_pixels, animated, malformed_image, malformed_pdf, active_pdf, encrypted_pdf); 413 `too_large` /
	 * `attachment_quota`.
	 */
	uploadAttachment(job: string, id: string, file: File): Promise<AttachmentRef>;
	/**
	 * `GET /api/jobs/{job}/conversations/{id}/attachments/{att}` — the bytes, behind the bearer (never a URL with a
	 * token): the page shows them from an object URL. 404 for anything not in this conversation.
	 */
	attachmentBlob(job: string, id: string, att: string): Promise<Blob>;
	/** `DELETE /api/jobs/{job}/conversations/{id}/attachments/{att}` (support_agent) — an unsent upload only; 409 `attachment_sent` once a reply carries it. */
	deleteAttachment(job: string, id: string, att: string): Promise<void>;
	/**
	 * `POST /api/jobs/{job}/handover/{id}/resolve` `{outcome, closing_message?, feedback?}` (support_agent, the owner)
	 * — closes the item, sends the closing message first, and gives the conversation back to the agent.
	 */
	resolve(job: string, itemId: string, input: Resolve): Promise<OwnerView>;
	/**
	 * `POST /api/jobs/{job}/conversations/{id}/presence` `{state}` — `viewing` or `typing`, shown to colleagues. Not
	 * stored; `viewing` also marks the conversation read for the caller.
	 */
	presence(job: string, id: string, state: PresenceState): Promise<void>;
	/** `GET /api/jobs/{job}/canned-replies` — the job's saved replies. */
	canned(job: string): Promise<CannedReply[]>;
	/** `POST /api/jobs/{job}/canned-replies` `{title, body}` (support_manager). 422 `invalid_canned_reply`. */
	createCanned(job: string, input: NewCannedReply): Promise<CannedReply>;
	/** `DELETE /api/jobs/{job}/canned-replies/{id}` (support_manager). */
	deleteCanned(job: string, id: string): Promise<void>;
	/** `GET /api/support/routing` — how queued handovers are handed out (any staff role). */
	supportRouting(): Promise<SupportRouting>;
	/**
	 * `PUT /api/support/routing` `{mode?, reassign_after_secs?}` (tenant_admin or support_manager) — turn round-robin
	 * auto-assignment on or off and set how long an auto-assigned item may sit untouched before it moves on. 422
	 * `invalid_routing`.
	 */
	setSupportRouting(input: SupportRoutingInput): Promise<SupportRouting>;
	/**
	 * `GET /api/support/csat?from=&to=` (tenant_admin or support_manager; else 403 `missing_role: support_manager`,
	 * whatever the query) — the tenant-wide CSAT report of the requests asked in the window (default the last 30 days).
	 * 400 `invalid_query`.
	 */
	supportCsat(query?: CsatQuery): Promise<CsatReport>;
	/**
	 * `GET /api/jobs/{job}/conversations/{id}` — the one typed transcript: the conversation, its owner, every message
	 * (customer, agent, staff, note, system) and the episodes (one per handover item).
	 */
	transcript(job: string, id: string): Promise<SupportTranscript>;
	/**
	 * `GET /api/jobs/{job}/conversations/{id}/customer-context` (support_agent, support_manager; operator read-only) —
	 * who the customer is and every order, payment, delivery and case we hold for them, plus the records the agent was
	 * dealing with (`focus`). Customers are KYC'd: nobody asks them for an id. `identity.source` null = not signed in:
	 * `customer` is null and every list is empty.
	 */
	customerContext(job: string, id: string): Promise<CustomerContext>;
	// ---- the workspace is tenant-wide: the existing handover + party routes, with the job named explicitly ------
	/** `GET /api/jobs/{job}/handover?state=&assignee=` — `job`'s handover queue (the item detail + assignees). */
	jobHandovers(
		job: string,
		opts?: { state?: HandoverState | 'open'; assignee?: string | 'none' },
	): Promise<HandoverQueue>;
	/** `GET /api/jobs/{job}/cases/{id}/party` — the customer behind a case of `job` (null when none). */
	jobCaseParty(job: string, caseId: string): Promise<PartyDetail | null>;
	/** `POST /api/jobs/{job}/cases/{id}/party` `{party_id}` (support_agent) — link a case of `job` to a customer. */
	jobLinkCaseParty(job: string, caseId: string, partyId: string): Promise<PartyDetail>;
	/**
	 * `POST /api/jobs/{job}/handover/{id}/feedback` `{verdict, note}` — "should the agent have handled this?", once
	 * per item (SupportAgent, SupportManager, Operator, JobDesigner). 409 `feedback_exists` (`details.feedback`).
	 */
	jobHandoverFeedback(
		job: string,
		id: string,
		verdict: 'agent_should_handle' | 'right_to_hand_over',
		note: string,
	): Promise<HandoverItem>;
};
