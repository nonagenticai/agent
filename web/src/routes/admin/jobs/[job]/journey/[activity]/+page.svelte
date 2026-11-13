<script lang="ts">
	import { page } from '$app/state';
	import ActivityView from './ActivityView.svelte';

	const activity = $derived(page.params.activity ?? '');
</script>

{#key activity}<ActivityView {activity} />{/key}
