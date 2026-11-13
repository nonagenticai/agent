<script lang="ts">
	// Where an effective item came from: one chip per overlay row in layer order, each linking to that row in the
	// layer explorer (`/platform/layers?row=`). Restyled from strata_configurator's teal `.origin` onto `.chip`.
	import type { Origin } from '$lib/api/types/platform';
	import { layerLabel } from './model';

	let { origins = [] }: { origins?: Origin[] } = $props();
	const VARIANT = { add: 'accent', patch: 'tool', tombstone: 'err' } as const;
</script>

<span class="prov" aria-label="Provenance">
	{#each origins as origin, i (origin.row_id)}
		{#if i}<span class="arrow" aria-hidden="true">→</span>{/if}
		<a
			class="chip {VARIANT[origin.op]}"
			href={`/platform/layers?row=${encodeURIComponent(origin.row_id)}`}
			title={`Open row ${origin.row_id} in the layer explorer`}
			>{layerLabel(origin.layer)} <strong>{origin.op}</strong></a
		>
	{:else}
		<span class="muted">—</span>
	{/each}
</span>

<style>
	.prov {
		display: inline-flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 4px;
	}
	.prov a {
		text-decoration: none;
	}
	.prov a:hover {
		text-decoration: underline;
	}
	.prov strong {
		font-weight: 600;
	}
	.arrow {
		color: var(--faint);
		font-size: 12px;
	}
</style>
