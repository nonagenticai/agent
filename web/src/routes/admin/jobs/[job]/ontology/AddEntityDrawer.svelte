<script lang="ts">
	import { untrack } from 'svelte';
	// J11 "Add entity": a tenant extension entity (refinement) with typed attributes and the platform object it is
	// a view of. Client-side checks mirror the route; the API stays the authority (its 4xx shows inline).
	// from cb:web/src/routes/admin/jobs/[job]/ontology/+page.svelte@45ee128 (add-entity drawer)
	import { api } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { writeAttrs } from '$lib/auth/roles';
	import Drawer from '$lib/ui/Drawer.svelte';
	import { toast } from '$lib/ui/toaster.svelte';
	import type { EntityInput, OntologyEntity, OntologyScope } from '$lib/api/types/ontology';
	import { distinct, entityFormErrors, type Filter } from '$lib/ontology/layout';
	import { OBJECT_KEYS, explain } from '$lib/ontology/shared';

	let {
		open = $bindable(),
		entities,
		industries,
		filter,
		oncreated,
	}: {
		open: boolean;
		entities: OntologyEntity[];
		industries: string[];
		filter: Filter;
		oncreated: (name: string) => Promise<void>;
	} = $props();

	type Draft = {
		name: string;
		label: string;
		scope: OntologyScope;
		industry: string;
		department: string;
		synonyms: string;
		object_key: string;
		attributes: { name: string; data_type: string; pii: boolean; synonyms: string }[];
	};
	const TYPES = ['string', 'text', 'enum', 'date', 'datetime', 'money', 'number', 'boolean'];
	const blankAttr = () => ({ name: '', data_type: 'string', pii: false, synonyms: '' });
	const blank = (): Draft => ({
		name: '',
		label: '',
		scope:
			filter.department !== 'all' ? 'industry_dept' : filter.industry !== 'all' ? 'industry' : 'industry',
		industry: filter.industry === 'all' ? (industries[0] ?? '') : filter.industry,
		department: filter.department === 'all' ? (distinct(entities, 'department')[0] ?? '') : filter.department,
		synonyms: '',
		object_key: '',
		attributes: [blankAttr()],
	});

	let draft = $state<Draft>({
		name: '',
		label: '',
		scope: 'industry',
		industry: '',
		department: '',
		synonyms: '',
		object_key: '',
		attributes: [],
	});
	let start = $state('');
	let formErrors = $state<string[]>([]);
	let apiError = $state<string | null>(null);
	let saving = $state(false);
	const dirty = $derived(open && JSON.stringify(draft) !== start);

	// A fresh form every time the drawer opens.
	$effect(() => {
		if (!open) return;
		untrack(() => {
			draft = blank();
			start = JSON.stringify(draft);
			formErrors = [];
			apiError = null;
		});
	});

	const list = (s: string) =>
		s
			.split(',')
			.map((x) => x.trim())
			.filter(Boolean);

	function toInput(d: Draft): EntityInput {
		return {
			name: d.name.trim(),
			label: d.label.trim(),
			scope: d.scope,
			industry: d.scope === 'generic' ? null : d.industry.trim() || null,
			department: d.scope === 'industry_dept' ? d.department.trim() || null : null,
			synonyms: list(d.synonyms),
			object_key: d.object_key || null,
			attributes: d.attributes.map((a) => ({
				name: a.name.trim(),
				data_type: a.data_type.trim(),
				pii: a.pii,
				synonyms: list(a.synonyms),
			})),
		};
	}

	async function submit(e: SubmitEvent) {
		e.preventDefault();
		const input = toInput(draft);
		formErrors = entityFormErrors(input);
		if (entities.some((x) => x.name === input.name)) formErrors.push(`${input.name} already exists.`);
		apiError = null;
		if (formErrors.length) return;
		saving = true;
		try {
			await api().createOntologyEntity(input);
			toast(`Added ${input.label} as a tenant entity. Saved to draft — publish to make it live.`);
			start = JSON.stringify(draft);
			open = false;
			await oncreated(input.name);
		} catch (err) {
			apiError = explain(err, 'add entities');
		} finally {
			saving = false;
		}
	}
</script>

<Drawer bind:open title="Add entity" wide {dirty}>
	<form class="stack" novalidate onsubmit={submit}>
		<p class="muted">
			Creates a tenant refinement on this job. Link it to the platform object it describes so operations can
			act on it.
		</p>
		<div class="form-grid">
			<div class="field">
				<label for="ent-name">Name</label>
				<input id="ent-name" class="input mono" bind:value={draft.name} placeholder="e.g. GiftCard" />
			</div>
			<div class="field">
				<label for="ent-label">Label</label>
				<input id="ent-label" class="input" bind:value={draft.label} placeholder="e.g. Gift card" />
			</div>
			<div class="field">
				<label for="ent-scope">Scope</label>
				<select id="ent-scope" class="input" bind:value={draft.scope}>
					<option value="generic">Generic</option>
					<option value="industry">Industry</option>
					<option value="industry_dept">Specialized (industry × department)</option>
				</select>
			</div>
			<div class="field">
				<label for="ent-object">Platform object</label>
				<select id="ent-object" class="input mono" bind:value={draft.object_key}>
					<option value="">Not linked</option>
					{#each OBJECT_KEYS as k (k)}<option value={k}>{k}</option>{/each}
				</select>
			</div>
			{#if draft.scope !== 'generic'}
				<div class="field">
					<label for="ent-ind">Industry</label>
					<input id="ent-ind" class="input" list="onto-industries" bind:value={draft.industry} />
					<datalist id="onto-industries"
						>{#each industries as i (i)}<option value={i}></option>{/each}</datalist
					>
				</div>
			{/if}
			{#if draft.scope === 'industry_dept'}
				<div class="field">
					<label for="ent-dep">Department</label>
					<input id="ent-dep" class="input" list="onto-departments" bind:value={draft.department} />
					<datalist id="onto-departments"
						>{#each distinct(entities, 'department') as d (d)}<option value={d}></option>{/each}</datalist
					>
				</div>
			{/if}
			<div class="field">
				<label for="ent-syn">Synonyms (comma separated)</label>
				<input id="ent-syn" class="input" bind:value={draft.synonyms} placeholder="cadeaukaart, voucher" />
			</div>
		</div>

		<div>
			<h3>Attributes</h3>
			<div class="tbl-wrap">
				<table class="tbl dense attrs">
					<thead><tr><th>Name</th><th>Type</th><th>Personal data</th><th>Synonyms</th><th></th></tr></thead>
					<tbody>
						{#each draft.attributes as a, i (i)}
							<tr>
								<td><input class="input mono" aria-label="Attribute {i + 1} name" bind:value={a.name} /></td>
								<td>
									<select class="input mono" aria-label="Attribute {i + 1} type" bind:value={a.data_type}>
										{#each TYPES as t (t)}<option value={t}>{t}</option>{/each}
									</select>
								</td>
								<td class="c"
									><input
										type="checkbox"
										aria-label="Attribute {i + 1} is personal data"
										bind:checked={a.pii}
									/></td
								>
								<td
									><input class="input" aria-label="Attribute {i + 1} synonyms" bind:value={a.synonyms} /></td
								>
								<td>
									<button
										type="button"
										class="btn sm"
										aria-label="Remove attribute {i + 1}"
										onclick={() => draft.attributes.splice(i, 1)}>Remove</button
									>
								</td>
							</tr>
						{:else}
							<tr><td colspan="5" class="muted">No attributes.</td></tr>
						{/each}
					</tbody>
				</table>
			</div>
			<button type="button" class="btn sm add-attr" onclick={() => draft.attributes.push(blankAttr())}
				>Add attribute</button
			>
		</div>

		{#if !draft.object_key}
			<p class="note">
				An entity without a platform object is vocabulary only: no operation can read or change it.
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
				>{saving ? 'Saving…' : 'Create entity'}</button
			>
			<button type="button" class="btn" onclick={() => (open = false)}>Cancel</button>
		</div>
	</form>
</Drawer>

<style>
	.attrs .input {
		padding: 4px 7px;
	}
	.add-attr {
		margin-top: 8px;
	}
	.c {
		text-align: center;
	}
	.row {
		display: flex;
		gap: 8px;
	}
	h3 {
		margin: 0 0 8px;
		font: 600 13px/1.3 var(--cond);
		letter-spacing: 0.06em;
		text-transform: uppercase;
		color: var(--muted);
	}
</style>
