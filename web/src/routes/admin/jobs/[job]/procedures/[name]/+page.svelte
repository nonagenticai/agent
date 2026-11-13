<script lang="ts">
	// J12 procedure page: one procedure's versions, flow, steps and reviews. `?v=` picks the version.
	import { page } from '$app/state';
	import ProcedureDetail from './ProcedureDetail.svelte';

	const name = $derived(page.params.name ?? '');
	// B5 R6: versions are integers; anything else picks the running version
	const version = $derived.by(() => {
		const v = Number(page.url.searchParams.get('v') ?? '');
		return Number.isInteger(v) && v >= 1 ? v : null;
	});
</script>

{#key name}
	<ProcedureDetail {name} {version} />
{/key}
