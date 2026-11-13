// Where a conversation is worked: the staff workspace, one URL per conversation (SW-8). Every older surface links here.
export const workspaceHref = (job: string, conversationId: string): string =>
	`/support/${encodeURIComponent(job)}/c/${encodeURIComponent(conversationId)}`;

/** The workspace inbox, when the surface cannot name a conversation. */
export const WORKSPACE_HOME = '/support';
