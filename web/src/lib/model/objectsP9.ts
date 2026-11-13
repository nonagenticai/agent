// P9 Objects — pure helpers for the object map: a deterministic grid layout (base handful in the upper band,
// industry/optional additions in the lower band) that keeps related objects close, plus small summaries.
import type { Modality, ObjectMapNode, ObjectRelation, ObjectTier } from '$lib/api/types/model';

export const MAP_COLS = 5;
export const CELL_W = 156;
export const CELL_H = 96;
export const NODE_W = 128;
export const NODE_H = 46;
/** Vertical gap between the base band and the additions band (room for the band label). */
export const BAND_GAP = 34;
export const PAD = 12;

export type Band = 'base' | 'additions';
export const bandOf = (t: ObjectTier): Band => (t === 'base' ? 'base' : 'additions');

export interface Placed {
	object: string;
	band: Band;
	col: number;
	row: number;
	/** Top-left corner of the node box in SVG units. */
	x: number;
	y: number;
}

export interface MapLayout {
	placed: Record<string, Placed>;
	baseRows: number;
	addRows: number;
	width: number;
	height: number;
	/** Top of the additions band (for the band label/divider). */
	addTop: number;
}

/** Seeded LCG so the layout is identical on every render, test and screenshot. */
function rng(seed: number) {
	let s = seed >>> 0;
	return () => {
		s = (Math.imul(s, 1664525) + 1013904223) >>> 0;
		return s / 2 ** 32;
	};
}

type Pt = { x: number; y: number };

function cross(a: Pt, b: Pt, c: Pt, d: Pt): boolean {
	const o = (p: Pt, q: Pt, r: Pt) => Math.sign((q.x - p.x) * (r.y - p.y) - (q.y - p.y) * (r.x - p.x));
	return o(a, b, c) * o(a, b, d) < 0 && o(c, d, a) * o(c, d, b) < 0;
}

/** Does segment a→b cross the axis-aligned box centred on c with half-sizes hw × hh? (Liang–Barsky.) */
export function segHitsBox(a: Pt, b: Pt, c: Pt, hw: number, hh: number): boolean {
	let t0 = 0;
	let t1 = 1;
	const dx = b.x - a.x;
	const dy = b.y - a.y;
	const clip = (p: number, q: number) => {
		if (p === 0) return q >= 0;
		const r = q / p;
		if (p < 0) {
			if (r > t1) return false;
			if (r > t0) t0 = r;
		} else {
			if (r < t0) return false;
			if (r < t1) t1 = r;
		}
		return true;
	};
	return (
		clip(-dx, a.x - (c.x - hw)) &&
		clip(dx, c.x + hw - a.x) &&
		clip(-dy, a.y - (c.y - hh)) &&
		clip(dy, c.y + hh - a.y) &&
		t0 < t1
	);
}

/**
 * Place every object on a grid cell of its band, minimising edge length, crossings and edges drawn through a
 * third node (simulated annealing over cell swaps; deterministic).
 */
export function layoutObjectMap(
	nodes: ObjectMapNode[],
	relations: ObjectRelation[],
	iterations = 5000,
	seed = 9,
): MapLayout {
	const keys = nodes.map((n) => n.object);
	const band: Record<string, Band> = Object.fromEntries(nodes.map((n) => [n.object, bandOf(n.tier)]));
	const baseKeys = keys.filter((k) => band[k] === 'base');
	const addKeys = keys.filter((k) => band[k] === 'additions');
	const baseRows = Math.max(1, Math.ceil(baseKeys.length / MAP_COLS));
	const addRows = addKeys.length ? Math.ceil(addKeys.length / MAP_COLS) : 0;
	const baseCells = baseRows * MAP_COLS;
	const addCells = addRows * MAP_COLS;
	// slot[i] = object key or null; base slots first, then additions slots.
	const slots: (string | null)[] = [
		...baseKeys,
		...Array(baseCells - baseKeys.length).fill(null),
		...addKeys,
		...Array(addCells - addKeys.length).fill(null),
	];
	const edges = relations
		.filter((r) => r.subject !== r.object && band[r.subject] && band[r.object])
		.map((r) => [r.subject, r.object] as const);

	// Cell units: one column = 1; rows keep the real aspect so "through a node" checks match the drawing.
	const ry = CELL_H / CELL_W;
	const hw = NODE_W / 2 / CELL_W + 0.03;
	const hh = NODE_H / 2 / CELL_W + 0.03;
	const gap = BAND_GAP / CELL_W;
	const pos = (i: number): Pt =>
		i < baseCells
			? { x: i % MAP_COLS, y: Math.floor(i / MAP_COLS) * ry }
			: { x: (i - baseCells) % MAP_COLS, y: (baseRows + Math.floor((i - baseCells) / MAP_COLS)) * ry + gap };

	function cost(): number {
		const at: Record<string, Pt> = {};
		slots.forEach((k, i) => {
			if (k) at[k] = pos(i);
		});
		let c = 0;
		for (const [a, b] of edges) c += Math.hypot(at[a]!.x - at[b]!.x, at[a]!.y - at[b]!.y);
		for (let i = 0; i < edges.length; i++)
			for (let j = i + 1; j < edges.length; j++) {
				const [a, b] = edges[i]!;
				const [p, q] = edges[j]!;
				if (a === p || a === q || b === p || b === q) continue;
				if (cross(at[a]!, at[b]!, at[p]!, at[q]!)) c += 1.6;
			}
		for (const [a, b] of edges)
			for (const k of keys) if (k !== a && k !== b && segHitsBox(at[a]!, at[b]!, at[k]!, hw, hh)) c += 8;
		// Keep the bands visually centred: a mild pull towards the middle columns.
		for (const k of keys) c += Math.abs(at[k]!.x - (MAP_COLS - 1) / 2) * 0.04;
		return c;
	}

	const start = [...slots];
	let best = Infinity;
	let bestSlots = [...slots];
	// A few deterministic restarts: annealing alone sometimes settles with a line through a node.
	for (let restart = 0; restart < 2; restart++) {
		slots.splice(0, slots.length, ...start);
		const rand = rng(seed + restart * 7919);
		let cur = cost();
		if (cur < best) {
			best = cur;
			bestSlots = [...slots];
		}
		for (let it = 0; it < iterations; it++) {
			const inBase = rand() < baseCells / (baseCells + addCells || 1);
			const lo = inBase ? 0 : baseCells;
			const n = inBase ? baseCells : addCells;
			if (n < 2) continue;
			const i = lo + Math.floor(rand() * n);
			const j = lo + Math.floor(rand() * n);
			if (i === j || (!slots[i] && !slots[j])) continue;
			[slots[i], slots[j]] = [slots[j]!, slots[i]!];
			const next = cost();
			const temp = 1.5 * (1 - it / iterations) + 0.01;
			if (next <= cur || rand() < Math.exp((cur - next) / temp)) {
				cur = next;
				if (cur < best) {
					best = cur;
					bestSlots = [...slots];
				}
			} else [slots[i], slots[j]] = [slots[j]!, slots[i]!];
		}
	}

	const addTop = PAD + baseRows * CELL_H + BAND_GAP;
	const placed: Record<string, Placed> = {};
	bestSlots.forEach((k, i) => {
		if (!k) return;
		const isBase = i < baseCells;
		const idx = isBase ? i : i - baseCells;
		const col = idx % MAP_COLS;
		const row = Math.floor(idx / MAP_COLS);
		placed[k] = {
			object: k,
			band: isBase ? 'base' : 'additions',
			col,
			row,
			x: PAD + col * CELL_W + (CELL_W - NODE_W) / 2,
			y: (isBase ? PAD : addTop) + row * CELL_H + (CELL_H - NODE_H) / 2,
		};
	});
	return {
		placed,
		baseRows,
		addRows,
		width: PAD * 2 + MAP_COLS * CELL_W,
		height: addTop + addRows * CELL_H + PAD,
		addTop,
	};
}

/** Relations drawn straight through a third object's box (pixel geometry) — what the layout tries to avoid. */
export function edgesThroughNodes(layout: MapLayout, relations: ObjectRelation[]): string[] {
	const ctr = (k: string) => ({ x: layout.placed[k]!.x + NODE_W / 2, y: layout.placed[k]!.y + NODE_H / 2 });
	const out: string[] = [];
	for (const r of relations) {
		if (r.subject === r.object || !layout.placed[r.subject] || !layout.placed[r.object]) continue;
		const a = ctr(r.subject);
		const b = ctr(r.object);
		for (const p of Object.values(layout.placed)) {
			if (p.object === r.subject || p.object === r.object) continue;
			if (segHitsBox(a, b, { x: p.x + NODE_W / 2, y: p.y + NODE_H / 2 }, NODE_W / 2, NODE_H / 2))
				out.push(`${r.subject} ${r.predicate} ${r.object} × ${p.object}`);
		}
	}
	return out;
}

/** Where a line from node centre `a` towards `b` leaves `a`'s box. */
export function boxExit(a: Pt, b: Pt, w = NODE_W, h = NODE_H): Pt {
	const dx = b.x - a.x;
	const dy = b.y - a.y;
	if (dx === 0 && dy === 0) return a;
	const s = Math.min(dx ? w / 2 / Math.abs(dx) : Infinity, dy ? h / 2 / Math.abs(dy) : Infinity);
	return { x: a.x + dx * s, y: a.y + dy * s };
}

export const MODALITIES: Modality[] = ['observe', 'do', 'request'];

export function modalityCounts(n: Pick<ObjectMapNode, 'operations'>): Record<Modality, number> {
	const c: Record<Modality, number> = { observe: 0, do: 0, request: 0 };
	for (const o of n.operations) c[o.modality]++;
	return c;
}

/** Relations touching an object, phrased from that object's side. */
export function relationsOf(object: string, relations: ObjectRelation[]) {
	return relations
		.filter((r) => r.subject === object || r.object === object)
		.map((r) => ({
			...r,
			outgoing: r.subject === object,
			other: r.subject === object ? r.object : r.subject,
		}));
}

/** Objects directly related to `object` (either direction). */
export function neighbours(object: string, relations: ObjectRelation[]): Set<string> {
	return new Set(relationsOf(object, relations).map((r) => r.other));
}

export function matchesFilter(
	n: Pick<ObjectMapNode, 'object' | 'label' | 'description' | 'tier' | 'in_retail_job'>,
	tier: string,
	q: string,
): boolean {
	const s = q.trim().toLowerCase();
	return (
		(tier === 'all' || (tier === 'retail' ? n.in_retail_job : n.tier === tier)) &&
		(!s ||
			n.object.includes(s) ||
			n.label.toLowerCase().includes(s) ||
			n.description.toLowerCase().includes(s))
	);
}

export interface Route {
	s: Pt;
	/** Quadratic control point; null for a straight line. */
	c: Pt | null;
	e: Pt;
	d: string;
	at: (t: number) => Pt;
}

/**
 * Route a relation between two placed objects: straight when the line clears every other box, otherwise the
 * smallest sideways bend (quadratic curve) that does — so a line never seems to end in a node it passes.
 */
export function routeEdge(layout: MapLayout, from: string, to: string): Route {
	const ctr = (k: string) => ({ x: layout.placed[k]!.x + NODE_W / 2, y: layout.placed[k]!.y + NODE_H / 2 });
	const a = ctr(from);
	const b = ctr(to);
	const others = Object.values(layout.placed)
		.filter((p) => p.object !== from && p.object !== to)
		.map((p) => ({ x: p.x + NODE_W / 2, y: p.y + NODE_H / 2 }));
	const quad = (c: Pt) => (t: number) => ({
		x: (1 - t) ** 2 * a.x + 2 * (1 - t) * t * c.x + t ** 2 * b.x,
		y: (1 - t) ** 2 * a.y + 2 * (1 - t) * t * c.y + t ** 2 * b.y,
	});
	const clear = (f: (t: number) => Pt) => {
		for (let i = 0; i < 24; i++) {
			const p = f(i / 24);
			const q = f((i + 1) / 24);
			if (others.some((o) => segHitsBox(p, q, o, NODE_W / 2 + 4, NODE_H / 2 + 4))) return false;
		}
		return true;
	};
	const line = (t: number) => ({ x: a.x + (b.x - a.x) * t, y: a.y + (b.y - a.y) * t });
	let ctrl: Pt | null = null;
	if (!clear(line)) {
		const len = Math.hypot(b.x - a.x, b.y - a.y) || 1;
		const nx = -(b.y - a.y) / len;
		const ny = (b.x - a.x) / len;
		const mid = line(0.5);
		search: for (const k of [40, 70, 100, 140]) {
			for (const sign of [1, -1]) {
				const c = { x: mid.x + nx * k * sign, y: mid.y + ny * k * sign };
				if (clear(quad(c))) {
					ctrl = c;
					break search;
				}
			}
		}
	}
	const at = ctrl ? quad(ctrl) : line;
	// Leave/enter the boxes along the first/last stretch of the path.
	const s = boxExit(a, at(0.12));
	const e = boxExit(b, at(0.88), NODE_W + 8, NODE_H + 8);
	const d = ctrl ? `M ${s.x} ${s.y} Q ${ctrl.x} ${ctrl.y} ${e.x} ${e.y}` : `M ${s.x} ${s.y} L ${e.x} ${e.y}`;
	return { s, c: ctrl, e, d, at };
}
