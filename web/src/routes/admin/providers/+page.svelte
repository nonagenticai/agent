<script lang="ts">
	// T6 Providers: one page for the whole provider config (06 §7, D38; owner call #5). It leads with the routing map
	// (use → provider → model, with funding, region and fallback), then three tabs that edit it: LLM providers for
	// Agent's own uses, Knowledge slots (strata_rag) and Ingest slots (strata_ingest), both provisioned down.
	import { onMount, tick } from 'svelte';
	import { page } from '$app/state';
	import { setQuery } from '$lib/query';
	import { api } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import PageHead from '$lib/ui/PageHead.svelte';
	import Tabs from '$lib/ui/Tabs.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import LlmTab from './LlmTab.svelte';
	import SlotsTab from './SlotsTab.svelte';
	import RoutingMap from './RoutingMap.svelte';
	import ResidencyBanner from './ResidencyBanner.svelte';
	import { buildRoutes, type Route } from './logic';

	const TABS: [string, string][] = [
		['llm', 'LLM providers'],
		['knowledge', 'Knowledge slots'],
		['ingest', 'Ingest slots'],
	];
	const fromUrl = () => {
		const t = page.url.searchParams.get('tab') ?? '';
		return TABS.some(([k]) => k === t) ? t : 'llm';
	};
	let tab = $state(fromUrl());
	$effect(() => {
		if (tab !== fromUrl()) void setQuery({ tab: tab === 'llm' ? '' : tab });
	});

	// The map reads the same three endpoints the tabs edit; usage is optional (no attribution yet → no tokens).
	const map = new Resource<Route[]>(
		async () => {
			const [llm, slots, usage] = await Promise.all([
				api().llmProviders(),
				api().providerSlots(),
				api()
					.tenantUsage({ window: 30, group: 'provider' })
					.catch(() => null),
			]);
			return buildRoutes(llm, slots, usage);
		},
		() => false,
	);
	onMount(() => map.refresh());
	const changed = () => void map.refresh();

	let editor = $state<HTMLElement | null>(null);
	async function open(t: Route['tab']) {
		tab = t;
		await tick();
		editor?.scrollIntoView?.({ behavior: 'smooth', block: 'start' });
	}
</script>

<PageHead
	id="T6"
	sub="Which model serves every call this tenant makes, where it runs and who pays: Agent's own uses, plus the slots Agent provisions down to strata_rag and strata_ingest. Keys are write-only."
/>

<div class="stack">
	<LoadState resource={map} loading="Loading the routing map…">
		{#snippet children(routes)}
			<ResidencyBanner {routes} />
			<RoutingMap {routes} onopen={(t) => open(t)} />
		{/snippet}
	</LoadState>

	<div bind:this={editor} class="t6-editor">
		<Tabs items={TABS} bind:active={tab} label="Provider kinds">
			<div class="stack tab-body">
				{#if tab === 'llm'}
					<LlmTab onchange={changed} />
				{:else if tab === 'knowledge'}
					<SlotsTab service="strata_rag" onchange={changed} />
				{:else}
					<SlotsTab service="strata_ingest" onchange={changed} />
				{/if}
			</div>
		</Tabs>
	</div>
</div>

<style>
	.tab-body {
		padding-top: 14px;
	}
	.t6-editor {
		scroll-margin-top: 72px;
	}
</style>
