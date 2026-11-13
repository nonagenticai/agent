<script lang="ts">
	// Field → ontology mapping (Entity.attribute of the job's effective ontology). A stored row whose attribute
	// no longer exists is shown as unknown; the server refuses it (422 unknown_attribute) until it is fixed.
	import { jobPath } from '$lib/scope';
	import { app } from '$lib/app.svelte';
	import type { MappingTarget, SourceMapping } from '$lib/api/types/tenant';
	import { mapAllFields, unknownTarget, unmappedCount } from '$lib/tenant/sources';

	let {
		rows = $bindable(),
		targets,
		fields,
		disabled,
		why = '',
	}: {
		rows: SourceMapping[];
		targets: MappingTarget[];
		fields: string[];
		disabled: boolean;
		why?: string;
	} = $props();
	const listId = `source-fields-${Math.random().toString(36).slice(2)}`;
	const target = (m: SourceMapping) => (m.entity ? `${m.entity}.${m.attribute}` : '');
	const unmapped = $derived(unmappedCount(fields, rows));
	let note = $state('');
	function setTarget(i: number, v: string) {
		const [entity = '', attribute = ''] = v.split('.');
		rows[i] = { ...rows[i]!, entity, attribute };
	}
	function mapAll() {
		const r = mapAllFields(fields, rows, targets);
		rows = r.rows;
		const rest = r.added - r.matched;
		note =
			`Added ${r.added} row${r.added === 1 ? '' : 's'}; ${r.matched} matched an attribute by name.` +
			(rest ? ` Pick an attribute for the other ${rest}, or remove them.` : '');
	}
	function add() {
		rows = [...rows, { source_field: '', entity: '', attribute: '' }];
		note = '';
	}
</script>

{#if targets.length === 0}
	<div class="callout gap" role="note">
		<strong>No ontology attributes yet</strong>
		<span
			>Mapped fields need an Entity.attribute target. <a href={jobPath(app.job, '/ontology')}
				>Define them in the ontology</a
			> first.</span
		>
	</div>
{/if}
<div class="tbl-wrap">
	<table class="tbl dense" data-testid="mapping">
		<thead><tr><th>Source field</th><th>Ontology attribute</th><th></th></tr></thead>
		<tbody>
			{#each rows as m, i (i)}
				{@const bad = unknownTarget(m, targets)}
				<tr class:bad>
					<td
						><input
							class="input mono"
							list={listId}
							bind:value={m.source_field}
							aria-label="Source field"
							{disabled}
						/></td
					>
					<td>
						<select
							class="input"
							value={target(m)}
							onchange={(e) => setTarget(i, e.currentTarget.value)}
							aria-label="Ontology attribute"
							aria-invalid={bad ? 'true' : undefined}
							{disabled}
						>
							<option value="">Pick an attribute</option>
							{#if bad}<option value={target(m)}>{target(m)} (unknown)</option>{/if}
							{#each targets as o (`${o.entity}.${o.attribute}`)}<option value={`${o.entity}.${o.attribute}`}
									>{o.entity}.{o.attribute}{o.pii ? ' (PII)' : ''} · {o.type}</option
								>{/each}
						</select>
						{#if bad}<span class="cell-sub err-text"
								>Not in this job's ontology: the API refuses it (422 unknown_attribute).</span
							>{/if}
					</td>
					<td class="r"
						><button
							type="button"
							class="btn sm"
							{disabled}
							title={why}
							onclick={() => (rows = rows.filter((_, j) => j !== i))}>Remove</button
						></td
					>
				</tr>
			{:else}
				<tr
					><td colspan="3" class="muted"
						>No fields mapped yet.{fields.length
							? ''
							: ' Test the connection (or preview columns) to discover the fields.'}</td
					></tr
				>
			{/each}
		</tbody>
	</table>
</div>
<datalist id={listId}
	>{#each fields as f (f)}<option value={f}></option>{/each}</datalist
>
<div class="actions">
	<button type="button" class="btn sm" {disabled} title={why} onclick={add}>Add mapping</button>
	{#if unmapped > 0}
		<button type="button" class="btn sm" {disabled} title={why} onclick={mapAll}
			>Map all {unmapped} field{unmapped === 1 ? '' : 's'}</button
		>
	{/if}
	{#if note}<span class="muted" role="status">{note}</span>{/if}
</div>

<style>
	tr.bad td {
		background: color-mix(in srgb, var(--err) 6%, transparent);
	}
	.err-text {
		color: var(--err);
	}
</style>
