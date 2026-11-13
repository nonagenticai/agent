<script lang="ts">
	// B12 job console: the job's conversations on every channel (web chat, Telegram, …), who is
	// answering each one (the shared owner record: the agent, nobody yet, or a named colleague) and the transcript, read
	// only. Taking, replying and handing back happen in the staff workspace: the drawer links there. The open
	// transcript is the workspace's own `ConversationPane` and follows the conversation's live feed; the list re-reads
	// every 15 s. Reads: any member. B13: each conversation shows the customer (party) behind it; times are local
	// (`clock`, `stamp` in labels.ts).
	import { onDestroy, onMount } from 'svelte';
	import { api, app } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { canWrite } from '$lib/auth/roles';
	import {
		partyKindShown,
		partyName,
		type ConsoleConversationRow,
		type TranscriptRole,
	} from '$lib/api/types/conversations';
	import { watchConversation } from '$lib/conversations/feed';
	import { CHANNEL_LABEL, clock, stamp } from '$lib/conversations/labels';
	import { workspaceHref } from '$lib/support/links';
	import { zoneLabel } from '$lib/support/time';
	import ConversationPane from '$lib/support/ui/ConversationPane.svelte';
	import OwnerChip from '$lib/support/ui/OwnerChip.svelte';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LiveNote from '$lib/release/LiveNote.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';

	const res = new Resource(() => api().consoleConversations({}));
	const job = $derived(`/admin/jobs/${app.job}`);
	const me = $derived(session.user?.sub ?? null);
	/** The customer records (/admin/parties) need the Support agent role (§R.1). */
	const partiesOk = $derived(canWrite(session, 'support_agent'));

	let poll: ReturnType<typeof setInterval> | null = null;
	onMount(() => {
		res.refresh();
		poll = setInterval(() => {
			if (typeof document === 'undefined' || document.visibilityState !== 'hidden') void res.refresh();
		}, 15_000);
	});
	onDestroy(() => {
		if (poll) clearInterval(poll);
	});

	// ---- drawer: the read-only conversation ----
	let open = $state(false);
	let row = $state<ConsoleConversationRow | null>(null);

	function show(r: ConsoleConversationRow) {
		row = r;
		open = true;
	}
	/** Lifts the open row's fresh copy (owner, last activity) into the drawer after the list re-read. */
	async function reread() {
		await res.refresh();
		const v = res.value;
		if (row && v.state === 'ok') {
			const fresh = v.data.items.find((r) => r.conversation_id === row!.conversation_id);
			if (fresh) row = fresh;
		}
	}
	/** The pane's feed: any frame of the conversation re-reads the transcript (and the row behind it). */
	const framesOf = (id: string) => (on: (f: { event: string; data: unknown }) => void) =>
		watchConversation(api().conversationEvents, id, {
			onFrame: () => {
				on({ event: 'resync', data: { conversation_id: id } });
				void reread();
			},
		});

	const ROLE: Record<TranscriptRole, string> = {
		customer: 'Customer',
		caller: 'Caller',
		agent: 'Agent',
		staff: 'Staff',
		system: 'System',
	};
</script>

<PageHead
	id="J21"
	title="Conversations"
	sub="Every conversation of this job on every channel, and who is answering it. Open one in the workspace to take it and answer the customer."
>
	{#snippet actions()}
		<a class="btn primary" href="/support" data-testid="workspace-link">Open the workspace</a>
		<a class="btn" href="{job}/handover">Handover queue</a>
		<a class="btn" href="{job}/channels">Channel bindings</a>
	{/snippet}
</PageHead>

<LiveNote what="These are live conversations. Times are shown in {zoneLabel()}." />

<LoadState
	resource={res}
	empty="No conversations yet. When a customer writes on a bound channel, it shows here."
>
	{#snippet children(page)}
		<Card cls="flush">
			<div class="tbl-wrap">
				<table class="tbl clickable" data-testid="conversations">
					<thead>
						<tr><th>Customer</th><th>Channel</th><th>Case</th><th>Answering</th><th>Last message</th></tr>
					</thead>
					<tbody>
						{#each page.items as r (r.conversation_id)}
							<tr data-testid="conv-row" data-id={r.conversation_id} data-owner={r.owner.owner}>
								<td
									><button type="button" class="table-action" onclick={() => show(r)}
										>{r.customer_name ?? r.conversation_id}</button
									>{#if r.party}<span class="cell-sub" data-testid="conv-party"
											>{partyName(r.party)}{r.party.subject ? ` · ${r.party.subject}` : ''}</span
										>{/if}{#if r.open_item_id}<span class="cell-sub"
											><a href="{job}/handover" data-testid="open-item">{r.open_item_id}</a> open in the queue</span
										>{/if}</td
								>
								<td data-label="Channel"
									><span class="chip muted">{CHANNEL_LABEL[r.channel] ?? r.channel}</span></td
								>
								<td data-label="Case"
									><span class="mono">{r.case_id ?? '—'}</span><span class="cell-sub">{r.status}</span></td
								>
								<td data-label="Answering"><OwnerChip owner={r.owner} meSub={me} /></td>
								<td data-label="Last message">
									{#if r.last_message}
										<span class="cv-last"
											>{ROLE[r.last_message.role] ?? r.last_message.role}: {r.last_message.text}</span
										>
										<span class="cell-sub" title={stamp(r.last_message.at_ms)}
											>{clock(r.last_message.at_ms)}</span
										>
									{:else}<span class="muted">—</span>{/if}
								</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		</Card>
		{#if page.next}<p class="muted small">Showing the {page.items.length} most recent conversations.</p>{/if}
	{/snippet}
</LoadState>

<Drawer bind:open title={row ? (row.customer_name ?? 'Conversation') : 'Conversation'} wide>
	{#snippet sub()}
		{#if row}{CHANNEL_LABEL[row.channel] ?? row.channel} · {row.case_id ?? 'no case'} · {row.status}{/if}
	{/snippet}
	{#if row}
		{@const r = row}
		<div class="stack cv-d">
			<div class="row">
				<a class="btn primary" href={workspaceHref(app.job, r.conversation_id)} data-testid="open-workspace"
					>Open in workspace</a
				>
				{#if r.open_item_id}<a href="{job}/handover">Handover {r.open_item_id}</a>{/if}
			</div>
			{#if r.party}
				<p class="muted small" data-testid="drawer-party">
					Customer: <b>{partyName(r.party)}</b>{partyKindShown(r.party)
						? ` (${r.party.known ? 'known' : 'anonymous'})`
						: ''}{r.party.subject ? ` · ${r.party.subject}` : ''}
					{#if partiesOk}· <a href="/admin/parties/{r.party.id}">customer record</a>{/if}
				</p>
			{/if}
			<p class="muted small">Last activity {stamp(r.updated_at)}.</p>
			<div class="cv-pane">
				<ConversationPane
					readonly
					job={app.job}
					conversationId={r.conversation_id}
					frames={framesOf(r.conversation_id)}
					onOwnerChange={() => void reread()}
				/>
			</div>
		</div>
	{/if}
</Drawer>

<style>
	.cv-last {
		display: -webkit-box;
		-webkit-line-clamp: 2;
		line-clamp: 2;
		-webkit-box-orient: vertical;
		overflow: hidden;
		max-width: 42ch;
	}
	.cv-pane {
		height: 55vh;
		min-height: 320px;
	}
</style>
