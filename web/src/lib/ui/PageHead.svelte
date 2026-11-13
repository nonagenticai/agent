<script lang="ts">
	// The page header every page uses: title (from the page registry when `id` is given), milestone chip, a one-line
	// subtitle, optional longer `help` behind an ⓘ button, and actions.
	// `<PageHead id="J14" sub="…" help="…">{#snippet actions()}…{/snippet}</PageHead>`.
	import type { Snippet } from 'svelte';
	import { pageById } from '$lib/pages';
	import MilestoneChip from './MilestoneChip.svelte';

	let {
		id,
		title,
		sub,
		help,
		actions,
	}: { id?: string; title?: string; sub?: string; help?: string; actions?: Snippet } = $props();
	const def = $derived(id ? pageById(id) : undefined);
	const heading = $derived(title ?? def?.title ?? '');
	const uid = $props.id();
	let showHelp = $state(false);
</script>

<header class="page-head">
	<div>
		<h1>
			{heading}
			{#if def}<MilestoneChip milestone={def.milestone} note={def.milestoneNote} />{/if}
		</h1>
		{#if sub || help}
			<p>
				{sub ?? ''}
				{#if help}
					<button
						class="help-btn"
						type="button"
						aria-label="About this page"
						aria-describedby="{uid}-help"
						aria-expanded={showHelp}
						data-testid="page-help"
						onclick={() => (showHelp = !showHelp)}
						onmouseenter={() => (showHelp = true)}
						onmouseleave={() => (showHelp = false)}
						onfocus={() => (showHelp = true)}
						onblur={() => (showHelp = false)}
						onkeydown={(e) => e.key === 'Escape' && (showHelp = false)}>ⓘ</button
					>
				{/if}
			</p>
			{#if help}<span id="{uid}-help" role="tooltip" class="help-tip" hidden={!showHelp}>{help}</span>{/if}
		{/if}
	</div>
	{#if actions}<div class="actions">{@render actions()}</div>{/if}
</header>

<style>
	.help-btn {
		border: 0;
		background: none;
		padding: 0 4px;
		cursor: help;
		color: var(--muted);
		font: inherit;
		border-radius: 50%;
	}
	.help-btn:hover,
	.help-btn:focus-visible {
		color: var(--fg);
	}
	.help-tip {
		display: block;
		max-width: 60ch;
		margin-top: 6px;
		padding: 8px 10px;
		font-size: 13px;
		line-height: 1.4;
		color: var(--fg);
		background: var(--panel);
		border: 1px solid var(--line);
		border-radius: 6px;
	}
	.help-tip[hidden] {
		display: none;
	}
</style>
