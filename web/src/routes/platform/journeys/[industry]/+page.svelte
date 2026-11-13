<script lang="ts">
	import { page } from '$app/state';
	import JourneyDesigner from './JourneyDesigner.svelte';

	const industry = $derived(page.params.industry ?? 'retail');
</script>

{#key `${industry}:${page.url.searchParams.get('department') ?? ''}`}<JourneyDesigner {industry} />{/key}
