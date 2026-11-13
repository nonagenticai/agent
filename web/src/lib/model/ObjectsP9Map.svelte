<script lang="ts">
	// P9 object map: objects on a grid (base handful above, industry/optional additions below), relations as lines.
	// The selected object, its relations and neighbours are highlighted; everything else recedes.
	import type { ObjectMapNode, ObjectRelation } from '$lib/api/types/model';
	import { NODE_H, NODE_W, layoutObjectMap, neighbours, routeEdge } from './objectsP9';

	let {
		nodes,
		relations,
		selected,
		dim,
		onselect,
	}: {
		nodes: ObjectMapNode[];
		relations: ObjectRelation[];
		selected: string;
		dim: (n: ObjectMapNode) => boolean;
		onselect: (object: string) => void;
	} = $props();

	const layout = $derived(layoutObjectMap(nodes, relations));
	const near = $derived(neighbours(selected, relations));

	const edges = $derived(
		relations
			.filter((r) => layout.placed[r.subject] && layout.placed[r.object])
			.map((r, i) => {
				const on = r.subject === selected || r.object === selected;
				if (r.subject === r.object) {
					const p = layout.placed[r.subject]!;
					const x = p.x + NODE_W - 18;
					const y = p.y;
					return {
						id: i,
						r,
						on,
						self: true,
						at: null,
						d: `M ${x} ${y} c 0 -22 30 -22 30 0 c 0 6 -4 10 -12 ${NODE_H / 2 - 4}`,
						lx: x + 15,
						ly: y - 20,
					};
				}
				const route = routeEdge(layout, r.subject, r.object);
				const m = route.at(0.5);
				return {
					id: i,
					r,
					on,
					self: false,
					d: route.d,
					at: route.at,
					lx: m.x,
					ly: m.y,
				};
			})
			// Highlighted edges last, so they draw on top.
			.sort((x, y) => Number(x.on) - Number(y.on)),
	);

	const labelText = (r: ObjectRelation) => `${r.predicate} · ${r.cardinality}`;
	const labelW = (t: string) => t.length * 5.8 + 10;

	/** Labels for the selected object's relations, slid along their line until they clear nodes and each other. */
	const labels = $derived.by(() => {
		type Box = { x: number; y: number; w: number; h: number };
		const boxes: Box[] = Object.values(layout.placed).map((p) => ({ x: p.x, y: p.y, w: NODE_W, h: NODE_H }));
		const overlap = (a: Box, b: Box) =>
			Math.max(0, Math.min(a.x + a.w, b.x + b.w) - Math.max(a.x, b.x)) *
			Math.max(0, Math.min(a.y + a.h, b.y + b.h) - Math.max(a.y, b.y));
		const out: { id: number; text: string; x: number; y: number; w: number }[] = [];
		for (const e of edges.filter((x) => x.on)) {
			const text = labelText(e.r);
			const w = labelW(text);
			let x = e.lx;
			let y = e.ly;
			if (e.at) {
				// Least-covered spot along the path (nodes and labels already placed).
				let bestArea = Infinity;
				// On the line first; beside it (above/below) only when every spot on it is covered.
				const tries = [0, -36, 36].flatMap((dy) =>
					[0.5, 0.4, 0.6, 0.3, 0.7, 0.2, 0.8].map((t) => ({ t, dy })),
				);
				for (const { t, dy } of tries) {
					const q = e.at(t);
					const p = { x: q.x, y: q.y + dy };
					const b = { x: p.x - w / 2, y: p.y - 9, w, h: 17 };
					const area = boxes.reduce((sum, o) => sum + overlap(o, b), 0);
					if (area < bestArea - 1) {
						bestArea = area;
						x = p.x;
						y = p.y;
					}
					if (area === 0) break;
				}
			}
			boxes.push({ x: x - w / 2, y: y - 9, w, h: 17 });
			out.push({ id: e.id, text, x, y, w });
		}
		return out;
	});

	function key(e: KeyboardEvent, o: string) {
		if (e.key === 'Enter' || e.key === ' ') {
			e.preventDefault();
			onselect(o);
		}
	}
	const opsOf = (n: ObjectMapNode) => n.operations.length;
</script>

<div class="map-wrap scroll-x">
	<svg
		viewBox="0 0 {layout.width} {layout.height}"
		width={layout.width}
		role="group"
		aria-label="Object relationship map"
		data-testid="object-map"
	>
		<defs>
			<marker
				id="p9-arrow"
				viewBox="0 0 10 10"
				refX="9"
				refY="5"
				markerWidth="7"
				markerHeight="7"
				orient="auto"
			>
				<path d="M0 0 L10 5 L0 10 z" class="arrowhead" />
			</marker>
			<marker
				id="p9-arrow-on"
				viewBox="0 0 10 10"
				refX="9"
				refY="5"
				markerWidth="7"
				markerHeight="7"
				orient="auto"
			>
				<path d="M0 0 L10 5 L0 10 z" class="arrowhead on" />
			</marker>
		</defs>

		{#if layout.addRows}
			<line class="band" x1="8" x2={layout.width - 8} y1={layout.addTop - 22} y2={layout.addTop - 22} />
			<text class="band-label" x="12" y={layout.addTop - 8}
				>INDUSTRY &amp; OPTIONAL ADDITIONS — selected by the business model</text
			>
		{/if}

		<g class="edges">
			{#each edges as e (e.id)}
				<path
					d={e.d}
					class="edge"
					class:on={e.on}
					class:faded={!e.on}
					marker-end={e.on ? 'url(#p9-arrow-on)' : 'url(#p9-arrow)'}
				>
					<title>{e.r.subject} {e.r.predicate} {e.r.object} ({e.r.cardinality})</title>
				</path>
			{/each}
		</g>

		{#each nodes as n (n.object)}
			{@const p = layout.placed[n.object]}
			{#if p}
				{@const isSel = n.object === selected}
				<g
					class="node {n.tier}"
					class:sel={isSel}
					class:near={near.has(n.object)}
					class:dim={dim(n)}
					transform="translate({p.x} {p.y})"
					role="button"
					tabindex="0"
					aria-pressed={isSel}
					aria-label="Select {n.object}"
					onclick={() => onselect(n.object)}
					onkeydown={(e) => key(e, n.object)}
				>
					<rect class="box" width={NODE_W} height={NODE_H} rx="6" />
					<rect class="tier" width="4" height={NODE_H} rx="2" />
					<text class="name" x="12" y="19">{n.object}</text>
					<text class="meta" x="12" y="35"
						>{n.states.length ? `${n.states.length} states` : 'no lifecycle'} · {opsOf(n)} ops</text
					>
					{#if n.in_retail_job}<circle class="retail" cx={NODE_W - 9} cy="9" r="3.5" />{/if}
				</g>
			{/if}
		{/each}

		<g class="labels">
			{#each labels as l (l.id)}
				<g transform="translate({l.x} {l.y})">
					<rect class="lbl-bg" x={-l.w / 2} y="-9" width={l.w} height="17" rx="8" />
					<text class="lbl" text-anchor="middle" y="3.5">{l.text}</text>
				</g>
			{/each}
		</g>
	</svg>
</div>

<style>
	.map-wrap {
		overflow-x: auto;
		margin: 0 -4px;
	}
	svg {
		display: block;
		max-width: 100%;
		height: auto;
		min-width: 620px;
		margin: 0 auto;
	}
	.band {
		stroke: var(--line);
		stroke-dasharray: 4 4;
	}
	.band-label {
		font: 600 10.5px var(--cond);
		letter-spacing: 0.06em;
		fill: var(--muted);
	}
	.edge {
		fill: none;
		stroke: var(--line);
		stroke-width: 1.3;
		transition: opacity 0.15s;
	}
	.edge.faded {
		opacity: 0.75;
	}
	.edge.on {
		stroke: var(--accent);
		stroke-width: 2;
		opacity: 1;
	}
	.arrowhead {
		fill: var(--line);
	}
	.arrowhead.on {
		fill: var(--accent);
	}
	.node {
		cursor: pointer;
		outline: none;
		transition: opacity 0.15s;
	}
	.node .box {
		fill: var(--panel);
		stroke: var(--line);
		stroke-width: 1;
	}
	.node .tier {
		fill: var(--ok);
	}
	.node.industry .tier {
		fill: var(--tool);
	}
	.node.optional .tier {
		fill: var(--muted);
	}
	.node:hover .box,
	.node:focus-visible .box {
		stroke: var(--accent);
	}
	.node.near .box {
		stroke: color-mix(in srgb, var(--accent) 55%, var(--line));
	}
	.node.sel .box {
		fill: color-mix(in srgb, var(--accent) 10%, var(--panel));
		stroke: var(--accent);
		stroke-width: 2;
	}
	.node.dim {
		opacity: 0.28;
	}
	.name {
		font: 600 12.5px var(--mono);
		fill: var(--fg);
	}
	.meta {
		font: 11px var(--sans);
		fill: var(--muted);
	}
	.retail {
		fill: var(--cust);
	}
	.lbl-bg {
		fill: var(--panel);
		stroke: color-mix(in srgb, var(--accent) 45%, transparent);
	}
	.lbl {
		font: 10.5px var(--sans);
		fill: var(--accent);
	}
</style>
