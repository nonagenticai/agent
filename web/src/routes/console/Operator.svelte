<script lang="ts">
	// L1 operator view: what is happening right now across the job's channels, and where a person has to step in.
	// Lead: the conversations that need you (unknown outcomes, failures, handovers), each linking to the staff workspace.
	// Then every live conversation, and for the one you watch, each turn with what the qualifier, the procedure and
	// the post-gate did. Who answers is the shared owner record; taking, replying and handing back happen in the
	// workspace (`/support`), never here.
	// Production runs the published release; the test env (staff test chat) runs the job's draft (D54).
	// A real caller reads B12's console rows (the open conversation with its transcript); the job feed and the open
	// conversation's feed (SSE) re-read it as frames arrive, a slow poll is the safety net. The fixture replays.
	import { onDestroy, onMount } from 'svelte';
	import { api, app } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import type { ConsoleConversation, ConsoleSnapshot } from '$lib/api/types/live';
	import type { DraftSummary } from '$lib/api/types/release';
	import { createFeed, type FeedStatus } from '$lib/live/feed';
	import {
		PHASE,
		attentionOf,
		byUrgency,
		hhmmss,
		heldByStaff,
		lastText,
		phaseOf,
		stepOf,
		turnsOf,
		type Phase,
	} from '$lib/live/console/L1-operator';
	import { CONSOLE_FRAMES, latestWins } from '$lib/live/console/L1-http';
	import { watchConversation, watchJob } from '$lib/conversations/feed';
	import { CHANNEL_LABEL } from '$lib/conversations/labels';
	import type { ConversationChannel, ConversationFrame } from '$lib/api/types/conversations';
	import { workspaceHref } from '$lib/support/links';
	import OwnerChip from '$lib/support/ui/OwnerChip.svelte';
	import VerifiedChip from '$lib/support/ui/VerifiedChip.svelte';
	import Seg from '$lib/ui/Seg.svelte';
	import TurnCard from './TurnCard.svelte';

	let { paused = false, onstatus }: { paused?: boolean; onstatus?: (s: FeedStatus) => void } = $props();

	let snap = $state<ConsoleSnapshot | null>(null);
	let held = $state<ConsoleSnapshot | null>(null);
	let status = $state<FeedStatus>('stopped');
	let draft = $state<DraftSummary | null>(null);
	let selectedId = $state<string | null>(null);
	let show = $state<'all' | 'needs_you' | 'live' | 'staff' | 'done'>('all');
	let env = $state<'all' | 'prod' | 'test'>('all');
	let pane = $state<'list' | 'detail'>('list');
	/** The conversation the operator has open (its whole transcript is read). */
	const openId = () => selected?.case.conversation_id ?? null;
	/** Every read (poll, frame, after an action) goes through one latest-wins gate: null = a newer one landed. */
	const latest = latestWins<ConsoleSnapshot>();
	const read = () => latest(() => api().operatorConsole({ open: openId() }));
	const present = (s: ConsoleSnapshot | null) => {
		if (!s) return;
		if (paused) held = s;
		else snap = s;
	};

	const feed = createFeed<ConsoleSnapshot>({
		pull: async () => {
			const s = await read();
			return s ? [s] : [];
		},
		onEvents: (s) => present(s.at(-1) ?? null),
		onStatus: (s) => {
			status = s;
			onstatus?.(s);
		},
		// frames drive the re-reads; the poll is only the safety net
		tickMs: 15_000,
		connectMs: 350,
		retryMs: 2600,
	});

	/** Frames arrive in bursts (message + handover + owner): one re-read per burst. */
	let soon: ReturnType<typeof setTimeout> | null = null;
	function onFrame(f: ConversationFrame) {
		if (!CONSOLE_FRAMES.has(f.event) || soon) return;
		soon = setTimeout(() => {
			soon = null;
			void read()
				.then(present)
				.catch(() => {});
		}, 250);
	}
	// The job's feed: new conversations, customer messages, handovers, owner changes — resubscribed per job.
	$effect(() => {
		void app.job;
		return watchJob((o) => api().jobEvents(o), { onFrame });
	});
	/** Simulates a dropped stream (the page's "Drop connection"). */
	export function drop() {
		feed.drop();
	}
	$effect(() => {
		if (!paused && held) {
			snap = held;
			held = null;
		}
	});

	const all = $derived(snap ? [...snap.conversations].sort(byUrgency) : []);
	const phases = $derived(new Map(all.map((c) => [c.case.case_id, phaseOf(c)] as [string, Phase])));
	const ph = (c: ConsoleConversation) => phases.get(c.case.case_id) ?? 'waiting';
	const count = (p: Phase) => all.filter((c) => ph(c) === p).length;
	const needs = $derived(all.filter((c) => ph(c) === 'needs_you'));
	const prod = $derived(all.filter((c) => c.case.env === 'prod'));
	const test = $derived(all.filter((c) => c.case.env === 'test'));
	const shown = $derived(
		all.filter(
			(c) =>
				(env === 'all' || c.case.env === env) &&
				(show === 'all' || (show === 'live' ? ph(c) === 'working' || ph(c) === 'waiting' : ph(c) === show)),
		),
	);
	/** Where the agents are right now: per step, the conversations whose latest turn is at that step. */
	const atStep = $derived.by(() => {
		const n = { qualifier: 0, procedure: 0, post_gate: 0, reply: 0 };
		for (const c of all) if (ph(c) === 'working') n[stepOf(turnsOf(c.events).at(-1)!)]++;
		return n;
	});

	const selected = $derived(
		all.find((c) => c.case.case_id === selectedId) ??
			needs[0] ??
			all.find((c) => ph(c) === 'working') ??
			all[0] ??
			null,
	);
	// The open conversation's own feed (its replay after a drop, `Last-Event-ID`, is per conversation).
	const watched = $derived(selected?.case.conversation_id ?? null);
	$effect(() => {
		const id = watched;
		if (!id) return;
		return watchConversation((cid, o) => api().conversationEvents(cid, o), id, { onFrame });
	});
	const turns = $derived(selected ? turnsOf(selected.events) : []);
	const attention = $derived(selected ? attentionOf(selected) : null);
	const me = $derived(session.user?.sub ?? null);
	const wsHref = (c: ConsoleConversation) => workspaceHref(app.job, c.case.conversation_id);
	const caseHref = (c: ConsoleConversation) =>
		`/admin/jobs/${encodeURIComponent(app.job)}/cases/${encodeURIComponent(c.case.case_id)}`;
	const releaseHref = $derived(`/admin/jobs/${encodeURIComponent(app.job)}/release`);
	const channelLabel = (c: ConsoleConversation) =>
		CHANNEL_LABEL[c.case.channel as ConversationChannel] ?? c.case.channel;
	const since = (iso: string) => {
		const s = Math.max(0, Math.round(((snap?.at_ms ?? Date.now()) - Date.parse(iso)) / 1000));
		return s < 60
			? `${s}s ago`
			: s < 3600
				? `${Math.round(s / 60)} min ago`
				: `${Math.round(s / 3600)} h ago`;
	};

	function pick(c: ConsoleConversation) {
		selectedId = c.case.case_id;
		pane = 'detail';
		// the newly opened conversation's transcript (the list carries only its last line)
		void refresh();
	}
	async function refresh() {
		try {
			present(await read());
		} catch {
			/* the feed shows the stream state */
		}
	}
	onMount(() => {
		feed.start();
		api()
			.draftSummary()
			.then((d) => (draft = d))
			.catch(() => (draft = null));
	});
	onDestroy(() => {
		feed.stop();
		if (soon) clearTimeout(soon);
	});
</script>

<div class="l1" data-pane-view={pane}>
	<section class="l1-flow" aria-label="How a turn flows">
		<div class="l1-flow-steps">
			<span class="l1-node l1-node-cust"><b>Customer</b><small>Telegram · staff test chat</small></span>
			<span class="l1-arrow" aria-hidden="true">→</span>
			<span class="l1-node"
				><b>Qualifier</b><small>P1/P2 gates · activity · slots</small><em>{atStep.qualifier} now</em></span
			>
			<span class="l1-arrow" aria-hidden="true">→</span>
			<span class="l1-node"
				><b>Procedure</b><small>operations · knowledge</small><em>{atStep.procedure} now</em></span
			>
			<span class="l1-arrow" aria-hidden="true">→</span>
			<span class="l1-node"
				><b>Post-gate</b><small>R1 checks the reply</small><em>{atStep.post_gate} now</em></span
			>
			<span class="l1-arrow" aria-hidden="true">→</span>
			<span class="l1-node l1-node-cust"><b>Reply</b><small>to the customer</small></span>
		</div>
		<p class="l1-flow-you">
			<b>You step in from the workspace:</b> open a conversation there and <span class="l1-kbd">Take</span>
			it; the agent is silent while a colleague has it, and answers the next message after a
			<span class="l1-kbd">Hand back</span>.
		</p>
	</section>

	<div class="l1-counts" role="status" aria-live="polite">
		{#each ['needs_you', 'staff', 'working', 'waiting', 'done'] as const as p (p)}
			<button
				type="button"
				class="l1-count l1-count-{p}"
				class:on={show === (p === 'working' || p === 'waiting' ? 'live' : p) &&
					(p !== 'waiting' || show !== 'live')}
				title={PHASE[p].hint}
				onclick={() => (show = p === 'working' || p === 'waiting' ? 'live' : p)}
			>
				<b>{count(p)}</b><span>{PHASE[p].label}</span>
			</button>
		{/each}
		<span class="l1-envsplit">
			<span><i class="l1-dot prod"></i>Production {prod.length}</span>
			<span><i class="l1-dot test"></i>Test env {test.length}</span>
		</span>
	</div>

	<section class="l1-needs" aria-label="Needs you now" data-testid="console-needs">
		<h2>Needs you now</h2>
		{#if snap === null}
			<p class="l1-quiet" role="status">
				{status === 'retrying' ? 'Reconnecting to the conversations…' : 'Connecting to the conversations…'}
			</p>
		{:else if needs.length === 0}
			<p class="l1-quiet l1-allclear">
				Nothing needs you. The agent is handling {count('working') + count('waiting')} conversation{count(
					'working',
				) +
					count('waiting') ===
				1
					? ''
					: 's'}{count('staff') ? `; staff answer ${count('staff')}` : ''}.
			</p>
		{:else}
			<ul class="l1-needs-list">
				{#each needs as c (c.case.case_id)}
					{@const a = attentionOf(c)!}
					<li class="l1-need {a.severity}" class:sel={selected?.case.case_id === c.case.case_id}>
						<div class="l1-need-top">
							<b>{a.title}</b>
							<span class="l1-time">{since(c.case.updated_at)}</span>
						</div>
						<span class="l1-need-who"
							>{c.case.customer_name} · {c.case.title}
							<span class="l1-env {c.case.env}">{c.case.env === 'prod' ? 'prod' : 'test · draft'}</span></span
						>
						<span class="l1-need-hint">{a.hint}</span>
						<div class="l1-need-act">
							<a class="btn sm primary" href={wsHref(c)}>Open in workspace</a>
							<button type="button" class="btn sm" onclick={() => pick(c)}>Watch</button>
						</div>
					</li>
				{/each}
			</ul>
		{/if}
	</section>

	<div class="l1-grid">
		<section class="l1-list" aria-label="Conversations" data-pane="list">
			<header class="l1-list-head">
				<h2>Conversations <span class="l1-n">{shown.length}</span></h2>
				<Seg
					items={[
						['all', 'All'],
						['prod', 'Production'],
						['test', 'Test env'],
					]}
					bind:active={env}
					label="Environment"
				/>
			</header>
			{#if show !== 'all'}
				<button type="button" class="l1-filter" onclick={() => (show = 'all')}
					>Showing: {show === 'live' ? 'agent working or waiting' : PHASE[show].label.toLowerCase()} · show all
					✕</button
				>
			{/if}
			<ul>
				{#each shown as c (c.case.case_id)}
					{@const p = ph(c)}
					<li>
						<button
							type="button"
							class="l1-row"
							class:sel={selected?.case.case_id === c.case.case_id}
							data-phase={p}
							data-testid="console-conversation"
							data-case={c.case.case_id}
							onclick={() => pick(c)}
						>
							<span class="l1-row-top">
								<span class="l1-phase {p}"
									>{#if p === 'working'}<i class="l1-pulse"></i>{/if}{PHASE[p].label}</span
								>
								<span class="l1-env {c.case.env}">{c.case.env === 'prod' ? 'prod' : 'test · draft'}</span>
								<span class="l1-time">{hhmmss(c.case.updated_at)}</span>
							</span>
							<span class="l1-row-name"
								>{c.case.customer_name}
								<VerifiedChip verified={c.case.identity_level >= 2} />
								<small>· {channelLabel(c)}</small></span
							>
							<span class="l1-row-title">{c.case.title}</span>
							<span class="l1-row-last">{lastText(c)}</span>
						</button>
					</li>
				{:else}
					<li class="l1-quiet">
						{snap === null
							? 'Waiting for the stream…'
							: 'No conversation matches. Clear the filter to see all.'}
					</li>
				{/each}
			</ul>
		</section>

		<section class="l1-detail" aria-label="Conversation" data-pane="detail" data-testid="console-detail">
			{#if selected}
				{@const c = selected}
				{@const p = ph(c)}
				<button type="button" class="btn sm l1-back" onclick={() => (pane = 'list')}
					>← All conversations</button
				>
				<header class="l1-detail-head">
					<div>
						<h2>{c.case.customer_name} <VerifiedChip verified={c.case.identity_level >= 2} /></h2>
						<p class="l1-detail-title">{c.case.title}</p>
					</div>
					<div class="l1-detail-meta">
						<span class="l1-phase {p}">{PHASE[p].label}</span>
						<span class="l1-env {c.case.env}"
							>{#if c.case.env === 'prod'}Production · {c.release.id}{:else}Test env · draft{/if}</span
						>
						<span class="l1-dim">{channelLabel(c)}</span>
						<a href={caseHref(c)} class="l1-mono">{c.case.case_id}</a>
					</div>
				</header>
				{#if c.case.env === 'test'}
					<p class="l1-draftnote">
						This staff test chat runs the job's <b>draft</b>{draft
							? ` (${draft.count} unpublished change${draft.count === 1 ? '' : 's'})`
							: ''}, not the live release. <a href={releaseHref}>Review &amp; publish</a>
					</p>
				{/if}

				<div class="l1-control" class:staff={heldByStaff(c)} data-testid="console-control">
					<span class="l1-owner"
						>Answering <OwnerChip owner={c.owner} meSub={me} />
						{#if heldByStaff(c)}The agent is silent in this conversation since {hhmmss(
								c.owner.since,
							)}.{/if}</span
					>
					<a class="btn sm primary" href={wsHref(c)} data-testid="open-workspace">Open in workspace</a>
				</div>
				{#if attention}
					<div class="l1-attn {attention.severity}" role="alert">
						<b>{attention.title}</b>
						<span>{attention.hint}</span>
					</div>
				{/if}

				<ol class="l1-turns" aria-label="Turns">
					{#each turns as t, i (t.n)}
						<TurnCard turn={t} working={p === 'working' && i === turns.length - 1} />
					{/each}
				</ol>
			{:else}
				<p class="l1-quiet">
					{snap === null ? 'Connecting…' : 'No conversations in this job yet. Start one in the test chat.'}
					<a href="/chat">Open the test chat</a>
				</p>
			{/if}
		</section>
	</div>
</div>
