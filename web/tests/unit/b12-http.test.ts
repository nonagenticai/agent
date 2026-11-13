// B12: the conversation feed's reconnect logic (pure; the stream opener is a literal callback).
import { waitFor } from '@testing-library/svelte';
import { describe, expect, it } from 'vitest';
import type { Api } from '../../src/lib/api/client';
import { ApiError } from '../../src/lib/api/errors';
import type { ConversationFrame } from '../../src/lib/api/types/conversations';
import { watchConversation, type FeedState } from '../../src/lib/conversations/feed';

describe('watchConversation', () => {
	it('reopens the stream after it ends, with back-off, until stopped', async () => {
		let opened = 0;
		const states: FeedState[] = [];
		const frames: ConversationFrame[] = [];
		const events: Api['conversationEvents'] = async (_id, o) => {
			opened++;
			o.onFrame({ event: 'message', data: opened });
		};
		const stop = watchConversation(events, 'c1', {
			onFrame: (f) => frames.push(f),
			onState: (s) => states.push(s),
			minDelayMs: 1,
			maxDelayMs: 4,
		});
		await waitFor(() => expect(opened).toBeGreaterThanOrEqual(3));
		stop();
		const n = opened;
		await new Promise((r) => setTimeout(r, 20));
		expect(opened).toBe(n);
		expect(frames[0]).toEqual({ event: 'message', data: 1 });
		expect(states[0]).toBe('connecting');
		expect(states).toContain('reconnecting');
		expect(states.at(-1)).toBe('closed');
	});

	it('stops for good on 404; stop() aborts an open stream', async () => {
		const states: FeedState[] = [];
		let calls = 0;
		watchConversation(
			async () => {
				calls++;
				throw new ApiError(404, 'conversation_not_found', 'gone');
			},
			'c1',
			{ onFrame: () => {}, onState: (s) => states.push(s), minDelayMs: 1 },
		);
		await waitFor(() => expect(states.at(-1)).toBe('closed'));
		await new Promise((r) => setTimeout(r, 10));
		expect(calls).toBe(1);

		let aborted = false;
		const stop = watchConversation(
			(_id, o) =>
				new Promise<void>((resolve) =>
					o.signal?.addEventListener('abort', () => {
						aborted = true;
						resolve();
					}),
				),
			'c2',
			{ onFrame: () => {} },
		);
		stop();
		expect(aborted).toBe(true);
	});
});
