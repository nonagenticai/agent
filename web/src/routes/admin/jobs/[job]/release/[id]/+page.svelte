<script lang="ts">
	// J15 Release detail `…/release/[id]`: the view takes the id from the URL.
	import { page } from '$app/state';
	import ReleaseDetailView from './ReleaseDetailView.svelte';
	const id = $derived(page.params.id ?? '');
</script>

{#key id}
	<ReleaseDetailView {id} />
{/key}
