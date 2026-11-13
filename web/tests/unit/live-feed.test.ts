// The live feed (L1/L2): connect, pull, reconnect, stop.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { createFeed, type FeedStatus } from '../../src/lib/live/feed';
beforeEach(() => vi.useFakeTimers({ now: Date.parse('2026-10-18T10:00:00Z') }));
afterEach(() => {
	vi.useRealTimers();
});

describe('createFeed', () => {
	it('connects, delivers pulls, reconnects after a failure and after a drop, and stop clears every timer', async () => {
		const statuses: FeedStatus[] = [];
		const got: number[] = [];
		let n = 0;
		let fail = false;
		const reconnects = vi.fn();
		const feed = createFeed<number>({
			pull: async () => {
				if (fail) throw new Error('503');
				return [++n];
			},
			onEvents: (e) => got.push(...e),
			onStatus: (s) => statuses.push(s),
			onReconnect: reconnects,
			tickMs: 100,
			connectMs: 50,
			retryMs: 300,
		});
		feed.start();
		expect(feed.status).toBe('connecting');
		await vi.advanceTimersByTimeAsync(50);
		expect(feed.status).toBe('open');
		await vi.advanceTimersByTimeAsync(200);
		expect(got).toEqual([1, 2, 3]);

		fail = true;
		await vi.advanceTimersByTimeAsync(100);
		expect(feed.status).toBe('retrying');
		fail = false;
		await vi.advanceTimersByTimeAsync(300);
		expect(feed.status).toBe('open');
		expect(reconnects).toHaveBeenCalledTimes(1);

		feed.drop();
		expect(feed.status).toBe('retrying');
		const held = got.length;
		await vi.advanceTimersByTimeAsync(250);
		expect(got.length, 'nothing flows while dropped').toBe(held);
		await vi.advanceTimersByTimeAsync(60);
		expect(feed.status).toBe('open');
		expect(reconnects).toHaveBeenCalledTimes(2);

		feed.stop();
		expect(feed.status).toBe('stopped');
		expect(vi.getTimerCount()).toBe(0);
		expect(statuses).toEqual(['connecting', 'open', 'retrying', 'open', 'retrying', 'open', 'stopped']);
	});
});
