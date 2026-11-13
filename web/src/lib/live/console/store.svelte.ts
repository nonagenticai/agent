// Console state (from cb:web/src/lib/console/console.svelte.ts@45ee128, minus the chat log — chat is L2 now): bus
// entries per lane in entry order, dedupe, the per-lane window, case ids seen, the selected entry and ack state.
import { SvelteSet } from 'svelte/reactivity';
import type { BusEvent } from '$lib/api/types/live';

/** customer-bus, the op stream (calls down to the runner), its results stream, and dead letters (doc 12 §3.1). */
export type Lane = 'customer' | 'op' | 'result' | 'dead';
export const LANES: Lane[] = ['customer', 'op', 'result', 'dead'];
export const laneOf = (stream: string): Lane =>
	stream.startsWith('bus:customer:')
		? 'customer'
		: stream === 'bus:op'
			? 'op'
			: stream === 'bus:op:results'
				? 'result'
				: 'dead';
const DOWN = ['.request.qualified', 'op.call'];
export const direction = (topic: string): 'down' | 'up' =>
	DOWN.some((s) => topic === s || topic.endsWith(s)) ? 'down' : 'up';

/** Bus events kept per lane in the browser; older ones are dropped from the display (the case keeps its trace). */
export const MAX_PER_LANE = 300;

export const busKey = (e: BusEvent) => `${e.stream}|${e.entry_id}`;
const byEntry = (a: BusEvent, b: BusEvent) => {
	const [am, as] = a.entry_id.split('-').map(Number),
		[bm, bs] = b.entry_id.split('-').map(Number);
	return am! - bm! || as! - bs!;
};
const zero = (): Record<Lane, number> => ({ customer: 0, op: 0, result: 0, dead: 0 });

export class ConsoleStore {
	customer = $state<BusEvent[]>([]);
	op = $state<BusEvent[]>([]);
	result = $state<BusEvent[]>([]);
	dead = $state<BusEvent[]>([]);
	caseIds = $state<string[]>([]);
	selected = $state<BusEvent | null>(null);
	/** Events dropped from the display because a lane exceeded MAX_PER_LANE. */
	trimmed = $state(0);
	/** New events accepted per lane since the last reset; drives "N new events". */
	added = $state<Record<Lane, number>>(zero());
	/** busKeys the recipient group has XACKed. Monotonic. */
	readonly acked = new SvelteSet<string>();
	private seen = new Set<string>();

	/** Every entry across lanes, oldest first (entry ids are millisecond-based). */
	get all(): BusEvent[] {
		return [...this.customer, ...this.op, ...this.result, ...this.dead].sort(byEntry);
	}

	/** True only when the event was new. Lanes stay in entry_id order. */
	addBus(e: BusEvent): boolean {
		const k = busKey(e);
		if (e.acked === true) this.acked.add(k);
		if (this.seen.has(k)) return false;
		this.seen.add(k);
		const lane = laneOf(e.stream);
		this.added[lane]++;
		const entries = this[lane];
		if (!entries.length || byEntry(entries.at(-1)!, e) <= 0) entries.push(e);
		else {
			let lo = 0,
				hi = entries.length;
			while (lo < hi) {
				const mid = (lo + hi) >>> 1;
				if (byEntry(entries[mid]!, e) <= 0) lo = mid + 1;
				else hi = mid;
			}
			entries.splice(lo, 0, e);
		}
		if (entries.length > MAX_PER_LANE) {
			const old = entries.splice(0, entries.length - MAX_PER_LANE);
			this.trimmed += old.length;
			if (this.selected && old.some((o) => busKey(o) === busKey(this.selected!))) this.selected = null;
		}
		const cid = e.envelope.correlation_id;
		if (cid && !this.caseIds.includes(cid)) this.caseIds = [...this.caseIds, cid];
		return true;
	}
	/** `acked` once the recipient group XACKed the entry, `pending` until then, null when no group is the recipient. */
	ackOf(e: BusEvent): 'acked' | 'pending' | null {
		if (!e.consumer_group) return null;
		return this.acked.has(busKey(e)) ? 'acked' : 'pending';
	}
	reset() {
		this.customer = [];
		this.op = [];
		this.result = [];
		this.dead = [];
		this.caseIds = [];
		this.selected = null;
		this.trimmed = 0;
		this.added = zero();
		this.acked.clear();
		this.seen.clear();
	}
}
