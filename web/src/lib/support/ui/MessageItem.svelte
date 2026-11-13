<script lang="ts">
	// One transcript row, by kind: customer left; agent / staff right with an avatar; system a centred one-line
	// divider; note amber with a lock and never delivered anywhere. Text is plain (no markup), wrapped anywhere.
	import Icon from '$lib/ui/Icon.svelte';
	import type { Row } from '$lib/support/rows';
	import { clockOf } from '$lib/support/time';
	import AttachmentView from './AttachmentView.svelte';

	let {
		message,
		onRetry,
		job = '',
		conversationId = '',
		customerName = '',
	}: {
		message: Row;
		onRetry?: (m: Row) => void;
		job?: string;
		conversationId?: string;
		/** The conversation's customer as the header names them (a customer row stored under a channel id shows it). */
		customerName?: string;
	} = $props();

	// a customer line's author is the channel's party id when the channel gave no name (web chat `wc:<session>`, `tg:…`):
	// never shown, the conversation's customer name (or plain "Customer") is
	const PARTY_ID = /^[a-z]{2,10}:[\w.-]{4,}$/i;
	const customerLabel = $derived(
		message.author_name && !PARTY_ID.test(message.author_name.trim())
			? message.author_name
			: customerName.trim() || 'Customer',
	);

	const initials = (n: string | null) =>
		(n ?? '')
			.split(/\s+/)
			.filter(Boolean)
			.slice(0, 2)
			.map((p) => p[0]!.toUpperCase())
			.join('') || '?';
	const status = $derived(
		message.local === 'sending'
			? 'Sending…'
			: message.local === 'failed'
				? (message.reason ?? 'Not sent')
				: message.delivery === 'failed'
					? 'Not delivered'
					: message.delivery === 'delivered'
						? 'Delivered'
						: message.delivery === 'sent'
							? 'Sent'
							: '',
	);
	const bad = $derived(message.local === 'failed' || message.delivery === 'failed');
</script>

{#if message.kind === 'system'}
	<div class="sys" role="separator" aria-label={message.text}>
		<span>{message.text}</span><time>{clockOf(message.at)}</time>
	</div>
{:else if message.kind === 'note'}
	<div class="line right" data-kind="note">
		<div class="bubble n-note">
			<div class="meta">
				<Icon name="lock" size={13} />
				<strong>{message.author_name ?? 'Staff'}</strong>
				<span>Visible to staff only</span>
				<time>{clockOf(message.at)}</time>
			</div>
			<p>{message.text}</p>
			{#if status}<div class="st" class:bad>{status}</div>{/if}
			{#if message.mirror === 'failed'}
				<div class="st">
					<span
						class="chip warn"
						data-testid="mirror-chip"
						title="The helpdesk ticket refused this note or could not be reached; it is kept here."
						>Not copied to ticket</span
					>
				</div>
			{/if}
			{#if message.local === 'failed' && !message.noRetry}
				<button class="btn sm" type="button" onclick={() => onRetry?.(message)}>Retry</button>
			{/if}
		</div>
	</div>
{:else}
	{@const mine = message.kind !== 'customer'}
	<div class="line" class:right={mine} data-kind={message.kind}>
		{#if mine}
			<div class="bubble out" class:agent={message.kind === 'agent'}>
				<div class="meta">
					<strong>{message.kind === 'agent' ? 'Agent' : (message.author_name ?? 'Staff')}</strong>
					<time>{clockOf(message.at)}</time>
				</div>
				<p>{message.text}</p>
				{#each message.attachments ?? [] as a (a.id)}<AttachmentView att={a} {job} {conversationId} />{/each}
				{#if status}<div class="st" class:bad>{status}</div>{/if}
				{#if message.local === 'failed' && !message.noRetry}
					<button class="btn sm" type="button" onclick={() => onRetry?.(message)}>Retry</button>
				{/if}
			</div>
			<div class="ava" class:bot={message.kind === 'agent'} aria-hidden="true">
				{#if message.kind === 'agent'}<Icon name="bot" size={16} />{:else}{initials(message.author_name)}{/if}
			</div>
		{:else}
			<div class="bubble in">
				<div class="meta">
					<strong>{customerLabel}</strong>
					<time>{clockOf(message.at)}</time>
				</div>
				<p>{message.text}</p>
				{#each message.attachments ?? [] as a (a.id)}<AttachmentView att={a} {job} {conversationId} />{/each}
			</div>
		{/if}
	</div>
{/if}

<style>
	.line {
		display: flex;
		align-items: flex-end;
		gap: 8px;
		justify-content: flex-start;
	}
	.line.right {
		justify-content: flex-end;
	}
	.bubble {
		max-width: 72%;
		overflow-wrap: anywhere;
		border: 1px solid var(--line);
		border-radius: 10px;
		padding: 7px 11px;
		background: var(--panel);
		min-width: 0;
	}
	.bubble.in {
		border-color: color-mix(in srgb, var(--cust) 35%, var(--line));
	}
	.bubble.out {
		background: color-mix(in srgb, var(--accent) 9%, var(--panel));
	}
	.bubble.out.agent {
		background: var(--sunk);
	}
	.n-note {
		background: var(--note);
		border-color: var(--note-line);
		color: var(--note-fg);
	}
	p {
		margin: 2px 0 0;
		white-space: pre-wrap;
	}
	.meta {
		display: flex;
		align-items: center;
		gap: 6px;
		font-size: 11px;
		color: var(--muted);
	}
	.n-note .meta {
		color: var(--note-fg);
	}
	.meta strong {
		color: var(--fg);
		font-weight: 600;
	}
	.n-note .meta strong {
		color: inherit;
	}
	time {
		margin-left: auto;
		font-variant-numeric: tabular-nums;
	}
	.st {
		margin-top: 3px;
		font-size: 11px;
		color: var(--muted);
		text-align: right;
	}
	.st.bad {
		color: var(--err);
	}
	.ava {
		flex: none;
		width: 28px;
		height: 28px;
		border-radius: 50%;
		display: grid;
		place-items: center;
		font-size: 11px;
		font-weight: 600;
		background: var(--accent);
		color: var(--panel);
	}
	.ava.bot {
		background: var(--sunk);
		color: var(--tool);
		border: 1px solid var(--line);
	}
	.sys {
		/* in the transcript's column flex: overflow:hidden would let a full pane squeeze this row to no height */
		flex: none;
		display: flex;
		justify-content: center;
		gap: 8px;
		font-size: 12px;
		color: var(--muted);
		white-space: nowrap;
		overflow: hidden;
	}
	.sys span {
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.sys time {
		margin: 0;
	}
</style>
