<script lang="ts">
	// "Auto": routing (round-robin) assigned this conversation; no person took it or assigned it. Renders nothing
	// otherwise. Reads `OwnerView.auto_assigned`, the one source for the inbox row and the conversation header.
	import type { OwnerView } from '$lib/api/types/support';

	let { owner }: { owner: Pick<OwnerView, 'auto_assigned'> } = $props();
</script>

{#if owner.auto_assigned}
	<span
		class="chip muted"
		data-testid="auto-chip"
		title="Assigned automatically (round-robin)"
		aria-label="Assigned automatically (round-robin)">Auto</span
	>
{/if}
