<script lang="ts">
	// D54 draft bar for a job design page: nothing a tenant changes is live on save. Every change is saved to the
	// job's draft, tried in the test chat (test env), and reaches Production only when a release is published
	// (cut → approve → apply). Shows the unpublished count (this page / the whole job) and the two next steps.
	import { api, app } from '$lib/app.svelte';
	import type { DraftSummary } from '$lib/api/types/release';

	let {
		page,
		tick = 0,
		onload,
	}: {
		/** Page id (`J7`) whose changes are counted as "here". */
		page: string;
		/** Change it after a save to re-read the draft. */
		tick?: unknown;
		/** Receives the draft after each read (pages mark their draft rows with it). */
		onload?: (d: DraftSummary) => void;
	} = $props();

	let draft = $state<DraftSummary | null>(null);
	$effect(() => {
		void tick;
		api()
			.draftSummary()
			.then((d) => {
				draft = d;
				onload?.(d);
			})
			.catch(() => (draft = null));
	});
	const here = $derived(draft ? draft.changes.filter((c) => c.page === page).length : 0);
</script>

<div class="draft-bar" class:has={(draft?.count ?? 0) > 0} data-testid="draft-bar">
	<span class="draft-bar-n">
		{#if draft === null}
			Changes you save here go to the draft.
		{:else if draft.count === 0}
			No unpublished changes — Production runs the published release.
		{:else}
			<b>{draft.count} unpublished change{draft.count === 1 ? '' : 's'}</b>
			{#if here}<span class="muted">({here} on this page)</span>{/if}
			<span class="muted">· saved to draft, not live until published</span>
		{/if}
	</span>
	<span class="draft-bar-links">
		<a href="/chat">Test in test chat</a>
		<a class="btn sm" href="/admin/jobs/{app.job}/release">Review &amp; publish</a>
	</span>
</div>

<style>
	.draft-bar {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		justify-content: space-between;
		gap: 8px 16px;
		padding: 8px 12px;
		margin: 0 0 14px;
		border: 1px solid var(--line);
		border-left: 3px solid var(--line);
		border-radius: 6px;
		font-size: 13px;
	}
	.draft-bar.has {
		border-left-color: var(--accent);
	}
	.draft-bar-links {
		display: flex;
		align-items: center;
		gap: 12px;
	}
</style>
