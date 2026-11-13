<script lang="ts">
	// "Verified": the customer is known (signed in on the tenant's site, or picked by staff). Renders nothing
	// otherwise. The one badge for the inbox row, the conversation header and the operator console.
	let { verified }: { verified: boolean } = $props();
</script>

{#if verified}
	<span
		class="chip ok"
		data-testid="verified-chip"
		title="Signed-in customer, verified"
		aria-label="Signed-in customer, verified">Verified</span
	>
{/if}
