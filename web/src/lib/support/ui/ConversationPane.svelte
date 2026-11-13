<script lang="ts">
	// The one conversation view of the staff workspace: header (who, channel, owner), the anchored transcript and the
	// composer. `readonly` drops the composer (Conversations, case detail). Live updates come from `frames`: the
	// caller feeds the pane from the single tenant stream (a subscribe function, or an async iterable) — the pane
	// never opens a stream of its own.
	import { api } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { WORK_HINT, canWorkConversations } from '$lib/auth/roles';
	import { CHANNEL_LABEL } from '$lib/conversations/labels';
	import type {
		AttachmentRef,
		Episode,
		OwnerView,
		SendKind,
		SupportConversation,
		SupportTranscript,
		MessageView,
	} from '$lib/api/types/support';
	import { refusalText } from '$lib/support/attachments';
	import type { Row } from '$lib/support/rows';
	import { newerOwner } from '$lib/support/owner';
	import Transcript from './Transcript.svelte';
	import Composer from './Composer.svelte';
	import OwnerChip from './OwnerChip.svelte';
	import AutoChip from './AutoChip.svelte';
	import VerifiedChip from './VerifiedChip.svelte';

	type Frame = { event: string; data: unknown };
	type Feed = ((onFrame: (f: Frame) => void) => () => void) | AsyncIterable<Frame>;

	let {
		job,
		conversationId,
		readonly = false,
		onOwnerChange,
		onLoaded,
		frames,
	}: {
		job: string;
		conversationId: string;
		readonly?: boolean;
		onOwnerChange?: (o: OwnerView) => void;
		/** What the pane loaded (head, owner, episodes) — after the first load and after every resync. */
		onLoaded?: (t: SupportTranscript) => void;
		/** Frames of the caller's one stream. `resync` (after a stream outage) re-reads the transcript. */
		frames?: Feed;
	} = $props();

	let conv = $state<SupportConversation | null>(null);
	let owner = $state<OwnerView | null>(null);
	let rows = $state<Row[]>([]);
	let episodes = $state<Episode[]>([]);
	let error = $state<string | null>(null);

	const meSub = $derived(session.user?.sub ?? null);
	const canReply = $derived(owner != null && meSub != null && owner.owner === `user:${meSub}`);
	/** A colleague has it: the composer says who, and offers no Take (only Assign… moves it). */
	const answering = $derived(
		owner && owner.owner.startsWith('user:') && !canReply ? (owner.name ?? 'A colleague') : null,
	);
	// A super_admin may always take (the server's `self_assignable`); everyone else needs Support agent or Support manager.
	const mayTake = $derived(session.superAdmin || canWorkConversations(session));

	const sameMsg = (a: Row, b: MessageView) =>
		a.message_id === b.message_id || (!!b.client_msg_id && a.client_msg_id === b.client_msg_id);
	function upsert(v: MessageView, clientId?: string) {
		const i = rows.findIndex((r) => sameMsg(r, v));
		if (i >= 0) rows[i] = v;
		else rows.push(v);
		// Whatever order the frame and the POST answer in, one row per message: drop every other row that is this
		// message (same id) or the optimistic row this send made.
		const keep = rows.findIndex((r) => r.message_id === v.message_id);
		rows = rows.filter(
			(r, j) =>
				j === keep ||
				!(
					r.message_id === v.message_id ||
					(v.client_msg_id && r.client_msg_id === v.client_msg_id) ||
					(clientId && r.client_msg_id === clientId && r.local)
				),
		);
	}
	function setOwner(o: OwnerView) {
		if (newerOwner(owner, o) !== o) return;
		owner = o;
		onOwnerChange?.(o);
	}

	async function load(resync = false) {
		error = null;
		try {
			const forJob = job;
			const forConv = conversationId;
			const t = await api().transcript(forJob, forConv);
			// A read for a conversation that is no longer open (the reader moved on) must not overwrite this one.
			if (forJob !== job || forConv !== conversationId) return;
			conv = t.conversation;
			owner = t.owner;
			// A resync keeps what this pane is still sending (or failed to send); the server has the rest.
			const pending = resync ? rows.filter((r) => r.local && !t.messages.some((m) => sameMsg(r, m))) : [];
			rows = [...t.messages, ...pending];
			episodes = t.episodes;
			if (resync) onOwnerChange?.(t.owner);
			onLoaded?.(t);
		} catch (e) {
			error = e instanceof Error ? e.message : 'Could not load the conversation.';
		}
	}
	$effect(() => {
		job;
		conversationId;
		conv = null;
		owner = null;
		rows = [];
		episodes = [];
		void load();
	});

	function onFrame(f: Frame) {
		const d = f.data as Record<string, unknown> | null;
		if (!d || d.conversation_id !== conversationId || (d.job != null && d.job !== job)) return;
		if (f.event === 'resync') {
			void load(true);
		} else if (f.event === 'message') {
			const { conversation_id: _c, job: _j, ...view } = d;
			upsert(view as unknown as MessageView);
		} else if (f.event === 'owner') {
			const { conversation_id: _c, job: _j, ...view } = d;
			setOwner(view as unknown as OwnerView);
		} else if (f.event === 'delivery_failed') {
			const r = rows.find((x) => x.message_id === d.message_id);
			if (r) r.delivery = 'failed'; // `code` (why) is accepted and not shown
		}
	}
	$effect(() => {
		const feed = frames;
		if (!feed) return;
		if (typeof feed === 'function') return feed(onFrame);
		let stop = false;
		const it = feed[Symbol.asyncIterator]();
		(async () => {
			while (!stop) {
				const n = await it.next();
				if (n.done || stop) break;
				onFrame(n.value);
			}
		})().catch(() => {});
		return () => {
			stop = true;
			void it.return?.();
		};
	});

	const ownerNow = (e: unknown): OwnerView | null =>
		((e as { details?: { owner?: OwnerView } })?.details?.owner as OwnerView | undefined) ?? null;

	async function take() {
		if (!owner || answering) return;
		try {
			setOwner(await api().take(job, conversationId, owner.owner));
		} catch (e) {
			const now = ownerNow(e);
			if (now) setOwner(now);
			else error = e instanceof Error ? e.message : 'Could not take this conversation.';
		}
	}

	async function deliver(kind: SendKind, text: string, clientId: string, refs: AttachmentRef[] = []) {
		const mine = rows.find((r) => r.client_msg_id === clientId);
		if (mine) mine.local = 'sending';
		else
			rows.push({
				message_id: `local-${clientId}`,
				kind: kind === 'note' ? 'note' : 'staff',
				event: null,
				text,
				author_name: session.user?.name ?? null,
				at: new Date().toISOString(),
				case_id: null,
				client_msg_id: clientId,
				delivery: null,
				mirror: null,
				local: 'sending',
				...(refs.length ? { attachments: refs } : {}),
			});
		try {
			upsert(
				await api().send(job, conversationId, {
					kind,
					text,
					client_msg_id: clientId,
					...(refs.length ? { attachments: refs.map((a) => a.id) } : {}),
				}),
				clientId,
			);
		} catch (e) {
			const now = ownerNow(e);
			if (now) setOwner(now);
			const r = rows.find((x) => x.client_msg_id === clientId);
			if (r && r.local) {
				r.local = 'failed';
				const code = (e as { code?: string })?.code;
				if (
					code === 'attachments_on_note' ||
					code === 'too_many_attachments' ||
					code === 'attachments_not_supported' ||
					code === 'attachments_off'
				) {
					r.reason = refusalText(e);
					r.noRetry = true;
				}
				if (code === 'not_owner' || code === 'owner_changed') {
					r.reason = 'You no longer own this conversation';
					r.noRetry = true;
				}
			}
		}
	}
	const send = (kind: SendKind, text: string, refs: AttachmentRef[] = []) =>
		deliver(kind, text, crypto.randomUUID(), refs);
	const retry = (m: Row) =>
		m.client_msg_id &&
		deliver(m.kind === 'note' ? 'note' : 'reply', m.text, m.client_msg_id, m.attachments ?? []);
</script>

<section class="pane" aria-label="Conversation">
	<header class="hd">
		{#if conv && owner}
			<strong class="who">{conv.display_name}</strong>
			<VerifiedChip verified={conv.verified} />
			<span class="ch">{(CHANNEL_LABEL as Record<string, string>)[conv.channel] ?? conv.channel}</span>
			<span class="sp"></span>
			<OwnerChip {owner} {meSub} />
			<AutoChip {owner} />
		{:else if error}
			<span class="err" role="alert">{error}</span>
			<button class="btn sm" type="button" onclick={() => load()}>Retry</button>
		{:else}
			<span class="ch">Loading…</span>
		{/if}
	</header>
	<Transcript
		messages={rows}
		{episodes}
		onRetry={retry}
		{job}
		{conversationId}
		customerName={conv?.display_name ?? ''}
	/>
	{#if !readonly && conv && owner}
		<!-- keyed: each conversation mounts its own composer, which starts from that conversation's kept draft -->
		{#key `${job}/${conversationId}`}
			<Composer
				{job}
				{conversationId}
				channel={conv.channel}
				staffFiles={conv.attachments?.staff}
				{canReply}
				canTake={mayTake}
				takeTitle={mayTake ? undefined : WORK_HINT}
				{answering}
				onTake={take}
				onSend={send}
			/>
		{/key}
	{/if}
</section>

<style>
	.pane {
		display: grid;
		grid-template-rows: auto 1fr auto;
		min-height: 0;
		height: 100%;
		border: 1px solid var(--line);
		border-radius: 8px;
		background: var(--panel);
		overflow: hidden;
	}
	.hd {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 6px 10px;
		min-width: 0;
		padding: 8px 12px;
		border-bottom: 1px solid var(--line);
	}
	.who {
		min-width: 0;
		max-width: 100%;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.ch {
		color: var(--muted);
		font-size: 12px;
	}
	.sp {
		flex: 1;
	}
	.err {
		color: var(--err);
	}
</style>
