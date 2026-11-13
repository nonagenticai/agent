// B12: one conversation's live feed (B14: or the job's), kept open. `conversationEvents` streams the SSE frames (over the bearer door,
// not EventSource); when the stream ends or drops it is reopened after a back-off (1 s doubling to 30 s), and a
// 403/404 stops it for good. `stop()` aborts the open stream and cancels any pending reconnect.
import type { Api } from '$lib/api/client';
import { ApiError } from '$lib/api/errors';
import type { ConversationFrame } from '$lib/api/types/conversations';

export type FeedState = 'connecting' | 'live' | 'reconnecting' | 'closed';

export interface WatchOptions {
	onFrame: (f: ConversationFrame) => void;
	onState?: (s: FeedState) => void;
	/** Back-off bounds in ms (tests shorten them). */
	minDelayMs?: number;
	maxDelayMs?: number;
}

export function watchConversation(
	events: Api['conversationEvents'],
	id: string,
	opts: WatchOptions,
): () => void {
	return watchFeed((o) => events(id, o), opts);
}

/** The job-wide feed (`GET /api/jobs/{job}/events`: every conversation's frames), kept open the same way. */
export function watchJob(events: Api['jobEvents'], opts: WatchOptions): () => void {
	return watchFeed(events, opts);
}

function watchFeed(events: Api['jobEvents'], opts: WatchOptions): () => void {
	const min = opts.minDelayMs ?? 1000;
	const max = opts.maxDelayMs ?? 30_000;
	let stopped = false;
	let ctrl: AbortController | null = null;
	let timer: ReturnType<typeof setTimeout> | null = null;
	let delay = min;
	const state = (s: FeedState) => opts.onState?.(s);

	const open = async () => {
		if (stopped) return;
		ctrl = new AbortController();
		// Live while the stream is open: a quiet conversation sends no frame, and the request cannot tell us more.
		state('live');
		try {
			await events({
				signal: ctrl.signal,
				onFrame: (f) => {
					delay = min;
					opts.onFrame(f);
				},
			});
		} catch (e) {
			if (stopped) return;
			if (e instanceof ApiError && (e.status === 403 || e.status === 404)) {
				stopped = true;
				state('closed');
				return;
			}
		}
		if (stopped) return;
		state('reconnecting');
		timer = setTimeout(open, delay);
		delay = Math.min(delay * 2, max);
	};
	state('connecting');
	void open();

	return () => {
		stopped = true;
		if (timer) clearTimeout(timer);
		ctrl?.abort();
		state('closed');
	};
}
