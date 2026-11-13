// Shapes for the "live" area (L1 Console, L2 Test chat, L3 retired): spec 14 §3 field names, snake_case as the
// API will send them. Bus/chat shapes are this app's own contract (Envelope, ChatEvent) per doc 12 §3.2 (op.call /
// op.result, mirrored to the console as ids + outcome codes only, K50). (UX reference: care-bus-agents console/demo.)
import type { Iso, JobKey, Sub, TenantId } from './core';
import type { CaseRow, TimelineEvent } from './runtime';
import type { AnswerGrounding } from './grounding';

// ---- conversations (L2) ------------------------------------------------------------------------------------------

/** Route env the staff member picks (doc 12 §2 K50): `test` (default) routes to the test connector instances. */
export type ChatEnv = 'prod' | 'test';

/** A demo customer the staff member chats as (a `customers` row of demo-backends). */
export interface DemoCustomer {
	customer_id: string;
	name: string;
	tier: string;
	city: string;
	/** Their newest order, used when a message names none. */
	latest_order: string | null;
}

/** L2: who a job's test chat talks to and what it can reach (`GET /api/jobs/{job}/chat/profile`, concept). */
export interface ChatProfile {
	/** `customers` (a customer-facing job with a demo stub) or `staff` (an internal job, e.g. the IT service desk). */
	audience: 'customers' | 'staff';
	agent_name: string;
	/** Quick prompts offered under the chat. */
	prompts: string[];
	/** Systems the Draft (Test env) reaches. */
	test_systems: string;
	/** Systems Live (Production) reaches; null when the job has no release yet. */
	live_systems: string | null;
	/** False for a job that was never released: there is no Live to talk to. */
	has_release: boolean;
	/** The polite unavailable reply a refused turn gets (gate 2: licence or tenant not active, K35). */
	refusal_text: string;
	/** What is missing before a test can say anything useful. Absent on older backends: treated as nothing missing. */
	test_readiness?: TestReadiness;
}

/** One thing to fix before testing (`ChatProfile.test_readiness.items`). */
export interface TestReadinessItem {
	code: string;
	title: string;
	fix_label: string;
	href: string;
}

export interface TestReadiness {
	qualify_bound: boolean;
	template_errors: string[];
	items: TestReadinessItem[];
}

/**
 * One chat event on this app's SSE stream (`event: chat`). Only the agent reply that opened a case carries `case_id`. Agent
 * replies also carry their grounding (`citations`/`sources`, faq_ask shape — see types/grounding).
 */
export interface ChatEvent extends AnswerGrounding {
	conversation_id: string;
	/** Edge message id; the customer echo carries the id the 202 answered with. */
	message_id: string;
	role: 'customer' | 'agent' | 'system';
	text: string;
	case_id?: string;
	/** The reply is the gate-2 refusal text (licence or tenant not active, K35). */
	refused?: boolean;
	at_ms: number;
	/** L2 (staff test chat only): how the agent reached this reply, and where the other design would differ. */
	trace?: ReplyTrace;
}

// ---- L2 reasoning trail + test cases (D54 TEST step) --------------------------------------------------------------

/** Which design answers a staff test chat: `draft` (unpublished changes, Test env) or `live` (the published
 *  release, Production). Derived from the conversation's env: test ↔ draft, prod ↔ live. */
export type ChatDesign = 'draft' | 'live';

export interface TraceGate {
	/** Gate key (`GM`). */
	gate: string;
	question: string;
	/** Option the gate picked (`request`), or a score / yes-no. */
	verdict: string;
	/** Label the design gives that option (`Arrange it for me`). */
	label: string;
	/** Set when the other design labels or thresholds this differently. */
	other_label?: string | null;
	/** Ledger key of the unpublished change that touches this gate (`gate:GM:request`). */
	draft_key?: string | null;
}

export interface TraceStep {
	label: string;
	state: 'done' | 'failed' | 'asked' | 'stopped';
	detail?: string;
}

export interface TraceCheck {
	/** Rule id (`RET-01`). */
	id: string;
	label: string;
	verdict: 'pass' | 'fail';
	detail: string;
	/** The other design's evaluation, when its values differ. */
	other_detail?: string | null;
	draft_key?: string | null;
}

export interface TracePostCheck {
	key: string;
	label: string;
	verdict: 'pass' | 'fail' | 'skipped';
	detail: string;
}

/** One way the other design (live when testing the draft, draft when talking to live) would act differently. */
export interface TraceDiff {
	kind: 'outcome' | 'label' | 'rule' | 'route';
	/** Ledger key of the unpublished change behind it, if any. */
	key: string | null;
	summary: string;
	/** What the other design would have replied (outcome diffs). */
	other_text?: string | null;
}

/** The reasoning trail of one agent reply in the staff test chat. Ids and outcome codes only, no PII (K50). */
export interface ReplyTrace {
	design: ChatDesign;
	env: ChatEnv;
	/** The published release the design starts from (Production runs exactly this). */
	release_id: string | null;
	/** The job’s draft revision (`draft_rev`) the turn ran on; null for live. Display: `formatDraftRev`. */
	draft_rev: number | null;
	/** Fingerprint of the draft (every unpublished change and its value) the turn ran on; null for live. */
	draft_fp: string | null;
	draft_changes: number;
	activity: { key: string; label: string } | null;
	procedure: string | null;
	gates: TraceGate[];
	steps: TraceStep[];
	ops: { operation: string; state: string; code: string; instance_id: string; latency_ms: number }[];
	checks: TraceCheck[];
	/** Knowledge passages cited (`policy.returns`). */
	cites: string[];
	post_checks: TracePostCheck[];
	differs: TraceDiff[];
	/** Ledger keys of the unpublished changes this turn exercised (same result or not). */
	exercised: string[];
	/** D108 (KH T5): the layer markers of the turn's system message (`[layer <key>@<version>]`), in prompt order. */
	layers?: string[];
	/** KH T5: the assistant rounds' layer markers (absent when no assistant ran). */
	assistant_layers?: string[];
	/** KH T5: set when the prompt had no template layer section (`template_errors` | `no_layers`). */
	prompt_warning?: string;
}

/** One turn of a saved test case. */
export interface TestCaseTurn {
	customer: string;
	agent: string;
	outcome: string | null;
	case_id: string | null;
	/** Summaries of where live would have differed on this turn. */
	differs: string[];
}

/**
 * D54: a staff test conversation saved as a test case (`POST /api/jobs/{job}/test-cases`). Always env=test on the
 * draft. J14 lists them to show which unpublished changes were tried before publishing; `stale` = the draft changed
 * after the test was saved.
 */
export interface TestCase {
	id: string;
	title: string;
	conversation_id: string;
	customer_id: string;
	env: 'test';
	design: 'draft';
	release_id: string | null;
	/** The job’s draft revision (`draft_rev`) when it was saved. Display: `formatDraftRev`. */
	draft_rev: number;
	/** Fingerprint of the draft when it was saved; `stale` = the current draft's fingerprint differs. */
	draft_fp: string;
	/** Ledger keys of every unpublished change in the draft when it was saved. */
	draft_keys: string[];
	/** Ledger keys the conversation exercised. */
	exercised: string[];
	/** Turns where live would have answered differently. */
	differs: number;
	turns: TestCaseTurn[];
	saved_by: Sub;
	saved_by_name: string;
	saved_at: Iso;
	/** The draft changed since this was saved: re-run it before publishing. */
	stale: boolean;
}

/** `POST /api/jobs/{job}/conversations` (201). */
export interface ConversationStart {
	conversation_id: string;
	customer_id: string;
	customer_name: string | null;
	env: ChatEnv;
	channel: 'staff';
	started_at: Iso;
}

/** `GET /api/jobs/{job}/conversations/{id}` — the delivered log (replies still in flight are not in it). */
export interface Conversation {
	conversation_id: string;
	tenant_id: TenantId;
	job: JobKey;
	customer_id: string;
	env: ChatEnv;
	/** Staff test chats run on `staff`; a conversation a customer started carries its real channel. */
	channel: import('./conversations').ConversationChannel;
	initiator_sub: string;
	messages: ChatEvent[];
	case_ids: string[];
	/** F-13 (B4 T16b): the conversation's latest rating request (agent-api `csat::latest`), null when none. */
	csat_request?: CsatRequest | null;
}

/**
 * F-13: a rating request for one case of the conversation (`case_csat`): asked when the case resolved, answered once
 * with 1–5 (`rating`, null while unanswered), closed at `expires_at` (24 h after the request).
 */
export interface CsatRequest {
	case_id: string;
	requested_at: Iso;
	rating: number | null;
	expires_at: Iso;
}

/**
 * `POST /api/jobs/{job}/conversations/{id}/messages` — the edge waits ≤20 s for the reply; past that it answers
 * **202** and the reply arrives on the conversation SSE (K39). The fixture always answers 202 so the pending turn
 * is visible.
 */
export interface MessageAccepted {
	status: 202;
	conversation_id: string;
	message_id: string;
}

// ---- bus (L1) ----------------------------------------------------------------------------------------------------

/** The envelope every bus entry carries (this app's runtime bus). The stamp (`tenant_id`, `job`) is the scope. */
export interface Envelope {
	id: string;
	topic: string;
	from: string;
	correlation_id: string;
	published_at_ms: number;
	payload: Record<string, unknown>;
	tenant_id: TenantId;
	job: JobKey;
}

/** One stream entry as the admin SSE `GET /api/jobs/{job}/events` sends it. */
export interface BusEvent {
	stream: string;
	entry_id: string;
	envelope: Envelope;
	/** The group that consumes this entry; null for dead letters. */
	consumer_group: string | null;
	acked: boolean | null;
}

/** An agent of the runtime with its consumer-group metrics (`GET /api/jobs/{job}/agents`, runtime area). */
export interface LiveAgent {
	id: string;
	name: string;
	kind: 'front' | 'coordination' | 'runner' | 'gate';
	group: string;
	consumers: number;
	pending: number;
	lag: number;
	status: 'ok' | 'warn' | 'idle';
}

/** `GET /api/jobs/{job}/events/backlog` (stand-in for the SSE replay on connect): recent entries, oldest first. */
export interface BusBacklog {
	items: BusEvent[];
	agents: LiveAgent[];
}

// ---- operator console (L1) ---------------------------------------------------------------------------------------

/**
 * One conversation as the operator console sees it right now (`GET /api/jobs/{job}/console/conversations`): the
 * case row as of now, the turn events that have happened so far (runtime `TimelineEvent` vocabulary: messages,
 * P1/P2/R1 gate verdicts, qualification, knowledge passages, operation calls, state changes, replies, alerts) and
 * who is in control. Operation events carry ids + outcome codes only (K50).
 */
export interface ConsoleConversation {
	case: CaseRow;
	events: TimelineEvent[];
	/** The agent is mid-turn: more events of this conversation are still on their way. */
	active: boolean;
	/** Production runs the published release; the test env runs the job's draft (D54). */
	release: { id: string; source: 'live' | 'draft' };
	/** Who answers (the shared owner record, SW-1). */
	owner: import('./support').OwnerView;
}

/** `GET /api/jobs/{job}/console/conversations` — every conversation of the job seen in the console window. */
export interface ConsoleSnapshot {
	at_ms: number;
	conversations: ConsoleConversation[];
}
