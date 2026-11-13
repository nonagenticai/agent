// B12 over HTTP: the handover queue's operational routes (B4 names and paths), the job's console conversations
// (take over, hand back, staff reply, transcript, live feed) and the job's channel bindings. Every route is job
// scoped (`/api/jobs/{job}/…`); `job()` is the job the page acts in (`app.job`). The live feed is a GET read as SSE
// through `req.stream` — the same door (bearer + headers) as every other call; an `EventSource` cannot send the
// bearer. Handover policy, feedback and the proposal draft are served by their own routes.
import type { Api } from '../client';
import { seg, type Req } from '../http-client';
import type { SseEvent } from '../sse';
import type { Paged } from '../types/core';
import type {
	AttachmentQuota,
	ChannelBinding,
	ConsoleConversationPage,
	ConsoleConversationRow,
	ConversationFrame,
	CreatedJobApiKey,
	ChannelTestSession,
	IdentitySecret,
	JobApiKey,
	CasePartyAnswer,
	PartyDetail,
	PartySummary,
} from '../types/conversations';
import type { HandoverItem, HandoverMessage, HandoverQueue, HandoverReason } from '../types/journey';
import type { SupportTranscript } from '../types/support';
import type { ChannelTestResult, ChannelsView } from '../types/tenant';
import { toConsoleSnapshot } from '$lib/live/console/L1-http';

export type B12Method =
	| 'handoverQueue'
	| 'consoleConversations'
	| 'conversationEvents'
	| 'jobEvents'
	// the L1 operator console (`/console`) on the same routes
	| 'operatorConsole'
	| 'attachmentQuota'
	| 'channelBindings'
	| 'createChannelBinding'
	| 'updateChannelBinding'
	| 'deleteChannelBinding'
	| 'publishBinding'
	| 'unpublishBinding'
	// B13
	| 'rotateWebChatKey'
	| 'rotateIdentitySecret'
	| 'createChannelTestSession'
	| 'parties'
	| 'party'
	| 'caseParty'
	| 'linkCaseParty'
	| 'jobApiKeys'
	| 'createJobApiKey'
	| 'revokeJobApiKey'
	// B7 T15: a Telegram binding's test message; T13: the tenant's doors
	| 'sendChannelTestMessage'
	| 'tenantChannels';

const REASONS: readonly HandoverReason[] = [
	'identity_failed',
	'gate_blocked',
	'customer_asked',
	'tool_failed',
	'unknown_outcome',
	'out_of_scope',
	'gate_unavailable',
	'staff_decision',
	'not_offered',
	'success_criteria_unmet',
];
/** B12 names the "asked for a person" reason `customer_request`; the queue knows it as `customer_asked`. */
const REASON_ALIAS: Record<string, HandoverReason> = { customer_request: 'customer_asked' };

const isoOf = (ms: unknown) =>
	typeof ms === 'number' ? new Date(ms).toISOString().replace(/\.\d{3}Z$/, 'Z') : '';

/**
 * A handover item as the queue page reads it: the fields B12 does not carry yet (agent steps, collected facts,
 * proposal hints — B4 runtime/B8) default to empty, an unknown reason reads as out of scope, and a message keyed
 * `role`/`at_ms` is read as `from`/`at`.
 */
export function toHandoverItem(raw: Partial<HandoverItem> & Record<string, unknown>): HandoverItem {
	const reason = REASON_ALIAS[String(raw.reason)] ?? (raw.reason as HandoverReason);
	const rawMessages = (Array.isArray(raw.messages)
		? raw.messages
		: []) as unknown as (Partial<HandoverMessage> & Record<string, unknown>)[];
	const messages = rawMessages.map(
		(m): HandoverMessage => ({
			at: (m.at as string) ?? isoOf(m.at_ms),
			from: (m.from ?? m.role ?? 'system') as HandoverMessage['from'],
			author: (m.author as string) ?? '',
			text: (m.text as string) ?? '',
		}),
	);
	return {
		case_type: 'handover',
		subject: '',
		customer: '',
		order_id: null,
		channel: 'staff',
		trigger: '',
		priority: 'normal',
		state: 'queued',
		queued_at: '',
		first_response_at: null,
		closed_at: null,
		first_response_due: null,
		resolve_due: null,
		assignee: null,
		assignee_name: null,
		case_ref: null,
		release_id: null,
		queue: '',
		confidence: null,
		suggested_next: '',
		proposal_hint: null,
		feedback: null,
		party: null,
		csat: null,
		...raw,
		id: String(raw.id),
		reason: REASONS.includes(reason) ? reason : 'out_of_scope',
		history: Array.isArray(raw.history) ? raw.history : [],
		agent_did: Array.isArray(raw.agent_did) ? raw.agent_did : [],
		collected: Array.isArray(raw.collected) ? raw.collected : [],
		messages,
	} as HandoverItem;
}

/** The queue page's view of `GET …/handover`: items normalised, missing lists empty, the clock now when absent. */
export function toHandoverQueue(raw: Partial<HandoverQueue> & Record<string, unknown>): HandoverQueue {
	return {
		items: (Array.isArray(raw.items) ? raw.items : []).map((i) =>
			toHandoverItem(i as unknown as HandoverItem & Record<string, unknown>),
		),
		next: raw.next ?? null,
		assignees: Array.isArray(raw.assignees) ? raw.assignees : [],
		now: raw.now ?? new Date().toISOString().replace(/\.\d{3}Z$/, 'Z'),
	};
}

/** A list answer as a page: `[…]` or `{items, next}`. */
function pageOf<T>(raw: unknown): Paged<T> {
	if (Array.isArray(raw)) return { items: raw as T[], next: null };
	const r = (raw ?? {}) as { items?: T[]; bindings?: T[]; next?: string | null };
	return { items: r.items ?? r.bindings ?? [], next: r.next ?? null };
}

/** One SSE event as a frame: its name, and its data parsed as JSON (kept as text when it is not JSON). */
export function frameOf(e: SseEvent): ConversationFrame {
	let data: unknown = e.data;
	try {
		data = e.data ? JSON.parse(e.data) : null;
	} catch {
		/* not JSON: keep the text */
	}
	return e.id ? { event: e.event, data, id: e.id } : { event: e.event, data };
}

export function httpB12(req: Req, job: () => string): Pick<Api, B12Method> {
	const j = () => seg(job());
	const rows = async () =>
		pageOf<ConsoleConversationRow>(await req<unknown>('GET', `/api/jobs/${j()}/console/conversations`)).items;
	const transcript = async (id: string) => {
		// the one typed transcript every surface reads: `{conversation, owner, messages, episodes}`
		const r = await req<SupportTranscript>('GET', `/api/jobs/${j()}/conversations/${seg(id)}`);
		return Array.isArray(r?.messages) ? r.messages : [];
	};
	return {
		// ---- handover queue (B4 T12 paths)
		handoverQueue: async (opts = {}) =>
			toHandoverQueue(
				await req<HandoverQueue & Record<string, unknown>>('GET', `/api/jobs/${j()}/handover`, {
					query: { state: opts.state, assignee: opts.assignee },
				}),
			),

		// ---- console conversations
		consoleConversations: async (opts = {}) =>
			pageOf<ConsoleConversationRow>(
				await req<unknown>('GET', `/api/jobs/${j()}/console/conversations`, {
					query: { before: opts.before },
				}),
			) as ConsoleConversationPage,
		conversationEvents: (id, opts) =>
			req.stream('GET', `/api/jobs/${j()}/conversations/${seg(id)}/events`, {
				onEvent: (e) => opts.onFrame(frameOf(e)),
				signal: opts.signal,
				headers: opts.lastEventId ? { 'Last-Event-ID': opts.lastEventId } : undefined,
				onOpen: opts.onOpen,
			}),
		jobEvents: (opts) =>
			req.stream('GET', `/api/jobs/${j()}/events`, {
				onEvent: (e) => opts.onFrame(frameOf(e)),
				signal: opts.signal,
			}),

		// ---- the L1 operator console: the console rows as ConsoleConversation, the open one with its transcript (a
		// transcript that cannot be read leaves the row's last line)
		operatorConsole: async (opts = {}) => {
			const open = opts.open ?? null;
			const [list, lines] = await Promise.all([
				rows(),
				open ? transcript(open).catch(() => null) : Promise.resolve(null),
			]);
			return toConsoleSnapshot(list, open && lines ? { id: open, messages: lines } : null, Date.now());
		},

		// ---- channel bindings
		attachmentQuota: () => req<AttachmentQuota>('GET', `/api/jobs/${j()}/attachment-quota`),
		channelBindings: async () =>
			pageOf<ChannelBinding>(await req<unknown>('GET', `/api/jobs/${j()}/channel-bindings`)),
		createChannelBinding: (input) =>
			req<ChannelBinding>('POST', `/api/jobs/${j()}/channel-bindings`, { body: input }),
		updateChannelBinding: (id, patch) =>
			req<ChannelBinding>('PUT', `/api/jobs/${j()}/channel-bindings/${seg(id)}`, { body: patch }),
		deleteChannelBinding: async (id) => {
			await req<unknown>('DELETE', `/api/jobs/${j()}/channel-bindings/${seg(id)}`);
		},
		rotateWebChatKey: (id) =>
			req<ChannelBinding>('POST', `/api/jobs/${j()}/channel-bindings/${seg(id)}/rotate-key`),
		publishBinding: (id) =>
			req<ChannelBinding>('POST', `/api/jobs/${j()}/channel-bindings/${seg(id)}/publish`),
		unpublishBinding: (id) =>
			req<ChannelBinding>('POST', `/api/jobs/${j()}/channel-bindings/${seg(id)}/unpublish`),
		rotateIdentitySecret: (id, opts) =>
			req<IdentitySecret>(
				'POST',
				`/api/jobs/${j()}/channel-bindings/${seg(id)}/identity-secret`,
				opts?.revokeSessions ? { query: { revoke_sessions: 'true' } } : undefined,
			),
		createChannelTestSession: (id, customerRef) =>
			req<ChannelTestSession>('POST', `/api/jobs/${j()}/channel-bindings/${seg(id)}/test-session`, {
				body: { customer_ref: customerRef },
			}),
		// B7 T15: live, `channelId` is the Telegram binding's id; `job` its job (the tenant Channels page), else the
		// page's job
		sendChannelTestMessage: (channelId, job) =>
			req<ChannelTestResult>(
				'POST',
				`/api/jobs/${job ? seg(job) : j()}/channel-bindings/${seg(channelId)}/test-message`,
			),
		// T13: tenant-wide, no job in the path
		tenantChannels: () => req<ChannelsView>('GET', '/api/tenant/channels'),

		// ---- B13 party identity
		parties: async (opts = {}) =>
			pageOf<PartySummary>(
				await req<unknown>('GET', `/api/parties`, { query: { q: opts.q || undefined, before: opts.before } }),
			),
		party: async (id) => {
			const r = await req<PartyDetail>('GET', `/api/parties/${seg(id)}`);
			return {
				...r,
				identities: Array.isArray(r?.identities) ? r.identities : [],
				cases: Array.isArray(r?.cases) ? r.cases : [],
			};
		},
		caseParty: async (caseId) =>
			(await req<CasePartyAnswer | null>('GET', `/api/jobs/${j()}/cases/${seg(caseId)}/party`))?.party ??
			null,
		linkCaseParty: async (caseId, partyId) => {
			const r = await req<CasePartyAnswer>('POST', `/api/jobs/${j()}/cases/${seg(caseId)}/party`, {
				body: { party_id: partyId },
			});
			if (!r?.party) throw new Error('The case was linked but the answer carried no party.');
			return r.party;
		},

		// ---- B13 per-job API keys (the job is the key's, not necessarily the active one)
		jobApiKeys: async (job) => pageOf<JobApiKey>(await req<unknown>('GET', `/api/jobs/${seg(job)}/api-keys`)),
		createJobApiKey: (job, input) =>
			req<CreatedJobApiKey>('POST', `/api/jobs/${seg(job)}/api-keys`, {
				body: input.expires_at
					? { label: input.label, expires_at: input.expires_at }
					: { label: input.label },
			}),
		revokeJobApiKey: async (job, id) => {
			await req<unknown>('DELETE', `/api/jobs/${seg(job)}/api-keys/${seg(id)}`);
		},
	};
}
