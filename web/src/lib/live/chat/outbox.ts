// The staff test chat's outbox: a port of cb:web/src/lib/console/outbox.ts (care-bus-agents origin/main) for the
// fixture seam. Pure functions over an immutable `Outbox`; the session (session.svelte.ts) drives them.
import type { ChatEvent } from '$lib/api/types/live';

/**
 * Messages typed while the agent is still answering. The composer never blocks: every message shows at once as a
 * pending bubble; while a turn is in flight it waits here, and after the agent replies everything waiting goes out
 * as ONE message (joined with "\n"). A 409 `conversation_busy` puts the batch back in the queue instead of losing
 * it. A definite rejection is `failed` (resend is safe). A lost response (no answer, or a gateway error) or a
 * reload mid-POST is `unknown`: the server may have stored the message, so it is never resent automatically; an
 * echo in the log settles it, or the staff member resends it deliberately or discards it.
 */
export type OutboxStatus = 'queued' | 'sending' | 'failed' | 'unknown';
export interface OutboxEntry {
	id: number;
	text: string;
	status: OutboxStatus;
	/** Batch the item was posted in; null while queued or failed. */
	batch: number | null;
	/** Customer messages already in the log when the batch was posted; its echo must come after them. */
	after: number;
	/** The id the 202 answered with: the echo carrying it settles the item. Set once the POST is accepted. */
	message_id?: string;
}
export interface Outbox {
	items: OutboxEntry[];
	/** The POST for the current batch has not answered yet. */
	posting: boolean;
	/** The POST landed (or a busy hold is waiting); the agent's reply has not arrived yet. */
	awaiting: boolean;
	error: string | null;
	nextId: number;
	nextBatch: number;
}
export interface Batch {
	id: number;
	text: string;
}

export const emptyOutbox = (): Outbox => ({
	items: [],
	posting: false,
	awaiting: false,
	error: null,
	nextId: 1,
	nextBatch: 1,
});

/** A turn is in flight: the POST is outstanding or its reply has not arrived. */
export const inFlight = (o: Outbox) => o.posting || o.awaiting;

/** The text the server stores for a batch (it trims the body). */
export const joinTexts = (texts: string[]) =>
	texts
		.map((t) => t.trim())
		.join('\n')
		.trim();

/** Adds a message to the queue; earlier failed messages go back in the queue ahead of it. Blank input changes nothing. */
export function submit(o: Outbox, text: string): Outbox {
	const t = text.trim();
	if (!t) return o;
	const r = retryFailed(o);
	return {
		...r,
		items: [...r.items, { id: r.nextId, text: t, status: 'queued', batch: null, after: 0 }],
		nextId: r.nextId + 1,
	};
}

/** Puts failed messages back in the queue (Retry pressed, or something new typed). */
export function retryFailed(o: Outbox): Outbox {
	if (!o.items.some((i) => i.status === 'failed')) return o;
	return {
		...o,
		error: null,
		items: o.items.map((i) => (i.status === 'failed' ? { ...i, status: 'queued' as const } : i)),
	};
}

/** Messages whose delivery cannot be established; the composer holds back new sends until they are settled. */
export const hasUnknown = (o: Outbox) => o.items.some((i) => i.status === 'unknown');

/** Resend an unknown message on purpose, accepting that it may arrive twice. */
export function resendUnknown(o: Outbox): Outbox {
	if (!hasUnknown(o)) return o;
	return {
		...o,
		error: null,
		items: o.items.map((i) =>
			i.status === 'unknown' ? { ...i, status: 'queued' as const, batch: null } : i,
		),
	};
}

/** Drops failed and unknown messages (discarded). Queued and sending ones are kept. */
export function discardUnsent(o: Outbox): Outbox {
	return {
		...o,
		error: null,
		items: o.items.filter((i) => i.status !== 'failed' && i.status !== 'unknown'),
	};
}

/**
 * When no turn is in flight, takes every queued message as one batch and marks it sending. Failed messages
 * wait for retryFailed(). `customers` is the number of customer messages already in the chat log.
 */
export function take(o: Outbox, customers: number): { o: Outbox; batch: Batch | null } {
	if (inFlight(o)) return { o, batch: null };
	const ready = o.items.filter((i) => i.status === 'queued');
	if (ready.length === 0) return { o, batch: null };
	const id = o.nextBatch;
	const items = o.items.map((i) =>
		i.status === 'queued' ? { ...i, status: 'sending' as const, batch: id, after: customers } : i,
	);
	return {
		o: { ...o, items, posting: true, awaiting: false, nextBatch: id + 1 },
		batch: { id, text: joinTexts(ready.map((i) => i.text)) },
	};
}

/** The POST for `batch` answered 202: the turn stays in flight until the echo and the agent's reply are seen. */
export function posted(o: Outbox, batch: number, messageId: string): Outbox {
	if (!o.items.some((i) => i.batch === batch) && !o.posting) return o;
	return {
		...o,
		posting: false,
		awaiting: true,
		error: null,
		items: o.items.map((i) => (i.batch === batch ? { ...i, message_id: messageId } : i)),
	};
}

/** The agent replied to the in-flight turn (or none is in flight): the next batch may go. */
export function replied(o: Outbox): Outbox {
	if (o.posting || !o.awaiting) return o;
	return { ...o, awaiting: false };
}

/**
 * The POST failed. `busy` (409 conversation_busy) puts the batch back in the queue and keeps waiting for the
 * reply that frees the conversation. Any other error marks the batch failed so it can be retried; when the
 * outcome is `ambiguous` (no response, or a gateway error) it is `unknown` instead and keeps its batch, so a
 * later echo in the log still settles it.
 */
export function failed(o: Outbox, batch: number, busy: boolean, message: string, ambiguous = false): Outbox {
	const items = o.items.map((i) =>
		i.batch === batch && i.status === 'sending'
			? busy
				? { ...i, status: 'queued' as const, batch: null }
				: ambiguous
					? { ...i, status: 'unknown' as const }
					: { ...i, status: 'failed' as const, batch: null }
			: i,
	);
	return busy
		? { ...o, items, posting: false, awaiting: true }
		: { ...o, items, posting: false, awaiting: false, error: message };
}

/** Stop waiting for a reply that may never come (the busy retry fallback); queued items can then go. */
export const release = (o: Outbox): Outbox => ({ ...o, posting: false, awaiting: false });

/**
 * Drops every sent batch whose echo (the customer message the server stored) is in the chat log, so the server's
 * message replaces the pending bubbles instead of appearing next to them. An accepted batch is matched by the
 * message id its 202 answered with; one without an id (unknown) by text and in order: it takes the first
 * unmatched customer message after its `after` mark.
 */
export function reconcile(o: Outbox, chat: ChatEvent[]): Outbox {
	const customer = chat.filter((m) => m.role === 'customer');
	const ids = new Set(customer.map((m) => m.message_id));
	const texts = customer.map((m) => m.text.trim());
	const batches = [...new Set(o.items.filter((i) => i.batch !== null).map((i) => i.batch!))].sort(
		(a, b) => a - b,
	);
	const done = new Set<number>();
	const used = new Set<number>(
		customer.flatMap((m, n) => (o.items.some((i) => i.message_id === m.message_id) ? [n] : [])),
	);
	let cursor = 0;
	for (const b of batches) {
		const its = o.items.filter((i) => i.batch === b);
		const mid = its.find((i) => i.message_id)?.message_id;
		if (mid) {
			if (ids.has(mid)) done.add(b);
			continue;
		}
		const text = joinTexts(its.map((i) => i.text));
		let at = texts.indexOf(text, Math.max(cursor, its[0]!.after));
		while (at >= 0 && used.has(at)) at = texts.indexOf(text, at + 1);
		if (at >= 0) {
			done.add(b);
			used.add(at);
			cursor = at + 1;
		}
	}
	if (done.size === 0) return o;
	return { ...o, items: o.items.filter((i) => i.batch === null || !done.has(i.batch)) };
}

/**
 * After a reload: a batch whose POST never answered is unknown (reconcile it against the log first; what
 * remains waits for a decision). One the server accepted keeps its message id, so its echo still settles it.
 */
export const requeueSending = (o: Outbox): Outbox => ({
	...o,
	items: o.items.map((i) =>
		i.status === 'sending' && !i.message_id ? { ...i, status: 'unknown' as const } : i,
	),
	posting: false,
	awaiting: false,
});

// ---- per-tab persistence (sessionStorage; every access guarded: storage may be blocked or full) ------------------

const KEY = (conversation: string) => `agent.chat.outbox.${conversation}`;
/** The conversation this tab resumes after a reload (`?conversation=` in the URL wins over it). */
export const CONVERSATION_KEY = 'agent.chat.conversation';

export type Store = Pick<Storage, 'getItem' | 'setItem' | 'removeItem'>;
export const storage = (): Store | null => {
	try {
		return typeof sessionStorage === 'undefined' ? null : sessionStorage;
	} catch {
		return null;
	}
};

export function saveOutbox(conversation: string, o: Outbox, s: Store | null = storage()) {
	try {
		if (o.items.length === 0) s?.removeItem(KEY(conversation));
		else s?.setItem(KEY(conversation), JSON.stringify({ items: o.items, nextId: o.nextId }));
	} catch {
		/* storage blocked or full: the outbox still works for this page */
	}
}

export function loadOutbox(conversation: string, s: Store | null = storage()): Outbox {
	try {
		const raw = s?.getItem(KEY(conversation));
		if (!raw) return emptyOutbox();
		const v = JSON.parse(raw) as { items?: unknown; nextId?: unknown };
		if (!Array.isArray(v.items)) return emptyOutbox();
		const items = v.items.filter(
			(i): i is OutboxEntry =>
				typeof i === 'object' &&
				i !== null &&
				typeof (i as OutboxEntry).id === 'number' &&
				typeof (i as OutboxEntry).text === 'string' &&
				['queued', 'sending', 'failed', 'unknown'].includes((i as OutboxEntry).status),
		);
		const maxBatch = Math.max(0, ...items.map((i) => i.batch ?? 0));
		const maxId = Math.max(0, ...items.map((i) => i.id));
		return {
			...emptyOutbox(),
			items,
			nextId: Math.max(Number(v.nextId) || 0, maxId + 1),
			nextBatch: maxBatch + 1,
		};
	} catch {
		return emptyOutbox();
	}
}

export function dropOutbox(conversation: string, s: Store | null = storage()) {
	try {
		s?.removeItem(KEY(conversation));
	} catch {
		/* ignore */
	}
}

export function readValue(key: string, s: Store | null = storage()): string | null {
	try {
		return s?.getItem(key) ?? null;
	} catch {
		return null;
	}
}
export function writeValue(key: string, value: string | null, s: Store | null = storage()) {
	try {
		if (value === null) s?.removeItem(key);
		else s?.setItem(key, value);
	} catch {
		/* ignore */
	}
}
