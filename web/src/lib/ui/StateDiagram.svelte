<script lang="ts" module>
	export interface DiagramState {
		id: string;
		label?: string;
		/** Drawn with a double border (no way out). */
		terminal?: boolean;
		/** Where the machine starts; states nobody transitions into count as initial too. */
		initial?: boolean;
	}
	export interface DiagramTransition {
		from: string;
		to: string;
		/** The verb (`order.cancel`) or guard shown on the arrow. */
		label?: string;
	}

	const H = 38;
	const GY = 26;
	const PAD = 16;
	const CH = 7; // ~px per character at the diagram's font sizes
	const MAX_LABEL = 24;

	export type Orientation = 'horizontal' | 'vertical';
	export interface DiagramLayout {
		pos: Map<string, { x: number; y: number }>;
		width: number;
		height: number;
		/** Box width (fits the longest state label, capped). */
		w: number;
		/** Vertical: x of the first lane right of the boxes where backward arrows run. */
		lane: number;
		/** Vertical: how many backward arrows (lanes) there are; their verbs sit right of the last lane. */
		lanes: number;
		orientation: Orientation;
	}

	const clamp = (v: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, v));
	export const shortLabel = (t: string) => (t.length > MAX_LABEL ? `${t.slice(0, MAX_LABEL - 1)}…` : t);

	/** Depth = shortest distance from an initial state; unreachable states go one past the deepest. */
	function depths(states: DiagramState[], transitions: DiagramTransition[]): Map<number, string[]> {
		const ids = states.map((s) => s.id);
		const incoming = new Set(transitions.filter((t) => t.from !== t.to).map((t) => t.to));
		const starts = states.filter((s) => s.initial || !incoming.has(s.id)).map((s) => s.id);
		const depth = new Map<string, number>();
		const queue = [...(starts.length ? starts : ids.slice(0, 1))];
		for (const s of queue) depth.set(s, 0);
		while (queue.length) {
			const cur = queue.shift()!;
			for (const t of transitions)
				if (t.from === cur && !depth.has(t.to) && ids.includes(t.to)) {
					depth.set(t.to, depth.get(cur)! + 1);
					queue.push(t.to);
				}
		}
		const maxDepth = Math.max(0, ...depth.values());
		const cols = new Map<number, string[]>();
		for (const id of ids) {
			const d = depth.get(id) ?? maxDepth + 1;
			cols.set(d, [...(cols.get(d) ?? []), id]);
		}
		return cols;
	}

	/**
	 * Horizontal: columns left→right by depth (the gap fits the longest verb). Vertical (narrow screens): rows
	 * top→bottom by depth, backward arrows bow out on the right.
	 */
	export function layoutStates(
		states: DiagramState[],
		transitions: DiagramTransition[],
		orientation: Orientation = 'horizontal',
	): DiagramLayout {
		const cols = depths(states, transitions);
		const longest = Math.max(0, ...states.map((s) => shortLabel(s.label ?? s.id).length));
		const w = clamp(longest * CH + 24, 112, MAX_LABEL * CH + 24);
		const verb = Math.max(0, ...transitions.map((t) => shortLabel(t.label ?? '').length));
		const gx = clamp(verb * CH * 0.95 + 20, 56, 200);
		const pos = new Map<string, { x: number; y: number }>();
		const n = Math.max(...cols.keys()) + 1;
		const widest = Math.max(...[...cols.values()].map((c) => c.length));
		if (orientation === 'horizontal') {
			for (const [d, col] of cols)
				col.forEach((id, i) => pos.set(id, { x: PAD + d * (w + gx), y: PAD + i * (H + GY) }));
			return {
				pos,
				w,
				lane: 0,
				lanes: 0,
				orientation,
				width: PAD * 2 + n * (w + gx) - gx,
				height: PAD * 2 + widest * (H + GY) - GY,
			};
		}
		const gap = 20;
		const gy = 52;
		for (const [d, col] of cols)
			col.forEach((id, i) => pos.set(id, { x: PAD + i * (w + gap), y: PAD + d * (H + gy) }));
		const rel = (t: DiagramTransition) => {
			const a = pos.get(t.from);
			const b = pos.get(t.to);
			return a && b && t.from !== t.to ? Math.sign(b.y - a.y) : 1;
		};
		const back = transitions.filter((t) => rel(t) < 0).length;
		const same = transitions.some((t) => rel(t) === 0);
		const lane = PAD + widest * (w + gap) - gap + 18;
		return {
			pos,
			w,
			lane,
			lanes: back,
			orientation,
			width: back ? lane + back * 10 + 8 + verb * CH * 0.95 : lane - 18 + PAD,
			height: PAD * 2 + n * (H + gy) - gy + (same ? 30 : 0),
		};
	}
</script>

<script lang="ts">
	// SVG state machine (object states and transitions, spec 11 §1.3): boxes laid out left→right by distance from
	// the initial state, arrows labelled with the verb. Never wider than its container: when the left→right drawing
	// does not fit it turns top→bottom (scaled down a little if needed), and below that it becomes a list of states
	// with their outgoing transitions. A list of the same transitions follows for screen readers.
	let {
		states,
		transitions,
		current,
		title = 'State diagram',
	}: {
		states: DiagramState[];
		transitions: DiagramTransition[];
		current?: string;
		title?: string;
	} = $props();
	const uid = $props.id();
	/** Width the figure may use (0 before mount / in jsdom: lay out horizontally at natural size). */
	let avail = $state(0);
	let fig = $state<HTMLElement>();
	$effect(() => {
		if (!fig || typeof ResizeObserver === 'undefined') return;
		const ro = new ResizeObserver(() => (avail = fig?.clientWidth ?? 0));
		ro.observe(fig);
		return () => ro.disconnect();
	});
	const wide = $derived(layoutStates(states, transitions, 'horizontal'));
	const tall = $derived(layoutStates(states, transitions, 'vertical'));
	/** Fits as drawn → horizontal; else vertical (scaled down to 70% at most); else the transitions as a list. */
	const mode = $derived.by(() => {
		if (!avail || wide.width <= avail) return 'horizontal';
		if (tall.width * 0.7 <= avail) return 'vertical';
		return 'list';
	});
	const layout = $derived(mode === 'vertical' ? tall : wide);
	const W = $derived(layout.w);
	const extra = $derived(layout.orientation === 'horizontal' ? 40 : 0);
	const drawW = $derived(avail ? Math.min(layout.width, avail) : layout.width);
	const labelOf = (id: string) => states.find((s) => s.id === id)?.label ?? id;

	interface Edge {
		d: string;
		lx: number;
		ly: number;
		anchor: 'middle' | 'start';
	}
	/** `k` numbers the backward arrows (vertical layout: each gets its own lane right of the boxes). */
	function edge(t: DiagramTransition, k: number): Edge | null {
		const a = layout.pos.get(t.from);
		const b = layout.pos.get(t.to);
		if (!a || !b) return null;
		if (t.from === t.to) {
			const x = a.x + W / 2;
			return {
				d: `M${x - 10} ${a.y} C${x - 26} ${a.y - 30} ${x + 26} ${a.y - 30} ${x + 10} ${a.y}`,
				lx: x,
				ly: a.y - 24,
				anchor: 'middle',
			};
		}
		if (layout.orientation === 'vertical') {
			if (b.y > a.y) {
				const x1 = a.x + W / 2,
					y1 = a.y + H,
					x2 = b.x + W / 2,
					y2 = b.y;
				const my = (y1 + y2) / 2;
				return {
					d: `M${x1} ${y1} C${x1} ${my} ${x2} ${my} ${x2} ${y2}`,
					lx: (x1 + x2) / 2 + 6,
					ly: my + 4,
					anchor: 'start',
				};
			}
			if (b.y === a.y) {
				// Same row: arc below the two boxes.
				const x1 = a.x + W / 2,
					x2 = b.x + W / 2,
					y = a.y + H;
				return {
					d: `M${x1} ${y} C${x1} ${y + 30} ${x2} ${y + 30} ${x2} ${y}`,
					lx: (x1 + x2) / 2,
					ly: y + 30,
					anchor: 'middle',
				};
			}
			// Backward: up out of the box's top, right into its own lane past every box, up, back in from the right.
			const lane = layout.lane + k * 10;
			const x1 = a.x + W - 14 - k * 6,
				y1 = a.y - 12 - k * 5,
				x2 = b.x + W,
				y2 = b.y + H / 2;
			return {
				d: `M${x1} ${a.y} V${y1 + 6} Q${x1} ${y1} ${x1 + 6} ${y1} H${lane - 8} Q${lane} ${y1} ${lane} ${y1 - 8} V${y2 + 8} Q${lane} ${y2} ${lane - 8} ${y2} H${x2}`,
				lx: layout.lane + (layout.lanes - 1) * 10 + 6,
				ly: (y1 + y2) / 2 + 4,
				anchor: 'start',
			};
		}
		if (b.x > a.x) {
			const x1 = a.x + W,
				y1 = a.y + H / 2,
				x2 = b.x,
				y2 = b.y + H / 2;
			const mx = (x1 + x2) / 2;
			return {
				d: `M${x1} ${y1} C${mx} ${y1} ${mx} ${y2} ${x2} ${y2}`,
				lx: mx,
				ly: (y1 + y2) / 2 - 4,
				anchor: 'middle',
			};
		}
		// Backward or same column: arc below the boxes.
		const x1 = a.x + W / 2,
			y1 = a.y + H,
			x2 = b.x + W / 2,
			y2 = b.y + H;
		const dip = Math.max(y1, y2) + 34;
		return {
			d: `M${x1} ${y1} C${x1} ${dip} ${x2} ${dip} ${x2} ${y2}`,
			lx: (x1 + x2) / 2,
			ly: dip - 6,
			anchor: 'middle',
		};
	}
	/** Edges with labels nudged down until no two labels overlap. */
	const edges = $derived.by(() => {
		const placed: { x0: number; x1: number; y: number }[] = [];
		let k = 0;
		return transitions.map((t) => {
			const a = layout.pos.get(t.from);
			const b = layout.pos.get(t.to);
			const backward = layout.orientation === 'vertical' && a && b && t.from !== t.to && b.y < a.y;
			const e = edge(t, backward ? k++ : 0);
			if (!e || !t.label) return e;
			const len = shortLabel(t.label).length * CH * 0.95;
			const x0 = e.anchor === 'middle' ? e.lx - len / 2 : e.lx;
			let y = e.ly;
			for (let tries = 0; tries < 8; tries++) {
				const hit = placed.some((p) => Math.abs(p.y - y) < 13 && x0 < p.x1 && x0 + len > p.x0);
				if (!hit) break;
				y += 14;
			}
			placed.push({ x0, x1: x0 + len, y });
			return { ...e, ly: y };
		});
	});
</script>

<figure class="state-diagram" data-testid="state-diagram" data-mode={mode} bind:this={fig}>
	{#if mode === 'list'}
		<ul class="as-list" aria-label={title}>
			{#each states as s (s.id)}
				{@const out = transitions.filter((t) => t.from === s.id)}
				<li class:current={s.id === current}>
					<b>{s.label ?? s.id}</b>{#if s.terminal}<span class="chip">final</span
						>{/if}{#if s.id === current}<span class="chip accent">current</span>{/if}
					{#each out as t, i (i)}
						<span class="to"
							>→ {labelOf(t.to)}{#if t.label}<code>{t.label}</code>{/if}</span
						>
					{/each}
				</li>
			{/each}
		</ul>
	{:else}
		<svg
			viewBox="0 0 {layout.width} {layout.height + extra}"
			width={drawW}
			height={((layout.height + extra) * drawW) / layout.width}
			role="img"
			aria-labelledby="{uid}-title"
		>
			<title id="{uid}-title">{title}</title>
			<defs>
				<marker
					id="{uid}-arrow"
					viewBox="0 0 10 10"
					refX="9"
					refY="5"
					markerWidth="7"
					markerHeight="7"
					orient="auto-start-reverse"
				>
					<path d="M0 0 10 5 0 10z" fill="currentColor" />
				</marker>
			</defs>
			{#each edges as e, i (i)}
				{#if e}<path class="edge" d={e.d} marker-end="url(#{uid}-arrow)" />{/if}
			{/each}
			{#each transitions as t, i (i)}
				{@const e = edges[i]}
				{#if e && t.label}<text class="edge-label" x={e.lx} y={e.ly} text-anchor={e.anchor}
						>{shortLabel(t.label)}</text
					>{/if}
			{/each}
			{#each states as s (s.id)}
				{@const p = layout.pos.get(s.id)!}
				{@const name = s.label ?? s.id}
				<g class="node" class:terminal={s.terminal} class:current={s.id === current}>
					{#if name.length > 24}<title>{name}</title>{/if}
					<rect x={p.x} y={p.y} width={W} height={H} rx="6" />
					{#if s.terminal}<rect
							class="inner"
							x={p.x + 3}
							y={p.y + 3}
							width={W - 6}
							height={H - 6}
							rx="4"
						/>{/if}
					<text x={p.x + W / 2} y={p.y + H / 2 + 4} text-anchor="middle">{shortLabel(name)}</text>
				</g>
			{/each}
		</svg>
	{/if}
	<details>
		<summary>Transitions as a list</summary>
		<ul>
			{#each transitions as t, i (i)}
				<li>{labelOf(t.from)} → {labelOf(t.to)}{t.label ? ` (${t.label})` : ''}</li>
			{:else}
				<li>No transitions.</li>
			{/each}
		</ul>
	</details>
</figure>

<style>
	.state-diagram {
		margin: 0;
		min-width: 0;
		display: grid;
		gap: 8px;
	}
	svg {
		display: block;
		max-width: 100%;
		color: var(--muted);
		font: 12px var(--sans);
	}
	.edge {
		fill: none;
		stroke: currentColor;
		stroke-width: 1.4;
	}
	.edge-label {
		fill: var(--muted);
		font: 11px var(--mono);
		/* A halo so a verb stays legible where it crosses an arrow. */
		paint-order: stroke;
		stroke: var(--panel);
		stroke-width: 4px;
		stroke-linejoin: round;
	}
	.node rect {
		fill: var(--panel);
		stroke: var(--line-2);
		stroke-width: 1.4;
	}
	.node .inner {
		fill: none;
	}
	.node text {
		fill: var(--fg);
	}
	.node.terminal rect {
		stroke: var(--muted);
	}
	.node.current rect {
		stroke: var(--accent);
		stroke-width: 2.4;
	}
	details {
		font-size: 13px;
		color: var(--muted);
	}
	.as-list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 6px;
		font-size: 13px;
	}
	.as-list li {
		display: flex;
		flex-wrap: wrap;
		gap: 4px 8px;
		align-items: baseline;
		padding: 7px 10px;
		border: 1px solid var(--line-2);
		border-radius: 6px;
		background: var(--panel);
		min-width: 0;
	}
	.as-list li.current {
		border-color: var(--accent);
	}
	.as-list .to {
		color: var(--muted);
		overflow-wrap: anywhere;
		display: inline-flex;
		gap: 6px;
		align-items: baseline;
	}
	.as-list code {
		font: 11.5px var(--mono);
	}
</style>
