<script lang="ts">
	import { api, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { superAdminAttrs } from '$lib/auth/roles';
	import type { NewObjectInput } from '$lib/api/types/model';
	import Drawer from '$lib/ui/Drawer.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import AuthoringLayer from './AuthoringLayer.svelte';

	let { oncreated }: { oncreated: () => void | Promise<void> } = $props();
	const blank = (): NewObjectInput => ({
		object: '',
		label: '',
		tier: 'base',
		description: '',
		layer_key: '',
	});
	let open = $state(false);
	let busy = $state(false);
	let form = $state(blank());
	let created = $state<NewObjectInput | null>(null);
	function start() {
		form = blank();
		created = null;
		open = true;
	}
	async function save() {
		if (busy) return;
		busy = true;
		const input = {
			...form,
			object: form.object.trim(),
			label: form.label.trim(),
			description: form.description.trim(),
		};
		if (await guard(() => api().createObject(input), `Object ${input.object} created.`)) {
			created = input;
			await oncreated();
		}
		busy = false;
	}
</script>

<button class="btn primary" type="button" {...superAdminAttrs(session)} onclick={start}>Create object</button>
<Drawer bind:open title="Create object" dirty={!created && !!(form.object || form.label)}>
	{#if created}
		<Callout kind="info" title="Object created">
			<p>{created.label} is an untagged change in {created.layer_key}.</p>
			<p>
				<a href="/platform/objects/{encodeURIComponent(created.object)}">Edit fields and lifecycle</a>, then
				<a href="/platform/templates">tag the template layer</a>.
			</p>
		</Callout>
	{:else}
		<form
			class="stack"
			onsubmit={(e) => {
				e.preventDefault();
				void save();
			}}
		>
			<AuthoringLayer bind:value={form.layer_key} disabled={busy} />
			<div class="field">
				<label for="new-object-key">Object key</label><input
					id="new-object-key"
					class="input"
					bind:value={form.object}
					required
					pattern="[a-z][a-z0-9_]*"
					placeholder="service_visit"
					disabled={busy}
				/><span class="muted">Lower-case letters, digits and underscores; start with a letter.</span>
			</div>
			<div class="field">
				<label for="new-object-label">Label</label><input
					id="new-object-label"
					class="input"
					bind:value={form.label}
					required
					pattern=".*\S.*"
					disabled={busy}
				/>
			</div>
			<div class="field">
				<label for="new-object-tier">Tier</label><select
					id="new-object-tier"
					class="input"
					bind:value={form.tier}
					disabled={busy}
					><option value="base">Base</option><option value="industry">Industry</option><option
						value="optional">Optional</option
					></select
				>
			</div>
			<div class="field">
				<label for="new-object-description">Description</label><textarea
					id="new-object-description"
					class="input"
					bind:value={form.description}
					disabled={busy}></textarea>
			</div>
			<p class="muted">
				Add the object's fields, states and relations in its detail editor after creating it.
			</p>
			<button class="btn primary" type="submit" disabled={busy || !form.layer_key}
				>{busy ? 'Creating…' : 'Create object draft'}</button
			>
		</form>
	{/if}
</Drawer>

<style>
	.stack {
		display: grid;
		gap: 14px;
	}
</style>
