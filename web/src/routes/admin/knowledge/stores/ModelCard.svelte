<script lang="ts">
	// T7 model card: which models strata_rag searches with for THIS tenant (D70). Nothing is platform-fixed: the
	// embedding and rerank models are the provider + model bound to the tenant's embed / rerank slots, else the
	// platform's models strata serves it (the internal tenant binds mlx_api). Read here; changed on Providers →
	// Knowledge slots, through draft → release (D69).
	import { onMount } from 'svelte';
	import { api } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import type { ModelBinding, RetrievalModels } from '$lib/api/types/retrieval';
	import { gpuKindLabel } from '$lib/api/types/tenant';
	import Card from '$lib/ui/Card.svelte';
	import KeyValues from '$lib/ui/KeyValues.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';

	const models = new Resource<RetrievalModels>(() => api().retrievalModels());
	onMount(() => void models.refresh());

	/** "mistral-embed · OpenAI-compatible · EU (Paris)", or why the slot does nothing. */
	const bound = (b: ModelBinding, extra: string | null): string =>
		!b.configured || !b.model
			? `Not bound: searches that need the ${b.slot} slot fail`
			: [
					b.model,
					gpuKindLabel(b.kind ?? ''),
					extra,
					b.region,
					b.platform && !b.internal ? 'platform model' : null,
				]
					.filter(Boolean)
					.join(' · ');
</script>

<Card
	title="Search models"
	sub="The provider and model this tenant's embed and rerank slots are bound to. Change them on Providers."
>
	{#if models.value.state === 'unavailable'}
		<Unavailable reason={models.value.reason} retry={models.retry} />
	{:else if models.value.state === 'ok' || models.value.state === 'empty'}
		{@const m = models.value.data}
		<div data-testid="t7-models">
			<KeyValues
				pairs={[
					['Embedding', bound(m.embedding, m.embedding.dimensions ? `${m.embedding.dimensions}-d` : null)],
					['Reranker', bound(m.reranker, `first ${m.reranker.candidates} candidates`)],
					['Chunking', `${m.chunking.target_tokens}-token passages, ${m.chunking.overlap_tokens} overlap`],
					[
						'Search defaults',
						`${m.query_defaults.mode} · ${m.query_defaults.top_k} results · beam ${m.query_defaults.beam} · min score ${m.query_defaults.min_score}`,
					],
				]}
			/>
		</div>
		<p class="note" data-testid="t7-models-note">
			{#if m.embedding.internal}
				nonagentic's own tenant: search runs on mlx_api (its LLM on the pgx vLLM).
			{:else if m.embedding.platform}
				The platform's search models, under your licence; bind your own on Providers to replace them.
			{:else}
				Your own providers, with your tokens.
			{/if}
			Changing the embedding model re-embeds every document of this tenant; a change is saved to the draft of every
			job and goes live per job on publish.
			<a href="/admin/providers?tab=knowledge">Change on Providers</a>
		</p>
	{:else}
		<p class="muted">Loading…</p>
	{/if}
</Card>
