<script lang="ts">
	import { untrack } from 'svelte';
	// J11 "Add relation": subject – predicate – object with a cardinality, as a tenant refinement.
	// from cb:web/src/routes/admin/jobs/[job]/ontology/+page.svelte@45ee128 (add-relation drawer)
	import { api } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { writeAttrs } from '$lib/auth/roles';
	import Drawer from '$lib/ui/Drawer.svelte';
	import { toast } from '$lib/ui/toaster.svelte';
	import type { Cardinality, OntologyEntity, OntologyRelation } from '$lib/api/types/ontology';
	import { CARDINALITIES, CARDINALITY_LABEL, relationFormErrors } from '$lib/ontology/layout';
	import { explain } from '$lib/ontology/shared';

	let {
		open = $bindable(),
		subject,
		entities,
		relations,
		oncreated,
	}: {
		open: boolean;
		subject: string;
		entities: OntologyEntity[];
		relations: OntologyRelation[];
		oncreated: () => Promise<void>;
	} = $props();

	type Draft = { subject: string; predicate: string; object: string; cardinality: Cardinality };
	let draft = $state<Draft>({ subject: '', predicate: '', object: '', cardinality: '1:n' });
	let start = $state('');
	let formErrors = $state<string[]>([]);
	let apiError = $state<string | null>(null);
	let saving = $state(false);
	const dirty = $derived(open && JSON.stringify(draft) !== start);
	const pickable = $derived([...entities].sort((a, b) => a.label.localeCompare(b.label)));
	const labelOf = (name: string) => entities.find((e) => e.name === name)?.label ?? name;

	$effect(() => {
		if (!open) return;
		untrack(() => {
			draft = { subject, predicate: '', object: '', cardinality: '1:n' };
			start = JSON.stringify(draft);
			formErrors = [];
			apiError = null;
		});
	});

	async function submit(e: SubmitEvent) {
		e.preventDefault();
		const input = { ...draft, predicate: draft.predicate.trim() };
		formErrors = relationFormErrors(input, entities, relations);
		apiError = null;
		if (formErrors.length) return;
		saving = true;
		try {
			await api().createOntologyRelation(input);
			toast(`Added ${labelOf(input.subject)} ${input.predicate} ${labelOf(input.object)}. Saved to draft.`);
			start = JSON.stringify(draft);
			open = false;
			await oncreated();
		} catch (err) {
			apiError = explain(err, 'add relations');
		} finally {
			saving = false;
		}
	}
</script>

<Drawer bind:open title="Add relation" {dirty}>
	<form class="stack" novalidate onsubmit={submit}>
		<div class="field">
			<label for="rel-subject">Subject</label>
			<select id="rel-subject" class="input" bind:value={draft.subject}>
				<option value="">Pick an entity…</option>
				{#each pickable as en (en.name)}<option value={en.name}>{en.label} ({en.name})</option>{/each}
			</select>
		</div>
		<div class="field">
			<label for="rel-predicate">Predicate</label>
			<input
				id="rel-predicate"
				class="input mono"
				bind:value={draft.predicate}
				placeholder="e.g. places, paidBy"
				autocomplete="off"
			/>
		</div>
		<div class="field">
			<label for="rel-object">Object</label>
			<select id="rel-object" class="input" bind:value={draft.object}>
				<option value="">Pick an entity…</option>
				{#each pickable as en (en.name)}<option value={en.name}>{en.label} ({en.name})</option>{/each}
			</select>
		</div>
		<div class="field">
			<label for="rel-card">Cardinality</label>
			<select id="rel-card" class="input" bind:value={draft.cardinality}>
				{#each CARDINALITIES as c (c)}<option value={c}>{c} · {CARDINALITY_LABEL[c]}</option>{/each}
			</select>
		</div>
		{#if draft.subject && draft.object && draft.predicate.trim()}
			<p class="muted preview">
				Reads as: <b>{labelOf(draft.subject)}</b>
				<span class="mono">{draft.predicate.trim()}</span>
				<b>{labelOf(draft.object)}</b>
			</p>
		{/if}
		{#if formErrors.length}
			<div class="callout err" role="alert">
				<strong>Fix these first</strong><span>{formErrors.join(' ')}</span>
			</div>
		{/if}
		{#if apiError}
			<div class="callout err" role="alert"><strong>Not saved</strong><span>{apiError}</span></div>
		{/if}
		<div class="row">
			<button
				type="submit"
				class="btn primary"
				{...writeAttrs(session, 'job_designer')}
				disabled={saving || writeAttrs(session, 'job_designer').disabled}
				>{saving ? 'Saving…' : 'Create relation'}</button
			>
			<button type="button" class="btn" onclick={() => (open = false)}>Cancel</button>
		</div>
	</form>
</Drawer>

<style>
	.preview {
		margin: 0;
		font-size: 13px;
	}
	.row {
		display: flex;
		gap: 8px;
	}
</style>
