// T10: the `/support` workspace — the page registry and the presence board (pure).
import { describe, expect, it, vi } from 'vitest';
import { accessFor, PAGES } from '../../src/lib/pages';
import { presenceBoard, presenceText, startViewing, typingNotifier } from '../../src/lib/support/presence';

const JOB = 'customer_care_agent';
const CONV = 'c-telegram-1';
const SANNE = 'sub-sanne';

describe('registry', () => {
	it('support is a live page every member may open', () => {
		const p = PAGES.find((x) => x.route === '/support')!;
		expect(p.group).toBe('live');
		expect(p.also?.map((r) => r.route)).toContain('/support/[job]/c/[id]');
		expect(accessFor('/support', { superAdmin: false, member: true })).toBe('ok');
		expect(accessFor('/support/[job]/c/[id]', { superAdmin: false, member: false })).toBe('no_access');
	});
});

describe('presence', () => {
	it('viewing beats every 5 s, typing at most once a second, colleagues expire after 10 s', () => {
		vi.useFakeTimers();
		const beats = vi.fn();
		const stop = startViewing(beats);
		vi.advanceTimersByTime(10_000);
		expect(beats).toHaveBeenCalledTimes(3);
		stop();
		vi.advanceTimersByTime(10_000);
		expect(beats).toHaveBeenCalledTimes(3);

		let t = 0;
		const typed = vi.fn();
		const n = typingNotifier(typed, 1000, () => t);
		n.input();
		t = 500;
		n.input();
		t = 1000;
		n.input();
		expect(typed).toHaveBeenCalledTimes(2);

		const board = presenceBoard();
		board.seen({ conversation_id: CONV, job: JOB, sub: 'sam', name: 'Sam Visser', state: 'typing' }, 0);
		board.seen({ conversation_id: CONV, job: JOB, sub: SANNE, name: 'Sanne', state: 'viewing' }, 0);
		expect(presenceText(board.of(JOB, CONV, SANNE, 9_000))).toBe('Sam is typing…');
		expect(board.of(JOB, CONV, SANNE, 10_000)).toEqual([]);
	});
});
