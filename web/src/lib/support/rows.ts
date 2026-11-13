import type { MessageView } from '$lib/api/types/support';

/** A transcript row. `local` marks an optimistic row the server has not confirmed yet (or that failed to send). */
export type Row = MessageView & { local?: 'sending' | 'failed'; reason?: string; noRetry?: boolean };
