// The staff test chat's outbox (port of cb console/outbox.ts + chat/session.svelte.ts): persisted per conversation,
// resumed after a reload, a 409 busy stays queued then sends, a lost response locks until Resend or Discard.
import { describe, expect, it } from 'vitest';
import {
	CONVERSATION_KEY,
	emptyOutbox,
	loadOutbox,
	reconcile,
	saveOutbox,
	submit,
	take,
	type Store,
} from '../../src/lib/live/chat/outbox';
import type { ChatEvent } from '../../src/lib/api/types/live';

/** An in-memory sessionStorage: one per "tab", survives a simulated reload (a new LiveChat on the same store). */
function memStore(): Store & { data: Map<string, string> } {
	const data = new Map<string, string>();
	return {
		data,
		getItem: (k) => data.get(k) ?? null,
		setItem: (k, v) => void data.set(k, v),
		removeItem: (k) => void data.delete(k),
	};
}
describe('outbox (pure)', () => {
	it('round-trips through storage, and a blocked storage never throws', () => {
		const s = memStore();
		const o = take(submit(submit(emptyOutbox(), 'a'), 'b'), 0).o;
		saveOutbox('conv-1', o, s);
		const back = loadOutbox('conv-1', s);
		expect(back.items.map((i) => [i.text, i.status, i.batch])).toEqual([
			['a', 'sending', 1],
			['b', 'sending', 1],
		]);
		expect(back.nextBatch).toBe(2);
		const blocked: Store = {
			getItem: () => {
				throw new Error('SecurityError');
			},
			setItem: () => {
				throw new Error('QuotaExceededError');
			},
			removeItem: () => {
				throw new Error('SecurityError');
			},
		};
		expect(() => saveOutbox('conv-1', o, blocked)).not.toThrow();
		expect(loadOutbox('conv-1', blocked)).toEqual(emptyOutbox());
		s.setItem('agent.chat.outbox.conv-2', '{not json');
		expect(loadOutbox('conv-2', s)).toEqual(emptyOutbox());
	});

	it('reconciles an accepted batch by its message id, an unknown one by text after its mark', () => {
		const ev = (id: string, text: string): ChatEvent => ({
			conversation_id: 'c',
			message_id: id,
			role: 'customer',
			text,
			at_ms: 0,
		});
		let o = take(submit(emptyOutbox(), 'hi'), 1).o;
		o = { ...o, items: o.items.map((i) => ({ ...i, status: 'unknown' as const })) };
		// An earlier identical message (before the mark) does not settle it; a later one does.
		expect(reconcile(o, [ev('m1', 'hi')]).items).toHaveLength(1);
		expect(reconcile(o, [ev('m1', 'hi'), ev('m2', 'hi')]).items).toHaveLength(0);
	});
});
