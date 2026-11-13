<script lang="ts">
	// One checklist item: what is missing, why it matters, who can fix it, and the link that fixes it.
	import type { Item } from '$lib/release/checklist';
	import { ownerLabel } from '$lib/release/checklist';
	import { plain } from '$lib/ui/labels';
	import TechDetail from '$lib/ui/TechDetail.svelte';

	let {
		item,
		tone,
		canFix,
		primary = false,
	}: { item: Item; tone: 'err' | 'warn'; canFix: boolean; primary?: boolean } = $props();
</script>

<li class="ck {tone}" data-gap={item.key}>
	<span class="ic" aria-hidden="true">{tone === 'err' ? '!' : 'i'}</span>
	<div class="what">
		<h4>{plain(item.title)}</h4>
		<p class="why">{plain(item.why)}</p>
		{#if item.testOnly}<p class="muted small">
				Test env only: limits testing the draft, not going live.
			</p>{/if}
		{#if item.cause}<p class="muted small">Comes from {plain(item.cause)}</p>{/if}
		<TechDetail><span class="mono">{item.tech}</span></TechDetail>
	</div>
	<div class="who">
		<small>Owner</small>
		<b>{ownerLabel(item.ownerRole)}</b>
		{#if !canFix && item.fixer === 'tenant_admin'}
			<span class="muted small">You can view it; ask a tenant admin to change it.</span>
		{/if}
	</div>
	<a class="btn sm" class:primary href={item.href}>{plain(item.fixLabel)} →</a>
</li>

<style>
	.ck {
		display: grid;
		grid-template-columns: 22px minmax(0, 1fr) minmax(110px, 160px) auto;
		gap: 10px 14px;
		align-items: start;
		padding: 12px 0;
		border-top: 1px solid var(--line-2);
	}
	.ck:first-child {
		border-top: 0;
	}
	.ic {
		display: grid;
		place-items: center;
		width: 20px;
		height: 20px;
		border-radius: 50%;
		font: 700 12px/1 var(--mono);
		margin-top: 1px;
	}
	.err .ic {
		color: var(--err);
		background: color-mix(in srgb, var(--err) 14%, transparent);
	}
	.warn .ic {
		color: var(--warn);
		background: color-mix(in srgb, var(--warn) 15%, transparent);
	}
	h4 {
		margin: 0;
		font-size: 14px;
	}
	p {
		margin: 2px 0 0;
		overflow-wrap: anywhere;
	}
	.why {
		font-size: 13px;
	}
	.small {
		font-size: 12px;
	}
	.who {
		display: flex;
		flex-direction: column;
		font-size: 13px;
	}
	.who small {
		color: var(--muted);
		font-size: 11px;
		text-transform: uppercase;
		letter-spacing: 0.04em;
	}
	@media (max-width: 860px) {
		.ck {
			grid-template-columns: 22px minmax(0, 1fr);
		}
		.who,
		.ck > .btn {
			grid-column: 2;
			justify-self: start;
		}
	}
</style>
