<script lang="ts">
	import { onMount } from 'svelte';
	import { api, guard } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import type { TemplateCatalog } from '$lib/api/types/model';

	let { value = $bindable(''), disabled = false }: { value?: string; disabled?: boolean } = $props();
	const res = new Resource<TemplateCatalog>(
		() => api().templateCatalog(),
		() => false,
	);
	let busy = $state(false);
	onMount(() => res.refresh());
	async function createBase() {
		busy = true;
		if (await guard(() => api().createLayer({ scope: 'base', label: 'Base' }), 'Base layer created.')) {
			await res.refresh();
			value = 'base';
		}
		busy = false;
	}
</script>

<LoadState resource={res}>
	{#snippet children(c)}
		<div class="field">
			<label for="authoring-layer">Template layer</label>
			<select id="authoring-layer" class="input" bind:value required {disabled}>
				<option value="" disabled>Choose a layer</option>
				{#each c.layers as l (l.layer_key)}
					<option value={l.layer_key}>{l.label} · {l.layer_key}</option>
				{/each}
			</select>
			{#if !c.layers.some((l) => l.layer_key === 'base')}
				<button class="btn" type="button" disabled={disabled || busy} onclick={createBase}>
					{busy ? 'Creating…' : 'Create base layer'}
				</button>
			{/if}
			<p class="muted">
				Tag this layer in <a href="/platform/templates">Templates</a> when its changes are ready.
			</p>
		</div>
	{/snippet}
</LoadState>
