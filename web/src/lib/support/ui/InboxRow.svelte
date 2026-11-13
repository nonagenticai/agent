<script lang="ts">
	// One inbox row: who (never a raw id), channel, reason, the last message on one line, how long ago, the SLA badge
	// (breached / at risk; none when fine) and the unread dot. A link to the conversation's URL.
	import Icon from '$lib/ui/Icon.svelte';
	import AutoChip from './AutoChip.svelte';
	import VerifiedChip from './VerifiedChip.svelte';
	import { CHANNEL_LABEL } from '$lib/conversations/labels';
	import type { InboxRow } from '$lib/api/types/support';
	import { relative } from '$lib/support/time';
	import { slaOf } from '$lib/support/inbox.svelte';

	let {
		row,
		now,
		selected = false,
		showJob = false,
		onOpen,
	}: {
		row: InboxRow;
		now: number;
		selected?: boolean;
		/** The tenant has more than one job: name it on the row. */
		showJob?: boolean;
		onOpen: (row: InboxRow) => void;
	} = $props();

	const href = $derived(
		`/support/${encodeURIComponent(row.job)}/c/${encodeURIComponent(row.conversation_id)}`,
	);
	// The one SLA rule (window from `sla_starts_at`), as in the toolbar and the context pane.
	const sla = $derived(slaOf(row, now));
	const channel = $derived((CHANNEL_LABEL as Record<string, string>)[row.channel] ?? row.channel);
	const lead = $derived(
		row.last_message.kind === 'customer'
			? ''
			: row.last_message.kind === 'note'
				? 'Note: '
				: row.last_message.kind === 'agent'
					? 'Agent: '
					: row.last_message.kind === 'staff'
						? 'Staff: '
						: '',
	);

	function open(e: MouseEvent) {
		if (e.metaKey || e.ctrlKey || e.shiftKey || e.button !== 0) return;
		e.preventDefault();
		onOpen(row);
	}
</script>

<a
	class="row"
	class:sel={selected}
	{href}
	data-conversation={row.conversation_id}
	data-job={row.job}
	aria-current={selected ? 'true' : undefined}
	onclick={open}
>
	<span class="top">
		{#if row.unread}<span class="dot" data-unread title="Unread" aria-label="Unread"></span>{/if}
		<strong class="name">{row.display_name}</strong>
		<VerifiedChip verified={row.verified} />
		<span class="ch" title={channel}><Icon name="chat" size={13} /><span class="sr">{channel}</span></span>
		<span class="sp"></span>
		<time class="ago" datetime={row.last_message.at}>{relative(row.last_message.at, now)}</time>
	</span>
	<span class="msg">{lead}{row.last_message.text}</span>
	<span class="tags">
		{#if row.reason_label}<span class="chip muted">{row.reason_label}</span>{/if}
		<AutoChip owner={row.owner} />
		{#if row.priority === 'urgent'}<span class="chip err" data-priority="urgent">Urgent</span>{/if}
		{#if row.priority === 'high'}<span class="chip warn">High</span>{/if}
		{#if sla === 'breached'}<span class="chip err" data-sla="breached">SLA breached</span>
		{:else if sla === 'at_risk'}<span class="chip warn" data-sla="at_risk">SLA at risk</span>{/if}
		{#if showJob}<span class="job">{row.job_name}</span>{/if}
	</span>
</a>

<style>
	.row {
		display: grid;
		gap: 3px;
		padding: 9px 12px;
		border-bottom: 1px solid var(--line-2);
		color: inherit;
		text-decoration: none;
		min-width: 0;
	}
	.row:hover {
		background: var(--sunk);
	}
	.row.sel {
		background: color-mix(in srgb, var(--accent) 12%, transparent);
		box-shadow: inset 3px 0 0 var(--accent);
	}
	.top {
		display: flex;
		align-items: center;
		gap: 6px;
		min-width: 0;
	}
	.name {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.dot {
		width: 8px;
		height: 8px;
		border-radius: 50%;
		background: var(--accent);
		flex: none;
	}
	.ch {
		color: var(--muted);
		display: inline-flex;
	}
	.sr {
		position: absolute;
		width: 1px;
		height: 1px;
		overflow: hidden;
		clip: rect(0 0 0 0);
	}
	.sp {
		flex: 1;
	}
	.ago {
		color: var(--muted);
		font-size: 12px;
		white-space: nowrap;
	}
	.msg {
		color: var(--muted);
		font-size: 13px;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.tags {
		display: flex;
		flex-wrap: wrap;
		gap: 4px;
		align-items: center;
	}
	.job {
		color: var(--muted);
		font-size: 11.5px;
	}
</style>
