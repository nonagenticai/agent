<script lang="ts">
	import { page } from '$app/state';
	import TenantRunView from './TenantRunView.svelte';

	const id = $derived(Number.parseInt(page.params.run ?? '', 10));
</script>

{#key id}<TenantRunView {id} />{/key}
