<script lang="ts">
	// J11 lane band behind one column of the ontology graph: which layer the cards come from and whether this tenant
	// can change them (inherited = locked, read-only here; "Added by you" = this tenant's refinements). Not
	// selectable; it only frames the cards.
	import type { NodeProps } from '@xyflow/svelte';
	import type { LaneNode } from './layout';

	let { data, width, height }: NodeProps<LaneNode> = $props();
</script>

<div
	class="j11-lane {data.lane}"
	class:locked={data.locked}
	style:width="{width}px"
	style:height="{height}px"
	data-lane={data.lane}
>
	<div class="j11-lane-head">
		<span class="j11-lane-title">
			{#if data.locked}
				<svg class="j11-lock" viewBox="0 0 24 24" width="12" height="12" aria-hidden="true">
					<path
						d="M7 11V8a5 5 0 0 1 10 0v3M5 11h14v10H5z"
						fill="none"
						stroke="currentColor"
						stroke-width="2.2"
						stroke-linejoin="round"
					/>
				</svg>
			{:else}
				<span class="j11-plus" aria-hidden="true">+</span>
			{/if}
			{data.title}
			<span class="j11-lane-count">{data.count}</span>
		</span>
		<span class="j11-lane-sub">{data.sub}</span>
	</div>
	{#if data.count === 0}
		<p class="j11-lane-empty">
			Nothing added yet. Entities you add appear here as drafts and go live when you publish a release.
		</p>
	{/if}
</div>

<style>
	.j11-lane {
		--lane: var(--line);
		box-sizing: border-box;
		border-radius: 12px;
		border: 1px solid color-mix(in srgb, var(--lane) 30%, transparent);
		background: color-mix(in srgb, var(--lane) 5%, transparent);
		pointer-events: none;
	}
	.j11-lane.generic {
		--lane: var(--cust);
	}
	.j11-lane.industry {
		--lane: var(--tool);
	}
	.j11-lane.industry_dept {
		--lane: var(--work);
	}
	.j11-lane.tenant {
		--lane: var(--accent);
		border-style: dashed;
		border-width: 1.5px;
		border-color: color-mix(in srgb, var(--accent) 55%, transparent);
		background: color-mix(in srgb, var(--accent) 7%, transparent);
	}
	.j11-lane-head {
		display: flex;
		flex-direction: column;
		gap: 1px;
		padding: 9px 14px 0;
	}
	.j11-lane-title {
		display: flex;
		align-items: center;
		gap: 6px;
		font: 700 12.5px/1.3 var(--sans);
		color: var(--lane);
		letter-spacing: 0.02em;
	}
	.j11-lane-count {
		font: 600 10.5px/1.5 var(--mono);
		padding: 0 6px;
		border-radius: 999px;
		color: var(--lane);
		background: color-mix(in srgb, var(--lane) 14%, transparent);
	}
	.j11-plus {
		font: 700 14px/1 var(--sans);
	}
	.j11-lane-sub {
		font: 11px/1.4 var(--sans);
		color: var(--muted);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.j11-lane-empty {
		margin: 18px 14px 0;
		font: 12px/1.5 var(--sans);
		color: var(--muted);
	}
</style>
