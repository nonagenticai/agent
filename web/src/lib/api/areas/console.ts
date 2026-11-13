// L1 operator console (`/console`): the job's conversations as the operator watches them (ownership lives on
// `/support`). The client reads B12's console rows and the open conversation's transcript (src/lib/api/http/b12.ts,
// mapped by src/lib/live/console/L1-http.ts). Live frames come from `jobEvents` / `conversationEvents` (areas/conversations.ts).
import type { ConsoleConversation, ConsoleSnapshot } from '$lib/api/types/live';

export interface OperatorConsoleOptions {
	/** The conversation the operator has open: its whole transcript is read, the others carry their last line. */
	open?: string | null;
}

export type ConsoleApi = {
	/**
	 * `GET /api/jobs/{job}/console/conversations` + `GET /api/jobs/{job}/conversations/{id}` (the open one) — the
	 * job's conversations as the operator console sees them now: case row, the turn events so far (ops as ids +
	 * codes, K50), prod (live release) vs test (draft, D54), and who is in control.
	 */
	operatorConsole(opts?: OperatorConsoleOptions): Promise<ConsoleSnapshot>;
};
