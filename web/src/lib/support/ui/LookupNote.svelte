<script lang="ts">
	// The staff test chat typed a customer's email the connected system could not resolve to one customer: say so.
	import { lookupNote } from '$lib/support/context';

	let { job, degraded }: { job: string; degraded: string[] | undefined } = $props();
	const note = $derived(lookupNote(degraded, job));
</script>

{#if note}
	<p class="callout warn" role="status" data-testid="lookup-note">
		{note.text}
		{#if note.href}<a href={note.href}>{note.linkLabel}</a>{/if}
	</p>
{/if}
