<script lang="ts">
	// J11 list view: the relations between the filtered entities, and the context spaces with their topic
	// prefix, entities, required slots and procedure.
	// from cb:web/src/routes/admin/jobs/[job]/ontology/+page.svelte@45ee128 (relations list + context spaces)
	import { app } from '$lib/app.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';
	import type { Resource } from '$lib/load.svelte';
	import type { Paged } from '$lib/api/types/core';
	import type { ContextSpace, OntologyEntity, OntologyRelation } from '$lib/api/types/ontology';
	import { draftTitle, relationMarkKey, type J11DraftMarks } from '$lib/ontology/J11drafts';

	let {
		edges,
		entities,
		relations,
		spaces,
		marks,
		onpick,
	}: {
		edges: OntologyRelation[];
		entities: OntologyEntity[];
		relations: Resource<Paged<OntologyRelation>>;
		spaces: Resource<Paged<ContextSpace>>;
		/** D54: rows the job's draft touches (from `draftDiff`) get a "Draft" chip. */
		marks?: J11DraftMarks;
		onpick: (name: string) => void;
	} = $props();
	const labelOf = (name: string) => entities.find((e) => e.name === name)?.label ?? name;
</script>

<Card title="Relations" sub="Between the entities in this filter" cls="flush">
	{#if relations.value.state === 'ok'}
		<div class="tbl-wrap">
			<table class="tbl dense">
				<thead
					><tr><th>Subject</th><th>Predicate</th><th>Object</th><th>Cardinality</th><th>Origin</th></tr
					></thead
				>
				<tbody>
					{#each edges as r (`${r.subject}|${r.predicate}|${r.object}`)}
						{@const rm = marks?.relations.get(relationMarkKey(r))}
						<tr data-testid={rm ? 'j11-draft-row' : undefined}>
							<td
								><button type="button" class="link" onclick={() => onpick(r.subject)}
									>{labelOf(r.subject)}</button
								>{#if marks?.entities.has(r.subject)}
									<Chip
										text="Draft"
										variant="warn"
										title={draftTitle(marks.entities.get(r.subject))}
									/>{/if}</td
							>
							<td class="mono">{r.predicate}</td>
							<td
								><button type="button" class="link" onclick={() => onpick(r.object)}
									>{labelOf(r.object)}</button
								>{#if marks?.entities.has(r.object)}
									<Chip
										text="Draft"
										variant="warn"
										title={draftTitle(marks.entities.get(r.object))}
									/>{/if}</td
							>
							<td class="mono">{r.cardinality}</td>
							<td
								><Chip
									text={r.origin === 'tenant' ? 'Tenant' : 'Base'}
									variant={r.origin === 'tenant' ? 'accent' : 'muted'}
								/>{#if rm}
									<Chip text="Draft" variant="warn" title={draftTitle(rm)} />{/if}</td
							>
						</tr>
					{:else}
						<tr><td colspan="5" class="muted">No relations between the entities in this filter.</td></tr>
					{/each}
				</tbody>
			</table>
		</div>
	{:else if relations.value.state === 'unavailable'}
		<Unavailable reason={relations.value.reason} retry={relations.retry} />
	{:else if relations.value.state === 'empty'}
		<div class="empty">No relations yet.</div>
	{:else}
		<p class="muted">Loading…</p>
	{/if}
</Card>

<Card
	title="Context spaces → bus topics"
	sub="The space key is the topic prefix; the procedure runs per space"
	cls="flush"
>
	{#if spaces.value.state === 'ok'}
		<div class="tbl-wrap">
			<table class="tbl dense">
				<thead><tr><th>Key</th><th>Entities</th><th>Required slots</th><th>Procedure</th></tr></thead>
				<tbody>
					{#each spaces.value.data.items as s (s.key)}
						<tr>
							<td
								><span class="cell-title mono"
									>{s.key}{#if marks?.spaces.has(s.key)}
										<Chip
											text="Draft"
											variant="warn"
											title={draftTitle(marks.spaces.get(s.key))}
										/>{/if}</span
								><span class="cell-sub">{s.label}</span></td
							>
							<td>{s.entities.map(labelOf).join(', ')}</td>
							<td class="mono">{s.required.join(', ') || '—'}</td>
							<td><a class="mono" href="/admin/jobs/{app.job}/procedures/{s.procedure}">{s.procedure}</a></td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	{:else if spaces.value.state === 'unavailable'}
		<Unavailable reason={spaces.value.reason} retry={spaces.retry} />
	{:else if spaces.value.state === 'empty'}
		<div class="empty">No context spaces.</div>
	{:else}
		<p class="muted">Loading…</p>
	{/if}
</Card>

<style>
	.link {
		all: unset;
		cursor: pointer;
		color: var(--accent);
	}
	.link:hover {
		text-decoration: underline;
	}
	.link:focus-visible {
		outline: 2px solid var(--accent);
		outline-offset: 2px;
	}
</style>
