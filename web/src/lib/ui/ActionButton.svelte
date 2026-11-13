<script lang="ts">
	// A button that never fails silently: when `disabledReason` is set it is disabled AND says why as visible text
	// (not only a title), linked with aria-describedby so screen readers read it with the button.
	import type { Snippet } from 'svelte';

	let {
		primary = false,
		sm = false,
		disabledReason = null,
		onclick,
		children,
	}: {
		primary?: boolean;
		sm?: boolean;
		disabledReason?: string | null;
		onclick?: (e: MouseEvent) => void;
		children: Snippet;
	} = $props();
	const reasonId = $props.id();
</script>

<span class="ab">
	<button
		type="button"
		class="btn"
		class:primary
		class:sm
		disabled={!!disabledReason}
		aria-describedby={disabledReason ? reasonId : undefined}
		onclick={(e) => {
			if (!disabledReason) onclick?.(e);
		}}>{@render children()}</button
	>
	{#if disabledReason}<span class="reason" id={reasonId}>{disabledReason}</span>{/if}
</span>

<style>
	.ab {
		display: inline-flex;
		flex-direction: column;
		align-items: flex-start;
		gap: 4px;
		min-width: 0;
	}
	.reason {
		font-size: 12px;
		color: var(--muted);
		max-width: 28ch;
	}
</style>
