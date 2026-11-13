<script lang="ts" generics="R, C">
	// Rows × columns grid with a cell snippet: the route matrix (operations × env), the journey grid (stages ×
	// activities/channels), gate verdict tables. The first column stays visible while the grid scrolls sideways.
	//   <MatrixTable rows={ops} columns={envs} rowKey={(o) => o.key} colKey={(e) => e} caption="Routes">
	//     {#snippet rowHeader(o)}{o.key}{/snippet}
	//     {#snippet colHeader(e)}{e}{/snippet}
	//     {#snippet cell(o, e)}…{/snippet}
	//   </MatrixTable>
	import type { Snippet } from 'svelte';

	let {
		rows,
		columns,
		rowKey,
		colKey,
		rowHeader,
		colHeader,
		cell,
		caption,
		corner = '',
		empty = 'Nothing to show.',
	}: {
		rows: R[];
		columns: C[];
		rowKey: (r: R) => string;
		colKey: (c: C) => string;
		rowHeader: Snippet<[R]>;
		colHeader?: Snippet<[C]>;
		cell: Snippet<[R, C]>;
		caption?: string;
		corner?: string;
		empty?: string;
	} = $props();
</script>

{#if rows.length === 0 || columns.length === 0}
	<div class="empty">{empty}</div>
{:else}
	<div class="tbl-wrap matrix-wrap">
		<table class="tbl dense matrix" data-testid="matrix">
			{#if caption}<caption class="sr-only">{caption}</caption>{/if}
			<thead>
				<tr>
					<th scope="col" class="corner">{corner}</th>
					{#each columns as c (colKey(c))}
						<th scope="col"
							>{#if colHeader}{@render colHeader(c)}{:else}{colKey(c)}{/if}</th
						>
					{/each}
				</tr>
			</thead>
			<tbody>
				{#each rows as r (rowKey(r))}
					<tr>
						<th scope="row" class="row-head">{@render rowHeader(r)}</th>
						{#each columns as c (colKey(c))}
							<td>{@render cell(r, c)}</td>
						{/each}
					</tr>
				{/each}
			</tbody>
		</table>
	</div>
{/if}

<style>
	.matrix-wrap {
		overflow-x: auto;
	}
	.matrix th.row-head,
	.matrix th.corner {
		position: sticky;
		left: 0;
		background: var(--panel);
		z-index: 1;
		text-align: left;
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
