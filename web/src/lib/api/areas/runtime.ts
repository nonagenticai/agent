// The `RuntimeApi` contract.
import type { Paged } from '$lib/api/types/core';
import type {
	AgentFleet,
	BusAgent,
	BusFlow,
	Env,
	Caseload,
	CaseDetail,
	CaseFilter,
	CaseHit,
	CaseRow,
	DeadLetter,
	Entry,
	JobAnalytics,
	JobOverview,
	OperationCall,
	OperationCallFilter,
	OperationStat,
	UnknownOutcome,
} from '$lib/api/types/runtime';

export type RuntimeApi = {
	/** `GET /api/jobs/{job}/overview` — J1: current release + status flags, 30-day KPIs, health, attention list. */
	jobOverview(): Promise<JobOverview>;
	/** `GET /api/jobs/{job}/analytics?days=7|30` — J2: daily series, escalation reasons, CSAT, north star. */
	jobAnalytics(days: 7 | 30): Promise<JobAnalytics>;
	/** `GET /api/jobs/{job}/cases?status=&env=&q=&before=` — J19, newest first. */
	cases(filter?: CaseFilter): Promise<Paged<CaseRow>>;
	/** `GET /api/search?q=` — the ⌘K palette's case hits across the tenant's jobs (newest first, at most 10);
	 * a query under 2 characters has none. */
	searchCases(q: string): Promise<CaseHit[]>;
	/** `GET /api/jobs/{job}/caseload?env=` — J19: every open or failed case (not paged), who holds it, closed counts
	 * for 7 days and what Production (release) and Test (draft) run. */
	caseload(filter?: { env?: CaseRow['env'] | 'all' }): Promise<Caseload>;
	/** `GET /api/jobs/{job}/cases/{id}` — J20: the case, its timeline and its raw envelope (ids + codes). */
	caseDetail(id: string): Promise<CaseDetail>;
	/** `POST /api/jobs/{job}/cases/{id}/handover` (support_agent) — routes `case.create{handover}` to agent_cases. */
	handOverCase(id: string, reason: string): Promise<CaseRow>;
	/** `GET /api/jobs/{job}/operation-calls?op=&case_id=&route=&outcome=&env=&instance=&handling=&before=` — J22,
	 * newest first. */
	operationCalls(filter?: OperationCallFilter): Promise<Paged<OperationCall>>;
	/** `GET /api/jobs/{job}/operation-stats` — 30-day calls, errors, denials and p95 per operation. */
	operationStats(): Promise<Paged<OperationStat>>;
	/** `GET /api/jobs/{job}/operation-calls/unknown` — J22: every `unknown_outcome` call, unchecked ones first. */
	unknownOutcomes(): Promise<Paged<UnknownOutcome>>;
	/**
	 * `POST /api/jobs/{job}/operation-calls/{call_id}/check` (operator) — a person records what the system shows
	 * for an unknown outcome (landed / did not land). Audited on the case timeline, clears the case alert; the call
	 * itself is never retried. The one route behind J20 (case page) and J22 (unknown-outcome list).
	 */
	checkUnknownOutcome(
		callId: string,
		verdict: 'landed' | 'not_landed',
		note: string,
	): Promise<UnknownOutcome>;
	/** `GET /api/jobs/{job}/agents` — J23: bus agents with consumers, pending, lag, last heartbeat. */
	busAgents(): Promise<Paged<BusAgent>>;
	/** `GET /api/jobs/{job}/agents/fleet` — J23: the release + revision Production and the Test env should serve. */
	agentFleet(): Promise<AgentFleet>;
	/** `GET /api/jobs/{job}/bus-flow?env=prod|test` — J24: the message path with live rates + the topics behind it. */
	busFlow(env: Env): Promise<BusFlow>;
	/** `GET /api/jobs/{job}/streams/{key}/entries?limit=&env=prod|test` — the newest entries of one stream of that
	 * environment's bus (agent-api defaults to prod, so the env is always sent). */
	streamEntries(key: string, env: Env, limit?: number): Promise<Paged<Entry>>;
	/** `GET /api/jobs/{job}/dead-letters` — J24 dead letters, newest first. */
	deadLetters(): Promise<Paged<DeadLetter>>;
	/** `POST /api/jobs/{job}/streams/{key}/groups/{group}/claim?env=prod|test` (operator) — take over entries idle
	 * ≥ 30 s on that environment's bus. */
	claimStuck(key: string, group: string, env: Env): Promise<{ claimed: number }>;
	/** `POST /api/jobs/{job}/dead-letters/{id}/replay` (operator) — refused for `scope_mismatch`; the replayed
	 * stream and its new entry id. */
	replayDeadLetter(entryId: string): Promise<{ stream: string; entry_id?: string }>;
	/** `DELETE /api/jobs/{job}/dead-letters/{id}` (operator). */
	discardDeadLetter(entryId: string): Promise<{ discarded: string }>;
};
