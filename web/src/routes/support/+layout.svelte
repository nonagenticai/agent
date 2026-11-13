<script lang="ts">
	// `/support` — the staff workspace (SW T10): inbox 320 px | conversation | context 360 px. One tenant stream
	// (`SupportApi.events()`) feeds the inbox, the presence line and the open conversation (routed by job +
	// conversation id); if it drops, a banner shows and the inbox is polled every 10 s until it is back. The URL
	// names the conversation: `/support/{job}/c/{conversationId}` (SW-8). Keyboard: J/K, T, R, N, E, H, `/`.
	// Every member may open it (SW-7); writes are gated by role and stay visible, disabled with the reason.
	import { tick, untrack, type Snippet } from 'svelte';
	import { page } from '$app/state';
	import { goto, pushState } from '$app/navigation';
	import { api } from '$lib/app.svelte';
	import { newerOwner } from '$lib/support/owner';
	import { session } from '$lib/auth/session.svelte';
	import { WORK_HINT, canWorkConversations, canWrite, writeAttrs } from '$lib/auth/roles';
	import type {
		InboxRow,
		OwnerView,
		SupportConversation,
		SupportTranscript,
		Episode,
	} from '$lib/api/types/support';
	import { createInbox, dueOf, isDenied, rowKey, slaOf } from '$lib/support/inbox.svelte';
	import { clearDrafts } from '$lib/support/drafts';
	import { bindKeys } from '$lib/support/keys';
	import { phonePane, watchLayout, type Layout } from '$lib/support/layout';
	import { presenceBoard, presenceText, startViewing, typingNotifier } from '$lib/support/presence';
	import { relative } from '$lib/support/time';
	import Drawer from '$lib/ui/Drawer.svelte';
	import Icon from '$lib/ui/Icon.svelte';
	import Inbox from '$lib/support/ui/Inbox.svelte';
	import ConversationPane from '$lib/support/ui/ConversationPane.svelte';
	import ContextPane, { stateWord } from '$lib/support/ui/ContextPane.svelte';
	import ResolveDialog from '$lib/support/ui/ResolveDialog.svelte';
	import AssignDialog from '$lib/support/ui/AssignDialog.svelte';
	import FeedbackDialog from '$lib/support/ui/FeedbackDialog.svelte';
	import CsatReport from '$lib/support/ui/CsatReport.svelte';
	import StreamBanner from '$lib/support/ui/StreamBanner.svelte';

	type Frame = { event: string; data: unknown };

	let { children }: { children?: Snippet } = $props();

	const job = $derived(page.params.job ?? null);
	const convId = $derived(page.params.id ?? null);
	const selKey = $derived(job && convId ? rowKey(job, convId) : null);
	const meSub = $derived(session.user?.sub ?? null);
	// Kept drafts are the signed-in person's: another person at this tab starts with none.
	let draftsOf: string | null | undefined;
	$effect(() => {
		const sub = meSub;
		if (draftsOf !== undefined && draftsOf !== sub) clearDrafts();
		draftsOf = sub;
	});

	// Tenant-wide: every call names the conversation's job; the active job is never switched from here (R1).

	// ---- the clock (SLA countdowns, presence expiry) ----------------------------------------------------------------
	let now = $state(Date.now());
	$effect(() => {
		const t = setInterval(() => (now = Date.now()), 1000);
		return () => clearInterval(t);
	});

	// ---- inbox + the one tenant stream ------------------------------------------------------------------------------
	const inbox = createInbox({ me: () => session.user?.sub ?? null });
	const board = presenceBoard();
	let presenceSeq = $state(0);
	let streamDown = $state(false);
	/** 401/403 from the stream or the inbox: no support role here. No banner, no polling, no retry. */
	let streamDenied = $state(false);
	const denied = $derived(streamDenied || inbox.denied);
	/** Bumped when the open conversation's handover may have changed: the context pane re-reads. */
	let ctxRev = $state(0);
	let lastSync = $state<string | null>(null);
	/** The open conversation's pane listens here; it never opens a stream of its own. */
	const paneSubs = new Set<(f: Frame) => void>();
	const paneFeed = (cb: (f: Frame) => void) => {
		paneSubs.add(cb);
		return () => paneSubs.delete(cb);
	};

	function forSelection(d: Record<string, unknown>): boolean {
		return !!convId && d.conversation_id === convId && (d.job == null || d.job === job);
	}
	function onFrame(f: Frame) {
		const d = f.data as Record<string, unknown> | null;
		if (!d) return;
		inbox.applyFrame(f);
		// The open conversation is being read: an inbound message on it is not unread.
		if (job && convId && forSelection(d) && f.event !== 'presence') inbox.seen(job, convId);
		if (f.event === 'presence') {
			if (typeof d.sub === 'string' && typeof d.conversation_id === 'string') {
				board.seen(d as never);
				presenceSeq++;
			}
			return;
		}
		if (!forSelection(d)) return;
		if (f.event === 'owner') {
			const { conversation_id: _c, job: _j, ...o } = d;
			owner = newerOwner(owner, o as unknown as OwnerView);
		}
		if (f.event === 'handover') ctxRev++;
		for (const cb of paneSubs) cb(f);
	}
	/** After an outage: the open conversation re-reads its transcript (frames were missed). */
	function resync() {
		if (!job || !convId) return;
		ctxRev++;
		const f: Frame = { event: 'resync', data: { conversation_id: convId, job } };
		for (const cb of paneSubs) cb(f);
	}

	let reconnect: (() => void) | null = null;
	$effect(() =>
		untrack(() => {
			let stopped = false;
			let ctl: AbortController | null = null;
			let retry: ReturnType<typeof setTimeout> | null = null;
			const run = async () => {
				retry = null;
				// Each run owns its controller: a superseded run (Reconnect now) ends without touching the state.
				const myCtl = new AbortController();
				ctl = myCtl;
				try {
					await api().events({
						onFrame,
						signal: myCtl.signal,
						onOpen: () => {
							if (myCtl !== ctl) return;
							// The FIRST open too: a message that landed between the pane's transcript read and the
							// stream opening was never framed, so the open conversation reads once more.
							if (!streamDown) return resync();
							streamDown = false;
							inbox.stopPolling();
							void inbox.load();
							resync();
						},
					});
				} catch (e) {
					if (isDenied(e) && !stopped && myCtl === ctl) {
						streamDenied = true;
						streamDown = false;
						inbox.stopPolling();
						return;
					}
					/* 503 stream_unavailable, network: handled below */
				}
				if (stopped || myCtl !== ctl || inbox.denied) return;
				// The stream ended or never opened: say so, poll (the open conversation resyncs too), retry in 10 s.
				streamDown = true;
				lastSync = new Date().toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
				inbox.startPolling(10_000, resync);
				retry = setTimeout(run, 10_000);
			};
			reconnect = () => {
				if (retry) clearTimeout(retry);
				ctl?.abort();
				void run();
			};
			void inbox.start();
			void run();
			return () => {
				stopped = true;
				ctl?.abort();
				if (retry) clearTimeout(retry);
				inbox.stop();
				reconnect = null;
			};
		}),
	);

	// ---- the open conversation ------------------------------------------------------------------------------------
	let conv = $state<SupportConversation | null>(null);
	let owner = $state<OwnerView | null>(null);
	let episodes = $state<Episode[]>([]);
	let lost = $state<string | null>(null);
	let actionError = $state<string | null>(null);
	let busy = $state(false);

	$effect(() => {
		const j = job;
		const id = convId;
		conv = null;
		owner = null;
		episodes = [];
		lost = null;
		actionError = null;
		handBackOpen = false;
		if (!j || !id) return;
		const stopViewing = untrack(() => {
			inbox.seen(j, id);
			return startViewing(() => api().presence(j, id, 'viewing'));
		});
		return () => stopViewing();
	});
	/** The pane loaded (or resynced) the conversation: its head is the toolbar's — one transcript read per selection. */
	function onLoaded(t: SupportTranscript) {
		if (t.conversation.conversation_id !== convId || t.conversation.job !== job) return;
		conv = t.conversation;
		owner = t.owner;
		episodes = t.episodes;
	}
	const typing = $derived(job && convId ? typingNotifier(() => api().presence(job, convId, 'typing')) : null);

	const row = $derived<InboxRow | undefined>(
		job && convId
			? (inbox.rows.find((r) => r.job === job && r.conversation_id === convId) ?? inbox.row(job, convId))
			: undefined,
	);
	const mine = $derived(!!owner && !!meSub && owner.owner === `user:${meSub}`);
	const otherPerson = $derived(!!owner && owner.owner.startsWith('user:') && !mine);
	const isAgent = $derived(session.superAdmin || canWorkConversations(session));
	const itemId = $derived(owner?.item_id ?? row?.item_id ?? episodes.at(-1)?.item_id ?? null);
	const caseLabel = $derived(
		episodes.find((e) => e.item_id === owner?.item_id)?.case_id ?? conv?.case_id ?? null,
	);
	const due = $derived(row ? dueOf(row) : null);
	const sla = $derived(row ? slaOf(row, now) : null);
	const here = $derived(presenceSeq >= 0 && job && convId ? board.of(job, convId, meSub, now) : []);

	const takeBlock = $derived(
		!isAgent
			? WORK_HINT
			: otherPerson
				? `${owner?.name ?? 'A colleague'} is answering this conversation`
				: !owner
					? 'Loading…'
					: null,
	);
	// Resolve is the owner's, like the server's (`not_owner`)
	const resolveBlock = $derived(
		!owner?.item_id
			? 'No open handover on this conversation — use Hand back'
			: !mine
				? 'Take this conversation first to resolve it'
				: null,
	);
	const handBackBlock = $derived(mine ? null : 'Only the person who has this conversation can hand it back');
	const linkBlock = $derived(
		!canWorkConversations(session) ? WORK_HINT : !conv?.case_id ? 'No case on this conversation yet' : null,
	);
	const assignAttrs = $derived(writeAttrs(session, 'support_manager'));
	/** Feedback on the handover: any working role, owner or not (R4). */
	const mayFeedback = $derived(
		session.superAdmin ||
			(['support_agent', 'support_manager', 'operator', 'job_designer'] as const).some((r) =>
				canWrite(session, r),
			),
	);
	const feedbackBlock = $derived(
		!mayFeedback
			? 'Needs the Support agent, Support manager, Operator or Job designer role'
			: !itemId
				? 'No handover on this conversation'
				: null,
	);

	/** A new owner from a command: the pane, the inbox and the header hear it as an `owner` frame. */
	function applyOwner(o: OwnerView) {
		if (!job || !convId) return;
		onFrame({ event: 'owner', data: { conversation_id: convId, job, ...o } });
	}
	const ownerNow = (e: unknown): OwnerView | null =>
		((e as { details?: { owner?: OwnerView } })?.details?.owner as OwnerView | undefined) ?? null;
	const errText = (e: unknown, fallback: string) => (e instanceof Error && e.message ? e.message : fallback);
	function ago(since: string): string {
		const s = Math.max(0, Math.round((Date.now() - Date.parse(since)) / 1000));
		return s < 60 ? `${s} s ago` : relative(since);
	}
	function lostTo(o: OwnerView) {
		applyOwner(o);
		lost =
			o.owner === 'agent'
				? 'The agent has this conversation again'
				: o.owner === 'queue'
					? 'This conversation went back to the queue'
					: `${o.name ?? 'Someone'} took this ${ago(o.since)}`;
	}

	async function take() {
		// Already mine: T does nothing (a self-take would come back 409 and read as someone else's).
		if (!job || !convId || !owner || busy || mine) return;
		if (takeBlock) {
			actionError = takeBlock;
			return;
		}
		busy = true;
		lost = null;
		actionError = null;
		try {
			applyOwner(await api().take(job, convId, owner.owner));
		} catch (e) {
			const o = ownerNow(e);
			if (o) lostTo(o);
			else actionError = errText(e, 'Could not take this conversation.');
		} finally {
			busy = false;
		}
	}

	let resolveOpen = $state(false);
	let assignOpen = $state(false);
	let feedbackOpen = $state(false);
	let menuOpen = $state(false);
	let handBackOpen = $state(false);
	let handBackNote = $state('');
	let handBackError = $state<string | null>(null);
	let detailsOpen = $state(false);
	let linking = $state(false);
	/** The open conversation's customer is known (signed in or picked): the Link customer menu item goes away. */
	let identified = $state<boolean | null>(null);

	function openResolve() {
		if (!mine) {
			actionError = 'Take this conversation first to resolve it.';
			return;
		}
		if (resolveBlock) {
			actionError = resolveBlock;
			return;
		}
		resolveOpen = true;
	}
	async function resolve(input: Parameters<ReturnType<typeof api>['resolve']>[2]) {
		if (!job || !owner?.item_id) return;
		try {
			applyOwner(await api().resolve(job, owner.item_id, input));
			lost = null;
		} catch (e) {
			const o = ownerNow(e);
			if (o) {
				lostTo(o);
				return;
			}
			throw e;
		}
	}
	async function assign(sub: string) {
		if (!job || !convId || !owner) return;
		try {
			applyOwner(await api().assign(job, convId, sub, owner.owner));
		} catch (e) {
			const o = ownerNow(e);
			if (o) {
				lostTo(o);
				return;
			}
			throw e;
		}
	}
	function openHandBack() {
		menuOpen = false;
		if (handBackBlock) {
			actionError = handBackBlock;
			return;
		}
		handBackOpen = true;
		handBackError = null;
	}
	async function handBack(e: SubmitEvent) {
		e.preventDefault();
		if (!job || !convId) return;
		const note = handBackNote.trim();
		if (!note) {
			handBackError = 'Add a note for the agent';
			return;
		}
		busy = true;
		try {
			applyOwner(await api().handBack(job, convId, note));
			handBackOpen = false;
			handBackNote = '';
		} catch (err) {
			const o = ownerNow(err);
			if (o) lostTo(o);
			else handBackError = errText(err, 'Could not hand it back.');
		} finally {
			busy = false;
		}
	}

	// ---- layout (SW-6): wide >= 1200 px three panes; tablet 768-1199 px the customer is a right drawer; phone < 768 px
	// one pane at a time, driven by the URL (inbox -> conversation -> customer sheet) ---------------------------------
	let layout = $state<Layout>('wide');
	$effect(() => watchLayout((l) => (layout = l)));
	const phone = $derived(layout === 'phone');
	/** The customer is a dialog (drawer / sheet) everywhere except the wide layout, where it is the third pane. */
	const ctxInline = $derived(layout === 'wide');
	let customerOpen = $state(false);
	let customerBtn = $state<HTMLButtonElement | null>(null);
	/** A phone's sheet is a history entry: the browser's back closes it, like it leaves a conversation. */
	const sheetEntry = () => !!(page.state as { customer?: boolean } | undefined)?.customer;
	function showContext() {
		if (ctxInline || customerOpen) return;
		// Safari does not focus a button on click: name the opener so closing returns focus to it.
		customerBtn?.focus();
		if (phone) pushState('', { customer: true });
		customerOpen = true;
	}
	function toggleContext() {
		if (customerOpen) customerOpen = false;
		else showContext();
	}
	$effect(() => {
		// The inline pane replaces the dialog; another conversation starts closed.
		void convId;
		if (ctxInline) customerOpen = false;
	});
	$effect(() => {
		const open = customerOpen;
		const entry = sheetEntry();
		const onPhone = phone;
		untrack(() => {
			if (onPhone) {
				// Back pressed: the entry is gone, the sheet goes. Closed by its own button: drop the entry too.
				if (open && !entry) customerOpen = false;
				else if (!open && entry) history.back();
			} else if (entry) {
				// Grown past a phone with the sheet open: it closes, and so does its history entry.
				customerOpen = false;
				history.back();
			}
		});
	});
	// What the phone shows now. The list is the root; the inbox keeps its selection (the conversation just left).
	const pane = $derived(phonePane({ selected: !!selKey, customerOpen }));
	let lastKey = $state<string | null>(null);
	$effect(() => {
		if (selKey) lastKey = selKey;
	});
	const inboxSelected = $derived(selKey ?? (phone ? lastKey : null));
	let inboxCol = $state<HTMLElement | null>(null);
	// Back on the list: the row just left takes focus again, so the keyboard and screen reader land where they were.
	$effect(() => {
		if (!phone || pane !== 'list' || !lastKey) return;
		const key = untrack(() => lastKey);
		tick().then(() => {
			const a = [...(inboxCol?.querySelectorAll<HTMLAnchorElement>('a[data-conversation]') ?? [])].find(
				(x) => rowKey(x.dataset.job ?? '', x.dataset.conversation ?? '') === key,
			);
			if (a && !inboxCol?.contains(document.activeElement)) a.focus({ preventScroll: true });
		});
	});
	/** "Back to inbox" on a phone: step back in history when the list was the previous page, else go to the list. */
	let fromList = false;
	function backToInbox() {
		if (fromList) {
			fromList = false;
			history.back();
		} else void goto('/support');
	}

	// ---- navigation + keyboard ------------------------------------------------------------------------------------
	const urlOf = (j: string, id: string) => `/support/${encodeURIComponent(j)}/c/${encodeURIComponent(id)}`;
	function openRow(r: InboxRow) {
		fromList = !selKey;
		inbox.seen(r.job, r.conversation_id);
		void goto(urlOf(r.job, r.conversation_id));
	}
	function move(d: 1 | -1) {
		const rows = inbox.rows;
		if (!rows.length) return;
		const i = selKey ? rows.findIndex((r) => rowKey(r.job, r.conversation_id) === selKey) : -1;
		const next = i < 0 ? 0 : Math.min(rows.length - 1, Math.max(0, i + d));
		if (next === i) return;
		openRow(rows[next]!);
	}

	let convCol = $state<HTMLElement | null>(null);
	function composer(mode: 'reply' | 'note', slash = false) {
		if (!convCol) return;
		const [reply, note] = convCol.querySelectorAll<HTMLButtonElement>('[aria-label="Message type"] button');
		(mode === 'note' ? note : reply)?.click();
		queueMicrotask(() => {
			const ta = convCol?.querySelector<HTMLTextAreaElement>('textarea');
			if (!ta || ta.disabled) return;
			if (slash && !ta.value) {
				ta.value = '/';
				ta.dispatchEvent(new Event('input', { bubbles: true }));
			}
			ta.focus();
		});
	}
	const modal = $derived(resolveOpen || assignOpen || feedbackOpen || (!ctxInline && customerOpen));
	$effect(() =>
		bindKeys(window, {
			j: () => !modal && move(1),
			k: () => !modal && move(-1),
			t: () => !modal && void take(),
			r: () => !modal && composer('reply'),
			n: () => !modal && composer('note'),
			e: () => !modal && openResolve(),
			h: () => !modal && openHandBack(),
			'/': () => !modal && composer('reply', true),
		}),
	);
</script>

<h1 class="sr-only" data-testid="support-h1">Support</h1>

{#if denied}
	<div class="denied" role="alert" aria-label="Support workspace">
		<strong>Support workspace isn't available for your role</strong>
		<span class="muted"
			>It needs the Support agent, Support manager, Operator or Job designer role in this tenant. Ask a tenant
			admin.</span
		>
	</div>
{:else}
	<div class="ws {layout}" class:sel={!!selKey} data-layout={layout} data-pane={phone ? pane : undefined}>
		{#if streamDown}
			<div class="banner"><StreamBanner {lastSync} onRetry={() => reconnect?.()} /></div>
		{/if}

		<div class="col inbox-col" bind:this={inboxCol} inert={phone && !!selKey}>
			<Inbox store={inbox} selected={inboxSelected} {now} onOpen={openRow} />
		</div>

		<div
			class="col conv-col"
			inert={phone && !selKey}
			bind:this={convCol}
			oninput={(e) => {
				// Only the composer's typing is shown to colleagues (not the hand-back note).
				if ((e.target as HTMLElement | null)?.closest?.('.pane')) typing?.input();
			}}
		>
			{#if job && convId}
				<div class="bar" role="toolbar" aria-label="Conversation actions">
					{#if phone}
						<button
							class="btn back"
							type="button"
							aria-label="Back to inbox"
							data-testid="back-to-inbox"
							onclick={backToInbox}
							><span class="flip" aria-hidden="true"><Icon name="arrow" size={18} /></span> Inbox</button
						>
					{/if}
					<span class="meta">
						{#if caseLabel}<span class="case">{caseLabel}</span>{/if}
						{#if row?.state}<span class="chip muted">{stateWord(row.state)}</span>{/if}
						{#if due}
							<span class="due" class:breached={sla === 'breached'} class:risk={sla === 'at_risk'}>
								{row?.first_response_due ? 'First response' : 'Resolve'}
								{sla === 'breached' ? 'overdue' : 'due'}
								{relative(due, now)}
							</span>
						{/if}
						{#if presenceText(here)}<span class="pres" aria-live="polite">{presenceText(here)}</span>{/if}
					</span>
					<span class="sp"></span>
					{#if mine}
						<button
							class="btn sm primary"
							type="button"
							disabled={!!resolveBlock}
							title={resolveBlock ?? 'Resolve (E)'}
							onclick={openResolve}>Resolve</button
						>
					{:else}
						<button
							class="btn sm primary"
							type="button"
							disabled={!!takeBlock || busy}
							title={takeBlock ?? 'Take (T)'}
							onclick={take}>Take</button
						>
					{/if}
					<div class="more">
						<button
							class="btn sm"
							type="button"
							aria-label="More actions"
							aria-haspopup="menu"
							aria-expanded={menuOpen}
							onclick={() => (menuOpen = !menuOpen)}>⋯</button
						>
						{#if menuOpen}
							<div class="menu" role="menu" aria-label="More actions">
								<button
									type="button"
									role="menuitem"
									disabled={assignAttrs.disabled}
									title={assignAttrs.title}
									onclick={() => {
										menuOpen = false;
										assignOpen = true;
									}}>Assign…</button
								>
								<button
									type="button"
									role="menuitem"
									disabled={!!handBackBlock}
									title={handBackBlock ?? 'Hand back (H)'}
									onclick={openHandBack}>Hand back</button
								>
								{#if !identified}<button
										type="button"
										role="menuitem"
										disabled={!!linkBlock}
										title={linkBlock ?? undefined}
										onclick={() => {
											menuOpen = false;
											linking = true;
											showContext();
										}}>Link customer</button
									>{/if}
								<button
									type="button"
									role="menuitem"
									onclick={() => {
										menuOpen = false;
										detailsOpen = true;
										showContext();
									}}>Details</button
								>
								<button
									type="button"
									role="menuitem"
									disabled={!!feedbackBlock}
									title={feedbackBlock ?? undefined}
									onclick={() => {
										menuOpen = false;
										feedbackOpen = true;
									}}>Give feedback</button
								>
							</div>
						{/if}
					</div>
					{#if !ctxInline}
						<button
							class="btn sm"
							type="button"
							bind:this={customerBtn}
							aria-haspopup="dialog"
							aria-expanded={customerOpen}
							onclick={toggleContext}>Customer</button
						>
					{/if}
				</div>
				{#if lost}<p class="lost" role="alert">{lost}</p>{/if}
				{#if actionError}<p class="lost" role="alert">{actionError}</p>{/if}
				{#if handBackOpen}
					<form class="hb" aria-label="Hand back to the agent" onsubmit={handBack}>
						<label for="hb-note">Note for the agent — what should it do next?</label>
						<textarea
							id="hb-note"
							rows="2"
							bind:value={handBackNote}
							oninput={() => (handBackError = null)}
							aria-invalid={handBackError ? 'true' : undefined}
							aria-describedby={handBackError ? 'hb-err' : undefined}
						></textarea>
						{#if handBackError}<p class="err" id="hb-err">{handBackError}</p>{/if}
						<div class="hb-actions">
							<button class="btn sm" type="button" onclick={() => (handBackOpen = false)}>Cancel</button>
							<button class="btn sm primary" type="submit" disabled={busy}>Hand back</button>
						</div>
					</form>
				{/if}
				<div class="pane">
					<ConversationPane
						{job}
						conversationId={convId}
						frames={paneFeed}
						{onLoaded}
						onOwnerChange={(o) => {
							owner = o;
							inbox.applyFrame({ event: 'owner', data: { conversation_id: convId, job, ...o } });
						}}
					/>
				</div>
			{:else}
				<div class="none">
					<p><strong>Pick a conversation</strong></p>
					<p class="muted">
						J / K move through the inbox · T take · R reply · N note · E resolve · H hand back
					</p>
					<CsatReport />
				</div>
			{/if}
		</div>

		{#if job && convId && ctxInline}
			<div class="col ctx-col">
				<ContextPane
					{job}
					conversationId={convId}
					{itemId}
					caseId={conv?.case_id ?? null}
					{now}
					rev={ctxRev}
					bind:detailsOpen
					bind:linking
					bind:identified
				/>
			</div>
		{/if}
	</div>
{/if}

{#if job && convId && !ctxInline}
	<Drawer
		bind:open={customerOpen}
		title="Customer"
		back={phone}
		fullscreen={phone}
		side={!phone}
		closeLabel={phone ? 'Back to conversation' : 'Close customer panel'}
	>
		<ContextPane
			{job}
			conversationId={convId}
			{itemId}
			caseId={conv?.case_id ?? null}
			{now}
			rev={ctxRev}
			bind:detailsOpen
			bind:linking
			bind:identified
		/>
	</Drawer>
{/if}

{#if job && convId}
	<ResolveDialog
		bind:open={resolveOpen}
		channel={conv?.channel ?? row?.channel ?? ''}
		customer={conv?.display_name ?? row?.display_name ?? 'The customer'}
		onSubmit={resolve}
	/>
	<AssignDialog
		bind:open={assignOpen}
		{job}
		current={owner?.owner.startsWith('user:') ? owner.owner.slice(5) : null}
		onSubmit={assign}
	/>
	{#if itemId}
		<FeedbackDialog bind:open={feedbackOpen} {job} {itemId} onDone={() => ctxRev++} />
	{/if}
{/if}

{@render children?.()}

<style>
	.sr-only {
		position: absolute;
		width: 1px;
		height: 1px;
		margin: -1px;
		padding: 0;
		overflow: hidden;
		clip-path: inset(50%);
		white-space: nowrap;
		border: 0;
	}
	.ws {
		display: grid;
		grid-template-columns: 300px minmax(0, 1fr);
		grid-template-rows: auto minmax(0, 1fr);
		grid-template-areas:
			'banner banner'
			'inbox conv';
		gap: 10px;
		height: calc(100dvh - 130px);
		min-height: 460px;
	}
	.ws.wide {
		grid-template-columns: 320px minmax(0, 1fr) 360px;
		grid-template-areas:
			'banner banner banner'
			'inbox conv ctx';
	}
	/* Phone: inbox and conversation share one cell and only the current one is shown. Hidden with `visibility`, not
	   `display`, so the inbox keeps its scroll position and the transcript its place while the other pane is up. */
	.ws.phone {
		grid-template-columns: minmax(0, 1fr);
		grid-template-areas:
			'banner'
			'pane';
		gap: 8px;
		min-height: 0;
		/* the shell's top bar and padding, and the home-indicator inset */
		height: calc(100dvh - var(--ws-above, 120px) - env(safe-area-inset-bottom, 0px));
	}
	.ws.phone .inbox-col,
	.ws.phone .conv-col {
		grid-area: pane;
	}
	.ws.phone.sel .inbox-col,
	.ws.phone:not(.sel) .conv-col {
		visibility: hidden;
		pointer-events: none;
	}
	.banner {
		grid-area: banner;
	}
	.col {
		min-height: 0;
		min-width: 0;
	}
	.inbox-col {
		grid-area: inbox;
	}
	.conv-col {
		grid-area: conv;
		display: flex;
		flex-direction: column;
		gap: 6px;
	}
	.ctx-col {
		grid-area: ctx;
	}
	.pane {
		flex: 1;
		min-height: 0;
	}
	.bar {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 8px;
		padding: 6px 10px;
		border: 1px solid var(--line);
		border-radius: 8px;
		background: var(--panel);
	}
	.case {
		font: 500 12.5px/1 var(--mono);
	}
	.due {
		font-size: 12.5px;
		color: var(--muted);
	}
	.due.risk {
		color: var(--warn);
	}
	.due.breached {
		color: var(--err);
		font-weight: 600;
	}
	.pres {
		font-size: 12.5px;
		color: var(--accent);
	}
	.sp {
		flex: 1;
	}
	.meta {
		display: contents;
	}
	.flip {
		display: inline-grid;
		transform: scaleX(-1);
	}
	.back {
		min-height: 44px;
		padding: 0 14px 0 8px;
		gap: 2px;
	}
	/* Touch: every control of the conversation header is a 44 px target below the wide layout. */
	@media (max-width: 1199.98px) {
		.bar .btn,
		.menu button {
			min-height: 44px;
			min-width: 44px;
		}
		.hb textarea {
			font-size: 16px;
		}
	}
	@media (max-width: 767.98px) {
		.meta {
			display: flex;
			flex-wrap: wrap;
			align-items: center;
			gap: 4px 8px;
			order: 5;
			flex: 1 0 100%;
		}
		.menu {
			min-width: min(240px, calc(100vw - 32px));
		}
	}
	.more {
		position: relative;
	}
	.menu {
		position: absolute;
		right: 0;
		top: calc(100% + 4px);
		z-index: 10;
		display: grid;
		min-width: 170px;
		padding: 4px;
		border: 1px solid var(--line);
		border-radius: 8px;
		background: var(--panel);
		box-shadow: 0 6px 24px rgb(0 0 0 / 0.14);
	}
	.menu button {
		text-align: left;
		border: 0;
		background: none;
		font: inherit;
		padding: 7px 10px;
		border-radius: 5px;
		cursor: pointer;
		color: var(--fg);
	}
	.menu button:hover:not(:disabled) {
		background: var(--sunk);
	}
	.menu button:disabled {
		color: var(--muted);
		cursor: not-allowed;
	}
	.lost {
		margin: 0;
		padding: 6px 10px;
		border-radius: 6px;
		background: color-mix(in srgb, var(--warn) 12%, var(--panel));
		font-size: 13px;
	}
	.hb {
		display: grid;
		gap: 6px;
		padding: 8px 10px;
		border: 1px solid var(--line);
		border-radius: 8px;
		background: var(--panel);
	}
	.hb label {
		font-size: 12.5px;
		color: var(--muted);
	}
	.hb-actions {
		display: flex;
		justify-content: flex-end;
		gap: 6px;
	}
	.err {
		color: var(--err);
		margin: 0;
		font-size: 12.5px;
	}
	.none {
		display: grid;
		place-content: safe center;
		overflow: auto;
		padding: 12px;
		text-align: center;
		height: 100%;
		border: 1px dashed var(--line);
		border-radius: 8px;
	}
	.muted {
		color: var(--muted);
	}
	.denied {
		display: grid;
		gap: 4px;
		max-width: 560px;
		padding: 14px 16px;
		border: 1px solid var(--line);
		border-radius: 8px;
		background: var(--panel);
	}
</style>
