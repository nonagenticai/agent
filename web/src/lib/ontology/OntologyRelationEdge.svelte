<script lang="ts">
	// Relation edge: the predicate as the label with the cardinality set small beside it. The route (with its waypoint
	// for a link that skips a column) and the label's spot on it come from geometry.ts / layout.ts, which placed every label so none overlaps a card or another label.
	import { BaseEdge, EdgeLabel, Position, type EdgeProps } from '@xyflow/svelte';
	import { route, routeAt, routeD, type Side } from './geometry';
	import type { RelationEdge } from './layout';

	let {
		sourceX,
		sourceY,
		targetX,
		targetY,
		sourcePosition,
		targetPosition,
		data,
		markerEnd,
		selected,
	}: EdgeProps<RelationEdge> = $props();
	const side = (p: Position): Side => (p === Position.Left ? 'left' : 'right');
	const r = $derived(
		route(
			{ x: sourceX, y: sourceY },
			side(sourcePosition),
			{ x: targetX, y: targetY },
			side(targetPosition),
			data?.via ?? null,
		),
	);
	const at = $derived(routeAt(r, data?.label.t ?? 0.5));
</script>

<BaseEdge
	path={routeD(r)}
	{markerEnd}
	class="onto-edge{selected ? ' on' : ''}{data?.focus ? ` ${data.focus}` : ''}{data?.relation.origin ===
	'tenant'
		? ' tenant'
		: ''}"
/>
{#if data}
	<EdgeLabel
		x={at.x + data.label.dx}
		y={at.y + data.label.dy}
		class="onto-edge-label{data.focus ? ` ${data.focus}` : ''}{data.relation.origin === 'tenant'
			? ' tenant'
			: ''}"
	>
		<span class="pred">{data.relation.predicate}</span>
		<span class="card">{data.relation.cardinality}</span>
	</EdgeLabel>
{/if}

<style>
	:global(.onto-edge-label) {
		display: inline-flex;
		align-items: baseline;
		gap: 4px;
		padding: 1px 5px;
		border-radius: 4px;
		border: 1px solid var(--line-2);
		background: var(--panel);
		color: var(--muted);
		font: 10.5px/1.4 var(--mono);
		white-space: nowrap;
	}
	:global(.onto-edge-label.tenant) {
		color: var(--accent);
		border-color: color-mix(in srgb, var(--accent) 45%, transparent);
	}
	:global(.onto-edge-label.dim) {
		opacity: 0.18;
	}
	:global(.onto-edge-label.lit) {
		color: var(--fg);
		border-color: var(--accent);
	}
	:global(.svelte-flow__edge-path.onto-edge.tenant) {
		stroke: var(--accent);
		stroke-dasharray: 5 3;
	}
	:global(.svelte-flow__edge-path.onto-edge.dim) {
		opacity: 0.12;
	}
	:global(.svelte-flow__edge-path.onto-edge.lit) {
		stroke: var(--accent);
		stroke-width: 2;
	}
	.card {
		font-size: 9px;
		color: var(--faint);
	}
</style>
