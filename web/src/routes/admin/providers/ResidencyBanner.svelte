<script lang="ts">
	// T6 (D59): data residency is a TENANT setting fixed at provisioning. It pins where this tenant's LLM providers,
	// knowledge stores and logs run, so every route below must declare a region inside it.
	import { onMount } from 'svelte';
	import { api } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { RESIDENCY_LABEL, type Tenant } from '$lib/api/types/core';
	import type { Route } from './logic';

	let { routes }: { routes: Route[] } = $props();

	let tenant = $state<Tenant | null>(null);
	onMount(() => {
		const id = session.me?.tenant?.id;
		if (id)
			api()
				.tenant(id)
				.then((t) => (tenant = t))
				.catch(() => (tenant = null));
	});
	const res = $derived(tenant?.residency ?? null);
	const label = $derived(res ? RESIDENCY_LABEL[res] : null);
	const outside = $derived(
		label ? routes.filter((r) => r.region && !r.region.toUpperCase().startsWith(label)) : [],
	);
	const undeclared = $derived(routes.filter((r) => r.provider && !r.region).length);
</script>

{#if label}
	<p class="t6-res" data-testid="t6-residency">
		<span class="t6-res-k">Data residency</span>
		<b>{label}</b> — a tenant setting fixed at provisioning. Every provider here must run in {label}.
		{#if outside.length}
			<span class="bad">{outside.length} route{outside.length === 1 ? '' : 's'} declare another region.</span>
		{:else}
			<span class="ok">All declared regions are in {label}.</span>
		{/if}
		{#if undeclared}<span class="muted">{undeclared} not declared yet.</span>{/if}
	</p>
{/if}

<style>
	.t6-res {
		margin: 0;
		padding: 8px 12px;
		border: 1px solid var(--line);
		border-left: 3px solid var(--accent);
		border-radius: var(--r);
		font-size: 13px;
		display: flex;
		flex-wrap: wrap;
		gap: 4px 8px;
		align-items: baseline;
	}
	.t6-res-k {
		font: 600 10.5px/1.2 var(--cond);
		letter-spacing: 0.07em;
		text-transform: uppercase;
		color: var(--muted);
	}
	.bad {
		color: var(--err);
	}
	.ok {
		color: var(--ok);
	}
</style>
