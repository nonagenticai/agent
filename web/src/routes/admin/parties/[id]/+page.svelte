<script lang="ts">
	// B13 Customer detail: one party — its channel identities (verified or not, first and last seen) and its cases
	// in every job of the tenant. A merged customer points at the one it answers as. Reading needs the Support agent
	// role (D88).
	import { onMount } from 'svelte';
	import { page } from '$app/state';
	import { api } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { canWrite, writeHint } from '$lib/auth/roles';
	import { partyName, type ConversationChannel, type PartyDetail } from '$lib/api/types/conversations';
	import { CHANNEL_LABEL, stamp } from '$lib/conversations/labels';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Card from '$lib/ui/Card.svelte';

	const id = $derived(page.params.id ?? '');
	const reader = $derived(canWrite(session, 'support_agent'));
	const res = new Resource<PartyDetail>(
		() => api().party(id),
		() => false,
	);
	onMount(() => {
		if (reader) void res.refresh();
	});
	const channelLabel = (c: string) => CHANNEL_LABEL[c as ConversationChannel] ?? c;
</script>

{#if !reader}
	<PageHead title="Customer" />
	<p class="muted" data-testid="party-role-note">{writeHint('support_agent')} to read customer records.</p>
{:else}
	<LoadState resource={res}>
		{#snippet children(p)}
			<PageHead
				title={partyName(p)}
				sub={`${p.known ? 'Known' : 'Anonymous'} customer${p.subject ? ` · ${p.subject}` : ''} · first seen ${stamp(p.created_at)}`}
			>
				{#snippet actions()}
					<a class="btn" href="/admin/parties">All customers</a>
				{/snippet}
			</PageHead>
			{#if p.merged_into}
				<p class="pt-merged" role="status" data-testid="party-merged">
					This customer was merged into <a href="/admin/parties/{p.merged_into}">another customer</a>, and
					answers as that one.
				</p>
			{/if}

			<h2>Channel identities</h2>
			<Card cls="flush">
				<div class="tbl-wrap">
					<table class="tbl" data-testid="party-identities">
						<thead>
							<tr><th>Channel</th><th>Identity</th><th>Verified</th><th>First seen</th><th>Last seen</th></tr>
						</thead>
						<tbody>
							{#each p.identities as i (i.channel + i.subject)}
								<tr>
									<td>{channelLabel(i.channel)}</td>
									<td data-label="Identity"><span class="mono pt-sub">{i.subject}</span></td>
									<td data-label="Verified"
										><span class="chip {i.verified ? 'ok' : 'muted'}"
											>{i.verified ? 'verified' : 'not verified'}</span
										></td
									>
									<td data-label="First seen">{stamp(i.first_seen_at)}</td>
									<td data-label="Last seen">{stamp(i.last_seen_at)}</td>
								</tr>
							{:else}
								<tr><td colspan="5" class="muted">No channel identities.</td></tr>
							{/each}
						</tbody>
					</table>
				</div>
			</Card>

			<h2>Cases</h2>
			<Card cls="flush">
				<div class="tbl-wrap">
					<table class="tbl" data-testid="party-cases">
						<thead>
							<tr><th>Case</th><th>Job</th><th>Channel</th><th>Status</th><th>Started</th></tr>
						</thead>
						<tbody>
							{#each p.cases as c (c.job + c.case_id)}
								<tr>
									<td
										><a class="mono" href="/admin/jobs/{c.job}/cases/{c.case_id}">{c.case_id}</a
										>{#if c.title}<span class="cell-sub">{c.title}</span>{/if}</td
									>
									<td data-label="Job">{c.job}</td>
									<td data-label="Channel">{channelLabel(c.channel)}</td>
									<td data-label="Status">{c.status}</td>
									<td data-label="Started">{stamp(c.started_at)}</td>
								</tr>
							{:else}
								<tr><td colspan="5" class="muted">No cases yet.</td></tr>
							{/each}
						</tbody>
					</table>
				</div>
			</Card>
		{/snippet}
	</LoadState>
{/if}

<style>
	.pt-sub {
		overflow-wrap: anywhere;
	}
	.pt-merged {
		padding: 8px 12px;
		border-radius: 8px;
		background: color-mix(in srgb, var(--warn, #c80) 10%, transparent);
	}
	h2 {
		margin-top: 16px;
	}
</style>
