<script lang="ts">
	// P13: every live job selection × every standard value — how selections combine dimensions, and which values
	// nobody picks yet (an empty column is still a value every tenant can choose).
	import type { Dimension, DimensionSelectionRow, DimensionUsage } from '$lib/api/types/platform';
	import '$lib/phone-stack.css';
	import MatrixTable from '$lib/ui/MatrixTable.svelte';
	import { selectionId } from '$lib/platform/P13Dimensions';

	let { dims, usage }: { dims: Dimension[]; usage: DimensionUsage } = $props();

	type Col = { dim: Dimension; key: string; label: string; first: boolean };
	const columns = $derived<Col[]>(
		dims.flatMap((d) => d.values.map((v, i) => ({ dim: d, key: v.key, label: v.label, first: i === 0 }))),
	);
</script>

<!-- Phone/tablet (≤860px, phone-stack.css): one block per job selection, its picked values as chips. -->
<div class="ph-stack p13m">
	<MatrixTable
		rows={usage.selections}
		{columns}
		rowKey={(s: DimensionSelectionRow) => selectionId(s)}
		colKey={(c: Col) => `${c.dim.key}=${c.key}`}
		caption="Job selections by dimension value"
		corner="Tenant · job"
		empty="No job has a selection yet."
	>
		{#snippet rowHeader(s)}
			<span class="rh">
				<a href={`/platform/tenants/${s.tenant_id}`}>{s.tenant_name}</a>
				<span class="cell-sub">{s.job_label}</span>
			</span>
		{/snippet}
		{#snippet colHeader(c)}
			<span class="col" class:first={c.first}>
				<span class="grp">{c.first ? c.dim.label : ' '}</span>
				<span class="v" title={c.key}>{c.label}</span>
			</span>
		{/snippet}
		{#snippet cell(s, c)}
			{#if (s.values[c.dim.key] ?? []).includes(c.key)}
				<span class="on" title={`${s.tenant_name} picks ${c.dim.label} ${c.label}`}
					><span class="dot">●</span><span class="sr">selected</span><span class="ph-only chip accent"
						>{c.dim.label}: {c.label}</span
					></span
				>
			{:else}
				<span class="off" aria-hidden="true">·</span>
			{/if}
		{/snippet}
	</MatrixTable>
</div>

<style>
	@media (max-width: 860px) {
		.p13m :global(.tbl-wrap .matrix td:has(> .off)) {
			display: none;
		}
		.p13m :global(.tbl-wrap .matrix td) {
			flex: 0 0 auto;
		}
		.p13m .dot {
			display: none;
		}
		.rh {
			white-space: normal;
		}
	}
	.rh {
		display: grid;
		text-transform: none;
		letter-spacing: 0;
		font: 500 12.5px/1.3 var(--sans);
		white-space: nowrap;
	}
	.col {
		display: grid;
		gap: 2px;
	}
	.grp {
		color: var(--accent);
		min-height: 1em;
	}
	.v {
		text-transform: none;
		letter-spacing: 0;
		font: 500 12px/1.2 var(--sans);
		color: var(--fg);
	}
	.on {
		color: var(--accent);
		font-size: 14px;
	}
	.off {
		color: var(--faint);
	}
	.sr {
		position: absolute;
		width: 1px;
		height: 1px;
		overflow: hidden;
		clip: rect(0 0 0 0);
	}
</style>
