// The live feeds, with the status vocabulary of cb `EventFeed` (connecting → open; retrying after a failed pull, a
// dropped stream or a simulated network drop; stopped):
// - `createFeed`: the fixture world's stand-in, a timer loop that pulls the next events and hands them to the page.
// - `createSseFeed`: the real client's SSE stream (`GET /api/jobs/{job}/conversations/{id}/events` for the chat),
//   kept open: a stream that ends or drops is reopened after a back-off (doubling up to `maxRetryMs`), sending the
//   last SSE id it saw as `Last-Event-ID`; `onOpen` runs every time the stream (re)opens, so the page re-reads
//   the log and catches up on frames that carried no id. A fatal answer (403/404) stops it for good.
// Every timer is cleared and every open stream aborted by stop().

export type FeedStatus = 'connecting' | 'open' | 'retrying' | 'stopped';

export const FEED_LABEL: Record<FeedStatus, string> = {
	connecting: 'connecting…',
	open: 'live',
	retrying: 'reconnecting…',
	stopped: 'not connected',
};

export interface FeedOpts<E> {
	/** The next events, or a rejection (the feed then shows `retrying` and tries again). */
	pull: () => Promise<E[]>;
	onEvents: (events: E[]) => void;
	onStatus: (s: FeedStatus) => void;
	/** Runs after a reconnect, before events flow again (rebuild from the server log). */
	onReconnect?: () => Promise<void> | void;
	/** Time between pulls while open. */
	tickMs?: number;
	/** Time from start() to the first pull (the handshake). */
	connectMs?: number;
	/** Wait before reconnecting after a failure or a drop. */
	retryMs?: number;
}

export interface Feed {
	readonly status: FeedStatus;
	start(): void;
	stop(): void;
	/** Simulates a dropped connection: `retrying` for `retryMs`, then reconnect + onReconnect. */
	drop(): void;
}

export function createFeed<E>(o: FeedOpts<E>): Feed {
	const tickMs = o.tickMs ?? 900;
	const connectMs = o.connectMs ?? 400;
	const retryMs = o.retryMs ?? 2500;
	let status: FeedStatus = 'stopped';
	let timer: ReturnType<typeof setTimeout> | null = null;
	/** Bumped on stop/drop so a pull that resolves late is dropped. */
	let gen = 0;

	const set = (s: FeedStatus) => {
		if (status === s) return;
		status = s;
		o.onStatus(s);
	};
	const clear = () => {
		if (timer) clearTimeout(timer);
		timer = null;
	};
	const schedule = (ms: number, fn: () => void) => {
		clear();
		timer = setTimeout(() => {
			timer = null;
			fn();
		}, ms);
	};

	async function tick(g: number, reconnecting: boolean) {
		try {
			if (reconnecting) await o.onReconnect?.();
			const events = await o.pull();
			if (g !== gen) return;
			set('open');
			if (events.length) o.onEvents(events);
			schedule(tickMs, () => void tick(g, false));
		} catch {
			if (g !== gen) return;
			set('retrying');
			schedule(retryMs, () => void tick(g, true));
		}
	}

	return {
		get status() {
			return status;
		},
		start() {
			gen++;
			const g = gen;
			set('connecting');
			schedule(connectMs, () => void tick(g, false));
		},
		stop() {
			gen++;
			clear();
			set('stopped');
		},
		drop() {
			if (status === 'stopped') return;
			gen++;
			const g = gen;
			set('retrying');
			schedule(retryMs, () => void tick(g, true));
		},
	};
}

export interface SseFeedOpts<E> {
	/**
	 * Open the stream once: resolves when it ends, rejects when it fails. Call `onOpen` when the server has answered,
	 * `onEvent` per event (with its SSE id, '' when none). `lastEventId` is the last id seen (send it as
	 * `Last-Event-ID`), null before the first.
	 */
	connect: (o: {
		signal: AbortSignal;
		lastEventId: string | null;
		onOpen: () => void;
		onEvent: (e: E, id: string) => void;
	}) => Promise<void>;
	onEvents: (events: E[]) => void;
	onStatus: (s: FeedStatus) => void;
	/** Runs each time the stream (re)opens: re-read the server log for anything the stream did not replay. */
	onOpen?: () => Promise<void> | void;
	/** A failure that must not be retried (e.g. 404: the conversation is gone). */
	fatal?: (e: unknown) => boolean;
	/** First reconnect delay; doubles per failed attempt up to `maxRetryMs`. */
	retryMs?: number;
	maxRetryMs?: number;
}

export interface SseFeed extends Feed {
	/** The last SSE id seen (sent as `Last-Event-ID` on the next reconnect). */
	readonly lastEventId: string | null;
}

export function createSseFeed<E>(o: SseFeedOpts<E>): SseFeed {
	const min = o.retryMs ?? 1000;
	const max = o.maxRetryMs ?? 30_000;
	let status: FeedStatus = 'stopped';
	let timer: ReturnType<typeof setTimeout> | null = null;
	let ctrl: AbortController | null = null;
	let lastId: string | null = null;
	let delay = min;
	/** Bumped on stop/drop so a stream that ends late changes nothing. */
	let gen = 0;

	const set = (s: FeedStatus) => {
		if (status === s) return;
		status = s;
		o.onStatus(s);
	};
	const clear = () => {
		if (timer) clearTimeout(timer);
		timer = null;
	};
	const abort = () => {
		ctrl?.abort();
		ctrl = null;
	};
	const retry = (g: number) => {
		set('retrying');
		clear();
		timer = setTimeout(() => {
			timer = null;
			void run(g);
		}, delay);
		delay = Math.min(delay * 2, max);
	};

	async function run(g: number) {
		if (g !== gen) return;
		const c = new AbortController();
		ctrl = c;
		try {
			await o.connect({
				signal: c.signal,
				lastEventId: lastId,
				onOpen: () => {
					if (g !== gen) return;
					delay = min;
					set('open');
					void Promise.resolve(o.onOpen?.()).catch(() => {});
				},
				onEvent: (e, id) => {
					if (g !== gen) return;
					if (id) lastId = id;
					o.onEvents([e]);
				},
			});
		} catch (e) {
			if (g !== gen) return;
			if (o.fatal?.(e)) {
				gen++;
				ctrl = null;
				set('stopped');
				return;
			}
		}
		if (g !== gen) return;
		ctrl = null;
		retry(g);
	}

	return {
		get status() {
			return status;
		},
		get lastEventId() {
			return lastId;
		},
		start() {
			gen++;
			clear();
			abort();
			delay = min;
			set('connecting');
			void run(gen);
		},
		stop() {
			gen++;
			clear();
			abort();
			set('stopped');
		},
		drop() {
			if (status === 'stopped') return;
			gen++;
			clear();
			abort();
			retry(gen);
		},
	};
}
