<script lang="ts">
	// B13 Customers (party identity, tenant-wide): who a customer is across channels — one party per customer, with
	// the channel identities it was seen under (a web chat session, a Telegram user, …). Staff
	// search by name, customer ref or channel id and open a customer to see its identities and cases. Reading needs
	// the Support agent role (D88).
	import { onMount } from 'svelte';
	import { api } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { canWrite, writeHint } from '$lib/auth/roles';
	import { partyName, type PartySummary } from '$lib/api/types/conversations';
	import type { Paged } from '$lib/api/types/core';
	import { CHANNEL_LABEL, stamp } from '$lib/conversations/labels';
	import type { ConversationChannel } from '$lib/api/types/conversations';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Card from '$lib/ui/Card.svelte';

	const reader = $derived(canWrite(session, 'support_agent'));

	let q = $state('');
	let applied = $state('');
	const res = new Resource<Paged<PartySummary>>(() => api().parties({ q: applied || undefined }));
	/** Older pages appended by "Show more". */
	let more = $state<PartySummary[]>([]);
	let next = $state<string | null>(null);
	let loadingMore = $state(false);
	$effect(() => {
		const v = res.value;
		if (v.state === 'ok' || v.state === 'empty') next = v.data.next;
	});

	function search() {
		applied = q.trim();
		more = [];
		void res.refresh();
	}
	async function showMore() {
		if (!next) return;
		loadingMore = true;
		try {
			const r = await api().parties({ q: applied || undefined, before: next });
			more = [...more, ...r.items];
			next = r.next;
		} finally {
			loadingMore = false;
		}
	}

	const channelLabel = (c: string) => CHANNEL_LABEL[c as ConversationChannel] ?? c;
	const channelsOf = (p: PartySummary) => [
		...new Set((p.identities ?? []).map((i) => channelLabel(i.channel))),
	];

	onMount(() => {
		if (reader) void res.refresh();
	});
</script>

<PageHead
	title="Customers"
	sub="Who your customers are across channels: one record per customer, with every channel identity it was seen under and its cases in every job."
/>

{#if !reader}
	<p class="muted" data-testid="parties-role-note">{writeHint('support_agent')} to read customer records.</p>
{:else}
	<form
		class="pt-search"
		onsubmit={(e) => {
			e.preventDefault();
			search();
		}}
	>
		<label class="field"
			>Search
			<input
				class="input"
				type="search"
				placeholder="name, customer ref or channel id"
				bind:value={q}
				data-testid="parties-q"
			/>
		</label>
		<button class="btn" type="submit">Search</button>
	</form>

	<LoadState
		resource={res}
		empty={applied
			? `No customer matches “${applied}”.`
			: 'No customers yet. They appear when they first write on a channel.'}
	>
		{#snippet children(page)}
			<Card cls="flush">
				<div class="tbl-wrap">
					<table class="tbl" data-testid="parties">
						<thead>
							<tr><th>Customer</th><th>Kind</th><th>Channels</th><th>First seen</th></tr>
						</thead>
						<tbody>
							{#each [...page.items, ...more] as p (p.id)}
								<tr data-testid="party-row" data-id={p.id}>
									<td
										><a href="/admin/parties/{p.id}"><b>{partyName(p)}</b></a>{#if p.subject}<span
												class="cell-sub mono">{p.subject}</span
											>{/if}{#if p.merged_into}<span class="cell-sub">merged into another customer</span
											>{/if}</td
									>
									<td data-label="Kind"
										><span class="chip {p.known ? 'ok' : 'muted'}">{p.known ? 'known' : 'anonymous'}</span
										></td
									>
									<td data-label="Channels">{channelsOf(p).join(', ') || '—'}</td>
									<td data-label="First seen">{stamp(p.created_at)}</td>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
			</Card>
			{#if next}
				<button class="btn" type="button" disabled={loadingMore} onclick={showMore}>Show more</button>
			{/if}
		{/snippet}
	</LoadState>
{/if}

<style>
	.pt-search {
		display: flex;
		gap: 8px;
		align-items: flex-end;
		margin-bottom: 12px;
		max-width: 560px;
	}
	.pt-search .field {
		flex: 1;
	}
</style>
