<script lang="ts">
	import { page } from '$app/state';
	import GeneratorRunView from './GeneratorRunView.svelte';

	const id = $derived(Number.parseInt(page.params.run ?? '', 10));
</script>

{#key id}<GeneratorRunView {id} />{/key}
