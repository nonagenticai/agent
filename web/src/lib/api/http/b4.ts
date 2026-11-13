// B4 over HTTP (T10a, milestone M1): the staff test chat (L2) — the job's chat profile, the demo customers staff
// chat as, start a conversation, read its delivered log and send a message (202; the reply arrives on the
// conversation SSE, B12's `conversationEvents`). Every route is job scoped (`/api/jobs/{job}/…`); `job()` is the job
// the page acts in (`app.job`). T12 adds the case reads (J19/J20) and B12's case hand-over. T21a adds the
// operation-call reads + check (T13, J20/J22), the bus admin reads and writes (T14, J23/J24) and the tenant usage
// report (T11, `/api/tenant/usage`, tenant scoped), and the job dashboard + analytics reads (T15, J1/J2).
import type { Api } from '../client';
import { seg, type Req } from '../http-client';
import type { Paged } from '../types/core';
import type { ConversationFrame } from '../types/conversations';
import type {
	AgentFleet,
	BusAgent,
	BusFlow,
	CaseDetail,
	CaseHit,
	CaseRow,
	Caseload,
	DeadLetter,
	Entry,
	JobAnalytics,
	JobOverview,
	OperationCall,
	OperationStat,
	UnknownOutcome,
} from '../types/runtime';
import type { UsageReport } from '../types/tenant';
import type { AgentStatus, Inbox } from '../types/status';
import type {
	BusBacklog,
	BusEvent,
	ChatEnv,
	ChatEvent,
	ChatProfile,
	Conversation,
	ConversationStart,
	LiveAgent,
	MessageAccepted,
} from '../types/live';

export type B4Method =
	| 'chatProfile'
	| 'startChat'
	| 'chatConversation'
	| 'sendChatMessage'
	// the console Bus stream tab (L1): `console:recent` read as JSON
	| 'busBacklog'
	// T12 cases (J19/J20)
	| 'cases'
	| 'caseload'
	| 'searchCases'
	| 'caseDetail'
	| 'handOverCase'
	// T21a over T13 operation calls (J20/J22)
	| 'operationCalls'
	| 'operationStats'
	| 'unknownOutcomes'
	| 'checkUnknownOutcome'
	// T21a over T14 bus admin (J23/J24)
	| 'busAgents'
	| 'agentFleet'
	| 'busFlow'
	| 'streamEntries'
	| 'deadLetters'
	| 'claimStuck'
	| 'replayDeadLetter'
	| 'discardDeadLetter'
	// T21a over T11 usage (tenant scoped)
	| 'tenantUsage'
	// T21a over T15 job dashboard + analytics (J1/J2)
	| 'jobOverview'
	| 'agentStatus'
	| 'inbox'
	| 'jobAnalytics';

const ROLES: readonly ChatEvent['role'][] = ['customer', 'agent', 'system'];

/**
 * One message as the chat reads it (a transcript line or an SSE `message` frame's data): ids as text, a null
 * `case_id` dropped, an unknown role read as `system`, `refused` only when true. Null when it is no message.
 */
export function toChatEvent(raw: unknown, conversationId?: string): ChatEvent | null {
	if (!raw || typeof raw !== 'object') return null;
	const r = raw as Record<string, unknown>;
	if (r.message_id === undefined || r.message_id === null) return null;
	const role = ROLES.includes(r.role as ChatEvent['role']) ? (r.role as ChatEvent['role']) : 'system';
	const e: ChatEvent = {
		...(r as Partial<ChatEvent>),
		conversation_id: String(r.conversation_id ?? conversationId ?? ''),
		message_id: String(r.message_id),
		role,
		text: typeof r.text === 'string' ? r.text : '',
		at_ms: typeof r.at_ms === 'number' ? r.at_ms : Date.now(),
	};
	if (r.case_id === null || r.case_id === undefined || r.case_id === '') delete e.case_id;
	else e.case_id = String(r.case_id);
	if (r.refused !== true) delete e.refused;
	return e;
}

/** The chat message a conversation SSE frame carries (`message`, or the legacy `chat`); null for other frames. */
export function chatEventOf(f: ConversationFrame, conversationId?: string): ChatEvent | null {
	if (f.event !== 'message' && f.event !== 'chat') return null;
	return toChatEvent(f.data, conversationId);
}

/** `GET …/conversations/{id}` as the chat's `Conversation`: messages normalised, missing lists empty. */
export function toConversation(raw: Record<string, unknown>, id: string): Conversation {
	const messages = (Array.isArray(raw.messages) ? raw.messages : [])
		.map((m) => toChatEvent(m, id))
		.filter((m): m is ChatEvent => m !== null);
	return {
		...(raw as Partial<Conversation>),
		conversation_id: String(raw.conversation_id ?? id),
		tenant_id: String(raw.tenant_id ?? ''),
		job: String(raw.job ?? ''),
		customer_id: String(raw.customer_id ?? ''),
		env: (raw.env === 'prod' ? 'prod' : 'test') as ChatEnv,
		channel: 'staff',
		initiator_sub: String(raw.initiator_sub ?? ''),
		case_ids: Array.isArray(raw.case_ids) ? raw.case_ids.map(String) : [],
		messages,
	} as Conversation;
}

/**
 * One `busBacklog` item as the console reads it: agent-api sends `{stream, entry_id, envelope, consumer_group,
 * acked}` (`sse_hub::backlog`) with the envelope's scope stamp (`tenant_id`, `job`) and `acked` possibly absent.
 * Null when it carries no entry id or envelope.
 */
export function toBusEvent(raw: unknown): BusEvent | null {
	if (!raw || typeof raw !== 'object') return null;
	const r = raw as Record<string, unknown>;
	const env = r.envelope;
	if (typeof r.entry_id !== 'string' || !env || typeof env !== 'object') return null;
	const e = env as Record<string, unknown>;
	const str = (v: unknown) => (typeof v === 'string' ? v : '');
	return {
		stream: str(r.stream),
		entry_id: r.entry_id,
		envelope: {
			...(e as Partial<BusEvent['envelope']>),
			id: str(e.id),
			topic: str(e.topic),
			from: str(e.from),
			correlation_id: str(e.correlation_id),
			published_at_ms: typeof e.published_at_ms === 'number' ? e.published_at_ms : 0,
			payload: e.payload && typeof e.payload === 'object' ? (e.payload as Record<string, unknown>) : {},
			tenant_id: str(e.tenant_id),
			job: str(e.job),
		},
		consumer_group: typeof r.consumer_group === 'string' ? r.consumer_group : null,
		acked: typeof r.acked === 'boolean' ? r.acked : null,
	};
}

/** `GET …/events?after_ms=` as a `BusBacklog`: items normalised and oldest first, `agents` as sent (agent-api: []). */
export function toBusBacklog(raw: unknown): BusBacklog {
	const r = (raw ?? {}) as { items?: unknown; agents?: unknown };
	const items = (Array.isArray(r.items) ? r.items : [])
		.map(toBusEvent)
		.filter((e): e is BusEvent => e !== null)
		.sort((a, b) => a.envelope.published_at_ms - b.envelope.published_at_ms);
	return { items, agents: Array.isArray(r.agents) ? (r.agents as BusBacklog['agents']) : [] };
}

const LIVE_KINDS: readonly LiveAgent['kind'][] = ['front', 'coordination', 'runner', 'gate'];
const LIVE_STATUS: readonly LiveAgent['status'][] = ['ok', 'warn', 'idle'];

/** A J23 `BusAgent` as the console strip reads it (`LiveAgent`); a `worker` sits with the runners. */
export function toLiveAgent(a: BusAgent): LiveAgent {
	const n = (v: unknown) => (typeof v === 'number' ? v : 0);
	return {
		id: String(a.id),
		name: String(a.name ?? a.id),
		kind: LIVE_KINDS.includes(a.kind as LiveAgent['kind']) ? (a.kind as LiveAgent['kind']) : 'runner',
		group: String(a.group ?? ''),
		consumers: n(a.consumers),
		pending: n(a.pending),
		lag: n(a.lag),
		status: LIVE_STATUS.includes(a.status) ? a.status : 'idle',
	};
}

/** How long the console's agent metrics are reused between backlog polls (the backlog polls every ~0.9 s). */
export const AGENTS_TTL_MS = 5000;

/** A list answer as a page: `[…]` or `{items, next}`. */
function pageOf<T>(raw: unknown): Paged<T> {
	if (Array.isArray(raw)) return { items: raw as T[], next: null };
	const r = (raw ?? {}) as { items?: T[]; next?: string | null };
	return { items: r.items ?? [], next: r.next ?? null };
}

export function httpB4(req: Req, job: () => string): Pick<Api, B4Method> {
	const j = () => seg(job());
	// The backlog route sends `agents: []` (sse_hub::backlog); the strip's metrics come from J23 `busAgents`, read at
	// most every AGENTS_TTL_MS per job. A failed read keeps the last answer (none yet: [] → the strip says unknown).
	let agents: { job: string; at: number; list: LiveAgent[] } | null = null;
	async function liveAgents(): Promise<LiveAgent[]> {
		const key = job();
		const now = Date.now();
		if (agents && agents.job === key && now - agents.at < AGENTS_TTL_MS) return agents.list;
		try {
			const page = pageOf<BusAgent>(await req<unknown>('GET', `/api/jobs/${j()}/agents`));
			agents = { job: key, at: now, list: page.items.map(toLiveAgent) };
		} catch {
			if (agents?.job !== key) return [];
			agents = { ...agents, at: now };
		}
		return agents.list;
	}
	return {
		chatProfile: () => req<ChatProfile>('GET', `/api/jobs/${j()}/chat/profile`),
		startChat: (input) =>
			req<ConversationStart>('POST', `/api/jobs/${j()}/conversations`, {
				body: { env: input.env, customer_email: input.customer_email },
			}),
		chatConversation: async (id) =>
			toConversation(
				await req<Record<string, unknown>>('GET', `/api/jobs/${j()}/conversations/${seg(id)}`),
				id,
			),
		sendChatMessage: (id, text) =>
			req<MessageAccepted>('POST', `/api/jobs/${j()}/conversations/${seg(id)}/messages`, {
				body: { text },
			}),
		// ---- T12 cases: `all` filters are sent as absent
		cases: async (f = {}) =>
			pageOf<CaseRow>(
				await req<unknown>('GET', `/api/jobs/${j()}/cases`, {
					query: {
						status: f.status === 'all' ? undefined : f.status,
						env: f.env === 'all' ? undefined : f.env,
						q: f.q?.trim() || undefined,
						before: f.before,
					},
				}),
			),
		searchCases: async (q) => {
			const text = q.trim();
			if (text.length < 2) return [];
			const r = await req<{ cases?: CaseHit[] }>('GET', '/api/search', { query: { q: text } });
			return Array.isArray(r.cases) ? r.cases : [];
		},
		caseload: (f = {}) =>
			req<Caseload>('GET', `/api/jobs/${j()}/caseload`, {
				query: { env: f.env === 'all' ? undefined : f.env },
			}),
		caseDetail: (id) => req<CaseDetail>('GET', `/api/jobs/${j()}/cases/${seg(id)}`),
		handOverCase: (id, reason) =>
			req<CaseRow>('POST', `/api/jobs/${j()}/cases/${seg(id)}/handover`, { body: { reason } }),
		// ---- T13 operation calls: blank filters are sent as absent (the query builder drops '' and null)
		operationCalls: async (f = {}) =>
			pageOf<OperationCall>(
				await req<unknown>('GET', `/api/jobs/${j()}/operation-calls`, {
					query: {
						op: f.op?.trim() || undefined,
						case_id: f.case_id?.trim() || undefined,
						route: f.route,
						outcome: f.outcome?.trim() || undefined,
						env: f.env,
						instance: f.instance?.trim() || undefined,
						handling: f.handling,
						before: f.before,
					},
				}),
			),
		operationStats: async () =>
			pageOf<OperationStat>(await req<unknown>('GET', `/api/jobs/${j()}/operation-stats`)),
		unknownOutcomes: async () =>
			pageOf<UnknownOutcome>(await req<unknown>('GET', `/api/jobs/${j()}/operation-calls/unknown`)),
		checkUnknownOutcome: (callId, verdict, note) =>
			req<UnknownOutcome>('POST', `/api/jobs/${j()}/operation-calls/${seg(callId)}/check`, {
				body: { verdict, note },
			}),
		// ---- L1 console Bus stream: `after_ms` (0 = everything retained) selects the JSON backlog, not the SSE feed
		// (agents from J23 `busAgents` when the backlog carries none)
		busBacklog: async (opts = {}) => {
			const [raw, fleet] = await Promise.all([
				req<unknown>('GET', `/api/jobs/${j()}/events`, { query: { after_ms: opts.after_ms ?? 0 } }),
				liveAgents(),
			]);
			const b = toBusBacklog(raw);
			return b.agents.length ? b : { ...b, agents: fleet };
		},
		// ---- T14 bus admin: stream keys are unprefixed (`bus:op`), one encoded path segment each
		busAgents: async () => pageOf<BusAgent>(await req<unknown>('GET', `/api/jobs/${j()}/agents`)),
		agentFleet: () => req<AgentFleet>('GET', `/api/jobs/${j()}/agents/fleet`),
		busFlow: (env) => req<BusFlow>('GET', `/api/jobs/${j()}/bus-flow`, { query: { env } }),
		streamEntries: async (key, env, limit) =>
			pageOf<Entry>(
				await req<unknown>('GET', `/api/jobs/${j()}/streams/${seg(key)}/entries`, { query: { limit, env } }),
			),
		deadLetters: async () => pageOf<DeadLetter>(await req<unknown>('GET', `/api/jobs/${j()}/dead-letters`)),
		claimStuck: (key, group, env) =>
			req<{ claimed: number }>('POST', `/api/jobs/${j()}/streams/${seg(key)}/groups/${seg(group)}/claim`, {
				query: { env },
				body: {},
			}),
		replayDeadLetter: (entryId) =>
			req<{ stream: string; entry_id: string }>(
				'POST',
				`/api/jobs/${j()}/dead-letters/${seg(entryId)}/replay`,
				{ body: {} },
			),
		discardDeadLetter: (entryId) =>
			req<{ discarded: string }>('DELETE', `/api/jobs/${j()}/dead-letters/${seg(entryId)}`),
		// ---- T11 usage: the acting tenant's report (not job scoped)
		tenantUsage: ({ window, group }) =>
			req<UsageReport>('GET', '/api/tenant/usage', { query: { window, group } }),
		// ---- T15 job dashboard (J1) + analytics (J2): Production figures only
		jobOverview: () => req<JobOverview>('GET', `/api/jobs/${j()}/overview`),
		agentStatus: (job) => req<AgentStatus>('GET', `/api/jobs/${job ? seg(job) : j()}/status`),
		inbox: (job) => req<Inbox>('GET', '/api/inbox', { query: { job } }),
		jobAnalytics: (days) => req<JobAnalytics>('GET', `/api/jobs/${j()}/analytics`, { query: { days } }),
	};
}
