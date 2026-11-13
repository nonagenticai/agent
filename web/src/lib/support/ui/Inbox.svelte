<script lang="ts">
	// The inbox pane: tabs Mine · Unassigned · All open · Resolved (7 d) with live counts, one filter popover, and the
	// rows of the tab (soonest SLA due first; an inbound answer on a waiting case jumps to the top).
	import type { InboxRow as Row, InboxTab } from '$lib/api/types/support';
	import { rowKey, type InboxStore } from '$lib/support/inbox.svelte';
	import InboxRow from './InboxRow.svelte';
	import FilterPopover from './FilterPopover.svelte';
	import RoutingControl from './RoutingControl.svelte';

	let {
		store,
		selected,
		now,
		onOpen,
	}: {
		store: InboxStore;
		/** `rowKey(job, conversation)` of the open conversation. */
		selected: string | null;
		now: number;
		onOpen: (row: Row) => void;
	} = $props();

	const TABS: { id: InboxTab; label: string }[] = [
		{ id: 'mine', label: 'Mine' },
		{ id: 'unassigned', label: 'Unassigned' },
		{ id: 'open', label: 'All open' },
		{ id: 'resolved', label: 'Resolved (7 d)' },
	];
	const countOf = (t: InboxTab) => (t === 'resolved' ? null : store.counts[t]);
	const reasons = $derived(
		[...new Set(store.rows.map((r) => r.reason_label).filter((x): x is string => !!x)), store.filters.reason]
			.filter((x): x is string => !!x)
			.filter((x, i, a) => a.indexOf(x) === i),
	);
	const channels = $derived([
		...new Set([...store.rows.map((r) => r.channel), store.filters.channel].filter((x): x is string => !!x)),
	]);
	const manyJobs = $derived(new Set(store.rows.map((r) => r.job)).size > 1);
	const empty: Record<InboxTab, string> = {
		mine: 'Nothing is yours right now.',
		unassigned: 'Nobody is waiting for a person.',
		open: 'No open conversations with a person.',
		resolved: 'Nothing resolved in the last 7 days.',
	};
</script>

<section class="inbox" aria-label="Inbox">
	<div class="tabs" role="tablist" aria-label="Inbox tabs">
		{#each TABS as t (t.id)}
			{@const n = countOf(t.id)}
			<button
				type="button"
				role="tab"
				aria-selected={store.tab === t.id}
				class:on={store.tab === t.id}
				onclick={() => store.setTab(t.id)}
			>
				{t.label}{#if n != null}<span class="n" aria-label="{n} conversations">{n}</span>{/if}
			</button>
		{/each}
	</div>
	<div class="tools">
		<span class="hint">{store.loading && !store.loaded ? 'Loading…' : `${store.rows.length} shown`}</span>
		<RoutingControl />
		<FilterPopover filters={store.filters} {reasons} {channels} onChange={(f) => store.setFilters(f)} />
	</div>
	{#if store.error && !store.rows.length}
		<p class="err" role="alert">{store.error}</p>
	{/if}
	<div class="list" role="tabpanel">
		{#each store.rows as r (rowKey(r.job, r.conversation_id))}
			<InboxRow
				row={r}
				{now}
				selected={selected === rowKey(r.job, r.conversation_id)}
				showJob={manyJobs}
				{onOpen}
			/>
		{:else}
			{#if store.loaded}<p class="empty">{empty[store.tab]}</p>{/if}
		{/each}
	</div>
</section>

<style>
	.inbox {
		display: grid;
		grid-template-rows: auto auto auto 1fr;
		min-height: 0;
		height: 100%;
		border: 1px solid var(--line);
		border-radius: 8px;
		background: var(--panel);
		overflow: hidden;
	}
	.tabs {
		display: flex;
		gap: 2px;
		padding: 6px 6px 0;
		border-bottom: 1px solid var(--line);
		overflow-x: auto;
	}
	.tabs button {
		border: 0;
		background: none;
		padding: 7px 8px;
		font: inherit;
		font-size: 12.5px;
		color: var(--muted);
		border-bottom: 2px solid transparent;
		white-space: nowrap;
		cursor: pointer;
		display: inline-flex;
		gap: 5px;
		align-items: center;
	}
	.tabs button.on {
		color: var(--fg);
		border-bottom-color: var(--accent);
	}
	.n {
		font: 500 10.5px/1 var(--mono);
		padding: 2px 5px;
		border-radius: 999px;
		background: var(--sunk);
	}
	.tools {
		display: flex;
		align-items: center;
		justify-content: space-between;
		padding: 6px 10px;
	}
	.hint {
		font-size: 12px;
		color: var(--muted);
	}
	.list {
		overflow-y: auto;
		min-height: 0;
	}
	.empty {
		padding: 16px 12px;
		color: var(--muted);
	}
	.err {
		color: var(--err);
		padding: 0 12px;
	}
	/* Touch: tabs and the two tools are 44 px targets below the wide layout; the list scrolls inside the pane. */
	@media (max-width: 1199.98px) {
		.tabs button,
		.tools :global(button),
		.tools :global(summary) {
			min-height: 44px;
		}
		.list {
			overscroll-behavior: contain;
			-webkit-overflow-scrolling: touch;
		}
	}
</style>
