<script lang="ts">
	// T13 Channels — the doors customers use to reach this tenant's agent. The tenant_admin answers: can my
	// customers get through right now, which job/release answers them, what can the agent do for them on each door
	// (the identity level a channel can reach caps the operations, 10 §3.1 / 12 §3 N5), and what do they see when
	// gate 2 (D12/K35) refuses. Today one shared Telegram bot is pinned to one tenant by env (K40); per-tenant bots,
	// web chat and voice come later (D11).
	import { onMount } from 'svelte';
	import { api, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { writeAttrs } from '$lib/auth/roles';
	import { Resource } from '$lib/load.svelte';
	import { fmt } from '$lib/format';
	import type {
		Channel,
		ChannelTestResult,
		ChannelsView,
		JobBoardRow,
		WebhookHealth,
	} from '$lib/api/types/tenant';
	import type { Paged } from '$lib/api/types/core';
	import { jobPath } from '$lib/scope';
	import { when } from '$lib/tenant/ui';
	import {
		IDENTITY_LEVELS,
		REFUSAL_CAUSES,
		channelGroups,
		coverageGroups,
		gateCause,
		gateOpen,
		levelLabel,
		levelRank,
		primaryCustomerChannel,
		reachPath,
		testMessageChannel,
		webChatTry,
	} from '$lib/tenant/channelsT13';
	import PageHead from '$lib/ui/PageHead.svelte';
	import Freshness from '$lib/ui/Freshness.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import KeyValues from '$lib/ui/KeyValues.svelte';
	import MilestoneChip from '$lib/ui/MilestoneChip.svelte';
	import ReachPath from './ReachPath.svelte';

	// Every job of the tenant, each linking to its own Channels page (web chat widgets, bindings), and the tenant-wide
	// door overview (`GET /api/tenant/channels`: every job's customer bindings, the staff chat, the gate).
	const jobs = new Resource<Paged<JobBoardRow>>(() => api().jobBoard());
	const res = new Resource<ChannelsView>(
		() => api().tenantChannels(),
		() => false,
	);
	onMount(() => {
		void jobs.refresh();
		void res.refresh();
	});
	let last = $state<ChannelTestResult | null>(null);
	let sending = $state(false);
	let picked = $state<string>('');
	let cause = $state<string>('');

	// The door shown in the coverage view and the cause shown in the refusal preview: the user's pick, else a
	// default from the data (the live customer door / staff chat; the actual gate cause / tenant_suspended).
	const view = $derived(res.value.state === 'ok' || res.value.state === 'empty' ? res.value.data : null);
	const pickDefault = $derived.by(() => {
		if (!view) return '';
		const cov = view.channels.filter((c) => c.identity_ceiling);
		return (
			primaryCustomerChannel(view)?.id ?? cov.find((c) => c.kind === 'staff_chat')?.id ?? cov[0]?.id ?? ''
		);
	});
	const causeDefault = $derived.by(() => {
		const why = view ? gateCause(view) : null;
		return why && REFUSAL_CAUSES.some((x) => x.id === why) ? why : 'tenant_suspended';
	});
	const getPick = () => picked || pickDefault;
	const setPick = (x: string) => (picked = x);
	const getCause = () => cause || causeDefault;
	const setCause = (x: string) => (cause = x);

	const stateChip = (c: Channel): { text: string; variant: 'ok' | 'muted' | 'warn' | 'accent' } =>
		c.state === 'live'
			? { text: 'live', variant: 'ok' }
			: c.state === 'other_tenant'
				? { text: 'serves another tenant', variant: 'warn' }
				: c.state === 'not_ready'
					? { text: 'not live', variant: 'warn' }
					: c.state === 'available'
						? { text: 'not set up', variant: 'muted' }
						: c.state === 'later'
							? { text: 'later', variant: 'muted' }
							: { text: 'reserved', variant: 'muted' };

	// Webhook health as the edge reports it: quiet (no update for 24 h) is informational, down is an error.
	const WEBHOOK_CHIP: Record<WebhookHealth['status'], 'ok' | 'warn' | 'muted' | 'err'> = {
		ok: 'ok',
		degraded: 'warn',
		quiet: 'muted',
		down: 'err',
	};

	const EFFECT: Record<string, string> = {
		R: 'read',
		W: 'write · confirm-back',
		X: 'irreversible · confirm-back',
		P: 'payment · hosted link',
	};

	const ladder = (c: Channel) =>
		IDENTITY_LEVELS.map((l) => {
			const r = levelRank(l.level);
			const fill =
				!c.identity_ceiling || l.level === 'S'
					? 'none'
					: r <= levelRank(c.identity_start ?? '0')
						? 'start'
						: r <= levelRank(c.identity_ceiling)
							? 'verify'
							: 'none';
			return { ...l, fill };
		});

	async function send(c: Channel) {
		sending = true;
		const box: { v: ChannelTestResult | null } = { v: null };
		await guard(
			async () => {
				box.v = await api().sendChannelTestMessage(c.id, c.bound_job ?? undefined);
			},
			() =>
				box.v?.refused
					? 'Gate 2 refused: the customer would get the neutral text.'
					: 'Test message delivered.',
			'Not sent',
		);
		last = box.v;
		sending = false;
		await res.refresh();
	}
</script>

<PageHead
	id="T13"
	sub="The doors your customers use to reach your agent: whether each one gets through right now, which job answers, what the agent may do there, and what customers see when it is closed."
>
	{#snippet actions()}<Freshness resource={res} />{/snippet}
</PageHead>

<Card
	title="Channels by job"
	sub="Each job has its own channels: web chat widgets, bots and their bindings. Open a job to set them up."
	cls="flush"
>
	<LoadState resource={jobs} empty="No job yet — ask your platform admin.">
		{#snippet children(page)}
			<ul class="t13-jobs" data-testid="channels-jobs">
				{#each page.items as r (r.job.key)}
					<li data-job={r.job.key}>
						<a href={jobPath(r.job.key, '/channels')}>{r.job.label}</a>
						<span class="mono muted t13-small">{r.job.key}</span>
						<a class="btn sm" href={jobPath(r.job.key, '/channels')}>Open channels</a>
					</li>
				{/each}
			</ul>
		{/snippet}
	</LoadState>
</Card>

<LoadState resource={res}>
	{#snippet children(v)}
		{@const primary = primaryCustomerChannel(v)}
		{@const tgTest = testMessageChannel(v)}
		{@const wcTry = webChatTry(v)}
		{@const open = gateOpen(v)}
		{@const why = gateCause(v)}
		{@const covChannels = v.channels.filter((c) => c.identity_ceiling)}
		{@const covPick = covChannels.find((c) => c.id === getPick()) ?? covChannels[0]}
		{@const shared = v.channels.find((c) => c.binding === 'shared_env_bot')}

		<!-- 1. The verdict: can customers get through, and along which path. -->
		<section class="t13-hero {primary ? (open ? 'ok' : 'err') : 'off'}" data-testid="channels-verdict">
			<div class="t13-hero-text">
				{#if primary && open}
					<h2>Customers reach {primary.answered_by?.job_label ?? 'your agent'} on {primary.label}</h2>
					<p>
						{fmt.n(primary.conversations_7d ?? 0)} conversations in 7 days. Every message passes the edge webhook
						and the licence gate before the agent sees it.
					</p>
				{:else if primary}
					<h2>Customers are being refused</h2>
					<p>
						Gate 2 is closed ({why}). Every customer message on {primary.label} gets the neutral text below;
						{v.refusals_24h}
						refusal{v.refusals_24h === 1 ? '' : 's'} in the last 24 h.
					</p>
				{:else}
					<h2>No customer channel delivers to this tenant yet</h2>
					<p>
						{#if shared?.state === 'other_tenant'}
							The deployment's one Telegram bot ({shared.bot_username}) is pinned to another tenant (K40). A
							bot of your own is not set up for this tenant <MilestoneChip milestone="M2" />. Meanwhile your
							staff can test as a customer in the <a href="/chat">test chat</a>.
						{:else}
							Customers have no way in yet. Your staff can test as a customer in the <a href="/chat"
								>test chat</a
							>.
						{/if}
					</p>
				{/if}
			</div>
			{#if primary}
				<ReachPath hops={reachPath(v, primary)} />
				<div class="t13-hero-actions">
					{#if wcTry?.direct}
						<a
							class="btn"
							href={wcTry.href}
							target="_blank"
							rel="noopener"
							data-testid="webchat-try"
							data-channel={wcTry.channel.id}>Try it</a
						>
						<span class="muted t13-small"
							>Web chat: opens the hosted test page with the real widget, answered by {wcTry.channel
								.answered_by?.job_label ?? 'the job'}.</span
						>
					{:else if wcTry}
						<a class="btn" href={wcTry.href} data-testid="webchat-try" data-channel={wcTry.channel.id}
							>Try it</a
						>
						<span class="muted t13-small"
							>Web chat: open the {wcTry.channel.answered_by?.job_label ?? 'job'}'s channels, then the web
							chat panel's Try it — the hosted test page with the real widget.</span
						>
					{/if}
					{#if tgTest || !wcTry}
						<button
							class="btn"
							type="button"
							{...writeAttrs(session, 'integrations_manager')}
							disabled={writeAttrs(session, 'integrations_manager').disabled || sending || !tgTest}
							title={writeAttrs(session, 'integrations_manager').title}
							data-channel={tgTest?.id}
							onclick={() => tgTest && send(tgTest)}>{sending ? 'Sending…' : 'Send test message'}</button
						>
						<span class="muted t13-small"
							>Sends through the real path to your Telegram account; a closed gate returns the refusal
							instead.</span
						>
					{/if}
				</div>
				{#if last}
					<div class="t13-last" data-testid="last-test">
						<b>Last test message</b>
						<span class="muted"
							>{when(last.at)} · {last.refused
								? 'refused by gate 2'
								: `delivered as ${last.message_id}`}</span
						>
						<div class="t13-chat"><div class="t13-bubble">{last.text}</div></div>
					</div>
				{/if}
			{/if}
		</section>

		<!-- 2. Every door, grouped by whether it reaches customers. -->
		{#each channelGroups(v.channels) as g (g.key)}
			<Card title={g.title} sub={g.sub} cls="flush">
				<ul class="t13-doors">
					{#each g.items as c (c.id)}
						{@const st = stateChip(c)}
						<li class="t13-door" data-channel={c.id}>
							<div class="t13-door-name">
								<div class="t13-door-title">
									<b>{c.label}</b>
									<Chip text={st.text} variant={st.variant} />
									<MilestoneChip milestone={c.milestone} />
								</div>
								<p class="muted">{c.note}</p>
							</div>
							<div class="t13-door-cell" data-testid="door-side">
								<span class="t13-k">Side</span>
								<span>{c.side === 'worker' ? 'staff (worker)' : 'customers'}</span>
								{#if c.job_key}
									<a href={jobPath(c.job_key, '/channels')} data-testid="door-job-link" data-job={c.job_key}
										>Manage in {c.job_key}</a
									>
								{/if}
							</div>
							<div class="t13-door-cell">
								<span class="t13-k">Answers</span>
								{#if c.answered_by}
									<a href={`/admin/jobs/${c.answered_by.job}/release`}>{c.answered_by.job_label}</a>
									<span class="muted t13-small"
										>{c.answered_by.release_id ?? 'no release'} · {c.answered_by.env}</span
									>
									{#each c.also_answers ?? [] as o (o.job)}
										<a href={`/admin/jobs/${o.job}/release`} data-also-answers={o.job}>{o.job_label}</a>
										<span class="muted t13-small">{o.release_id ?? 'draft only'} · {o.env}</span>
									{/each}
								{:else if c.state === 'other_tenant'}
									<span class="muted">another tenant's job</span>
								{:else}
									<span class="muted">—</span>
								{/if}
							</div>
							<div class="t13-door-cell">
								<span class="t13-k">Edge</span>
								{#if c.webhook}
									<span
										><Chip
											text={`webhook ${c.webhook.status}`}
											variant={WEBHOOK_CHIP[c.webhook.status] ?? 'err'}
										/></span
									>
									<span class="muted t13-small">last update {when(c.webhook.last_update_at)}</span>
								{:else}
									<span class="muted">{c.edge_host ?? 'not routed'}</span>
								{/if}
							</div>
							<div class="t13-door-cell">
								<span class="t13-k">Identity</span>
								<span class="t13-ladder" title={c.identity_how}>
									{#each ladder(c) as l (l.level)}<span
											class="t13-rung {l.fill}"
											aria-label={`${l.label}: ${l.fill === 'start' ? 'from the first message' : l.fill === 'verify' ? 'after verification' : 'not reachable'}`}
											>{l.level}</span
										>{/each}
								</span>
								<span class="muted t13-small"
									>{levelLabel(c.identity_start)} → {levelLabel(c.identity_ceiling)}</span
								>
							</div>
							<div class="t13-door-cell t13-door-act">
								{#if c.kind === 'staff_chat'}
									<a class="btn" href="/chat">Open test chat</a>
								{:else if c.webhook}
									<details>
										<summary>Webhook</summary>
										<KeyValues
											pairs={[
												...(c.webhook.down_reason
													? ([['Down because', c.webhook.down_reason]] as [string, string][])
													: []),
												[
													'Pending updates',
													c.webhook.pending_update_count === null
														? 'not known'
														: String(c.webhook.pending_update_count),
												],
												[
													'Last error',
													c.webhook.last_error
														? `${c.webhook.last_error} (${when(c.webhook.last_error_at)})`
														: 'none',
												],
												['Env', c.env ?? '—'],
											]}
										/>
									</details>
								{/if}
							</div>
						</li>
					{/each}
				</ul>
			</Card>
		{/each}

		<!-- 3. Identity level caps what the agent may do on a door. -->
		{#if covPick && (v.job_operations?.length ?? 0) > 0}
			{@const groups = coverageGroups(v.job_operations ?? [], covPick)}
			<Card
				title="What the agent may do on each door"
				sub="Every operation needs an identity level. A door that cannot reach it gets identity_required — the agent offers a handover instead."
			>
				<div class="t13-cov-head">
					<div class="t13-seg">
						<div class="seg" role="group" aria-label="Channel">
							{#each covChannels as c (c.id)}<button
									type="button"
									class="tab"
									class:on={c.id === covPick.id}
									aria-pressed={c.id === covPick.id}
									onclick={() => setPick(c.id)}>{c.label}</button
								>{/each}
						</div>
					</div>
					<span class="muted t13-small">Operations of <b>{v.operations_job}</b></span>
				</div>
				<p class="t13-how">{covPick.identity_how}</p>
				<div class="t13-cov" data-testid="identity-coverage">
					{#each [['at_start', 'From the first message', `level ≤ ${covPick.identity_start}`], ['after_verify', 'After a step-up', `level ≤ ${covPick.identity_ceiling}`], ['refused', 'Refused on this door', 'identity_required']] as [k, title, sub] (k)}
						{@const ops = groups[k as keyof typeof groups]}
						<div class="t13-cov-col {k}" data-cov={k}>
							<div class="t13-cov-title"><b>{title}</b> <span class="muted t13-small">{sub}</span></div>
							{#if ops.length === 0}
								<p class="muted t13-small">none</p>
							{:else}
								<ul>
									{#each ops as o (o.op)}
										<li>
											<code>{o.op}</code>
											<span class="t13-eff" title={EFFECT[o.side_effect]}>{o.side_effect}</span>
											<span class="muted t13-small"
												>needs {o.identity_level}{o.raises_to ? ` → raises to ${o.raises_to}` : ''}{o.note
													? ` · ${o.note}`
													: ''}</span
											>
										</li>
									{/each}
								</ul>
							{/if}
						</div>
					{/each}
				</div>
				<p class="note">
					R read · W write · X irreversible · P payment. W/X/P also need confirm-back from a customer message;
					P only as a hosted link. Levels and operations are set by the release (<a
						href={`/admin/jobs/${v.operations_job}/operations`}>operations &amp; routes</a
					>).
				</p>
			</Card>
		{/if}

		<!-- 4. What a customer sees when the gate is closed. -->
		{@const causeRow = REFUSAL_CAUSES.find((x) => x.id === getCause()) ?? REFUSAL_CAUSES[0]!}
		<div class="grid g2 t13-refusal-grid">
			<Card
				title={open ? 'If the gate closes, customers see this' : 'Customers see this right now'}
				sub="One neutral message for every cause; the licence state never leaks to a customer (03 §2.9)."
			>
				<div class="t13-seg">
					<div class="seg" role="group" aria-label="Cause">
						{#each REFUSAL_CAUSES as x (x.id)}<button
								type="button"
								class="tab"
								class:on={x.id === causeRow.id}
								aria-pressed={x.id === causeRow.id}
								onclick={() => setCause(x.id)}>{x.label}</button
							>{/each}
					</div>
				</div>
				<div class="t13-refusal-pair">
					<div>
						<span class="t13-k">Customer on Telegram</span>
						<div class="t13-chat" data-testid="refusal-preview">
							<div class="t13-bubble customer">Hi, where is my order O-1001?</div>
							<div class="t13-bubble">{v.refusal_text}</div>
						</div>
					</div>
					<div>
						<span class="t13-k">Staff in test chat</span>
						<div class="t13-staff"><code>{causeRow.staff}</code></div>
						<p class="muted t13-small">Staff may see the reason; customers never do.</p>
					</div>
				</div>
				<p class="note">
					Sent at most once per conversation per hour; Telegram still gets HTTP 200. One
					<code>channel_turn_refused</code> event per tenant per hour in the
					<a href="/admin/audit">audit log</a>.
				</p>
			</Card>
			<Card
				title="Gate 2 verdict"
				sub="Re-checked every ≤ 60 s; the edge fails closed when it is older than 5 minutes (K35)."
			>
				<p class="t13-verdict">
					<Chip text={open ? 'lets customers through' : 'refusing customers'} variant={open ? 'ok' : 'err'} />
				</p>
				<KeyValues
					pairs={[
						['Licence', v.gate.status],
						['Tenant', v.tenant_status ?? 'active'],
						['Plan', v.gate.plan],
						['Key id', v.gate.kid ? `v2:${v.gate.kid}` : '—'],
						['Checked', when(v.gate.checked_at)],
						['Reason', v.gate.reason ?? '—'],
						['Refusals (24 h)', String(v.refusals_24h)],
					]}
				/>
			</Card>
		</div>
	{/snippet}
</LoadState>

<style>
	.t13-jobs {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.t13-jobs li {
		display: flex;
		align-items: center;
		gap: 12px;
		flex-wrap: wrap;
		padding: 10px 16px;
		border-top: 1px solid var(--line);
	}
	.t13-jobs li:first-child {
		border-top: 0;
	}
	.t13-jobs .btn {
		margin-left: auto;
	}
	.t13-hero {
		--tone: var(--line);
		border: 1px solid var(--line);
		border-left: 4px solid var(--tone);
		border-radius: var(--r);
		background: var(--panel);
		padding: 16px;
		display: grid;
		grid-template-columns: minmax(0, 1fr);
		gap: 14px;
		margin-bottom: 16px;
	}
	.t13-hero > :global(*),
	.t13-door-name {
		min-width: 0;
	}
	.t13-hero.ok {
		--tone: var(--ok);
	}
	.t13-hero.err {
		--tone: var(--err);
	}
	.t13-hero h2 {
		margin: 0 0 4px;
		font: 600 19px/1.25 var(--cond);
	}
	.t13-hero p {
		margin: 0;
		color: var(--muted);
		font-size: 13.5px;
		max-width: 75ch;
	}
	.t13-hero-actions {
		display: flex;
		gap: 12px;
		align-items: center;
		flex-wrap: wrap;
	}
	.t13-small {
		font-size: 12px;
	}
	.t13-last {
		display: grid;
		gap: 6px;
		font-size: 13px;
	}
	.t13-k {
		display: block;
		font: 600 11px/1.2 var(--cond);
		text-transform: uppercase;
		letter-spacing: 0.06em;
		color: var(--muted);
		margin-bottom: 4px;
	}
	.t13-doors {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.t13-door {
		display: grid;
		grid-template-columns:
			minmax(0, 2fr) minmax(0, 0.9fr) minmax(0, 1.2fr) minmax(0, 1.1fr) minmax(0, 1.2fr)
			minmax(0, 0.9fr);
		gap: 14px;
		padding: 12px 16px;
		border-top: 1px solid var(--line-2);
		font-size: 13px;
	}
	.t13-door:first-child {
		border-top: 0;
	}
	.t13-door-title {
		display: flex;
		gap: 6px;
		align-items: center;
		flex-wrap: wrap;
	}
	.t13-door-name p {
		margin: 4px 0 0;
		font-size: 12.5px;
	}
	.t13-door-cell {
		display: grid;
		align-content: start;
		gap: 2px;
		min-width: 0;
		overflow-wrap: anywhere;
	}
	.t13-door-act {
		justify-items: end;
	}
	.t13-door-act details {
		justify-self: stretch;
	}
	.t13-door-act summary {
		cursor: pointer;
		text-align: right;
		color: var(--accent);
	}
	.t13-ladder {
		display: inline-flex;
		gap: 2px;
	}
	.t13-rung {
		width: 22px;
		height: 20px;
		display: inline-grid;
		place-items: center;
		font: 600 11px/1 var(--mono);
		border: 1px solid var(--line);
		border-radius: 3px;
		color: var(--muted);
	}
	.t13-rung.start {
		background: var(--cust);
		border-color: var(--cust);
		color: var(--panel);
	}
	.t13-rung.verify {
		background: color-mix(in srgb, var(--cust) 22%, var(--panel));
		border-color: var(--cust);
		color: var(--fg);
	}
	.t13-cov-head {
		display: flex;
		gap: 12px;
		align-items: center;
		flex-wrap: wrap;
	}
	.t13-seg {
		max-width: 100%;
		overflow-x: auto;
	}
	.t13-how {
		margin: 10px 0;
		font-size: 13px;
	}
	.t13-cov {
		display: grid;
		grid-template-columns: repeat(3, minmax(0, 1fr));
		gap: 12px;
	}
	.t13-cov-col {
		--tone: var(--line);
		border: 1px solid var(--line);
		border-top: 3px solid var(--tone);
		border-radius: 8px;
		padding: 10px 12px;
		background: var(--panel);
		min-width: 0;
	}
	.t13-cov-col.at_start {
		--tone: var(--cust);
	}
	.t13-cov-col.after_verify {
		--tone: color-mix(in srgb, var(--cust) 45%, var(--line));
	}
	.t13-cov-col.refused {
		--tone: var(--err);
		background: var(--sunk);
	}
	.t13-cov-col ul {
		list-style: none;
		margin: 8px 0 0;
		padding: 0;
		display: grid;
		gap: 6px;
	}
	.t13-cov-col li {
		display: flex;
		flex-wrap: wrap;
		gap: 4px 8px;
		align-items: baseline;
		font-size: 13px;
	}
	.t13-cov-col code {
		font: 500 12.5px var(--mono);
	}
	.t13-eff {
		font: 600 10.5px/1 var(--mono);
		padding: 2px 4px;
		border: 1px solid var(--line);
		border-radius: 3px;
		color: var(--muted);
	}
	.t13-refusal-grid {
		margin-top: 16px;
	}
	.t13-refusal-pair {
		display: grid;
		grid-template-columns: minmax(0, 1.6fr) minmax(0, 1fr);
		gap: 14px;
		margin-top: 12px;
	}
	.t13-staff {
		padding: 10px 12px;
		border: 1px dashed var(--line);
		border-radius: 8px;
		background: var(--sunk);
	}
	.t13-staff code {
		font: 500 12.5px var(--mono);
		color: var(--err);
	}
	.t13-verdict {
		margin: 0 0 8px;
	}
	.t13-chat {
		display: grid;
		gap: 8px;
		padding: 14px;
		border-radius: 8px;
		background: color-mix(in srgb, var(--accent) 6%, var(--sunk));
	}
	.t13-bubble {
		max-width: 85%;
		padding: 8px 12px;
		border-radius: 12px 12px 12px 4px;
		background: var(--panel);
		border: 1px solid var(--line);
		font-size: 13.5px;
	}
	.t13-bubble.customer {
		justify-self: end;
		border-radius: 12px 12px 4px 12px;
		background: color-mix(in srgb, var(--accent) 16%, var(--panel));
	}
	@media (max-width: 1100px) {
		.t13-door {
			grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
		}
		.t13-door-name {
			grid-column: 1 / -1;
		}
		.t13-door-act {
			justify-items: start;
		}
	}
	@media (max-width: 900px) {
		.t13-cov,
		.t13-refusal-pair {
			grid-template-columns: 1fr;
		}
	}
</style>
