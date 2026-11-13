// SW-6: which shape the staff workspace takes at a width, and which single pane a phone shows.
//   wide   (>= 1200 px)  inbox | conversation | customer, side by side
//   tablet (768-1199 px) inbox | conversation; the customer opens as a right drawer from the conversation header
//   phone  (< 768 px)    one pane at a time, driven by the URL: inbox -> conversation -> customer sheet
export type Layout = 'wide' | 'tablet' | 'phone';
export type Pane = 'list' | 'conversation' | 'customer';

export const WIDE_MIN = 1200;
export const TABLET_MIN = 768;

export const layoutFor = (width: number): Layout =>
	width >= WIDE_MIN ? 'wide' : width >= TABLET_MIN ? 'tablet' : 'phone';

/** The pane a phone shows: the list at the root, the open conversation, or its customer sheet on top. */
export const phonePane = (s: { selected: boolean; customerOpen: boolean }): Pane =>
	!s.selected ? 'list' : s.customerOpen ? 'customer' : 'conversation';

/**
 * Follow the viewport width. Calls `onChange` once with the current layout, then on every change. Without
 * `matchMedia` (a server render, a bare test DOM) the workspace stays `wide`.
 */
export function watchLayout(onChange: (l: Layout) => void): () => void {
	if (typeof matchMedia !== 'function') {
		onChange('wide');
		return () => {};
	}
	const wide = matchMedia(`(min-width: ${WIDE_MIN}px)`);
	const tablet = matchMedia(`(min-width: ${TABLET_MIN}px)`);
	const sync = () => onChange(wide.matches ? 'wide' : tablet.matches ? 'tablet' : 'phone');
	sync();
	wide.addEventListener?.('change', sync);
	tablet.addEventListener?.('change', sync);
	return () => {
		wide.removeEventListener?.('change', sync);
		tablet.removeEventListener?.('change', sync);
	};
}
