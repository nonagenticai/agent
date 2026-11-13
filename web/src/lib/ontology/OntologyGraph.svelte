<script lang="ts">
	// Read-only entity-relation graph (SvelteFlow). Deterministic lanes from layout.ts — the three inherited template
	// layers (locked) and a lane for what this tenant added — each framed by a lane band with its heading. No drag,
	// connect or delete — clicking a card selects the entity for the page's drawer, and the selected entity's
	// neighbourhood stays lit while the rest fades. Colours come from the app.css tokens through the --xy-* variables,
	// so the canvas follows light and dark mode with the rest of the app.
	import { SvelteFlow, Background, Controls, BackgroundVariant, MarkerType } from '@xyflow/svelte';
	import '@xyflow/svelte/dist/style.css';
	import type { OntologyEntity, OntologyRelation } from '$lib/api/types/ontology';
	import OntologyEntityNode from './OntologyEntityNode.svelte';
	import OntologyRelationEdge from './OntologyRelationEdge.svelte';
	import J11OntologyLane from './J11OntologyLane.svelte';
	import {
		layout,
		laneOf,
		neighbourhood,
		title,
		type EntityNode,
		type LaneBox,
		type LaneNode,
		type RelationEdge,
	} from './layout';

	let {
		entities,
		relations,
		selected = null,
		onselect,
	}: {
		entities: OntologyEntity[];
		relations: OntologyRelation[];
		selected?: string | null;
		onselect: (entity: OntologyEntity) => void;
	} = $props();

	const nodeTypes = { entity: OntologyEntityNode, lane: J11OntologyLane };
	const edgeTypes = { relation: OntologyRelationEdge };

	/** Heading + sub line of a lane, from the entities drawn in it. */
	function laneText(box: LaneBox): { title: string; sub: string } {
		const inLane = entities.filter((e) => laneOf(e) === box.lane);
		const one = (k: 'industry' | 'department') => {
			const v = [...new Set(inLane.map((e) => e[k]).filter((x): x is string => !!x))];
			return v.length === 1 ? title(v[0]!) : v.length ? 'several' : null;
		};
		const layer = box.layer ? ` · ${box.layer}` : '';
		switch (box.lane) {
			case 'generic':
				return { title: 'Base', sub: `Every job · inherited${layer}` };
			case 'industry':
				return { title: 'Industry', sub: `${one('industry') ?? 'Industry'} · inherited${layer}` };
			case 'industry_dept':
				return {
					title: 'Specialized',
					sub: `${one('industry') ?? 'Industry'} × ${one('department') ?? 'department'} · inherited`,
				};
			case 'tenant':
				return { title: 'Added by you', sub: `Refinements · editable${layer}` };
		}
	}

	// Start fitted to the canvas width and aligned to the top (lane headings visible); the map is taller than the
	// canvas, so the wheel pans it (ctrl/pinch zooms). Re-fitted when the set of entities changes (a filter).
	let w = $state(0);
	const PAD_L = 115;
	const viewportFor = (width: number) => {
		const ls = layout(entities, relations).lanes;
		const minX = Math.min(...ls.map((l) => l.x)) - PAD_L;
		const maxX = Math.max(...ls.map((l) => l.x + l.w)) + 30;
		const minY = Math.min(...ls.map((l) => l.y));
		const zoom = Math.max(0.5, Math.min(1, width / (maxX - minX)));
		return { x: Math.max(0, (width - (maxX - minX) * zoom) / 2) - minX * zoom, y: 12 - minY * zoom, zoom };
	};
	const setKey = $derived(entities.map((e) => e.name).join('|'));

	let nodes = $state.raw<(EntityNode | LaneNode)[]>([]);
	let edges = $state.raw<RelationEdge[]>([]);
	$effect(() => {
		const next = layout(entities, relations);
		const lit = selected ? neighbourhood(selected, relations) : null;
		const lanes: LaneNode[] = next.lanes.map((b) => ({
			id: `lane:${b.lane}`,
			type: 'lane',
			position: { x: b.x, y: b.y },
			width: b.w,
			height: b.h,
			zIndex: -1,
			selectable: false,
			draggable: false,
			focusable: false,
			data: { lane: b.lane, count: b.count, locked: b.lane !== 'tenant', ...laneText(b) },
		}));
		nodes = [
			...lanes,
			...next.nodes.map((n) => ({
				...n,
				selected: n.id === selected,
				data: { ...n.data, dim: lit ? !lit.has(n.id) : false },
			})),
		];
		edges = next.edges.map((e) => {
			const on = selected && (e.source === selected || e.target === selected);
			return {
				...e,
				data: { ...e.data!, focus: selected ? (on ? 'lit' : 'dim') : undefined },
				markerEnd: {
					type: MarkerType.ArrowClosed,
					width: 14,
					height: 14,
					color: on ? 'var(--accent)' : 'var(--faint)',
				},
			};
		});
	});
</script>

<div class="onto-graph" dir="ltr" bind:clientWidth={w}>
	{#if w > 0}
		{#key setKey}
			<SvelteFlow
				bind:nodes
				bind:edges
				{nodeTypes}
				{edgeTypes}
				initialViewport={viewportFor(w)}
				panOnScroll
				minZoom={0.3}
				nodesDraggable={false}
				nodesConnectable={false}
				elementsSelectable={true}
				deleteKey={null}
				proOptions={{ hideAttribution: true }}
				onnodeclick={({ node }) => {
					if (node.type === 'entity') onselect((node as EntityNode).data.entity);
				}}
			>
				<Background variant={BackgroundVariant.Dots} gap={18} size={1} />
				<Controls position="bottom-left" showLock={false} />
			</SvelteFlow>
		{/key}
	{/if}
</div>

<style>
	.onto-graph {
		position: relative;
		width: 100%;
		height: 100%;
		min-height: 620px;
		background: var(--bg);
		--xy-background-color: var(--bg);
		--xy-background-pattern-dots-color: var(--line);
		--xy-edge-stroke: var(--faint);
		--xy-edge-stroke-selected: var(--accent);
		--xy-edge-label-background-color: var(--panel);
		--xy-edge-label-color: var(--muted);
		--xy-handle-background-color: var(--panel);
		--xy-handle-border-color: var(--line);
		--xy-controls-button-background-color: var(--panel);
		--xy-controls-button-background-color-hover: var(--sunk);
		--xy-controls-button-color: var(--fg);
		--xy-controls-button-color-hover: var(--fg);
		--xy-controls-button-border-color: var(--line);
		--xy-controls-box-shadow: 0 1px 3px color-mix(in srgb, var(--fg) 15%, transparent);
		--xy-attribution-background-color: transparent;
	}
	.onto-graph :global(.svelte-flow) {
		background: var(--bg);
	}
	.onto-graph :global(.svelte-flow__controls-button svg) {
		fill: currentColor;
	}
	.onto-graph :global(.svelte-flow__edge-path) {
		stroke-width: 1.25;
	}
</style>
