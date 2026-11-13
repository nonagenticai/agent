// P12 Business models — pure helpers behind the page: axes grouped by the canonical object they classify (11 §1.2),
// what a selection derives (fired vs idle axis rules), and the diff between a saved business model and the picker.
import type { Axis, AxisRule } from '$lib/api/types/platform';
import { evaluate } from '$lib/platform/model';

export type Selection = Record<string, string[]>;

/** The canonical objects that carry classifier axes, in the order a sale happens (11 §1.2). */
export const AXIS_OBJECTS: { object: string; question: string }[] = [
	{ object: 'Party', question: 'Who buys' },
	{ object: 'Offering', question: 'What is sold' },
	{ object: 'Order', question: 'How it is ordered' },
	{ object: 'Renewal', question: 'How it renews' },
	{ object: 'Delivery', question: 'How it reaches the customer' },
	{ object: 'Payment', question: 'How it is paid' },
	{ object: 'Contract', question: 'Under what agreement' },
];

export interface AxisGroup {
	object: string;
	question: string;
	axes: Axis[];
}

/** Axes grouped per object, in `AXIS_OBJECTS` order; an object no listed axis names is appended at the end. */
export function groupAxes(axes: Axis[]): AxisGroup[] {
	const known = AXIS_OBJECTS.map((o) => ({ ...o, axes: axes.filter((a) => a.object === o.object) }));
	const extra = [...new Set(axes.map((a) => a.object))]
		.filter((o) => !AXIS_OBJECTS.some((k) => k.object === o))
		.map((object) => ({ object, question: '', axes: axes.filter((a) => a.object === object) }));
	return [...known, ...extra].filter((g) => g.axes.length);
}

/** Rules whose condition holds for the selection (by priority) and the ones that stay idle. */
export function derive(rules: AxisRule[], sel: Selection): { fired: AxisRule[]; idle: AxisRule[] } {
	const sorted = [...rules].sort((a, b) => a.priority - b.priority);
	return {
		fired: sorted.filter((r) => evaluate(r.condition, sel)),
		idle: sorted.filter((r) => !evaluate(r.condition, sel)),
	};
}

/** Flat `axis=value` pairs, sorted. */
export function pairs(sel: Selection): string[] {
	return Object.entries(sel)
		.flatMap(([ax, vals]) => vals.map((v) => `${ax}=${v}`))
		.sort();
}

/** What changed between the saved selection and the picker: `axis=value` pairs added and removed. */
export function diffSelection(
	saved: Selection | null,
	current: Selection,
): { added: string[]; removed: string[] } {
	const before = new Set(pairs(saved ?? {}));
	const after = new Set(pairs(current));
	return {
		added: [...after].filter((p) => !before.has(p)),
		removed: [...before].filter((p) => !after.has(p)),
	};
}

/** How many axes a selection leaves open (no value picked). */
export function openAxes(axes: Axis[], sel: Selection): Axis[] {
	return axes.filter((a) => !(sel[a.key]?.length ?? 0));
}
