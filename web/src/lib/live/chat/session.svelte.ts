// One staff test conversation (a port of cb:web/src/lib/chat/session.svelte.ts, care-bus-agents origin/main, for
// the fixture seam): start as a demo customer in an env — or resume the one named by `?conversation=` / stored
// for this tab — post messages through the outbox (outbox.ts: one turn in flight, a 409 busy keeps the text
// queued and retries, a lost response locks the composer until it is resent or discarded), and follow the reply
// on a feed. Unsent messages are kept per conversation in sessionStorage, so a reload resumes them. The feed is
// the fake replay loop (`createFeed`) over `chatConversation` for the fixture world; for a real caller (`sse`) it
// is the SSE stream on `GET /api/jobs/{job}/conversations/{id}/events` (`conversationEvents`, `createSseFeed`):
// each `message` frame is a `ChatEvent`, a reconnect sends `Last-Event-ID`, and every (re)open re-reads the log
// (`chatConversation`) to catch up on what the stream did not replay. The class's surface is the same for both.
import type { Api } from '$lib/api/client';
import { ApiError } from '$lib/api/errors';
import type { ChatEnv, ChatEvent, Conversation, ConversationStart, CsatRequest } from '$lib/api/types/live';
import { chatEventOf } from '$lib/api/http/b4';
import { createFeed, createSseFeed, type Feed, type FeedOpts, type FeedStatus } from '../feed';
import type { Pending } from './looks';
import {
	CONVERSATION_KEY,
	type Batch,
	type Outbox,
	type Store,
	discardUnsent,
	dropOutbox,
	emptyOutbox,
	failed,
	hasUnknown,
	inFlight,
	loadOutbox,
	posted,
	readValue,
	reconcile,
	release,
	replied,
	requeueSending,
	resendUnknown,
	retryFailed,
	saveOutbox,
	storage,
	submit,
	take,
	writeValue,
} from './outbox';

/** `conversationEvents` is needed only with `sse` (the replay loop reads `chatConversation`). */
export type ChatApi = Pick<Api, 'startChat' | 'chatConversation' | 'sendChatMessage'> &
	Partial<Pick<Api, 'conversationEvents'>>;

export interface ChatOpts {
	api: () => ChatApi;
	/** Feed timings (tests shorten them). */
	timing?: Pick<FeedOpts<ChatEvent>, 'tickMs' | 'connectMs' | 'retryMs'>;
	/** Swap the feed (tests); default is the replay loop, or the SSE stream when `sse()` holds. */
	makeFeed?: (o: FeedOpts<ChatEvent>) => Feed;
	/** Follow the conversation on its SSE stream (a real caller) instead of the replay loop (the fixture world). */
	sse?: () => boolean;
	/** Where the outbox and the resumable conversation id live; default sessionStorage (null = keep nothing). */
	storage?: Store | null;
	/** `?conversation=` in the URL, which wins over the stored id. */
	readUrl?: () => string | null;
	writeUrl?: (id: string | null) => void;
	/** After a 409 busy, retry this long after it unless an agent reply frees the conversation sooner. */
	busyRetryMs?: number;
}

/** What the looks draw for an unsent message (`queued` there = 202 accepted, reply on its way). */
export type OutboxItem = Pending & { error?: string };

export class LiveChat {
	conversation = $state<ConversationStart | null>(null);
	messages = $state<ChatEvent[]>([]);
	caseIds = $state<string[]>([]);
	/**
	 * F-13 (B4 T16b): the conversation's latest rating request (`csat_request` of the log), re-read on every pull
	 * and on the SSE `csat` frame; the page asks for a rating while it is unanswered and open.
	 */
	csat = $state<CsatRequest | null>(null);
	feedStatus = $state<FeedStatus>('stopped');
	/** Starting or resuming the conversation failed (the page offers "Start again"). */
	error = $state('');
	/** A stored/linked conversation that could not be loaded (not a 404): kept, so it is not silently replaced. */
	unreachable = $state<string | null>(null);
	starting = $state(false);
	#box = $state<Outbox>(emptyOutbox());

	private feed: Feed | null = null;
	private seen = new Set<string>();
	private gen = 0;
	/** While a conversation is being opened, nothing is posted before the stored batches are reconciled. */
	private resuming = false;
	/** After a 409, wait for an agent reply (or the fallback timer) before trying again. */
	private busyHold = false;
	private busyTimer: ReturnType<typeof setTimeout> | null = null;
	private readonly store: Store | null;

	constructor(private readonly o: ChatOpts) {
		this.store = o.storage === undefined ? storage() : o.storage;
	}

	/** The raw outbox. Every change is stored for the open conversation, so a reload keeps unsent messages. */
	get box(): Outbox {
		return this.#box;
	}
	private set box(v: Outbox) {
		this.#box = v;
		const id = this.conversation?.conversation_id;
		if (id) saveOutbox(id, v, this.store);
	}

	/** The pending bubbles: waiting or posting = `sending`, accepted (202) = `queued`, then `unknown` / `failed`. */
	get outbox(): OutboxItem[] {
		const err = this.#box.error ?? undefined;
		return this.#box.items.map((i) => ({
			id: `local-${i.id}`,
			text: i.text,
			status:
				i.status === 'queued' ? 'sending' : i.status === 'sending' && i.message_id ? 'queued' : i.status,
			...(i.status === 'failed' || i.status === 'unknown' ? { error: err } : {}),
		}));
	}
	/** A message is out (or waiting) and its reply has not arrived: the shells show the agent typing. */
	get awaiting(): boolean {
		if (inFlight(this.#box) || this.#box.items.some((i) => i.status === 'queued' || i.status === 'sending'))
			return true;
		return this.lastSpeaker() === 'customer';
	}
	/** A send whose outcome is unknown: new messages wait until it is resent or discarded. */
	get unknown(): boolean {
		return hasUnknown(this.#box);
	}
	/** A send the server rejected outright (resending is safe). */
	get failedSend(): boolean {
		return !this.unknown && this.#box.items.some((i) => i.status === 'failed');
	}
	/** Why the last send failed or is unknown. */
	get sendError(): string {
		return this.#box.error ?? '';
	}
	/** The last agent reply was the gate-2 refusal. */
	get refused(): boolean {
		return this.messages.filter((m) => m.role === 'agent').at(-1)?.refused === true;
	}
	get ended(): boolean {
		return this.messages.at(-1)?.text.startsWith('Conversation ended') === true;
	}

	private lastSpeaker(): ChatEvent['role'] | undefined {
		return this.messages.filter((m) => m.role !== 'system').at(-1)?.role;
	}

	/** Starts a new conversation (env and test customer are stamped at intake, so changing either starts over). */
	async start(env: ChatEnv, customerEmail: string): Promise<boolean> {
		const g = this.reset();
		this.starting = true;
		try {
			const c = await this.o.api().startChat({ env, customer_email: customerEmail });
			if (g !== this.gen) return false;
			// A new conversation has nothing unsent; a stale entry under a reused id (fixture world reset) is dropped.
			dropOutbox(c.conversation_id, this.store);
			await this.open(c, g, []);
			return g === this.gen;
		} catch (e) {
			if (g === this.gen) this.error = (e as Error).message;
			return false;
		} finally {
			if (g === this.gen) this.starting = false;
		}
	}

	/**
	 * Resumes the conversation named by `?conversation=` or (when `useStored`) the one this tab last had, with its
	 * stored outbox. Null when there is none, it no longer exists (404: forgotten), or it could not be loaded
	 * (then `unreachable` and `error` are set and the id is kept).
	 */
	async resume(useStored = true): Promise<Conversation | null> {
		const id = this.o.readUrl?.() ?? (useStored ? readValue(CONVERSATION_KEY, this.store) : null);
		if (!id) return null;
		const g = this.reset();
		this.starting = true;
		try {
			const conv = await this.o.api().chatConversation(id);
			if (g !== this.gen) return null;
			this.csat = conv.csat_request ?? null;
			const start: ConversationStart = {
				conversation_id: conv.conversation_id,
				customer_id: conv.customer_id,
				customer_name: null,
				env: conv.env,
				channel: 'staff',
				started_at: new Date(conv.messages[0]?.at_ms ?? Date.now()).toISOString(),
			};
			await this.open(start, g, conv.messages);
			return g === this.gen ? conv : null;
		} catch (e) {
			if (g !== this.gen) return null;
			if (e instanceof ApiError && e.status === 404) {
				dropOutbox(id, this.store);
				writeValue(CONVERSATION_KEY, null, this.store);
				this.o.writeUrl?.(null);
				return null;
			}
			this.unreachable = id;
			this.error = (e as Error).message;
			return null;
		} finally {
			if (g === this.gen) this.starting = false;
		}
	}

	/**
	 * Lets go of the open conversation locally, without a request: nothing is posted, the stored id and
	 * `?conversation=` are cleared (as a forgotten 404), and a start or resume in flight is cancelled. The page calls
	 * it when the env or customer changes, so the next send starts a fresh conversation there.
	 */
	forget() {
		this.reset();
		this.starting = false;
		this.feedStatus = 'stopped';
		writeValue(CONVERSATION_KEY, null, this.store);
		this.o.writeUrl?.(null);
	}

	/** Bumps the generation and clears everything the previous conversation held (its stored outbox stays). */
	private reset(): number {
		const g = ++this.gen;
		this.feed?.stop();
		this.feed = null;
		this.clearBusy();
		this.busyHold = false;
		this.conversation = null;
		this.messages = [];
		this.caseIds = [];
		this.csat = null;
		this.#box = emptyOutbox();
		this.seen.clear();
		this.error = '';
		this.unreachable = null;
		return g;
	}

	private async open(c: ConversationStart, g: number, log: ChatEvent[]) {
		const id = c.conversation_id;
		writeValue(CONVERSATION_KEY, id, this.store);
		this.o.writeUrl?.(id);
		this.resuming = true;
		this.conversation = c;
		this.#box = loadOutbox(id, this.store);
		this.add(log);
		const opts: FeedOpts<ChatEvent> = {
			pull: async () => {
				const conv = await this.o.api().chatConversation(id);
				if (g === this.gen) this.csat = conv.csat_request ?? null;
				return conv.messages.filter((m) => !this.seen.has(m.message_id));
			},
			onEvents: (events) => {
				if (g !== this.gen) return;
				this.add(events);
				this.pump();
			},
			onStatus: (s) => {
				if (g === this.gen) this.feedStatus = s;
			},
			tickMs: 700,
			connectMs: 300,
			retryMs: 2200,
			...this.o.timing,
		};
		this.feed = this.o.makeFeed
			? this.o.makeFeed(opts)
			: this.o.sse?.()
				? this.sseFeed(id, opts)
				: createFeed(opts);
		this.feed.start();
		// A batch stored mid-POST either reached the log (reconcile drops it) or waits as unknown.
		this.box = requeueSending(reconcile(this.#box, this.messages));
		this.resuming = false;
		this.pump();
	}

	/** The conversation's SSE stream; every (re)open re-reads the log so nothing between streams is lost. */
	private sseFeed(id: string, opts: FeedOpts<ChatEvent>): Feed {
		return createSseFeed<ChatEvent>({
			connect: ({ signal, lastEventId, onOpen, onEvent }) => {
				const a = this.o.api();
				if (!a.conversationEvents) return Promise.reject(new Error('This client has no conversation feed.'));
				return a.conversationEvents(id, {
					signal,
					lastEventId,
					onOpen,
					onFrame: (f) => {
						// F-13: a rating was asked for (or recorded): re-read the log for the request
						if (f.event === 'csat') void this.pullCsat(id);
						const e = chatEventOf(f, id);
						if (e) onEvent(e, f.id ?? '');
					},
				});
			},
			onEvents: opts.onEvents,
			onStatus: opts.onStatus,
			onOpen: async () => {
				const fresh = await opts.pull();
				if (fresh.length) opts.onEvents(fresh);
			},
			fatal: (e) => e instanceof ApiError && (e.status === 403 || e.status === 404),
			retryMs: opts.retryMs,
		});
	}

	/** Re-reads the conversation's rating request (after an SSE `csat` frame). */
	private async pullCsat(id: string) {
		const g = this.gen;
		try {
			const conv = await this.o.api().chatConversation(id);
			if (g === this.gen) this.csat = conv.csat_request ?? null;
		} catch {
			/* the next pull catches up */
		}
	}

	/** The rating the page should ask for: a request that is unanswered and still open, else null. */
	get csatAsk(): CsatRequest | null {
		const r = this.csat;
		return r && r.rating === null && Date.parse(r.expires_at) > Date.now() ? r : null;
	}

	private add(events: ChatEvent[]) {
		for (const e of events) {
			if (this.seen.has(e.message_id)) {
				// The transcript may deliver a reply before its SSE frame; only the frame carries the trace.
				if (e.trace) {
					const i = this.messages.findIndex((m) => m.message_id === e.message_id);
					if (i >= 0 && !this.messages[i]!.trace) this.messages[i] = { ...this.messages[i]!, trace: e.trace };
				}
				continue;
			}
			this.seen.add(e.message_id);
			this.messages.push(e);
			if (e.case_id && !this.caseIds.includes(e.case_id)) this.caseIds.push(e.case_id);
			// An agent reply frees a conversation that answered 409 busy.
			if (e.role === 'agent' && this.busyHold) {
				this.busyHold = false;
				this.clearBusy();
				this.#box = release(this.#box);
			}
		}
	}

	/**
	 * Moves the outbox forward: drop batches the server has echoed, notice the agent's reply, and post the next
	 * batch when no turn is in flight. Called after every log change and every POST outcome.
	 */
	private pump() {
		if (!this.conversation || this.resuming) return;
		let box = reconcile(this.#box, this.messages);
		const echoed = !box.items.some((i) => i.message_id);
		if (!this.busyHold && echoed && this.lastSpeaker() !== 'customer') box = replied(box);
		const next = take(box, this.messages.filter((m) => m.role === 'customer').length);
		this.box = next.o;
		if (next.batch) void this.post(next.batch);
	}

	private async post(batch: Batch) {
		const g = this.gen;
		const id = this.conversation?.conversation_id;
		if (!id) return;
		try {
			const r = await this.o.api().sendChatMessage(id, batch.text);
			if (g !== this.gen) return;
			this.box = posted(this.#box, batch.id, r.message_id);
		} catch (e) {
			if (g !== this.gen) return;
			const busy = e instanceof ApiError && e.status === 409 && e.code === 'conversation_busy';
			// No response (status 0, a network error) or a gateway/server error: the message may have been stored.
			const ambiguous = !(e instanceof ApiError) || e.status === 0 || e.status >= 500;
			this.box = failed(this.#box, batch.id, busy, (e as Error).message, ambiguous);
			if (busy) {
				// Another turn holds the conversation: keep the text queued, retry after its reply (or the timer).
				this.busyHold = true;
				this.clearBusy();
				this.busyTimer = setTimeout(() => {
					this.busyTimer = null;
					if (g !== this.gen) return;
					this.busyHold = false;
					this.box = release(this.#box);
					this.pump();
				}, this.o.busyRetryMs ?? 3000);
			}
		}
		this.pump();
	}

	private clearBusy() {
		if (this.busyTimer) clearTimeout(this.busyTimer);
		this.busyTimer = null;
	}

	/**
	 * Never blocks: the message shows at once and goes out now, or after the agent's current reply. False when
	 * nothing was taken (blank, no conversation, ended, or an unknown send waits to be settled), so the composer
	 * keeps its text.
	 */
	send(text: string): boolean {
		if (!text.trim() || !this.conversation || this.ended || this.unknown) return false;
		this.box = submit(this.#box, text);
		this.pump();
		return true;
	}
	/** Puts rejected messages back in the queue. */
	retrySend() {
		this.box = retryFailed(this.#box);
		this.pump();
	}
	/** Sends an unknown message again, accepting it may arrive twice. */
	resendAnyway() {
		this.box = resendUnknown(this.#box);
		this.pump();
	}
	/** Drops failed and unknown messages; the composer is free again. */
	discard() {
		this.box = discardUnsent(this.#box);
		this.pump();
	}
	/** Simulates a dropped live connection: `reconnecting…`, then the log is re-read and replies catch up. */
	dropFeed() {
		this.feed?.drop();
	}
	destroy() {
		this.gen++;
		this.feed?.stop();
		this.feed = null;
		this.clearBusy();
	}
}
