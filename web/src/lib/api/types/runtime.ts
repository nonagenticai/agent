// Shapes for the "runtime" area: job dashboard, analytics, cases + case timeline, operation calls, bus agents,
// streams and dead letters. Field names follow spec 14 §3, snake_case as this app's API sends them
// (UX reference: care-bus-agents). Owned by the runtime area agent.
import type { Iso, JsonObject, JobKey, ReleaseRef, Sub, TenantId } from './core';
import type { AnswerGrounding } from './grounding';
import type { MissingItem } from './release';
import type { AttentionItem } from './tenant';

// ---- J1 dashboard ------------------------------------------------------------------------------------------------

/** Release status flags the dashboard shows next to the current release (05 §4.7). */
export interface ReleaseStatus {
	/** A newer tag of a pinned layer exists: `{layer, pinned, latest}` per behind layer. */
	base_changed: { layer: string; pinned: number; latest: number; tagged_at: Iso }[];
	/** An approved release not yet applied. */
	approved_waiting: { id: string; hash: string; approved_by: string; approved_at: Iso } | null;
	/** The job's selection does not compile (05 §4.7); null when it does. */
	selection_error: string | null;
	/** Readiness items that block apply: W5 slots and required routes (`409 release_not_ready`). */
	missing_slots: MissingItem[];
}

export interface HealthRow {
	name: string;
	status: 'ok' | 'down' | 'degraded' | 'unconfigured';
	detail: string | null;
}

/** `GET /api/jobs/{job}/overview` — J1. */
export interface JobOverview {
	job: JobKey;
	label: string;
	north_star: string;
	on_bus: boolean;
	current_release:
		| (ReleaseRef & {
				released_by: string | null;
				released_at: Iso | null;
		  })
		| null;
	release_status: ReleaseStatus;
	/** Usage window totals (30 days) and the previous 30 days for deltas. */
	kpis: {
		turns_30d: number;
		turns_prev: number;
		containment: number | null;
		containment_prev: number | null;
		escalation: number | null;
		escalation_prev: number | null;
		open_cases: number;
		north_star: { label: string; value: number | null; unit: 'pct' | 'n' };
	};
	health: HealthRow[];
	/** Everything on this job that needs a person (`readmodel::job_attention`), ranked; hrefs are absolute. */
	attention: AttentionItem[];
	/** J1 "right now": open cases by status, bus agents, failing operations (absent on older payloads). */
	pulse?: JobPulse;
	/** J1 template stack: every pin in compose order with the newer tag when the platform published one. */
	pins?: { layer: string; pinned: number; latest: number | null; tagged_at: Iso | null }[];
}

/** What the job is doing this minute (J1): live work, who serves it, which operations fail. */
export interface JobPulse {
	/** Open (not closed) cases per status. */
	open_by_status: Partial<Record<CaseStatus, number>>;
	/** Open cases in prod vs test. */
	open_prod: number;
	open_test: number;
	/** Handed-over cases nobody claimed yet. */
	unclaimed_handovers: number;
	/** Open cases carrying an alert (`unknown_outcome`): never retried automatically. */
	alerts: number;
	agents: { total: number; ok: number; behind: number; idle: number };
	dead_letters: number;
	/** 30-day operations whose error rate is ≥ 10 %, worst first. */
	failing_ops: { op: string; instance: string | null; calls: number; errors: number; rate: number }[];
	/** 30-day operation calls and errors (all operations). */
	op_calls_30d: number;
	op_errors_30d: number;
}

// ---- J2 analytics --------------------------------------------------------------------------------------------------

export interface DayPoint {
	day: string;
	turns: number;
	cases: number;
	contained: number;
	escalated: number;
	not_eligible: number;
}

export interface EscalationReason {
	reason: string;
	label: string;
	count: number;
}

/** `GET /api/jobs/{job}/analytics?days=7|30` — J2. */
export interface JobAnalytics {
	days: DayPoint[];
	previous: { turns: number; cases: number; contained: number; escalated: number };
	escalation_reasons: EscalationReason[];
	csat: { score: number | null; responses: number; previous: number | null };
	north_star: { label: string; value: number | null; target: number | null; unit: 'pct' | 'n' };
	by_activity: {
		activity: string;
		cases: number;
		contained: number;
		escalated: number;
		p50_ms: number | null;
	}[];
	tokens: number;
	funding: { local_gpu: number; platform_external: number; tenant_byo: number };
	/** J2 "did the last release help?": outcomes per applied release since it went live, newest first (optional). */
	release_windows?: ReleaseWindow[];
	/** Server time the windows are measured to (J2 "live for N days"). */
	as_of?: Iso;
	/**
	 * Test-environment traffic (staff test chats against the unpublished draft), kept apart: every other figure in
	 * this payload is production only (D54).
	 */
	test_env?: TestEnvStats;
}

/** J2: staff test chats against the draft since the last release (`env = test`). */
export interface TestEnvStats {
	/** The draft revision the test chats ran on. */
	draft_rev: number;
	since: Iso;
	chats: number;
	contained: number;
	escalated: number;
	/** Draft changes being tried, in the operator's words. */
	trying: string[];
}

/** Outcomes of one release while it was the live one (`applied_at` → `until`, or now). J2 release comparison. */
export interface ReleaseWindow {
	release_id: string;
	/** `approved` = cut and approved, not applied yet (no traffic). */
	status: 'current' | 'earlier' | 'approved';
	applied_at: Iso | null;
	until: Iso | null;
	cases: number;
	contained: number;
	escalated: number;
	csat: number | null;
	csat_responses: number;
	p50_ms: number | null;
	tokens_per_case: number | null;
	/** What the release changed, in the operator's words (from the release diff). */
	changes: string[];
	/** The tenant's own refinements this release published, newest first. */
	refinements?: { rev: number; summary: string }[];
	/** Activities whose containment moved most against the release before (rates 0–1). */
	moves: { activity: string; from: number; to: number; cases: number }[];
}

// ---- J19 / J20 cases ----------------------------------------------------------------------------------------------

export type CaseStatus =
	| 'open'
	| 'identity_required'
	| 'confirmation_required'
	| 'handover'
	| 'resolved'
	| 'not_eligible'
	| 'failed'
	| 'cancelled_offboard';

export type Env = 'prod' | 'test';
/** The case's channel: that of its conversation (staff test chat, Telegram, web chat, …). */
export type Channel = import('./conversations').ConversationChannel;

/** `GET /api/jobs/{job}/cases` row (03 §2.7, spec 14 §3 Case). */
export interface CaseRow {
	case_id: string;
	tenant_id: TenantId;
	job: JobKey;
	conversation_id: string;
	channel: Channel;
	env: Env;
	/** `stage:activity`. */
	activity: string;
	request_type: string;
	title: string;
	status: CaseStatus;
	/** Last outcome code (`ok`, `invalid_state:in_transit`, `unknown_outcome`, …); null while open. */
	outcome: string | null;
	escalation_reason: string | null;
	identity_level: 0 | 1 | 2 | 3;
	customer_id: string;
	customer_name: string;
	/** Staff sub for staff chats (test env), else null. */
	initiator_sub: string | null;
	/** D63: a Production case records the release it ran on; a Test-env case the draft revision (release null). */
	release_id: string | null;
	draft_rev: number | null;
	started_at: Iso;
	updated_at: Iso;
	/** Handover queue SLA, when the case is in a staff queue. */
	sla: { hours: number; due_at: Iso } | null;
	assignee: Sub | null;
	/** Who answers the conversation now (SW: the one owner record); present when the case has a conversation. */
	owner?: import('./support').OwnerView;
	/** Display name of the assignee (filled by the API). */
	assignee_name?: string | null;
	/** Alert raised on the case (`unknown_outcome`) — never retried automatically. */
	alert: string | null;
	op_calls: number;
}

export interface GateVerdict {
	/** Question key: GT, GS, GA, GO, GM, GF, GU, GP, B7, F1, F3 … */
	key: string;
	label: string;
	value: string;
	confidence: number;
	/** Below the question's threshold → it was asked again or fell back. */
	below_threshold?: boolean;
}

export interface Passage {
	title: string;
	kind: string;
	source: string;
	score: number;
	excerpt: string;
}

export type RouteKind = 'instance' | 'builtin' | 'fallback' | 'deep_link' | 'not_offered';

export type TimelineEvent =
	| {
			kind: 'message';
			at: Iso;
			from: 'customer' | 'staff';
			author: string;
			text: string;
			/** J20: inbound message id — what an operation's `confirm_ref` points at (12 §3.3 step 4). */
			msg_id?: string;
	  }
	| {
			kind: 'gate';
			at: Iso;
			point: 'P1' | 'P2' | 'R1';
			verdict: 'pass' | 'ask' | 'block' | 'handover';
			latency_ms: number;
			verdicts: GateVerdict[];
			note?: string;
	  }
	| {
			kind: 'qualified';
			at: Iso;
			activity: string;
			object: string;
			modality: 'do' | 'request' | 'observe';
			slots: Record<string, string | null>;
			context_space: string;
	  }
	| { kind: 'knowledge'; at: Iso; query: string; store: string; latency_ms: number; passages: Passage[] }
	| {
			kind: 'operation';
			at: Iso;
			call_id: string;
			op: string;
			route: RouteKind;
			instance: string | null;
			template: string | null;
			state: OpCallState;
			outcome: string;
			latency_ms: number | null;
			fallback_from?: string;
			confirm_ref?: string;
			detail?: string;
			/** J20: the procedure step that issued this call (absent = issued by the edge/qualifier or a person). */
			step?: OpStep;
			/** J20: the business object the call changed (11 §6 state machine), when it is a write. */
			object_change?: ObjectChange;
	  }
	| { kind: 'state'; at: Iso; status: CaseStatus; note: string; by?: string }
	/** `cites`: passage kinds; `citations`/`sources`: the chunks it cites (faq_ask shape) — see types/grounding. */
	| ({ kind: 'reply'; at: Iso; text: string; cites?: string[] } & AnswerGrounding)
	| { kind: 'alert'; at: Iso; code: string; text: string };

/** J20: the procedure step behind an operation call (procedure@version, step number + name as in J12). */
export interface OpStep {
	procedure: string;
	version: string;
	n: number;
	name: string;
}

/** J20: one object state change caused by a call. `to: null` = unknown (the write did not answer). */
export interface ObjectChange {
	object: string;
	ref: string;
	from: string | null;
	to: string | null;
}

/** `GET /api/jobs/{job}/cases/{id}` — J20. Raw envelope: ids + codes only (K50). */
export interface CaseDetail {
	case: CaseRow;
	timeline: TimelineEvent[];
	envelope: JsonObject;
}

export interface CaseFilter {
	status?: CaseStatus | 'all';
	env?: Env | 'all';
	q?: string;
	before?: string | null;
}

/**
 * F-19 (B4 T9b): the advisory route hint of a turn's P1 gate verdict — on the `request.qualified` envelope and the
 * case's `qualified` event (`route_hint`, null until B5's hinter). The qualifier still decides the context space.
 */
export interface RouteHint {
	request_type: string;
	stage?: string | null;
	activity?: string | null;
	/** 0–1. */
	confidence: number;
}

// ---- J22 operation calls ------------------------------------------------------------------------------------------

export type OpCallState = 'pending' | 'ok' | 'error' | 'unknown_outcome' | 'denied';

/** `GET /api/jobs/{job}/operation-calls` row (12 §3.7, §9). */
export interface OperationCall {
	call_id: string;
	case_id: string;
	op: string;
	route: RouteKind;
	instance: string | null;
	template: string | null;
	env: Env;
	state: OpCallState;
	outcome: string;
	latency_ms: number | null;
	at: Iso;
	/** `<agent_role>@<consumer>` for agents, or a staff sub. */
	initiator: string;
	attempt: number;
	// J22 enrichment (filled by the API from operation_def, error_state_def and the case; optional for old rows).
	/** Side-effect class of the operation (doc 10 §3.1): R read, W reversible write, X irreversible, P payment. */
	side_effect?: 'R' | 'W' | 'X' | 'P';
	/** Failed or denied call: did the agent answer the customer (user outcome) or escalate (system error)? */
	handling?: CallHandling | null;
	/** How a repeat is made safe: `key` (idempotency key), `dup` (check-then-create), `nat` (reads). */
	idempotency?: string | null;
	/** W/X/P: the inbound customer message that confirmed this call (12 §3.3 step 4). */
	confirm_ref?: string | null;
	/** X/P: the read the runner uses to confirm a write landed (doc 10 write-then-verify). */
	verify_read?: string | null;
	/** Runner note: timeout, fallback reason, ids only. */
	detail?: string | null;
	/** Release the case ran on; test-env cases run the draft (D54). */
	release_id?: string;
	/** Test-env calls: the draft revision the case ran on (D54); null on prod calls. */
	draft_rev?: number | null;
	/** The case's status now; null for a call with no stored case. */
	case_status?: CaseStatus | null;
	/** unknown_outcome only: the person's check of the system, once recorded. */
	check?: UnknownCheck | null;
}

/** reply = user outcome, the agent answers; escalate = system error, handed to staff (doc 10 §3.1). */
export type CallHandling = 'reply' | 'escalate';

/** A person's check of an unknown outcome in the tenant's system (12 §3.3 step 10: never auto-retried). */
export interface UnknownCheck {
	call_id: string;
	verdict: 'landed' | 'not_landed';
	note: string;
	by: Sub | null;
	by_name?: string;
	at: Iso;
}

/** `GET /api/jobs/{job}/operation-calls/unknown` — every unknown outcome of the job, open ones first. */
export interface UnknownOutcome {
	call: OperationCall;
	case: CaseRow;
}

export interface OperationCallFilter {
	op?: string;
	case_id?: string;
	route?: RouteKind | '';
	outcome?: string;
	/** J22: only prod or only test (draft) calls. */
	env?: Env | '';
	/** J22: only calls on this system instance. */
	instance?: string;
	/** J22: only failed calls the agent answered (`reply`) or escalated (`escalate`). */
	handling?: CallHandling | '';
	before?: string | null;
}

/** `GET /api/jobs/{job}/operation-stats` — 30-day per-op aggregate. */
export interface OperationStat {
	op: string;
	route: RouteKind;
	instance: string | null;
	calls: number;
	errors: number;
	denied: number;
	p95_ms: number | null;
	/** J22: pinned template (`shopify_admin@2026-10.1`) of the instance. */
	template?: string | null;
	side_effect?: 'R' | 'W' | 'X' | 'P';
	/** J22: writes sent with no answer (subset of `errors`). */
	unknown?: number;
	/** J22: failed calls by canonical outcome code and how the agent handled each (sums to `errors`). */
	error_codes?: { code: string; count: number; handling: CallHandling }[];
}

// ---- J23 agents / J24 buses ---------------------------------------------------------------------------------------

export type AgentKind = 'front' | 'gate' | 'coordination' | 'worker' | 'runner';

export interface BusAgent {
	id: string;
	name: string;
	kind: AgentKind;
	/** Agent role it runs as (12 §2.1), audit `actor_id=<role>@<consumer>`. */
	role: string;
	group: string;
	consumers: number;
	pending: number;
	lag: number;
	streams: string[];
	status: 'ok' | 'warn' | 'idle';
	model_routes: string[];
	last_heartbeat: Iso | null;
	version: string;
	/** What changes in M4 (the tool-executor becomes the operation runner). */
	note?: string;
	/** J23: the running processes (one consumer each) of this agent in Production. */
	instances?: AgentInstance[];
	/** J23: messages acknowledged per minute now, and in the last hour. */
	per_min?: number;
	handled_1h?: number;
	/** J23: handler errors in the last hour (retried; dead-lettered after 5 deliveries). */
	errors_1h?: number;
	/** J23: process restarts in the last 24 h (all instances). */
	restarts_24h?: number;
	/** J23: age in seconds of the oldest delivered-but-unacknowledged entry; null = none pending. */
	oldest_pending_s?: number | null;
	/** J23 (D54): the config this agent serves in Production — the published release. */
	prod?: AgentServing;
	/** J23 (D54): its Test-environment copy, which serves the draft; null = no test copy running. */
	test?: AgentServing | null;
}

/** J23: one process of a bus agent (a consumer in its group). */
export interface AgentInstance {
	/** Consumer name (pod name). */
	name: string;
	state: 'running' | 'restarting' | 'stopped';
	/** Null when neither a heartbeat nor a recorded start names the process. */
	started_at: Iso | null;
	restarts: number;
	last_heartbeat: Iso | null;
}

/** J23 (D54): which config an agent copy is serving. Agents re-read every 30 s and restart 5 s later on a change. */
export interface AgentServing {
	/** Production: the published release it read; Test: null (the draft). */
	release: string | null;
	/** Test: the draft revision it read. Production: null — it serves a release (D63: no separate live revision). */
	rev: number | null;
	/** When it last re-read its config. */
	read_at: Iso | null;
	consumers: number;
	pending: number;
	lag: number;
}

/** `GET /api/jobs/{job}/agents/fleet` — J23: what Production and the Test environment should be serving. */
export interface AgentFleet {
	/** Server time the numbers were read at (heartbeat ages are relative to it). */
	as_of: Iso;
	/** What Production serves: a release (D62/D63: its stored snapshot; there is no separate live revision). */
	production: { release: ReleaseRef | null; released_at: Iso | null };
	/** The draft: its revision and unpublished changes (D54). */
	test: { rev: number; unpublished: number };
	/** Approved, not yet applied: agents switch to it after apply. */
	approved_waiting: string | null;
	/** Seconds after a publish (or draft save) until every agent serves it: re-read 30 s + restart 5 s. */
	follow_s: number;
}

export interface GroupInfo {
	name: string;
	consumers: number;
	pending: number;
	lag: number | null;
	last_delivered_id: string;
}

export interface StreamInfo {
	key: string;
	/** `op` = the operation streams `bus:op` / `bus:op:results` (B4 runtime, agent-api `bus_admin::bus_kind`). */
	bus: 'customer' | 'worker' | 'tool' | 'op' | 'dead' | 'other';
	area: string | null;
	length: number;
	last_entry_id: string | null;
	groups: GroupInfo[];
}

export interface Envelope {
	id: string;
	topic: string;
	from: string;
	correlation_id: string;
	published_at_ms: number;
	payload: JsonObject;
	tenant_id: TenantId;
	job: JobKey;
}

export interface Entry {
	stream: string;
	entry_id: string;
	envelope: Envelope;
}

export interface DeadLetter extends Entry {
	/** The environment whose `bus:dead` holds it (the list spans both). */
	env?: Env;
	/** Where it came from, why it died, how often it was delivered. */
	dead: {
		stream: string;
		entry_id: string;
		deliveries: number;
		/** `gave_up`, `scope_mismatch`, `decode_error`, `handler_panic`. */
		reason_code: string;
		reason: string;
	};
}

// ---- J24 message path ---------------------------------------------------------------------------------------------

/** One hop on a job's message path (03 §1.3, §2.8, §3): a channel, a gate, an agent, or a topic between them. */
export interface FlowHop {
	id: string;
	/** In = customer → case, work = the procedure runs, out = reply → customer. */
	phase: 'in' | 'work' | 'out';
	kind: 'channel' | 'gate' | 'agent' | 'topic';
	label: string;
	/** What happens at this hop, one line. */
	does: string;
	/** Stream keys carried by a topic hop (unscoped: the env prefix is on the flow). */
	streams: string[];
	/** Event names on those streams (`request.qualified`, `work.requested`, …). */
	events: string[];
	/** Consumer groups / processes doing the work at this hop. */
	agents: string[];
	/** Messages per minute through this hop, averaged over `BusFlow.window_min`. */
	rate_per_min: number;
	p95_ms: number | null;
	/** A round trip off the main path (worker ⇄ tool executor). */
	loop?: boolean;
	/** Messages that leave the main path here (deflected, handed over, failed closed). */
	exits?: { label: string; per_min: number; tone: 'ok' | 'warn' | 'err' }[];
	/** Job sub-path (`/gates`) or absolute route (`/admin/channels`) to the page that configures or shows this hop. */
	link?: { href: string; label: string };
	status: 'ok' | 'warn' | 'err' | 'idle';
}

/** One topic (stream) of the job with who writes it, who reads it, how deep it is and how long it keeps entries. */
export interface TopicInfo {
	key: string;
	bus: StreamInfo['bus'];
	events: string[];
	/** Process ids that XADD onto it. */
	producers: string[];
	groups: GroupInfo[];
	rate_per_min: number;
	length: number;
	/** `XADD MAXLEN ~` cap (03 §1.3: 100 000). */
	maxlen: number;
	/** Dead letters that came from this stream and are still in `bus:dead`. */
	dead: number;
	note?: string;
}

/** `GET /api/jobs/{job}/bus-flow?env=` — J24: the message path with live rates, and the topics behind it. */
export interface BusFlow {
	env: Env;
	/** Scoped key prefix every stream of this env lives under. */
	prefix: string;
	/** What the agents in this env read: the live release (prod) or the draft (test, D54). */
	reads: 'live' | 'draft';
	window_min: number;
	hops: FlowHop[];
	topics: TopicInfo[];
	dead_letters: number;
}

// ---- J19 caseload ---------------------------------------------------------------------------------------------------

/** `GET /api/jobs/{job}/caseload?env=` — J19: every case someone still holds (or that failed), live, not paged. */
export interface Caseload {
	/** Server time the caseload is measured at (SLA due / overdue are relative to it). */
	as_of: Iso;
	/** Open cases (agent, customer or staff holds them) and failed / alerted cases, newest first. */
	active: CaseRow[];
	/** Cases closed in the 7 days before `as_of`, per closing status (resolved, not eligible, offboard). */
	closed_7d: { resolved: number; not_eligible: number; cancelled_offboard: number };
	/** Active cases per environment, before the `env` filter. */
	env_counts: { prod: number; test: number };
	/** What Production and the Test env run now: production = the release, test = the draft (D54). */
	fleet: AgentFleet;
}

/** One hit of `GET /api/search?q=`: a case is only unique within its job, so the hit names the job. */
export interface CaseHit {
	job: string;
	job_name?: string;
	case_id: string;
	title: string;
	customer_name?: string | null;
	status: CaseStatus;
	/** The console page of the case. */
	href: string;
}
