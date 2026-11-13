<script lang="ts">
	// Shown while the tenant stream is down: the page polls the inbox every 10 s so nothing is silently stale.
	let { lastSync = null, onRetry }: { lastSync?: string | null; onRetry?: () => void } = $props();
</script>

<div class="banner" role="status" aria-label="Live updates">
	<strong>Live updates paused.</strong>
	<span>Refreshing every 10 s while we reconnect{lastSync ? ` · last refreshed ${lastSync}` : ''}.</span>
	{#if onRetry}<button class="btn sm" type="button" onclick={onRetry}>Reconnect now</button>{/if}
</div>

<style>
	.banner {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 8px;
		padding: 7px 12px;
		border: 1px solid color-mix(in srgb, var(--warn) 45%, transparent);
		border-radius: 8px;
		background: color-mix(in srgb, var(--warn) 12%, var(--panel));
		color: var(--fg);
		font-size: 13px;
	}
</style>
