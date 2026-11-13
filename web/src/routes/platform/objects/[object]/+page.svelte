<script lang="ts">
	// P10 Object detail — /platform/objects/<object key>, e.g. /platform/objects/order.
	import { page } from '$app/state';
	import ObjectDetailPage from '$lib/model/ObjectDetailPage.svelte';

	const object = $derived(page.params.object ?? '');
</script>

<ObjectDetailPage {object} />
