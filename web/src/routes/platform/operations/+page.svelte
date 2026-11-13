<script lang="ts">
	// P11 Operations catalogue. `?object=order` / `?q=case.create` preset the filters (links from P9, P10, P7);
	// `?op=order.cancel` opens that operation's drawer.
	import { page } from '$app/state';
	import OperationsPage from '$lib/model/OperationsPage.svelte';

	const object = page.url.searchParams.get('object') ?? '';
	const q = page.url.searchParams.get('q') ?? '';
	const op = page.url.searchParams.get('op') ?? '';
</script>

<OperationsPage {object} {q} {op} />
