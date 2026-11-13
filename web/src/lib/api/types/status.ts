// Shapes for the one agent status and the one inbox (console redesign T6/T7). Field names mirror the Rust JSON of
// `crates/agent-api/src/readmodel/agent_status.rs` and `routes/inbox.rs` exactly (snake_case).
import type { Iso } from './core';
import type { AttentionItem, AttentionSeverity } from './tenant';

/** The one state an agent shows everywhere (`agent_status::state_of`). */
export type AgentState = 'draft' | 'ready' | 'live' | 'live_with_issues' | 'paused';

export interface LiveRelease {
	/** The raw `cfg-…` id: shown only with "Show technical details". */
	id: string;
	/** The human date of the release ("23 Oct release"). */
	label: string;
	since: Iso;
}

/** Open cases of the job: prod cases by holder, and staff test cases apart. */
export interface OpenCases {
	/** The agent is answering (`open`). */
	agent: number;
	/** Handed over to a person (`handover`). */
	staff: number;
	/** Waiting on the customer (identity or a confirmation). */
	waiting: number;
	/** Stopped on an error (an alert is set). */
	failed: number;
	staff_tests: number;
}

export interface NextRelease {
	draft_rev: number;
	blockers: number;
	changes: number;
}

export type TestsState = 'no_questions' | 'not_run' | 'running' | 'passing' | 'failing';

/** The test suite (the validation questions) against the live release, else the newest one. */
export interface TestSuiteStatus {
	state: TestsState;
	release_id: string | null;
	questions: number;
	passed: number;
	failed: number;
	pending: number;
	last_run_at: string | null;
}

/** `GET /api/jobs/{job}/status`. */
export interface AgentStatus {
	state: AgentState;
	live_release: LiveRelease | null;
	on_bus: boolean;
	/** Inbox items with severity `blocking` for this job. */
	urgent: number;
	/** Inbox items with severity `action`. */
	review: number;
	open_cases: OpenCases;
	/** Share of cases in the last 30 days the agent closed alone; null when there were none. */
	resolved_by_agent_30d: number | null;
	/** Present when the draft differs from the live release or has blockers. */
	next_release: NextRelease | null;
	tests: TestSuiteStatus;
}

/** `GET /api/inbox[?job=]`: ranked blocking, action, info; then due time; then newest. */
export interface Inbox {
	items: AttentionItem[];
	counts: Record<AttentionSeverity, number>;
}
