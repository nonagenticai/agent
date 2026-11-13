<script lang="ts">
	// The only scroll container of the pane. Opens at the newest message; a message that arrives while the reader is
	// scrolled up does not move the view, it raises a "N new messages" pill. Episode dividers sit before the first
	// message of the episode (by time, so system rows without a case land in the right place).
	import { tick, untrack } from 'svelte';
	import type { Episode } from '$lib/api/types/support';
	import type { Row } from '$lib/support/rows';
	import MessageItem from './MessageItem.svelte';
	import EpisodeDivider from './EpisodeDivider.svelte';

	let {
		messages,
		episodes,
		onRetry,
		job = '',
		conversationId = '',
		customerName = '',
	}: {
		messages: Row[];
		episodes: Episode[];
		onRetry?: (m: Row) => void;
		job?: string;
		conversationId?: string;
		/** The conversation's customer as the header names them (MessageItem's label for channel-id authors). */
		customerName?: string;
	} = $props();

	type Item = { key: string; episode?: Episode; message?: Row };
	const items = $derived.by<Item[]>(() => {
		const eps = [...episodes].sort((a, b) => Date.parse(a.queued_at) - Date.parse(b.queued_at));
		const out: Item[] = [];
		let e = 0;
		for (const m of messages) {
			const t = Date.parse(m.at);
			for (let ep = eps[e]; ep && Date.parse(ep.queued_at) <= t; ep = eps[++e]) {
				out.push({ key: `ep-${ep.item_id}`, episode: ep });
			}
			out.push({ key: m.client_msg_id ?? m.message_id, message: m });
		}
		for (const ep of eps.slice(e)) out.push({ key: `ep-${ep.item_id}`, episode: ep });
		return out;
	});

	let el = $state<HTMLDivElement | null>(null);
	let atBottom = true;
	let seen = 0;
	let started = false;
	let unseen = $state(0);

	function toEnd() {
		if (!el) return;
		el.scrollTop = el.scrollHeight - el.clientHeight;
		atBottom = true;
		unseen = 0;
	}
	function onScroll() {
		if (!el) return;
		atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 48;
		if (atBottom) unseen = 0;
	}

	// The pane can change height under the reader (the toolbar gains a line, the composer grows, the window
	// resizes): a reader at the end stays at the end.
	$effect(() => {
		if (!el || typeof ResizeObserver === 'undefined') return;
		const ro = new ResizeObserver(() => {
			if (atBottom) toEnd();
		});
		ro.observe(el);
		return () => ro.disconnect();
	});

	$effect(() => {
		const n = messages.length;
		const last = messages[n - 1];
		untrack(() => {
			if (!el) return;
			if (!started) {
				if (n === 0) return;
				started = true;
				seen = n;
				tick().then(toEnd);
				return;
			}
			const added = n - seen;
			seen = n;
			if (added <= 0) return;
			// Your own message always follows you down; somebody else's only when you are already at the end.
			if (atBottom || last?.local) tick().then(toEnd);
			else unseen += added;
		});
	});
</script>

<div class="wrap">
	<div class="tr" data-testid="transcript" style="overflow-y:auto" bind:this={el} onscroll={onScroll}>
		{#each items as it (it.key)}
			{#if it.episode}<EpisodeDivider episode={it.episode} />{:else if it.message}<MessageItem
					message={it.message}
					{onRetry}
					{job}
					{conversationId}
					{customerName}
				/>{/if}
		{/each}
	</div>
	{#if unseen > 0}
		<button class="pill btn sm primary" type="button" onclick={toEnd}
			>{unseen} new message{unseen === 1 ? '' : 's'}</button
		>
	{/if}
</div>

<style>
	.wrap {
		position: relative;
		min-height: 0;
		display: grid;
	}
	.tr {
		overflow-y: auto;
		min-height: 0;
		padding: 12px;
		display: flex;
		flex-direction: column;
		gap: 8px;
		background: var(--bg);
	}
	.pill {
		position: absolute;
		bottom: 10px;
		left: 50%;
		transform: translateX(-50%);
		border-radius: 999px;
		box-shadow: var(--shadow);
	}
</style>
