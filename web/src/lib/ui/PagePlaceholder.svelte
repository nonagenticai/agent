<script lang="ts">
	// Stand-in for a page its area agent has not built yet. Every registry route starts as one of these.
	import { pageById } from '$lib/pages';
	import PageHead from './PageHead.svelte';

	let { id }: { id: string } = $props();
	const def = $derived(pageById(id));
</script>

<PageHead {id} title={def ? undefined : `Unknown page ${id}`} />
<div class="callout info" data-testid="page-placeholder">
	<strong>Not built yet</strong>
	<span>
		Page <b class="mono">{id}</b>
		{#if def}· area <b>{def.area}</b> · spec 14 §2{/if}. This page shows its concept once its area is built.
	</span>
</div>
