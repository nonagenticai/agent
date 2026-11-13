// The `StatusApi` contract.
import type { JobKey } from '$lib/api/types/core';
import type { AgentStatus, Inbox } from '$lib/api/types/status';

export type StatusApi = {
	/** `GET /api/jobs/{job}/status` — the one status of a job (the active one unless `job` is given): state, live
	 * release, urgent and review counts, open cases, resolved-by-agent share and the next release's blockers. */
	agentStatus(job?: JobKey): Promise<AgentStatus>;
	/** `GET /api/inbox` — everything that needs a person, ranked blocking, action, info then by due time; with
	 * `job` only that agent's items (`?job=`). */
	inbox(job?: JobKey): Promise<Inbox>;
};
