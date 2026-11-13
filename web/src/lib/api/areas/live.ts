// The `LiveApi` contract.
import type { ConsoleApi } from './console';
import type { Paged } from '$lib/api/types/core';
import type {
	BusBacklog,
	ChatEnv,
	Conversation,
	ConversationStart,
	ChatProfile,
	CsatRequest,
	MessageAccepted,
	TestCase,
} from '$lib/api/types/live';

/** L1 operator console methods are `ConsoleApi` (./console.ts). */
export type LiveApi = ConsoleApi & {
	/** `GET /api/jobs/{job}/chat/profile` (concept) — L2: who the job talks to, its quick prompts, the systems each
	 *  env reaches and whether there is a Live (a published release) to talk to at all. */
	chatProfile(): Promise<ChatProfile>;
	/** `POST /api/jobs/{job}/conversations` (edge, K38) — starts a staff test conversation. `customer_email`: who the
	 *  STAFF MEMBER tests as (required for a customer job; 422 `test_customer_required`). */
	startChat(input: { env: ChatEnv; customer_email: string }): Promise<ConversationStart>;
	/** `GET /api/jobs/{job}/conversations/{id}` (edge) — the delivered log. */
	chatConversation(id: string): Promise<Conversation>;
	/**
	 * `POST /api/jobs/{job}/conversations/{id}/messages` (edge) — ≤20 s then **202**; the reply arrives on
	 * `GET …/conversations/{id}/events`. Gate 2 runs at the inbox drain: a refused turn still gets 202 and the
	 * reply is the polite unavailable text (K35, K39).
	 */
	sendChatMessage(id: string, text: string): Promise<MessageAccepted>;
	/**
	 * `GET /api/jobs/{job}/events` (admin SSE) — U1 stand-in: the entries published after `after_ms`, oldest first,
	 * plus the agents' consumer-group metrics. Op envelopes carry ids + outcome codes only (K50).
	 */
	busBacklog(opts?: { after_ms?: number }): Promise<BusBacklog>;
	/**
	 * `GET /api/jobs/{job}/test-cases` — D54 TEST step: staff test conversations saved as test cases (env=test, on
	 * the draft), newest first. `stale` = the draft changed after the test was saved. J14 shows them as "tested".
	 */
	testCases(): Promise<Paged<TestCase>>;
	/**
	 * `POST /api/jobs/{job}/test-cases` (job_designer) — save a staff TEST-env conversation as a test case on the
	 * current draft. 409 `not_a_test_conversation` for a Production chat; 422 `nothing_to_save` before a reply.
	 */
	saveTestCase(input: { conversation_id: string; title: string }): Promise<TestCase>;
	/**
	 * `POST /api/jobs/{job}/conversations/{id}/csat` `{case_id, rating}` (any member; B4 T16b, F-13) — the
	 * customer's 1–5 rating of a resolved case, once. Answers the conversation's latest request (`csat_request`).
	 * 404 `conversation_not_found` / `csat_not_requested`, 409 `csat_answered` / `csat_expired`, 422 `invalid_rating`.
	 * Staff test-chat ratings carry env=test and stay out of the analytics.
	 */
	submitCsat(conversationId: string, input: { case_id: string; rating: number }): Promise<CsatRequest | null>;
};
