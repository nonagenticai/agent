<script lang="ts">
	// L2 Test chat — the D54 TEST step. Nothing a tenant changes is live on save: it lands in the job's draft. Here a
	// staff member talks to the agent as a test customer (the email they type), either to the DRAFT (unpublished changes, Test env and its
	// test systems) or to LIVE (the published release, Production), reads how each reply was reached and where the
	// other design would answer differently, and saves a good run as a test case that Review & publish (J14) shows
	// as tested. Sends answer 202; the reply arrives on the live feed (K39), replayed by a fake timer.
	import { onDestroy, onMount, untrack } from 'svelte';
	import { replaceState } from '$app/navigation';
	import { api, app, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { writeAttrs } from '$lib/auth/roles';
	import { Resource } from '$lib/load.svelte';
	import type { ChatEnv, ChatProfile, TestCase } from '$lib/api/types/live';
	import type { JobSummary, Paged, Tenant } from '$lib/api/types/core';
	import type { DraftSummary, JobLanguagesView } from '$lib/api/types/release';
	import { LANGUAGE_LABEL } from '$lib/api/types/core';
	// The polite unavailable reply (gate 2, K35) comes from the API, never from the fixtures.
	let REFUSAL_TEXT = $state('');
	void api()
		.chatProfile()
		.then((p) => (REFUSAL_TEXT = p.refusal_text))
		.catch(() => {});
	import { FEED_LABEL } from '$lib/live/feed';
	import { LiveChat } from '$lib/live/chat/session.svelte';
	import { chatLines } from '$lib/live/chat/looks';
	import { promptFor } from '$lib/live/chat/L2-prompts';
	import L2Trail from '$lib/live/chat/L2Trail.svelte';
	import TelegramShell from '$lib/live/chat/shells/TelegramShell.svelte';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LookupNote from '$lib/support/ui/LookupNote.svelte';
	import Card from '$lib/ui/Card.svelte';
	import AnswerSources from '$lib/grounding/AnswerSources.svelte';
	import AnswerFeedback from '$lib/grounding/AnswerFeedback.svelte';
	import { groundingOf } from '$lib/grounding/grounding';
	import type { ChatEvent } from '$lib/api/types/live';

	/** Generic on purpose: the runtime picks the customer's order that fits the request (no order number needed).
	 *  Used until the job's chat profile (its own prompts) has loaded. */
	const QUICK = [
		'Where is my order?',
		'I want to return my order',
		'Please cancel my order',
		'I think I was charged twice',
		'My lamp stopped working',
		'How long does a refund take?',
		'Can I talk to a person?',
	];

	/** Talk to: Draft = Test env (reads the draft) · Live = Production (reads the published release). */
	let env = $state<ChatEnv>('test');
	/** Who the staff member tests as: a customer's email (the agent never asks the customer for it). */
	let customerEmail = $state('');
	let text = $state('');
	let title = $state('');
	let saving = $state(false);

	const tenant = new Resource<Tenant>(
		() => api().tenant(session.me?.tenant?.id ?? ''),
		() => false,
	);
	const job = new Resource<JobSummary>(
		() => api().job(app.job),
		() => false,
	);
	const draft = new Resource<DraftSummary>(
		() => api().draftSummary(),
		() => false,
	);
	const tests = new Resource<Paged<TestCase>>(
		() => api().testCases(),
		() => false,
	);
	const profile = new Resource<ChatProfile>(
		() => api().chatProfile(),
		() => false,
	);
	/** D59: the languages the job serves — Draft (test env) reads the draft list, Live the published one. */
	const languages = new Resource<JobLanguagesView>(
		() => api().jobLanguages(),
		() => false,
	);
	const langList = $derived(
		languages.value.state === 'ok'
			? env === 'test'
				? languages.value.data.draft
				: languages.value.data.live
			: [],
	);
	/** `?conversation=` keeps the open conversation in the URL, so a reload resumes it (and its unsent messages). */
	const chat = new LiveChat({
		api,
		// the chat follows the conversation's SSE stream
		sse: () => true,
		readUrl: () => {
			try {
				return new URLSearchParams(location.search).get('conversation');
			} catch {
				return null;
			}
		},
		writeUrl: (id) => {
			try {
				const u = new URL(location.href);
				if (id) u.searchParams.set('conversation', id);
				else u.searchParams.delete('conversation');
				if (u.href !== location.href) replaceState(u, {});
			} catch {
				/* before the router is ready (or outside a browser): the stored id still resumes */
			}
		},
	});
	/** Multi-job: the job's own prompts, audience and systems (the IT service desk talks to staff, has no Live). */
	const prof = $derived(profile.value.state === 'ok' ? profile.value.data : null);
	const quick = $derived(prof?.prompts ?? QUICK);
	const noLive = $derived(prof ? !prof.has_release : false);
	/** "Before you test": what is missing in the draft (Test env only; an older backend sends nothing). */
	const readiness = $derived(env === 'test' ? (prof?.test_readiness?.items ?? []) : []);
	const who = $derived(prof?.audience === 'staff' ? 'staff member' : 'customer');

	const lines = $derived(chatLines(chat.messages, chat.outbox));
	/** The typed test customer's lookup result: the case's context resolves after the first message, so re-read as the
	 *  conversation's cases and messages arrive. Only a miss is kept (the route sends `degraded` only then). */
	let lookupMiss = $state<string[] | undefined>(undefined);
	$effect(() => {
		const id = chat.conversation?.conversation_id;
		chat.caseIds.length;
		chat.messages.length;
		const j = app.job;
		if (!id || !customerEmail.trim()) {
			lookupMiss = undefined;
			return;
		}
		void api()
			.customerContext(j, id)
			.then((c) => (lookupMiss = c.degraded))
			.catch(() => (lookupMiss = undefined));
	});
	/** Agent replies by their shell line key (`conversation|message|role`), for the sources + feedback under each. */
	const replyByKey = $derived(
		new Map<string, { m: ChatEvent; question: string }>(
			chat.messages.flatMap((m, i) =>
				m.role === 'agent'
					? [
							[
								`${m.conversation_id}|${m.message_id}|${m.role}`,
								{ m, question: chat.messages.slice(0, i).findLast((x) => x.role === 'customer')?.text ?? '' },
							],
						]
					: [],
			),
		),
	);
	const rated = (m: ChatEvent, question: string) => ({
		surface: 'test_chat' as const,
		conversation_id: m.conversation_id,
		answer_id: m.message_id,
		case_id: m.case_id ?? null,
		question,
		answer: m.text,
		grounding: groundingOf(m),
	});
	const brand = $derived(
		tenant.value.state === 'ok' ? (tenant.value.data.brand ?? tenant.value.data.name) : 'Care',
	);
	const agentName = $derived(prof?.agent_name ?? `${brand} Care`);
	const release = $derived(job.value.state === 'ok' ? (job.value.data.current_release?.id ?? null) : null);
	const rev = $derived(job.value.state === 'ok' ? `r${job.value.data.draft_rev}` : '');
	const changes = $derived(draft.value.state === 'ok' ? draft.value.data.changes : []);
	const agentReplies = $derived(chat.messages.filter((m) => m.role === 'agent'));
	const exercised = $derived(new Set(agentReplies.flatMap((m) => m.trace?.exercised ?? [])));
	const differing = $derived(agentReplies.filter((m) => m.trace?.differs.length).length);
	const savedHere = $derived(
		tests.value.state === 'ok'
			? tests.value.data.items.filter((t) => t.conversation_id === chat.conversation?.conversation_id)
			: [],
	);
	const caseHref = (id: string) =>
		`/admin/jobs/${encodeURIComponent(app.job)}/cases/${encodeURIComponent(id)}`;
	const releaseHref = $derived(`/admin/jobs/${encodeURIComponent(app.job)}/release`);

	onMount(async () => {
		void tenant.refresh();
		void job.refresh();
		void draft.refresh();
		void tests.refresh();
		void profile.refresh();
		void languages.refresh();
		// Deep links (L3 "Chat as X", design pages): ?customer=<email> sets who to test as, ?say=<text> prefills the
		// first message. The target stays Draft.
		const q = new URLSearchParams(typeof location === 'undefined' ? '' : location.search);
		const wanted = q.get('customer')?.trim() ?? '';
		customerEmail = wanted;
		const say = q.get('say')?.trim();
		if (say) text = say.slice(0, 2000);
		// Resume `?conversation=` or this tab's last one; a "Chat as X" deep link (?customer=) starts fresh.
		// Nothing is started on load: the first send creates the conversation (lazy start, no empty ones).
		const resumed = await chat.resume(!wanted);
		if (resumed) {
			startedEnv = resumed.env;
			env = resumed.env;
			customerEmail = resumed.customer_id;
		}
	});
	onDestroy(() => chat.destroy());

	// The env (and so the design) is stamped at intake: switching it drops the conversation; the next send starts one.
	let startedEnv: ChatEnv = 'test';
	$effect(() => {
		const e = env;
		if (e === startedEnv) return;
		startedEnv = e;
		untrack(restart);
	});

	/** Drops the open conversation locally (no request); the next send starts a fresh one with the current env and
	 *  customer. Env switch, customer switch, "New conversation" and "Start again" all land here. */
	function restart() {
		title = '';
		chat.forget();
	}
	/** Composer and quick prompts work before any conversation exists: the first send starts it, then posts. */
	const blocked = $derived(chat.starting || !!chat.unreachable || chat.ended);
	async function send(msg = text) {
		if (!msg.trim() || chat.starting || chat.unreachable) return;
		if (!title) title = msg.trim().slice(0, 80);
		if (!chat.conversation) {
			if (!customerEmail.trim()) return;
			// A failed start keeps the text in the composer and shows "Conversation unavailable … Start again".
			if (!(await chat.start(env, customerEmail.trim()))) return;
		}
		if (chat.send(msg) && msg === text) text = '';
	}
	async function saveTest() {
		const conv = chat.conversation;
		if (!conv) return;
		saving = true;
		const name = title.trim() || `Test of ${conv.conversation_id}`;
		let id = '';
		const ok = await guard(
			async () =>
				(id = (await api().saveTestCase({ conversation_id: conv.conversation_id, title: name })).id),
			() => `Saved as test case ${id} on the draft — Review & publish shows it as tested.`,
		);
		saving = false;
		if (ok) void tests.refresh();
	}
	/** F-13: rating requests the staff member skipped (by case); a skipped one is not asked again here. */
	let csatSkipped = $state<string[]>([]);
	let rating = $state(false);
	const csatAsk = $derived(chat.csatAsk && !csatSkipped.includes(chat.csatAsk.case_id) ? chat.csatAsk : null);
	async function rate(n: number) {
		const r = csatAsk;
		const conv = chat.conversation;
		if (!r || !conv || rating) return;
		rating = true;
		const ok = await guard(async () => {
			const v = await api().submitCsat(conv.conversation_id, { case_id: r.case_id, rating: n });
			chat.csat = v ?? { ...r, rating: n };
		}, `Rated ${n} of 5. Test-chat ratings stay out of the analytics.`);
		rating = false;
		// answered elsewhere or expired (409): do not ask again
		if (!ok) csatSkipped = [...csatSkipped, r.case_id];
	}
</script>

<PageHead
	id="L2"
	sub="Try your unpublished changes before customers see them. Chat as a test customer (type their email) with the Draft or with Live, read how each reply was reached and where the two differ, then save a good run as a test case."
>
	{#snippet actions()}
		<a class="btn" href={releaseHref}>Review &amp; publish</a>
	{/snippet}
</PageHead>

<div class="l2-talk" role="group" aria-label="Talk to" data-testid="talk-to">
	<span class="l2-talk-label">Talk to</span>
	<button
		type="button"
		class="l2-mode"
		class:on={env === 'test'}
		aria-pressed={env === 'test'}
		onclick={() => (env = 'test')}
	>
		<span class="l2-mode-name">Draft <span class="chip accent sm">test env</span></span>
		<span class="l2-mode-sub">
			{#if draft.value.state === 'ok'}
				<b>{draft.value.data.count} unpublished change{draft.value.data.count === 1 ? '' : 's'}</b>
				{#if noLive}on a job that was never released{:else}on top of
					<span class="mono">{release ?? 'the published release'}</span>{/if}
			{:else}Your unpublished changes{/if}
		</span>
		<span class="l2-mode-sub muted"
			>draft {rev} · test systems ({prof?.test_systems ?? 'demo-retail-stub, shopify-test'})</span
		>
	</button>
	<button
		type="button"
		class="l2-mode live"
		class:on={env === 'prod'}
		aria-pressed={env === 'prod'}
		disabled={noLive}
		title={noLive ? 'This job has no release yet — nothing is live. Publish the draft first.' : undefined}
		data-testid="talk-live"
		onclick={() => (env = 'prod')}
	>
		<span class="l2-mode-name">Live <span class="chip warn sm">production</span></span>
		{#if noLive}
			<span class="l2-mode-sub"
				><b>Not live yet</b>: this job has no release. Test the draft, then publish it.</span
			>
			<span class="l2-mode-sub muted">no production systems are reached</span>
		{:else}
			<span class="l2-mode-sub"
				>The published release <span class="mono">{release ?? '—'}</span>: what {who}s get today</span
			>
			<span class="l2-mode-sub muted"
				>live systems ({prof?.live_systems ?? 'shopify-prod, …'}) · writes are real</span
			>
		{/if}
	</button>
</div>

{#if langList.length}
	<p class="l2-langs" data-testid="chat-languages">
		<span class="l2-langs-k">Languages</span>
		Replies in <b>{LANGUAGE_LABEL[langList[0]!] ?? langList[0]}</b> by default · also serves
		{langList
			.slice(1)
			.map((l) => LANGUAGE_LABEL[l] ?? l)
			.join(', ') || 'no other language'}. Gate B6 detects the customer’s language; one the job does not serve
		gets the default language and the offer of a person.
		{#if env === 'test' && languages.value.state === 'ok' && languages.value.data.unpublished}
			<span class="chip accent sm">draft</span>
		{/if}
	</p>
{/if}

{#if draft.value.state === 'ok' && draft.value.data.count > 0}
	<section class="l2-play" data-testid="changes-in-play" aria-label="Unpublished changes">
		<header>
			<b
				>{env === 'test' ? 'In play in this chat' : 'Not in play — Live runs without these'} · {draft.value
					.data.count} unpublished change{draft.value.data.count === 1 ? '' : 's'}</b
			>
			<span class="muted"
				>{env === 'test'
					? `${changes.filter((c) => exercised.has(c.key)).length} tried in this conversation`
					: 'switch to Draft to try them'}</span
			>
		</header>
		<ul>
			{#each changes as c (c.id)}
				{@const p = promptFor(c)}
				{@const tried = exercised.has(c.key)}
				<li class:tried>
					<span class="l2-play-state" aria-label={tried ? 'tried' : 'not tried yet'}>{tried ? '✓' : '○'}</span
					>
					<span class="l2-play-main">
						<span title="live: {c.live ?? '—'} → draft: {c.draft ?? '—'}"
							>{c.label} <span class="chip sm">{c.page}</span></span
						>
						<small class="muted"
							>{c.in_release ? `Already in ${c.in_release} (approved). ` : ''}{p.hint}</small
						>
					</span>
					{#if p.prompt}
						<button
							type="button"
							class="btn sm"
							disabled={blocked}
							title="Send as {customerEmail || 'the test customer'}"
							onclick={() => send(p.prompt!)}>Try: “{p.prompt}”</button
						>
					{/if}
				</li>
			{/each}
		</ul>
	</section>
{:else if draft.value.state === 'ok'}
	<p class="l2-none muted">No unpublished changes: Draft and Live answer the same way.</p>
{/if}

{#if readiness.length}
	<div class="callout gap before-test" role="status" data-testid="before-you-test">
		<strong class="callout-title">Before you test</strong>
		<ul>
			{#each readiness as it (it.code)}
				<li>{it.title} <a href={it.href}>{it.fix_label}</a></li>
			{/each}
		</ul>
	</div>
{/if}

<div class="chat-bar">
	<label class="field">
		<span class="label">Test as customer (email)</span>
		<input
			class="input"
			type="email"
			bind:value={customerEmail}
			onchange={restart}
			placeholder="customer@example.com"
			autocomplete="off"
			aria-label="Test as customer (email)"
		/>
	</label>
	<div class="actions chat-actions">
		<span
			class="chip"
			class:ok={chat.feedStatus === 'open'}
			class:warn={chat.feedStatus === 'retrying'}
			role="status">Live feed: {FEED_LABEL[chat.feedStatus]}</span
		>
		<button
			class="btn sm"
			type="button"
			onclick={() => chat.dropFeed()}
			disabled={chat.feedStatus !== 'open'}
			title="Simulate a dropped connection">Drop connection</button
		>
		<button class="btn sm" type="button" onclick={restart}>New conversation</button>
	</div>
</div>

<div class="chat-grid">
	<div class="chat-col">
		<div class="chat-stage" class:prod={env === 'prod'} data-testid="chat-stage">
			<TelegramShell
				{lines}
				typing={chat.awaiting}
				{agentName}
				customerName={customerEmail || 'You'}
				bind:text
				locked={blocked || chat.unknown}
				onsend={() => send()}
			>
				{#snippet answerFoot(key: string)}
					{@const r = replyByKey.get(key)}
					{#if r && !r.m.refused}
						<AnswerSources g={r.m} compact />
						<AnswerFeedback answer={rated(r.m, r.question)} />
					{/if}
				{/snippet}
				{#snippet notice()}
					{#if chat.unknown}
						<div class="cn err" role="alert" data-testid="send-unknown">
							Outcome unknown: {chat.sendError} The message may have arrived. New messages wait until you decide.
							<div class="cn-row">
								<button class="btn sm" type="button" onclick={() => chat.resendAnyway()}>Resend</button>
								<button class="btn sm" type="button" onclick={() => chat.discard()}>Discard</button>
							</div>
						</div>
					{:else if chat.failedSend}
						<div class="cn err" role="alert">
							Not sent: {chat.sendError}
							<div class="cn-row">
								<button class="btn sm" type="button" onclick={() => chat.retrySend()}>Retry message</button>
								<button class="btn sm" type="button" onclick={() => chat.discard()}>Discard</button>
							</div>
						</div>
					{/if}
					{#if chat.error}
						<div class="cn err" role="status">
							Conversation unavailable: {chat.error}
							<div class="cn-row">
								<button class="btn sm" type="button" onclick={restart}>Start again</button>
							</div>
						</div>
					{:else if chat.feedStatus === 'retrying'}
						<div class="cn" role="status">
							Reconnecting to the live feed… replies arrive as soon as it is back.
						</div>
					{:else if chat.ended}
						<div class="cn" role="status">
							This conversation has ended. <button class="btn sm" type="button" onclick={restart}
								>New conversation</button
							>
						</div>
					{/if}
					{#if csatAsk}
						<div class="cn" role="group" aria-label="Rate case {csatAsk.case_id}" data-testid="csat-ask">
							How did we do? Case {csatAsk.case_id} is resolved — rate it as the customer would.
							<div class="cn-row csat-stars">
								{#each [1, 2, 3, 4, 5] as n (n)}
									<button
										class="btn sm"
										type="button"
										aria-label="{n} of 5"
										disabled={rating}
										onclick={() => rate(n)}>{'★'.repeat(n)}</button
									>
								{/each}
								<button
									class="btn sm"
									type="button"
									disabled={rating}
									onclick={() => (csatSkipped = [...csatSkipped, csatAsk!.case_id])}>Skip</button
								>
							</div>
						</div>
					{/if}
				{/snippet}
			</TelegramShell>
		</div>
		<div class="chips quick" aria-label="Try a message">
			{#each quick as q (q)}
				<button
					type="button"
					class="chip accent qchip"
					disabled={blocked || chat.unknown}
					onclick={() => send(q)}>{q}</button
				>
			{/each}
		</div>
		<small class="muted" data-testid="chat-sim-hint">
			Add <code>#busy</code> or <code>#lost</code> to a message to see a busy conversation or a lost response.
		</small>
	</div>

	<aside class="chat-side">
		<L2Trail messages={chat.messages} />

		<Card title="Save as test case" sub="Keeps this run with the draft it ran on (env=test)">
			{#if env === 'prod'}
				<p class="muted">
					Live conversations are not test cases: switch to <b>Draft</b> to test the changes you will publish.
				</p>
			{:else}
				<p class="l2-save-sum">
					{agentReplies.length} repl{agentReplies.length === 1 ? 'y' : 'ies'} ·
					{exercised.size} change{exercised.size === 1 ? '' : 's'} tried ·
					<span class:warn-t={differing > 0}>{differing} differ{differing === 1 ? 's' : ''} from Live</span>
				</p>
				<label class="field">
					<span class="label">Name</span>
					<input
						class="input"
						bind:value={title}
						placeholder="e.g. Worktop return is refused"
						maxlength="120"
					/>
				</label>
				<div class="cn-row">
					<button
						class="btn primary sm"
						type="button"
						title={writeAttrs(session, 'job_designer').title ??
							(agentReplies.length ? undefined : 'Send a message and wait for the reply first')}
						disabled={writeAttrs(session, 'job_designer').disabled || saving || !agentReplies.length}
						onclick={saveTest}>Save test case</button
					>
					<a class="btn sm" href={releaseHref}>Review &amp; publish</a>
				</div>
				{#if savedHere.length}
					<p class="muted l2-saved">Saved: {savedHere.map((t) => t.id).join(', ')}</p>
				{/if}
			{/if}
			{#if tests.value.state === 'ok' && tests.value.data.items.length}
				<ul class="list-plain l2-tests">
					{#each tests.value.data.items.slice(0, 5) as t (t.id)}
						<li>
							<span class="mono">{t.id}</span>
							{t.title}
							{#if t.stale}<span class="chip warn sm" title="The draft changed after this test was saved"
									>re-test</span
								>{:else}<span class="chip ok sm">tested on draft</span>{/if}
						</li>
					{/each}
				</ul>
			{/if}
		</Card>

		<Card title="Conversation">
			{#if chat.conversation}
				<LookupNote job={app.job} degraded={lookupMiss} />
				<dl class="kvs">
					<dt>Id</dt>
					<dd class="mono">{chat.conversation.conversation_id}</dd>
					<dt>{prof?.audience === 'staff' ? 'Staff member' : 'Customer'}</dt>
					<dd>
						{chat.conversation.customer_name ?? ''}
						{#if prof?.audience === 'staff'}
							<span class="mono">{chat.conversation.customer_id}</span>
							<span class="muted">· staff</span>
						{:else}
							<span class="mono">{chat.conversation.customer_id}</span>
						{/if}
					</dd>
					<dt>Answered by</dt>
					<dd>
						<span class="chip" class:warn={chat.conversation.env === 'prod'}
							>{chat.conversation.env === 'test' ? 'draft' : 'live'} · {chat.conversation.env}</span
						>
						{chat.conversation.env === 'test'
							? (prof?.test_systems ?? 'demo-retail-stub, shopify-test')
							: `live systems (${prof?.live_systems ?? 'shopify-prod, …'})`}
					</dd>
					<dt>Channel</dt>
					<dd>staff · as {session.user?.name ?? 'you'}</dd>
					<dt>Turn</dt>
					<dd>
						{#if chat.outbox.some((p) => p.status === 'sending')}sending…{:else if chat.awaiting}<span
								class="chip accent">202 accepted</span
							> reply on the live feed{:else if chat.refused}<span class="chip err">refused at gate 2</span
							>{:else if chat.messages.length}answered{:else}waiting for the first message{/if}
					</dd>
					<dt>Cases</dt>
					<dd>
						{#each chat.caseIds as id (id)}<a class="mono l2-case" href={caseHref(id)}>{id}</a>{:else}<span
								class="muted">none yet — one opens once the agent has everything it needs</span
							>{/each}
					</dd>
				</dl>
			{:else if chat.starting}
				<p class="muted" role="status">Starting a conversation…</p>
			{:else}
				<p class="muted">No conversation yet — send a message to start one.</p>
			{/if}
		</Card>

		<details class="l2-refusal">
			<summary>What a customer sees when the licence or tenant is not active (gate 2, K35)</summary>
			<blockquote class="refusal">{REFUSAL_TEXT}</blockquote>
			<p class="note">
				No reason and no internals are shown to the customer. The turn still answers 202; one audit event per
				tenant per hour (licence.gate_refused). Try it as the Owner acting in Rivervale Energy (suspended).
			</p>
		</details>
	</aside>
</div>

<style>
	.l2-talk {
		display: grid;
		grid-template-columns: auto minmax(0, 1fr) minmax(0, 1fr);
		gap: 10px;
		align-items: stretch;
		margin-bottom: 12px;
	}
	.l2-talk-label {
		align-self: center;
		font-size: 12px;
		font-weight: 600;
		text-transform: uppercase;
		letter-spacing: 0.06em;
		color: var(--muted);
	}
	.l2-mode {
		display: flex;
		flex-direction: column;
		gap: 3px;
		text-align: left;
		padding: 10px 12px;
		border: 1px solid var(--line);
		border-radius: 10px;
		background: var(--panel);
		color: var(--fg);
		font: inherit;
		font-size: 13px;
		cursor: pointer;
		min-width: 0;
	}
	.l2-mode.on {
		border-color: var(--accent);
		box-shadow: 0 0 0 1px var(--accent) inset;
		background: color-mix(in srgb, var(--accent) 6%, var(--panel));
	}
	.l2-mode.live.on {
		border-color: var(--warn);
		box-shadow: 0 0 0 1px var(--warn) inset;
		background: color-mix(in srgb, var(--warn) 7%, var(--panel));
	}
	.l2-mode-name {
		font-size: 15px;
		font-weight: 600;
		display: flex;
		align-items: center;
		gap: 6px;
	}
	.l2-mode-sub {
		overflow-wrap: anywhere;
	}
	.l2-play {
		border: 1px solid var(--line);
		border-left: 3px solid var(--accent);
		border-radius: 8px;
		padding: 8px 12px;
		margin-bottom: 12px;
		background: var(--panel);
		font-size: 13px;
	}
	.l2-play header {
		display: flex;
		flex-wrap: wrap;
		justify-content: space-between;
		gap: 4px 12px;
	}
	.l2-play ul {
		list-style: none;
		margin: 8px 0 0;
		padding: 0;
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(min(100%, 330px), 1fr));
		gap: 8px 16px;
	}
	.l2-play li {
		display: grid;
		grid-template-columns: auto minmax(0, 1fr);
		gap: 2px 8px;
		align-items: start;
	}
	.l2-play li .btn {
		grid-column: 2;
		justify-self: start;
		white-space: normal;
		text-align: left;
	}
	.l2-play-main {
		display: flex;
		flex-direction: column;
		gap: 1px;
		min-width: 0;
		overflow-wrap: anywhere;
	}
	.l2-play-state {
		color: var(--muted);
		font-weight: 700;
	}
	.l2-play li.tried .l2-play-state {
		color: var(--ok);
	}
	.before-test ul {
		margin: 4px 0 0;
		padding-left: 20px;
	}
	.l2-none {
		margin: 0 0 12px;
	}
	.chat-bar {
		display: flex;
		flex-wrap: wrap;
		align-items: flex-end;
		gap: 12px 18px;
	}
	.chat-bar .field input[type='email'] {
		min-width: min(260px, 100%);
		max-width: 100%;
	}
	.chat-actions {
		margin-left: auto;
	}
	.chat-bar .field {
		min-width: 0;
		max-width: 100%;
	}
	.chat-bar :global(.seg) {
		flex-wrap: wrap;
	}
	.chat-grid {
		display: grid;
		grid-template-columns: minmax(0, 1fr) 380px;
		gap: 16px;
		min-width: 0;
		margin-top: 12px;
	}
	.chat-col {
		display: grid;
		gap: 10px;
		align-content: start;
		min-width: 0;
	}
	.l2-langs {
		margin: 0 0 10px;
		font-size: 12.5px;
		color: var(--muted);
	}
	.l2-langs-k {
		font: 600 10.5px/1.2 var(--cond);
		letter-spacing: 0.07em;
		text-transform: uppercase;
		margin-right: 6px;
	}
	.chat-stage {
		height: max(480px, calc(100dvh - 380px));
		border: 1px solid var(--line);
		border-top: 3px solid var(--accent);
		border-radius: 10px;
		overflow: hidden;
		min-width: 0;
	}
	.chat-stage.prod {
		border-top-color: var(--warn);
	}
	.chat-side {
		display: grid;
		gap: 12px;
		align-content: start;
		min-width: 0;
	}
	.quick {
		gap: 6px;
	}
	.qchip {
		border: 0;
		cursor: pointer;
		white-space: normal;
		text-align: left;
		line-height: 1.3;
	}
	.qchip:disabled {
		opacity: 0.5;
		cursor: not-allowed;
	}
	.l2-save-sum {
		margin: 0 0 8px;
		font-size: 13px;
	}
	.warn-t {
		color: var(--warn);
		font-weight: 600;
	}
	.l2-saved {
		margin: 6px 0 0;
		font-size: 12px;
	}
	.l2-tests {
		margin-top: 10px;
		padding-top: 8px;
		border-top: 1px solid var(--line-2);
		font-size: 13px;
		display: grid;
		gap: 4px;
	}
	.l2-case {
		margin-right: 8px;
	}
	.l2-refusal {
		font-size: 13px;
		border: 1px solid var(--line);
		border-radius: 8px;
		padding: 8px 12px;
		background: var(--panel);
	}
	.l2-refusal summary {
		cursor: pointer;
		color: var(--muted);
	}
	.refusal {
		margin: 8px 0 0;
		padding: 8px 12px;
		border-left: 3px solid var(--err);
		background: var(--sunk);
		border-radius: 4px;
		font-size: 13px;
	}
	.cn {
		margin-bottom: 8px;
		padding: 8px 12px;
		border-radius: 8px;
		font: 13px/1.45 var(--sans);
		background: var(--panel);
		color: var(--fg);
		border: 1px solid var(--line);
	}
	.cn.err {
		border-color: color-mix(in srgb, var(--err) 45%, transparent);
		background: color-mix(in srgb, var(--err) 8%, var(--panel));
	}
	.cn-row {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		margin-top: 6px;
	}
	@media (max-width: 1100px) {
		.chat-grid {
			grid-template-columns: minmax(0, 1fr);
		}
	}
	@media (max-width: 640px) {
		.l2-talk {
			grid-template-columns: minmax(0, 1fr);
		}
		.chat-actions {
			margin-left: 0;
		}
		.chat-stage {
			height: 520px;
		}
	}
</style>
