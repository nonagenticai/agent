<script lang="ts">
	// "Needs you": the ranked inbox items the caller can act on, each with a severity bar, what and where, who owns it,
	// when it is due and one action. Items whose owner role the caller's effective roles do not cover are hidden
	// (tenant admins see all), so view-as previews exactly what that role would be asked to do.
	import type { AttentionItem } from '$lib/api/types/tenant';
	import type { Role } from '$lib/api/types/core';
	import { ROLE_LABEL } from '$lib/api/types/core';
	import { session } from '$lib/auth/session.svelte';
	import { when } from '$lib/runtime/labels';
	import { visibleItems } from '$lib/status/inbox';
	import { prefs } from './prefs.svelte';
	import TechDetail from './TechDetail.svelte';

	let {
		items,
		roles,
		jobLabel,
		empty = 'Nothing needs you right now.',
	}: {
		items: AttentionItem[];
		/** The caller's effective roles; the session's by default. */
		roles?: readonly Role[];
		/** Name of an item's agent, shown when the inbox spans several. */
		jobLabel?: (key: string) => string;
		empty?: string;
	} = $props();

	const shown = $derived(visibleItems(items, roles ?? session.effectiveRoles));
</script>

{#if shown.length === 0}
	<p class="muted ib-empty">{empty}</p>
{:else}
	<ul class="inbox" data-testid="inbox">
		{#each shown as a (a.id)}
			<li class="ib sev-{a.severity}" data-kind={a.kind} data-severity={a.severity}>
				<span class="bar" aria-hidden="true"></span>
				<div class="body">
					<b class="t">{a.title}</b>
					<div class="m">
						{#if a.detail && a.detail !== a.title}<span>{a.detail}</span>{/if}
						{#if jobLabel && a.job}<span>{jobLabel(a.job)}</span>{/if}
						<span>{a.owner_role === 'automatic' ? 'Handled automatically' : ROLE_LABEL[a.owner_role]}</span>
						{#if a.due_at}<span class="due">Due {when(a.due_at)}</span>{/if}
						<TechDetail><span class="mono">{a.kind} · {a.id}</span></TechDetail>
						{#if prefs.showTech && a.code}<span class="mono" data-testid="inbox-code">{a.code}</span>{/if}
					</div>
				</div>
				<a class="btn sm" class:primary={a.severity === 'blocking'} href={a.href}>{a.action}</a>
			</li>
		{/each}
	</ul>
{/if}

<style>
	.inbox {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 8px;
	}
	.ib {
		--c: var(--faint);
		display: grid;
		grid-template-columns: 4px minmax(0, 1fr) auto;
		gap: 12px;
		align-items: center;
		padding: 10px 12px 10px 0;
		border: 1px solid var(--line);
		border-radius: var(--r);
		background: var(--panel);
		overflow: hidden;
	}
	.ib.sev-blocking {
		--c: var(--err);
	}
	.ib.sev-action {
		--c: var(--warn);
	}
	.ib.sev-info {
		--c: var(--accent);
	}
	.bar {
		align-self: stretch;
		background: var(--c);
	}
	.body {
		min-width: 0;
	}
	.t {
		display: block;
		overflow-wrap: anywhere;
	}
	.m {
		display: flex;
		flex-wrap: wrap;
		gap: 2px 12px;
		margin-top: 2px;
		font-size: 12.5px;
		color: var(--muted);
	}
	.due {
		color: var(--fg);
	}
	.ib-empty {
		margin: 0;
	}
	@media (max-width: 560px) {
		.ib {
			grid-template-columns: 4px minmax(0, 1fr);
		}
		.ib .btn {
			grid-column: 2;
			justify-self: start;
		}
	}
</style>
