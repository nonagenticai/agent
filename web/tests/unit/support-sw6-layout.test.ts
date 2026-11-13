// SW-6: the staff workspace layout rules (breakpoints, the phone's one pane, watching the layout).
import { describe, expect, it } from 'vitest';
import { TABLET_MIN, WIDE_MIN, layoutFor, phonePane, watchLayout } from '../../src/lib/support/layout';

// ---- a matchMedia whose width the test moves --------------------------------------------------------------------------
let width = 1440;
const listeners = new Set<() => void>();
function stubMatchMedia() {
	window.matchMedia = ((query: string) => {
		const min = Number(/min-width:\s*(\d+)px/.exec(query)?.[1] ?? 0);
		return {
			get matches() {
				return width >= min;
			},
			media: query,
			addEventListener: (_: string, cb: () => void) => listeners.add(cb),
			removeEventListener: (_: string, cb: () => void) => listeners.delete(cb),
		};
	}) as unknown as typeof window.matchMedia;
}
describe('layout rules', () => {
	it('breakpoints: phone < 768 <= tablet < 1200 <= wide', () => {
		expect([767, 768, 1199, 1200].map(layoutFor)).toEqual(['phone', 'tablet', 'tablet', 'wide']);
		expect([TABLET_MIN, WIDE_MIN]).toEqual([768, 1200]);
		expect(layoutFor(390)).toBe('phone');
		expect(layoutFor(1920)).toBe('wide');
	});

	it('a phone shows one pane: the list at the root, the conversation, the customer sheet on top', () => {
		expect(phonePane({ selected: false, customerOpen: false })).toBe('list');
		expect(phonePane({ selected: true, customerOpen: false })).toBe('conversation');
		expect(phonePane({ selected: true, customerOpen: true })).toBe('customer');
		// no conversation, no customer
		expect(phonePane({ selected: false, customerOpen: true })).toBe('list');
	});

	it('watchLayout reports now and on every change, and stops when released', () => {
		stubMatchMedia();
		width = 500;
		const seen: string[] = [];
		const stop = watchLayout((l) => seen.push(l));
		for (const w of [900, 1300, 700]) {
			width = w;
			for (const cb of [...listeners]) cb();
		}
		stop();
		expect(listeners.size).toBe(0);
		expect(seen.at(0)).toBe('phone');
		expect(seen.filter((l, i) => l !== seen[i - 1])).toEqual(['phone', 'tablet', 'wide', 'phone']);
	});

	it('without matchMedia the workspace is wide', () => {
		const mm = window.matchMedia;
		delete (window as { matchMedia?: unknown }).matchMedia;
		const seen: string[] = [];
		watchLayout((l) => seen.push(l))();
		window.matchMedia = mm;
		expect(seen).toEqual(['wide']);
	});
});
