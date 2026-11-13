<script lang="ts">
	// KC completeness matrix (plan kc-layer-contract §5): every layer × object type against the layer contract.
	// Bold 0 = a kind the contract requires that has no rows; n✗ on a job layer = a kind that belongs above job level.
	// Rows load on demand (one `layerRows` call per layer), so the P4 page stays light until someone asks.
	import type { DepartmentArchetype, Layer, LayerRow } from '$lib/api/types/platform';
	import Callout from '$lib/ui/Callout.svelte';
	import { MATRIX_COLUMNS, completenessMatrix, matrixTotals, type MatrixCell } from './completeness';

	let {
		layers,
		archetypes = [],
		load,
		onopen,
	}: {
		layers: Layer[];
		archetypes?: Pick<DepartmentArchetype, 'key' | 'activities' | 'knowledge_kinds'>[];
		/** Current rows of one layer. */
		load: (layerId: string) => Promise<LayerRow[]>;
		/** Open a layer in the explorer. */
		onopen?: (layer: Layer) => void;
	} = $props();

	let rows = $state<LayerRow[] | null>(null);
	let busy = $state(false);
	let error = $state('');
	let onlyViolations = $state(false);

	const matrix = $derived(rows ? completenessMatrix(layers, rows, archetypes) : []);
	const totals = $derived(matrixTotals(matrix));
	const shown = $derived(onlyViolations ? matrix.filter((r) => r.judged && !r.ok) : matrix);

	async function build() {
		busy = true;
		error = '';
		try {
			const all = await Promise.all(layers.filter((l) => l.scope !== 'profile').map((l) => load(l.id)));
			rows = all.flat();
		} catch (e) {
			error = (e as Error).message;
		} finally {
			busy = false;
		}
	}

	function cellTitle(kind: string, c: MatrixCell): string {
		if (c.state === 'empty') return `${kind}: required, empty`;
		if (c.state === 'misplaced') return `${kind}: ${c.count} on a job layer — belongs above job level`;
		return `${kind}: ${c.count}${c.required ? ' (required)' : ''}`;
	}
</script>

<div class="kc" data-testid="completeness">
	<div class="kc-bar">
		<button
			type="button"
			class="table-action"
			onclick={build}
			disabled={busy}
			data-testid="completeness-build">{busy ? 'Building…' : rows ? 'Rebuild' : 'Build matrix'}</button
		>
		{#if rows}
			<label class="small"
				><input type="checkbox" bind:checked={onlyViolations} /> Only layers with violations</label
			>
			<span class="small muted" data-testid="completeness-totals"
				>{totals.ok} of {totals.judged} layers ok · {totals.empty} required kinds empty · {totals.misplaced} kinds
				above job level</span
			>
		{/if}
	</div>
	<p class="legend small muted">
		<span class="cell empty">0</span> required by the contract, no rows ·
		<span class="cell misplaced">2✗</span> on a job layer, belongs above job level ·
		<span class="cell none">·</span> not required, none · Profiles are not judged; region sub-layers are shown
		but optional.
	</p>
	{#if error}<Callout kind="err" title="Could not build the matrix">{error}</Callout>{/if}
	{#if rows}
		<div class="tbl-wrap kc-wrap">
			<table class="tbl dense kc-table" data-testid="completeness-table">
				<caption class="sr-only">Layer completeness against the KC layer contract</caption>
				<thead>
					<tr>
						<th scope="col" class="row-head">Layer</th>
						{#each MATRIX_COLUMNS as col (col.kind)}
							<th scope="col"><abbr title={col.kind}>{col.short}</abbr></th>
						{/each}
						<th scope="col">Contract</th>
					</tr>
				</thead>
				<tbody>
					{#each shown as r (r.id)}
						<tr class:bad={r.judged && !r.ok} data-testid="completeness-row" data-layer={r.label}>
							<th scope="row" class="row-head">
								{#if r.layer && onopen}
									{@const l = r.layer}
									<button type="button" class="link mono" onclick={() => onopen(l)}>{r.label}</button>
								{:else}
									<span class="mono">{r.label}</span>
									{#if !r.layer}<span class="small muted">(catalogue only)</span>{/if}
								{/if}
							</th>
							{#each r.cells as c (c.kind)}
								<td class="num"
									><span class="cell {c.state}" title={cellTitle(c.kind, c)}
										>{#if c.state === 'misplaced'}{c.count}✗{:else if c.state === 'none'}·{:else}{c.count}{/if}</span
									>{#if c.violation}<span class="sr-only">
											— {c.state === 'empty' ? 'required, empty' : 'above job level'}</span
										>{/if}</td
								>
							{/each}
							<td class="verdict" class:ok={r.ok && r.judged}>{r.verdict}</td>
						</tr>
					{:else}
						<tr><td colspan={MATRIX_COLUMNS.length + 2} class="muted">No layer violates the contract.</td></tr
						>
					{/each}
				</tbody>
			</table>
		</div>
	{/if}
</div>

<style>
	.kc-bar {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 8px 14px;
	}
	.legend {
		margin: 8px 0;
	}
	.kc-wrap {
		overflow-x: auto;
		max-width: 100%;
	}
	.kc-table th.row-head {
		position: sticky;
		left: 0;
		background: var(--panel);
		z-index: 1;
		text-align: left;
		white-space: nowrap;
	}
	.kc-table td.num {
		text-align: center;
	}
	.kc-table abbr {
		text-decoration: none;
	}
	.cell.empty {
		font-weight: 700;
		color: var(--err);
	}
	.cell.misplaced {
		font-weight: 700;
		color: var(--warn);
	}
	.cell.none {
		color: var(--faint);
	}
	tr.bad .verdict {
		color: var(--err);
	}
	.verdict {
		white-space: nowrap;
	}
	.verdict.ok {
		color: var(--ok);
	}
	.link {
		background: none;
		border: 0;
		padding: 0;
		color: var(--accent);
		cursor: pointer;
		text-align: left;
	}
	.link:hover {
		text-decoration: underline;
	}
	.sr-only {
		position: absolute;
		width: 1px;
		height: 1px;
		overflow: hidden;
		clip: rect(0 0 0 0);
		white-space: nowrap;
	}
</style>
