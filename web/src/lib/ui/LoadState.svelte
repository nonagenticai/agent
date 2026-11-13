<script lang="ts" generics="T">
	// The four states every list/detail shows (spec 14 §2): loading, unavailable (with retry), empty, data.
	//   const res = new Resource(() => api().cases());  onMount(() => res.refresh());
	//   <LoadState resource={res} empty="No cases yet.">{#snippet children(page)}…{/snippet}</LoadState>
	import type { Snippet } from 'svelte';
	import type { Resource } from '$lib/load.svelte';
	import Unavailable from './Unavailable.svelte';

	let {
		resource,
		children,
		empty = 'Nothing here yet.',
		loading = 'Loading…',
	}: {
		resource: Resource<T>;
		children: Snippet<[T]>;
		/** Text, or a snippet for an empty state with an action. */
		empty?: string | Snippet;
		loading?: string;
	} = $props();
	const v = $derived(resource.value);
</script>

{#if v.state === 'loading'}
	<p class="muted" role="status" data-testid="load-loading">{loading}</p>
{:else if v.state === 'unavailable'}
	<Unavailable reason={v.reason} retry={resource.retry} />
{:else if v.state === 'empty'}
	<div class="empty" data-testid="load-empty">
		{#if typeof empty === 'string'}{empty}{:else}{@render empty()}{/if}
	</div>
{:else}
	{@render children(v.data)}
{/if}
