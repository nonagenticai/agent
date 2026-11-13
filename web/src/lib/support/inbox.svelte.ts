// The staff workspace inbox store (SW T10): the rows of one tab, the live count badges, filters, the frames of the
// ONE tenant stream applied in place (`applyFrame`), and the 10 s poll that stands in while the stream is down.
// Order: an inbound customer message on a case that waited on the customer puts it on top (spec: "moves a waiting
// case back to the owner's top of inbox"); the rest keep the server order — sooner SLA due first, then waited longer.
import { api } from '$lib/app.svelte';
import { newerOwner } from '$lib/support/owner';
import type {
	InboxCounts,
	InboxQuery,
	InboxRow,
	InboxTab,
	MessageView,
	OwnerView,
} from '$lib/api/types/support';

export type SlaState = 'breached' | 'at_risk' | null;
/** Remaining share of the window under which a case is at risk. */
export const AT_RISK = 0.25;

const ms = (v: string | number | null | undefined): number | null => {
	if (v == null || v === '') return null;
	const n = typeof v === 'number' ? v : Date.parse(v);
	return Number.isFinite(n) ? n : null;
};

/** SW-12, computed client-side: `breached` past due, `at_risk` with under 25 % of `due − queuedAt` left, else null. */
export function slaState(
	due: string | number | null | undefined,
	queuedAt: string | number | null | undefined,
	now: number,
): SlaState {
	const d = ms(due);
	if (d == null) return null;
	if (now > d) return 'breached';
	const q = ms(queuedAt);
	if (q == null || d <= q) return null;
	return (d - now) / (d - q) < AT_RISK ? 'at_risk' : null;
}

export type SlaFields = Pick<
	InboxRow,
	'first_response_due' | 'first_response_at' | 'resolve_due' | 'sla_starts_at'
>;

/** The due time that counts now: `first_response_due` until the first response, then `resolve_due`. */
export const dueOf = (r: Omit<SlaFields, 'sla_starts_at'>): string | null =>
	r.first_response_at ? r.resolve_due : r.first_response_due;

/** The one SLA rule the inbox row and the context pane share, so their badges always agree. */
export const slaOf = (r: SlaFields, now: number): SlaState => slaState(dueOf(r), r.sla_starts_at, now);

/** A 401/403: the caller has no support role in this tenant. Not retried, not polled. */
export const isDenied = (e: unknown): boolean => {
	const s = (e as { status?: number } | null)?.status;
	return s === 401 || s === 403;
};

export const rowKey = (job: string, conversationId: string) => `${job}|${conversationId}`;

export type Filters = Pick<InboxQuery, 'reason' | 'channel' | 'priority'>;
type Frame = { event: string; data: unknown };

/** Whether `r` belongs in `tab` for the viewer `me` (mirrors the server's tab rules). */
export function inTab(r: InboxRow, tab: InboxTab, me: string | null): boolean {
	const o = r.owner.owner;
	if (tab === 'mine') return me != null && o === `user:${me}`;
	if (tab === 'unassigned') return o === 'queue';
	if (tab === 'open') return o !== 'agent';
	// Resolved (7 d): back with the agent. The server decides which; a live row only stays or leaves.
	return o === 'agent';
}
const matches = (r: InboxRow, f: Filters) =>
	(!f.reason || r.reason_label === f.reason) &&
	(!f.channel || r.channel === f.channel) &&
	(!f.priority || r.priority === f.priority);

function serverOrder(a: InboxRow, b: InboxRow): number {
	const x = ms(dueOf(a)) ?? Number.POSITIVE_INFINITY;
	const y = ms(dueOf(b)) ?? Number.POSITIVE_INFINITY;
	if (x !== y) return x < y ? -1 : 1;
	return a.waiting_since.localeCompare(b.waiting_since);
}

export interface InboxOptions {
	/** The viewer's sub (tab membership of a live row). */
	me: () => string | null;
	/** How long frames settle before the counts and order are re-read from the server. */
	reconcileMs?: number;
}

export function createInbox(opts: InboxOptions) {
	const reconcileMs = opts.reconcileMs ?? 1500;
	let rows = $state<InboxRow[]>([]);
	let counts = $state<InboxCounts>({ mine: 0, unassigned: 0, open: 0 });
	let tab = $state<InboxTab>('mine');
	let filters = $state<Filters>({});
	let loading = $state(false);
	let error = $state<string | null>(null);
	let loaded = $state(false);
	let denied = $state(false);
	/** A tab asked for whose rows have not arrived yet: a later reload (poll, reconcile) loads it, not the old one. */
	let pendingTab: InboxTab | null = null;
	let onTick: (() => void) | null = null;
	/** Conversations an inbound message put on top, newest first (cleared when opened). */
	let bumped = $state<Record<string, number>>({});
	/** Every row seen (any tab), so a frame can move counts by what the row was before. */
	const known = new Map<string, InboxRow>();
	let poll: ReturnType<typeof setInterval> | null = null;
	let settle: ReturnType<typeof setTimeout> | null = null;
	let seq = 0;

	const order = (list: InboxRow[]) =>
		[...list].sort((a, b) => {
			const x = bumped[rowKey(a.job, a.conversation_id)] ?? 0;
			const y = bumped[rowKey(b.job, b.conversation_id)] ?? 0;
			if (x !== y) return y - x;
			return serverOrder(a, b);
		});

	async function load(next?: InboxTab): Promise<void> {
		if (denied) return;
		if (next) pendingTab = next;
		const want = pendingTab ?? tab;
		const mySeq = ++seq;
		loading = true;
		try {
			const r = await api().supportInbox({ tab: want, ...filters });
			if (mySeq !== seq) return;
			// The tab switches with its rows, never before them.
			tab = want;
			if (pendingTab === want) pendingTab = null;
			for (const x of r.rows) known.set(rowKey(x.job, x.conversation_id), x);
			rows = order(r.rows);
			counts = r.counts;
			error = null;
			loaded = true;
		} catch (e) {
			if (isDenied(e)) {
				denied = true;
				stopPolling();
			}
			if (mySeq === seq) error = e instanceof Error ? e.message : 'Could not load the inbox.';
		} finally {
			if (mySeq === seq) loading = false;
		}
	}

	/** First load: Mine when it has rows, else Unassigned. */
	async function start(): Promise<void> {
		await load('mine');
		// Not when the viewer already picked a tab meanwhile.
		if (!denied && pendingTab === null && tab === 'mine' && counts.mine === 0 && rows.length === 0)
			await load('unassigned');
	}

	function bucketDelta(prev: InboxRow | undefined, next: InboxRow | undefined) {
		const me = opts.me();
		const c = { ...counts };
		for (const b of ['mine', 'unassigned', 'open'] as const) {
			const was = prev ? inTab(prev, b, me) : false;
			const is = next ? inTab(next, b, me) : false;
			if (was !== is) c[b] = Math.max(0, c[b] + (is ? 1 : -1));
		}
		counts = c;
	}

	function put(next: InboxRow) {
		const key = rowKey(next.job, next.conversation_id);
		const prev = known.get(key);
		known.set(key, next);
		bucketDelta(prev, next);
		const others = rows.filter((r) => rowKey(r.job, r.conversation_id) !== key);
		rows = order(inTab(next, tab, opts.me()) && matches(next, filters) ? [...others, next] : others);
	}

	function scheduleReconcile() {
		if (settle) clearTimeout(settle);
		settle = setTimeout(() => {
			settle = null;
			void load();
		}, reconcileMs);
	}

	/** One frame of the tenant stream. Frames name `job` and `conversation_id`; anything else is ignored. */
	function applyFrame(f: Frame): void {
		const d = f.data as Record<string, unknown> | null;
		if (!d || typeof d.conversation_id !== 'string') return;
		const job = typeof d.job === 'string' ? d.job : null;
		if (f.event === 'inbox_row') {
			put(d as unknown as InboxRow);
			return;
		}
		const key = job ? rowKey(job, d.conversation_id) : null;
		const cur =
			(key && known.get(key)) ||
			[...known.values()].find((r) => r.conversation_id === d.conversation_id && (!job || r.job === job));
		if (f.event === 'owner') {
			const { conversation_id: _c, job: _j, ...owner } = d;
			if (cur) {
				const next = newerOwner(cur.owner, owner as unknown as OwnerView);
				if (next !== cur.owner) put({ ...cur, owner: next, item_id: next.item_id ?? null });
			}
			scheduleReconcile();
		} else if (f.event === 'message') {
			if (!cur) {
				scheduleReconcile();
				return;
			}
			const m = d as unknown as MessageView;
			const inbound = m.kind === 'customer' || m.kind === 'caller';
			const next: InboxRow = {
				...cur,
				last_message: { kind: m.kind, text: m.text.slice(0, 140), at: m.at },
				unread: inbound ? true : cur.unread,
				// A customer answering a case that waited on them: back to the owner (reopened).
				state: inbound && cur.state === 'waiting_customer' ? 'assigned' : cur.state,
			};
			if (inbound && cur.state === 'waiting_customer')
				bumped = { ...bumped, [rowKey(cur.job, cur.conversation_id)]: Date.now() };
			put(next);
		} else if (f.event === 'handover') {
			if (cur && typeof d.state === 'string') put({ ...cur, state: d.state as InboxRow['state'] });
			scheduleReconcile();
		}
	}

	/** The viewer opened it: no longer unread or bumped. */
	function seen(job: string, conversationId: string) {
		const key = rowKey(job, conversationId);
		if (key in bumped) {
			const { [key]: _drop, ...rest } = bumped;
			bumped = rest;
		}
		const cur = known.get(key);
		if (cur?.unread) put({ ...cur, unread: false });
	}

	/** Re-read the inbox every `every` ms; `tick` also runs on each poll (the open conversation resyncs). */
	function startPolling(every = 10_000, tick?: () => void) {
		onTick = tick ?? null;
		if (poll || denied) return;
		poll = setInterval(() => {
			void load();
			onTick?.();
		}, every);
	}
	function stopPolling() {
		if (poll) clearInterval(poll);
		poll = null;
		onTick = null;
	}
	function stop() {
		stopPolling();
		if (settle) clearTimeout(settle);
		settle = null;
		seq++;
	}

	return {
		get rows() {
			return rows;
		},
		get counts() {
			return counts;
		},
		get tab() {
			return tab;
		},
		get filters() {
			return filters;
		},
		get loading() {
			return loading;
		},
		get loaded() {
			return loaded;
		},
		/** The caller may not use the workspace (401/403). */
		get denied() {
			return denied;
		},
		get error() {
			return error;
		},
		get polling() {
			return poll != null;
		},
		/** A row this store has seen, in any tab. */
		row(job: string, conversationId: string): InboxRow | undefined {
			return known.get(rowKey(job, conversationId));
		},
		setTab(t: InboxTab) {
			if (t !== tab) void load(t);
		},
		setFilters(f: Filters) {
			filters = { ...f };
			void load();
		},
		load,
		start,
		applyFrame,
		seen,
		startPolling,
		stopPolling,
		stop,
	};
}

export type InboxStore = ReturnType<typeof createInbox>;
