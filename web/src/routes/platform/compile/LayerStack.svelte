<script lang="ts">
	// P5: the compiled layer stack as five scope lanes in compile order. Each applied layer shows why it is in the
	// stack and what its rows did (add / patch / tombstone); empty lanes say what would have to be picked to fill them.
	import type { LayerScope } from '$lib/api/types/platform';
	import type { StackLayer } from '$lib/platform/compileDiff';
	import { layerHref } from '$lib/platform/layers-P4';

	let { stack }: { stack: StackLayer[] } = $props();

	const LANES: { scope: LayerScope; label: string; empty: string }[] = [
		{ scope: 'base', label: 'Base', empty: 'No base layer' },
		{ scope: 'profile', label: 'Business model', empty: 'No axis rule fired — pick a business model' },
		{ scope: 'industry', label: 'Industry', empty: 'No industry picked' },
		{
			scope: 'industry_dept',
			label: 'Department (specialises an archetype)',
			empty: 'No department picked',
		},
		{ scope: 'job', label: 'Job', empty: 'No job — ontology only' },
	];
	const lanes = $derived(LANES.map((l) => ({ ...l, layers: stack.filter((s) => s.scope === l.scope) })));
</script>

<ol class="stack-lanes" aria-label="Layers in compile order">
	{#each lanes as lane (lane.scope)}
		<li class="lane scope-{lane.scope}" class:off={!lane.layers.length}>
			<span class="lane-name">{lane.label}</span>
			<div class="lane-layers">
				{#each lane.layers as l (l.label)}
					<div class="layer" data-testid="stack-layer">
						<div class="layer-id">
							<a href={layerHref(l.label)} class="mono" title="Open this layer in Layers">{l.label}</a>
							{#if l.tag}<span class="chip">{l.tag.split('@')[1]}</span>{:else}<span
									class="chip proposed"
									title="Current revisions, not yet in a template tag">untagged</span
								>{/if}
						</div>
						<span class="because">{l.because}</span>
						<span class="ops" aria-label="Rows applied">
							{#if l.ops.add}<span class="op add" title="rows that add an item">+{l.ops.add}</span>{/if}
							{#if l.ops.patch}<span class="op patch" title="rows that patch an earlier item"
									>~{l.ops.patch}</span
								>{/if}
							{#if l.ops.tombstone}<span class="op tomb" title="rows that remove an earlier item"
									>×{l.ops.tombstone}</span
								>{/if}
						</span>
					</div>
				{:else}
					<span class="lane-empty">{lane.empty}</span>
				{/each}
			</div>
		</li>
	{/each}
</ol>
<p class="note">
	Applied top to bottom; a later layer wins. <span class="op add">+ add</span> sets an item,
	<span class="op patch">~ patch</span> replaces named fields, <span class="op tomb">× tombstone</span> removes
	it (an entity takes its attributes and relations along).
</p>

<style>
	.stack-lanes {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 6px;
		position: relative;
	}
	.lane {
		--lane: var(--faint);
		display: grid;
		grid-template-columns: 120px minmax(0, 1fr);
		gap: 10px;
		align-items: start;
		border-left: 3px solid var(--lane);
		padding: 6px 0 6px 10px;
		background: color-mix(in srgb, var(--lane) 5%, transparent);
		border-radius: 0 6px 6px 0;
	}
	.lane.off {
		background: transparent;
		border-left-style: dashed;
		--lane: var(--line);
	}
	.scope-base {
		--lane: var(--accent);
	}
	.scope-profile {
		--lane: var(--work);
	}
	.scope-industry {
		--lane: var(--cust);
	}
	.scope-industry_dept {
		--lane: var(--tool);
	}
	.scope-job {
		--lane: var(--ok);
	}
	.lane-name {
		font: 600 11px/1.2 var(--cond);
		letter-spacing: 0.08em;
		text-transform: uppercase;
		color: var(--muted);
		padding-top: 4px;
	}
	.lane-layers {
		display: grid;
		gap: 4px;
	}
	.layer {
		display: grid;
		grid-template-columns: minmax(0, 1.4fr) minmax(0, 1fr) auto;
		gap: 4px 12px;
		align-items: center;
		background: var(--panel);
		border: 1px solid var(--line-2);
		border-radius: 6px;
		padding: 5px 10px;
	}
	.layer-id {
		display: flex;
		align-items: center;
		gap: 6px;
		min-width: 0;
		flex-wrap: wrap;
	}
	.layer-id a {
		overflow-wrap: anywhere;
	}
	.because {
		color: var(--muted);
		font-size: 12.5px;
	}
	.ops {
		display: inline-flex;
		gap: 8px;
		justify-content: flex-end;
	}
	.op {
		font: 600 12px var(--mono);
	}
	.op.add {
		color: var(--accent);
	}
	.op.patch {
		color: var(--tool);
	}
	.op.tomb {
		color: var(--err);
	}
	.lane-empty {
		color: var(--faint);
		font-size: 12.5px;
		padding: 4px 0;
	}
	@media (max-width: 640px) {
		.lane {
			grid-template-columns: minmax(0, 1fr);
			gap: 4px;
		}
		.layer {
			grid-template-columns: minmax(0, 1fr) auto;
		}
		.because {
			grid-column: 1 / -1;
			grid-row: 2;
		}
	}
</style>
