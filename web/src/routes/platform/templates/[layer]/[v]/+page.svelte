<script lang="ts">
	// P7 Template tag detail. URL: /platform/templates/<layer with "/" as ".">/v<N>, e.g.
	// /platform/templates/job.customer_care_agent/v4 (see $lib/model/logic `tagHref`).
	import { page } from '$app/state';
	import TagDetailPage from '$lib/model/TagDetailPage.svelte';
	import { slugLayer } from '$lib/model/logic';

	const layer = $derived(slugLayer(page.params.layer ?? ''));
	const version = $derived(Number.parseInt((page.params.v ?? '').replace(/^v/, ''), 10));
</script>

<TagDetailPage {layer} version={Number.isFinite(version) ? version : -1} />
