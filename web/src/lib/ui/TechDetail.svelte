<script lang="ts">
	// Slugs, ids, hashes and error codes: shown only when "Show technical details" is on.
	import type { Snippet } from 'svelte';
	import { prefs } from './prefs.svelte';
	let { children }: { children: Snippet } = $props();
</script>

{#if prefs.showTech}<span class="tech">{@render children()}</span>{/if}

<style>
	.tech {
		font-family: var(--mono, ui-monospace, monospace);
		font-size: 0.85em;
		opacity: 0.8;
	}
</style>
