// Shapes of the staff workspace (SW plan T2-T7), mirroring agent-api EXACTLY (snake_case as it sends them):
// `OwnerView` (T2), `MessageView` (T3), `Episode` (T6), `InboxRow` (T7), `CannedReply` (T7).
import type { Iso } from './core';
import type { ConversationChannel } from './conversations';
import type { HandoverPriority, HandoverState } from './journey';

/** Who answers a conversation: the agent, nobody yet (an open handover waits in the queue), or a staff member. */
export type OwnerKey = 'agent' | 'queue' | `user:${string}`;

/** `GET …/conversations/{id}` `owner`, the 200 of take / assign / hand-back / resolve, and the `owner` stream frame. */
export interface OwnerView {
	owner: OwnerKey;
	/** The staff member's display name; null for `agent` and `queue`. */
	name: string | null;
	since: Iso;
	/** The open handover item, when there is one. */
	item_id: string | null;
	/** Routing assigned the open item and its owner is still that assignee: no person took or assigned it. */
	auto_assigned: boolean;
}

export type MessageKind = 'customer' | 'caller' | 'agent' | 'staff' | 'note' | 'system';
/** `author` of a `system` row. */
export type SystemEvent = 'agent_cases' | 'resolution' | 'csat' | 'hand_back' | 'owner';
export type DeliveryState = 'sent' | 'delivered' | 'failed';
/** Where a staff note's copy to the helpdesk ticket stands (durable outbox). */
export type MirrorState = 'pending' | 'sent' | 'skipped' | 'failed';

/** The one wire shape of an attachment (SW-9). Never carries bytes, a hash or a URL: the bytes sit behind auth. */
export interface AttachmentRef {
	id: string;
	media_type: 'image/jpeg' | 'image/png' | 'image/webp' | 'application/pdf' | string;
	display_name: string;
	byte_size: number;
	width?: number | null;
	height?: number | null;
}

export interface MessageView {
	message_id: string;
	kind: MessageKind;
	/** Set on `system` rows only. */
	event: SystemEvent | string | null;
	text: string;
	author_name: string | null;
	/** ISO-UTC. */
	at: Iso;
	case_id: string | null;
	client_msg_id: string | null;
	/** Null for rows that are not sent by staff. */
	delivery: DeliveryState | null;
	/** Null for rows that are not notes, and for a note that is not mirrored to a ticket. */
	mirror: MirrorState | null;
	/** Photos and PDFs on this message, both directions (SW-9). */
	attachments?: AttachmentRef[];
}

/** One handover item on a conversation: the transcript is split into episodes. */
export interface Episode {
	item_id: string;
	case_id: string | null;
	/** Plain language, never a decision code. */
	reason_label: string;
	queued_at: Iso;
	closed_at: Iso | null;
	resolution: string | null;
}

export type InboxTab = 'mine' | 'unassigned' | 'open' | 'resolved';

export interface InboxQuery {
	tab?: InboxTab;
	reason?: string;
	channel?: ConversationChannel | string;
	priority?: HandoverPriority;
}

export interface InboxRow {
	job: string;
	job_name: string;
	conversation_id: string;
	item_id: string | null;
	display_name: string;
	/** The customer is known and verified (`identity_source` assertion / staff_pick). */
	verified: boolean;
	channel: ConversationChannel | string;
	reason_label: string | null;
	priority: HandoverPriority;
	last_message: { kind: MessageKind; text: string; at: Iso };
	owner: OwnerView;
	state: HandoverState | null;
	/**
	 * SLA (SW-12, computed client-side): before `first_response_at` the due time is `first_response_due`, after it
	 * `resolve_due`. `sla_starts_at` (= the item's `clock_starts_at ?? queued_at`) is where the window starts.
	 */
	first_response_due: Iso | null;
	first_response_at: Iso | null;
	resolve_due: Iso | null;
	sla_starts_at: Iso;
	waiting_since: Iso;
	unread: boolean;
}

export interface InboxCounts {
	mine: number;
	unassigned: number;
	open: number;
}

export interface Inbox {
	rows: InboxRow[];
	counts: InboxCounts;
}

export interface CannedReply {
	id: string;
	title: string;
	body: string;
	created_by: string;
	updated_at: Iso;
}

export interface NewCannedReply {
	title: string;
	body: string;
}

/** `support_routing.mode`: `round_robin` hands each queued handover to an online staff member. */
export type RoutingMode = 'off' | 'round_robin';

/**
 * `GET /api/support/routing` (SW routing, migration 0204). A tenant without a row reads the defaults
 * (`round_robin`, 300 s, `updated_at` / `updated_by` null). `reassign_after_secs` is 60 to 86 400.
 */
export interface SupportRouting {
	mode: RoutingMode;
	reassign_after_secs: number;
	updated_at: Iso | null;
	updated_by: string | null;
	/**
	 * SW-7 (owner 2026-10-27): the platform flag `tenants.support_auto_assign`, OFF by default and switched only by a
	 * platform super_admin. Off = nothing is auto-assigned, whatever `mode` says; `mode` stays editable meanwhile.
	 */
	enabled: boolean;
}

/** `PUT /api/support/routing`: a field left out keeps its value. */
export interface SupportRoutingInput {
	mode?: RoutingMode;
	reassign_after_secs?: number;
}

export type SendKind = 'reply' | 'note';

export interface SendMessage {
	kind: SendKind;
	text: string;
	/** Client-made id: a retry with the same id returns the same message. */
	client_msg_id: string;
	/** Ids of uploaded, unsent attachments (at most 5; replies only). */
	attachments?: string[];
}

export type ResolveOutcome = 'resolved' | 'not_resolvable' | 'duplicate';

export interface Resolve {
	outcome: ResolveOutcome;
	closing_message?: string;
	/**
	 * SW-6: ask the customer for a CSAT rating with the Resolve. Sent only when the person changed the default; left out,
	 * the server's default applies (on, but off after a complaint). Never asked on a channel nobody reads (api, mcp, voice, staff).
	 */
	ask_rating?: boolean;
	feedback?: { verdict: 'agent_should_handle' | 'right_to_hand_over'; note?: string };
}

export type PresenceState = 'viewing' | 'typing';

/** The conversation head the transcript answers with. `display_name` follows the fallback rules (no raw ids). */
export interface SupportConversation {
	conversation_id: string;
	job: string;
	channel: ConversationChannel | string;
	display_name: string;
	/** The customer is known and verified (`identity_source` assertion / staff_pick). */
	verified: boolean;
	case_id: string | null;
	/** What the admin allowed on this conversation's channel binding (SW-9); absent = both on. */
	attachments?: { customer: boolean; staff: boolean };
}

/** `GET /api/jobs/{job}/conversations/{id}` — one projection for every surface. */
export interface SupportTranscript {
	conversation: SupportConversation;
	owner: OwnerView;
	messages: MessageView[];
	episodes: Episode[];
}

// ---- stream frames (`GET /api/support/events`) ------------------------------------------------------------------

/** The `owner` frame (renamed from `control`). The tenant stream also names the `job`. */
export interface OwnerFrame extends OwnerView {
	conversation_id: string;
	job?: string;
}

export type SupportFrameName =
	| 'inbox_row'
	| 'message'
	| 'owner'
	| 'handover'
	| 'delivery_failed'
	| 'presence';

/** Why a take / assign lost the race: 409 `owner_changed`, `details.owner` is the owner now. */
export const OWNER_CHANGED = 'owner_changed';

// ---- KYC customer context (owner ruling 2026-10-26: "I know my customer that I serve") ---------------------------

/** How the customer was identified: the channel's signed-in assertion, or a staff member's pick. Null = not known. */
export type IdentitySource = 'assertion' | 'staff_pick' | 'staff_test';

export interface ContextOrder {
	order_id: string;
	status: string;
	placed_at: Iso;
	items: { name: string; qty: number }[];
}
export interface ContextPayment {
	payment_id: string;
	order_id: string;
	amount: number;
	currency: string;
	status: string;
	refunded_amount: number;
	at: Iso;
}
export interface ContextDelivery {
	delivery_id: string;
	order_id: string;
	carrier: string;
	status: string;
	eta: Iso | null;
	delivered_at: Iso | null;
	tracking_url?: string;
}
export interface ContextCase {
	case_id: string;
	state: string;
	title: string;
	opened_at: Iso;
}
/** The record the agent was dealing with, and why. */
export interface ContextFocus {
	kind: 'order' | 'payment' | 'delivery' | 'case';
	id: string;
	why: string;
}

/**
 * `GET /api/jobs/{job}/conversations/{id}/customer-context` — who the customer is and every order, payment,
 * delivery and case we hold for them. When `identity.source` is null all lists are empty and `customer` is null.
 */
export interface CustomerContext {
	identity: { source: IdentitySource | null; level: 0 | 1 | 2; verified_at: Iso | null };
	customer: { id: string; name: string; email: string } | null;
	orders: ContextOrder[];
	payments: ContextPayment[];
	deliveries: ContextDelivery[];
	cases: ContextCase[];
	focus: ContextFocus[];
	/** A staff test chat whose typed email found no single customer: `party_not_found`, `party_ambiguous` or
	 *  `party_lookup_unavailable` (sent only then). The web words it itself (`lookupNote`); `note` is the server's. */
	degraded?: string[];
	note?: string | null;
}

/** `GET /api/support/csat?from=&to=` (SW-6; tenant_admin or support_manager). Both bounds optional: the last 30 days. */
export interface CsatQuery {
	/** ISO instant or `YYYY-MM-DD`. */
	from?: string;
	to?: string;
}

/** Requests asked, answered and the mean rating (null when none was answered) of one slice of the report. */
export interface CsatSlice {
	asked: number;
	answered: number;
	mean: number | null;
}

export interface CsatByChannel extends CsatSlice {
	channel: string;
}

/** Who asked: the agent (one row, no `sub`) or one staff member (`sub`, `name`). */
export interface CsatByResolver extends CsatSlice {
	asked_by: 'agent' | 'staff';
	sub: string | null;
	name: string | null;
}

/** A rating of 1 or 2, newest answer first (customer comment: shown to these roles only). */
export interface CsatLowScore {
	case_id: string;
	conversation_id: string;
	job_key: string;
	channel: string;
	rating: number;
	comment: string | null;
	answered_at: Iso;
}

/**
 * The window `[from, to)` is on when the request was asked; `answered`, the mean, the distribution and the rest count
 * only requests asked in it. `response_rate` = answered / asked (null when nothing was asked). prod conversations only.
 */
export interface CsatReport extends CsatSlice {
	from: Iso;
	to: Iso;
	response_rate: number | null;
	/** `"1"`..`"5"` → how many answered with it. */
	distribution: Record<'1' | '2' | '3' | '4' | '5', number>;
	by_channel: CsatByChannel[];
	by_resolver: CsatByResolver[];
	/** At most 50. */
	low_scores: CsatLowScore[];
}
