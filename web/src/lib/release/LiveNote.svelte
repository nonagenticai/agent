<script lang="ts">
	// D69: a page whose saves are LIVE — data or per-environment resources, not agent design — says so in one line,
	// worded the same on every page. Design changes go to the draft instead (DraftBar / TenantDraftBar).
	let { what, of = 'data' }: { what: string; of?: 'data' | 'environment' } = $props();
</script>

<p class="live-note" data-testid="live-note">
	<b>Saved live — {of === 'data' ? 'data' : 'an environment resource'}, not design (D69).</b>
	{what}
</p>

<style>
	.live-note {
		margin: 0 0 14px;
		font-size: 13px;
		color: var(--muted);
	}
	.live-note b {
		color: var(--fg);
		font-weight: 600;
	}
</style>
