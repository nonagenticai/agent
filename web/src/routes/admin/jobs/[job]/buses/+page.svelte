<script lang="ts">
	// J24 Buses & topics — how a message travels through this job: channel → pre-gate/qualifier → request.qualified →
	// orchestrator → work.requested → workers ⇄ tool topics → results → request.resolved → post-gate → reply, with live
	// rates per hop (the centrepiece); then the topics behind it (producers, consumers, depth/lag, retention) and the
	// dead letters (inspect / replay / discard). Production reads the live release, the Test environment the draft (D54).
	// Claim stuck / Replay / Discard are tenant_admin writes. Helpers: $lib/runtime/J24-flow.ts + labels.ts.
	import { onMount, tick } from 'svelte';
	import { page } from '$app/state';
	import { api, app, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { canWrite, writeHint } from '$lib/auth/roles';
	import { jobPath } from '$lib/scope';
	import { Latest, Resource } from '$lib/load.svelte';
	import { fmt } from '$lib/format';
	import type { DeadLetter, Entry, Env, FlowHop, TopicInfo } from '$lib/api/types/runtime';
	import {
		agentName,
		busName,
		busVariant,
		claimBlocked,
		clock,
		entryTime,
		groupHealth,
		replayBlocked,
		streamLabel,
	} from '$lib/runtime/labels';
	import {
		DEAD_ADVICE,
		MAX_DELIVERIES,
		PHASES,
		deadByReason,
		groupsStatus,
		procName,
		rate,
		retention,
	} from '$lib/runtime/J24-flow';
	import PageHead from '$lib/ui/PageHead.svelte';
	import Freshness from '$lib/ui/Freshness.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Seg from '$lib/ui/Seg.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import KeyValues from '$lib/ui/KeyValues.svelte';
	import Status from '$lib/ui/Status.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import JsonView from '$lib/ui/JsonView.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';

	let env = $state<Env>(page.url.searchParams.get('env') === 'test' ? 'test' : 'prod');
	const flow = new Resource(
		() => api().busFlow(env),
		(f) => f.topics.length === 0,
	);
	const dead = new Resource(() => api().deadLetters());
	const both = {
		get refreshing() {
			return flow.refreshing || dead.refreshing;
		},
		get updatedAt() {
			return flow.updatedAt;
		},
		refresh: async () => {
			await Promise.all([flow.refresh(), dead.refresh()]);
		},
	};
	onMount(async () => {
		await both.refresh();
		if (page.url.searchParams.get('tab') === 'dead') {
			await tick();
			document.getElementById('dead')?.scrollIntoView?.({ block: 'start' });
		}
	});
	// The env the flow was last loaded for (not reactive): a Seg switch reloads the flow.
	let shownEnv: Env = page.url.searchParams.get('env') === 'test' ? 'test' : 'prod';
	$effect(() => {
		if (env !== shownEnv) {
			shownEnv = env;
			focus = null;
			// The topic drawer showed the other env's stream: close it rather than show its entries under this env.
			open = false;
			void flow.refresh();
		}
	});

	const admin = $derived(canWrite(session, 'operator'));
	/** A hop picked in the diagram narrows the topic table to its streams. */
	let focus = $state<FlowHop | null>(null);
	let busy = $state('');

	// ---- topic drawer -------------------------------------------------------------------------------------------
	let open = $state(false);
	let sel = $state<TopicInfo | null>(null);
	let entries = $state<Entry[]>([]);
	let entriesError = $state('');
	let entriesLoading = $state(false);
	const latestEntries = new Latest();

	async function openTopic(t: TopicInfo) {
		const ticket = latestEntries.next();
		sel = t;
		open = true;
		entries = [];
		entriesError = '';
		entriesLoading = true;
		try {
			// The drawer reads the stream of the env on screen: agent-api defaults to prod without `?env=`.
			const items = (await api().streamEntries(t.key, env, 20)).items;
			if (latestEntries.stale(ticket)) return;
			entries = items;
		} catch (e) {
			if (latestEntries.stale(ticket)) return;
			entriesError = (e as Error).message;
		} finally {
			if (!latestEntries.stale(ticket)) entriesLoading = false;
		}
	}

	// ---- dead-letter drawer --------------------------------------------------------------------------------------
	let dlOpen = $state(false);
	let dlSel = $state<DeadLetter | null>(null);

	async function claim(key: string, group: string) {
		let n = 0;
		busy = `claim:${key}:${group}`;
		const ok = await guard(
			async () => {
				n = (await api().claimStuck(key, group, env)).claimed;
			},
			() =>
				n === 0
					? `Nothing to take over for ${agentName(group)}: no entry has been idle 30 s or more.`
					: `Took over ${n} stuck entr${n === 1 ? 'y' : 'ies'} for ${agentName(group)}.`,
			'Not claimed',
		);
		busy = '';
		if (ok) await flow.refresh();
	}

	async function replay(d: DeadLetter) {
		busy = `replay:${d.entry_id}`;
		const ok = await guard(
			() => api().replayDeadLetter(d.entry_id),
			`Replayed ${d.envelope.correlation_id} to ${streamLabel(d.dead.stream)}.`,
			'Not replayed',
		);
		busy = '';
		if (ok) {
			dlOpen = false;
			await both.refresh();
		}
	}

	async function discard(d: DeadLetter) {
		busy = `discard:${d.entry_id}`;
		const ok = await guard(
			() => api().discardDeadLetter(d.entry_id),
			`Discarded the dead letter for ${d.envelope.correlation_id}.`,
			'Not discarded',
		);
		busy = '';
		if (ok) {
			dlOpen = false;
			await both.refresh();
		}
	}

	const hopHref = (h: FlowHop) =>
		h.link
			? h.link.href.startsWith('/admin') || h.link.href === '/chat'
				? h.link.href
				: jobPath(app.job, h.link.href)
			: null;
	const KIND: Record<FlowHop['kind'], string> = {
		channel: 'channel',
		gate: 'gate',
		agent: 'agent',
		topic: 'topic',
	};
	const STATUS_LABEL: Record<FlowHop['status'], string> = {
		ok: 'flowing',
		warn: 'falling behind',
		err: 'behind',
		idle: 'idle',
	};
	const hopBus = (h: FlowHop, topics: TopicInfo[]) =>
		topics.find((t) => h.streams.includes(t.key))?.bus ?? 'other';
	const pendingOf = (ts: TopicInfo[]) =>
		ts.reduce((a, t) => a + t.groups.reduce((b, g) => b + g.pending, 0), 0);
	const lagOf = (ts: TopicInfo[]) => ts.reduce((a, t) => Math.max(a, ...t.groups.map((g) => g.lag ?? 0)), 0);
	const deadOfHop = (h: FlowHop, ts: TopicInfo[]) =>
		ts.filter((t) => h.streams.includes(t.key)).reduce((a, t) => a + t.dead, 0);
</script>

<PageHead
	id="J24"
	sub="How a customer message travels through this job — channel, gates, agents, reply — and the topics on the bus that carry it between them. Find where messages pile up or die, and get them moving again."
>
	{#snippet actions()}
		<Seg
			label="Environment"
			items={[
				['prod', 'Production'],
				['test', 'Test environment'],
			]}
			bind:active={env}
		/>
		<Freshness resource={both} />
	{/snippet}
</PageHead>

{#if !admin}
	<p class="note">You can look but not change: claim, replay and discard need the tenant_admin role.</p>
{/if}

<LoadState
	resource={flow}
	empty="This job has not run on the bus yet. The message path appears after its first customer message."
>
	{#snippet children(f)}
		{@const behind = f.hops.filter((h) => h.status === 'warn' || h.status === 'err')}
		{@const inRate = f.hops[0]?.rate_per_min ?? 0}
		{@const outRate = f.hops.at(-1)?.rate_per_min ?? 0}
		{@const shown = focus ? f.topics.filter((t) => focus!.streams.includes(t.key)) : f.topics}

		<div class="j24-env" class:test={f.env === 'test'} data-testid="bus-env">
			<span class="j24-env-tag">{f.env === 'test' ? 'Test environment' : 'Production'}</span>
			<span>
				{#if f.env === 'test'}
					Agents here read the <strong>draft</strong> (unpublished changes) and call test-env systems; only staff
					test chats flow. Separate streams, nothing reaches a customer.
				{:else}
					Agents here read the <strong>live release</strong> and serve real customers.
				{/if}
			</span>
			<span class="mono j24-prefix" title="Every stream key of this environment starts with this prefix"
				>{f.prefix}…</span
			>
		</div>

		<p class="j24-verdict" data-testid="bus-verdict">
			{#if inRate === 0}
				<Status word="idle" label="Idle" /> Nothing is flowing: no consumer is running for this job.
			{:else}
				<strong>{rate(inRate)}</strong> messages in, <strong>{rate(outRate)}</strong> replies out (last {f.window_min}
				min).
				{#if behind.length}
					<span class="j24-warn"
						>{behind.length} hop{behind.length === 1 ? '' : 's'} falling behind: {behind
							.map((h) => h.label)
							.join(', ')}.</span
					>
				{:else}
					Every hop keeps up.
				{/if}
				{#if f.dead_letters}
					<a href="#dead" class="j24-deadlink"
						>{f.dead_letters} dead letter{f.dead_letters === 1 ? '' : 's'} to deal with ↓</a
					>
				{/if}
			{/if}
		</p>

		<!-- The message path: three phases, each a column of hops joined by arrows that carry the rate. -->
		<div class="j24-flow" data-testid="bus-flow" aria-label="Message path">
			{#each PHASES as ph, pi (ph.id)}
				{@const hops = f.hops.filter((h) => h.phase === ph.id)}
				<section class="j24-phase" aria-labelledby="ph-{ph.id}">
					<header class="j24-phase-head">
						<span class="j24-phase-n">{pi + 1}</span>
						<span id="ph-{ph.id}" class="j24-phase-label">{ph.label}</span>
						<span class="j24-phase-sub">{ph.sub}</span>
					</header>
					<ol class="j24-hops">
						{#each hops as h, i (h.id)}
							{@const ts = f.topics.filter((t) => h.streams.includes(t.key))}
							{@const dl = deadOfHop(h, f.topics)}
							{#if i > 0}
								<li class="j24-arrow" aria-hidden="true"><span>{rate(h.rate_per_min)}</span></li>
							{/if}
							<li
								class="j24-hop k-{h.kind} s-{h.status} b-{h.kind === 'topic' ? hopBus(h, f.topics) : 'none'}"
								class:loop={h.loop}
								class:on={focus?.id === h.id}
							>
								{#if h.kind === 'topic'}
									<button
										type="button"
										class="j24-topic"
										aria-pressed={focus?.id === h.id}
										title="Show these streams in the topic table"
										onclick={() => (focus = focus?.id === h.id ? null : h)}
									>
										<span class="j24-kind">{h.loop ? 'topics · round trip' : 'topic'}</span>
										<span class="j24-event mono">{h.label}</span>
										<span class="j24-does">{h.does}</span>
										<span class="j24-stats">
											<span>{ts.length} stream{ts.length === 1 ? '' : 's'}</span>
											<span>{rate(h.rate_per_min)}</span>
											<span class:warnc={pendingOf(ts) > 3}>{pendingOf(ts)} pending</span>
											<span class:warnc={lagOf(ts) > 5}>lag {lagOf(ts)}</span>
											{#if dl}<span class="deadc">{dl} dead</span>{/if}
										</span>
									</button>
								{:else}
									<div class="j24-node">
										<div class="j24-node-top">
											<span class="j24-kind">{KIND[h.kind]}</span>
											<Status word={h.status === 'err' ? 'err' : h.status} label={STATUS_LABEL[h.status]} />
										</div>
										<strong class="j24-node-label">{h.label}</strong>
										<span class="j24-does">{h.does}</span>
										<span class="j24-stats">
											<span class="j24-rate">{rate(h.rate_per_min)}</span>
											{#if h.p95_ms}<span>p95 {fmt.n(h.p95_ms)} ms</span>{/if}
											{#if h.agents.length > 1}<span>{h.agents.length} agents</span>{/if}
										</span>
										{#if hopHref(h)}<a class="j24-link" href={hopHref(h)}>{h.link?.label} →</a>{/if}
									</div>
								{/if}
								{#if h.exits?.length}
									<ul class="j24-exits">
										{#each h.exits as x (x.label)}
											<li class="x-{x.tone}">
												<span aria-hidden="true">↳</span>
												{rate(x.per_min) === '—' ? '0' : rate(x.per_min)}
												{x.label}
											</li>
										{/each}
									</ul>
								{/if}
							</li>
						{/each}
					</ol>
				</section>
			{/each}
		</div>
		<p class="j24-legend">
			<span><i class="sw sw-node"></i> channel / gate / agent — does the work</span>
			<span
				><i class="sw sw-topic"></i> topic — carries messages between them; pick one to see its streams</span
			>
			<span>↳ messages that leave the path here</span>
			<span>A message that fails {MAX_DELIVERIES}× goes to <span class="mono">bus:dead</span></span>
		</p>

		<!-- Topics behind the path. -->
		<Card
			cls="flush j24-topics"
			title={focus ? `Topics · ${focus.label}` : 'Topics'}
			sub="Who writes each stream, who reads it (one consumer group per agent), how deep it is and how long it keeps entries. Pending = delivered but not yet acknowledged."
		>
			{#snippet actions()}
				{#if focus}<button class="btn sm" type="button" onclick={() => (focus = null)}>Show all topics</button
					>{/if}
			{/snippet}
			<div class="tbl-wrap">
				<table class="tbl clickable" data-testid="streams">
					<thead>
						<tr>
							<th>Stream</th><th>Carries</th><th>Written by</th><th>Read by</th><th class="r">Rate</th><th
								class="r">Length</th
							><th>Retention</th><th>Health</th>
						</tr>
					</thead>
					<tbody>
						{#each shown as t (t.key)}
							{@const hs = groupsStatus(t.groups)}
							<tr onclick={() => openTopic(t)}>
								<td
									><button
										type="button"
										class="table-action cell-title"
										onclick={(e) => {
											e.stopPropagation();
											void openTopic(t);
										}}>{streamLabel(t.key)}</button
									><span class="cell-sub"
										><Chip text={busName(t.bus)} variant={busVariant(t.bus)} />
										<span class="mono">{t.key}</span></span
									></td
								>
								<td><span class="mono j24-events">{t.events.join(' · ') || '—'}</span></td>
								<td>{t.producers.map(procName).join(', ') || '—'}</td>
								<td>
									{#if t.groups.length === 0}
										<span class="muted">nobody — you replay or discard</span>
									{:else}
										<div class="chips">
											{#each t.groups as g (g.name)}
												<Chip
													text="{agentName(g.name)} · {g.pending} pending · lag {g.lag ?? '—'}"
													title={g.name}
													variant={groupHealth(g) === 'ok' ? 'muted' : groupHealth(g)}
												/>
											{/each}
										</div>
									{/if}
								</td>
								<td class="r">{rate(t.rate_per_min)}</td>
								<td class="r">{fmt.n(t.length)}</td>
								<td
									><span class="cell-sub">{env === 'test' ? 'reset with each test run' : retention(t)}</span
									></td
								>
								<td>
									{#if t.bus === 'dead'}
										<Status word={t.length ? 'err' : 'ok'} label={t.length ? 'Has entries' : 'Empty'} />
									{:else}
										<Status
											word={hs}
											label={hs === 'ok'
												? 'Healthy'
												: hs === 'idle'
													? 'No consumers'
													: hs === 'warn'
														? 'Falling behind'
														: 'Behind'}
										/>
										{#if t.dead}<span class="cell-sub deadc"
												>{t.dead} dead letter{t.dead === 1 ? '' : 's'}</span
											>{/if}
									{/if}
									{#if t.note}<span class="cell-sub">{t.note}</span>{/if}
								</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		</Card>
	{/snippet}
</LoadState>

<!-- Dead letters: what died, why, and what to do about it. -->
<section id="dead" class="j24-dead">
	<h2>Dead letters</h2>
	<p class="muted j24-dead-sub">
		A consumer gives up on a message after {MAX_DELIVERIES} deliveries, or at once when it cannot read it or it
		belongs to another scope, and parks it in <span class="mono">bus:dead</span>. Nothing reads that stream:
		each letter waits until you inspect it, then replay it onto its stream or discard it.
	</p>
	{#if env === 'test'}
		<p class="muted" data-testid="dead-test">
			No dead letters in the test environment: its streams are reset with each test run. Switch to Production
			to see and handle production dead letters.
		</p>
	{:else}
		<LoadState resource={dead} empty="No dead letters. Every message on this job was handled.">
			{#snippet children(p)}
				<div class="j24-reasons">
					{#each deadByReason(p.items) as r (r.code)}
						{@const adv = DEAD_ADVICE[r.code]}
						<div class="j24-reason t-{adv?.tone ?? 'muted'}">
							<span class="j24-reason-n">{r.n}</span>
							<span class="mono j24-reason-code">{r.code}</span>
							{#if adv}<span class="j24-reason-what">{adv.what}</span><strong class="j24-reason-then"
									>{adv.then}</strong
								>{/if}
						</div>
					{/each}
				</div>
				<Card cls="flush">
					<div class="tbl-wrap">
						<table class="tbl" data-testid="dead-letters">
							<thead>
								<tr><th>Case</th><th>Died on</th><th>Reason</th><th class="r">Deliveries</th><th></th></tr>
							</thead>
							<tbody>
								{#each p.items as d (d.entry_id)}
									{@const why = replayBlocked(admin, d.dead.reason_code)}
									{@const corr = d.envelope.correlation_id}
									<tr>
										<td
											><span class="cell-title mono">{corr}</span><span class="cell-sub"
												>{entryTime(d.entry_id)} · <span class="mono">{d.entry_id}</span></span
											></td
										>
										<td
											><span class="cell-title"
												>{streamLabel(d.dead.stream)} · {agentName(d.envelope.from)}</span
											><span class="cell-sub mono">{d.dead.stream}</span></td
										>
										<td>
											<Chip
												text={d.dead.reason_code}
												variant={d.dead.reason_code === 'scope_mismatch'
													? 'err'
													: d.dead.reason_code === 'gave_up'
														? 'warn'
														: 'muted'}
											/>
											<span class="cell-sub">{d.dead.reason}</span>
										</td>
										<td class="r">{d.dead.deliveries}</td>
										<td>
											<div class="actions">
												<button
													class="btn sm"
													type="button"
													onclick={() => {
														dlSel = d;
														dlOpen = true;
													}}>Inspect</button
												>
												<button
													class="btn sm"
													type="button"
													disabled={!!why || !!busy}
													title={why || `Put it back on ${d.dead.stream}`}
													onclick={() => replay(d)}>Replay</button
												>
												<button
													class="btn sm danger"
													type="button"
													disabled={!admin || !!busy}
													title={admin ? 'Delete it from bus:dead' : writeHint('operator')}
													onclick={() => discard(d)}>Discard</button
												>
											</div>
											{#if why}<span class="cell-sub">{why}</span>{/if}
										</td>
									</tr>
								{/each}
							</tbody>
						</table>
					</div>
				</Card>
			{/snippet}
		</LoadState>
	{/if}
</section>

<Drawer bind:open title={sel ? streamLabel(sel.key) : ''} wide>
	{#if sel}
		{@const t = sel}
		<KeyValues
			pairs={[
				['Key', `${env === 'test' ? 'test: ' : ''}${t.key}`],
				['Bus', busName(t.bus)],
				['Carries', t.events.join(', ') || '—'],
				['Written by', t.producers.map(procName).join(', ') || '—'],
				['Rate', rate(t.rate_per_min)],
				['Length', fmt.n(t.length)],
				['Retention', `${retention(t)} (trimmed at ~${fmt.n(t.maxlen)} entries)`],
			]}
		/>
		{#if t.note}<p class="muted">{t.note}</p>{/if}
		<h3>Read by</h3>
		{#if t.groups.length === 0}
			<p class="muted">No consumer group reads this stream. Entries stay until you replay or discard them.</p>
		{:else}
			<p class="muted">
				Each agent reads through its own consumer group and acknowledges an entry once handled. Claim stuck
				hands entries idle for 30 s or more to another consumer of the same group.
			</p>
			<div class="tbl-wrap">
				<table class="tbl dense">
					<thead>
						<tr
							><th>Group</th><th class="r">Consumers</th><th class="r">Pending</th><th class="r">Lag</th><th
								>Last delivered</th
							><th></th></tr
						>
					</thead>
					<tbody>
						{#each t.groups as g (g.name)}
							{@const why = claimBlocked(admin, g.pending)}
							<tr>
								<td
									><span class="cell-title">{agentName(g.name)}</span><span class="cell-sub mono"
										>{g.name}</span
									></td
								>
								<td class="r">{g.consumers}</td>
								<td class="r">{g.pending}</td>
								<td class="r">{g.lag ?? '—'}</td>
								<td>{entryTime(g.last_delivered_id) || '—'}</td>
								<td>
									<button
										class="btn sm"
										type="button"
										disabled={!!why || !!busy}
										title={why || 'Hand entries idle 30 s or more to another consumer'}
										onclick={() => claim(t.key, g.name)}>Claim stuck</button
									>
									{#if why}<span class="cell-sub">{why}</span>{/if}
								</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		{/if}
		<h3>Recent entries</h3>
		{#if env === 'test'}
			<p class="muted" data-testid="test-entries-note">
				Test entries belong to staff test chats: their cases are under <a
					href={jobPath(app.job, '/cases?env=test')}>Cases (env = test)</a
				>.
			</p>
		{/if}
		{#if entriesLoading}
			<p class="muted">Loading…</p>
		{:else if entriesError}
			<Unavailable reason={entriesError} retry={() => openTopic(t)} />
		{:else if entries.length === 0}
			<div class="empty">No entries on this stream in the retained window.</div>
		{:else}
			<ul class="list-plain entries">
				{#each entries as e (e.entry_id + e.envelope.id)}
					<li>
						<details>
							<summary>
								<span class="mono muted">{clock(new Date(e.envelope.published_at_ms).toISOString())}</span>
								<Chip text={e.envelope.topic} variant={busVariant(t.bus)} />
								<span>{agentName(e.envelope.from)}</span>
								<span class="cell-sub mono">{e.envelope.correlation_id} · {e.entry_id}</span>
							</summary>
							<JsonView value={e} />
						</details>
					</li>
				{/each}
			</ul>
		{/if}
	{/if}
</Drawer>

<Drawer bind:open={dlOpen} title={dlSel ? `Dead letter · ${dlSel.envelope.correlation_id}` : ''} wide>
	{#if dlSel}
		{@const d = dlSel}
		{@const adv = DEAD_ADVICE[d.dead.reason_code]}
		{@const why = replayBlocked(admin, d.dead.reason_code)}
		<KeyValues
			pairs={[
				['Died on', `${streamLabel(d.dead.stream)} (${d.dead.stream})`],
				['Consumer', agentName(d.envelope.from)],
				['Reason', d.dead.reason_code],
				['Deliveries', String(d.dead.deliveries)],
				['When', entryTime(d.entry_id)],
			]}
		/>
		<p>{d.dead.reason}</p>
		{#if adv}<p class="j24-advice t-{adv.tone}">{adv.what} <strong>{adv.then}</strong></p>{/if}
		<div class="actions">
			<button
				class="btn sm"
				type="button"
				disabled={!!why || !!busy}
				title={why || `Put it back on ${d.dead.stream}`}
				onclick={() => replay(d)}>Replay onto {streamLabel(d.dead.stream)}</button
			>
			<button
				class="btn sm danger"
				type="button"
				disabled={!admin || !!busy}
				title={admin ? 'Delete it from bus:dead' : writeHint('operator')}
				onclick={() => discard(d)}>Discard</button
			>
			<a class="btn sm" href={jobPath(app.job, `/cases/${encodeURIComponent(d.envelope.correlation_id)}`)}
				>Open case</a
			>
		</div>
		<h3>Original message</h3>
		<JsonView value={d.envelope.payload} />
	{/if}
</Drawer>

<style>
	.j24-env {
		display: flex;
		flex-wrap: wrap;
		gap: 6px 12px;
		align-items: baseline;
		padding: 10px 12px;
		border: 1px solid var(--line);
		border-left: 3px solid var(--ok);
		border-radius: var(--r);
		background: var(--panel);
		font-size: 13px;
		margin-bottom: 12px;
	}
	.j24-env.test {
		border-left-color: var(--warn);
		background: color-mix(in srgb, var(--warn) 6%, var(--panel));
	}
	.j24-env-tag {
		font: 600 11px/1.4 var(--cond);
		text-transform: uppercase;
		letter-spacing: 0.06em;
		color: var(--ok);
	}
	.j24-env.test .j24-env-tag {
		color: var(--warn);
	}
	.j24-prefix {
		margin-left: auto;
		color: var(--faint);
		font-size: 11.5px;
		overflow-wrap: anywhere;
	}
	.j24-verdict {
		font-size: 15px;
		margin: 0 0 14px;
		display: flex;
		flex-wrap: wrap;
		gap: 4px 8px;
		align-items: baseline;
	}
	.j24-warn {
		color: var(--warn);
	}
	.j24-deadlink {
		color: var(--dead);
		font-weight: 500;
	}

	/* ---- flow ---- */
	.j24-flow {
		display: grid;
		grid-template-columns: repeat(3, minmax(0, 1fr));
		gap: 14px;
		align-items: start;
	}
	.j24-phase {
		position: relative;
		background: var(--sunk);
		border-radius: var(--r);
		padding: 10px;
	}
	.j24-phase + .j24-phase::before {
		content: '→';
		position: absolute;
		left: -13px;
		top: 10px;
		color: var(--faint);
		font-size: 14px;
	}
	.j24-phase-head {
		display: flex;
		gap: 6px;
		align-items: baseline;
		flex-wrap: wrap;
		margin-bottom: 8px;
	}
	.j24-phase-n {
		display: inline-grid;
		place-items: center;
		width: 18px;
		height: 18px;
		border-radius: 50%;
		background: var(--fg);
		color: var(--panel);
		font: 600 11px/1 var(--mono);
	}
	.j24-phase-label {
		font: 600 13px/1.2 var(--cond);
		text-transform: uppercase;
		letter-spacing: 0.06em;
	}
	.j24-phase-sub {
		color: var(--muted);
		font-size: 12px;
	}
	.j24-hops {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.j24-arrow {
		display: flex;
		align-items: center;
		gap: 6px;
		height: 26px;
		margin-left: 18px;
		padding-left: 10px;
		border-left: 2px solid var(--line);
		font: 500 11px/1 var(--mono);
		color: var(--muted);
		position: relative;
	}
	.j24-arrow::after {
		content: '';
		position: absolute;
		left: -6px;
		bottom: -2px;
		border: 5px solid transparent;
		border-top-color: var(--line);
		border-bottom: 0;
	}
	.j24-node,
	.j24-topic {
		display: flex;
		flex-direction: column;
		gap: 3px;
		width: 100%;
		text-align: left;
		padding: 9px 11px;
		border-radius: var(--r);
		border: 1px solid var(--line);
		background: var(--panel);
		color: var(--fg);
		font: inherit;
	}
	.j24-hop.s-warn .j24-node {
		border-color: var(--warn);
		box-shadow: inset 3px 0 0 var(--warn);
	}
	.j24-hop.s-err .j24-node {
		border-color: var(--err);
		box-shadow: inset 3px 0 0 var(--err);
	}
	.j24-hop.k-gate .j24-node {
		border-style: dashed;
	}
	.j24-topic {
		cursor: pointer;
		border-radius: 18px;
		border-width: 1px 1px 1px 4px;
		background: color-mix(in srgb, var(--accent) 3%, var(--panel));
	}
	.j24-hop.b-customer .j24-topic {
		border-left-color: var(--cust);
	}
	.j24-hop.b-worker .j24-topic {
		border-left-color: var(--work);
	}
	.j24-hop.b-tool .j24-topic {
		border-left-color: var(--tool);
	}
	.j24-hop.s-warn .j24-topic,
	.j24-hop.s-err .j24-topic {
		border-top-color: var(--warn);
		border-right-color: var(--warn);
		border-bottom-color: var(--warn);
	}
	.j24-hop.loop .j24-topic {
		border-style: dashed;
		border-left-style: solid;
		margin-left: 18px;
		width: calc(100% - 18px);
	}
	.j24-hop.on .j24-topic,
	.j24-topic:hover {
		outline: 2px solid var(--accent);
		outline-offset: 1px;
	}
	.j24-node-top {
		display: flex;
		justify-content: space-between;
		gap: 6px;
		align-items: center;
	}
	.j24-kind {
		font: 600 10px/1.3 var(--cond);
		text-transform: uppercase;
		letter-spacing: 0.07em;
		color: var(--faint);
	}
	.j24-node-label {
		font-size: 14px;
	}
	.j24-event {
		font-size: 13px;
		font-weight: 600;
	}
	.j24-does {
		color: var(--muted);
		font-size: 12px;
		line-height: 1.4;
	}
	.j24-stats {
		display: flex;
		flex-wrap: wrap;
		gap: 2px 10px;
		font: 500 11.5px/1.5 var(--mono);
		color: var(--muted);
	}
	.j24-rate {
		color: var(--fg);
		font-weight: 600;
	}
	.warnc {
		color: var(--warn);
	}
	.deadc {
		color: var(--dead);
	}
	.j24-link {
		font-size: 12px;
		align-self: flex-start;
	}
	.j24-exits {
		list-style: none;
		margin: 4px 0 0 18px;
		padding: 0;
		font-size: 11.5px;
		color: var(--muted);
	}
	.j24-exits .x-warn {
		color: var(--warn);
	}
	.j24-exits .x-err {
		color: var(--err);
	}
	.j24-legend {
		display: flex;
		flex-wrap: wrap;
		gap: 4px 18px;
		font-size: 12px;
		color: var(--muted);
		margin: 10px 0 18px;
	}
	.sw {
		display: inline-block;
		width: 16px;
		height: 10px;
		vertical-align: -1px;
		border: 1px solid var(--line);
		background: var(--panel);
		border-radius: 2px;
	}
	.sw-topic {
		border-radius: 6px;
		border-left: 3px solid var(--cust);
	}
	.j24-events {
		font-size: 12px;
	}

	/* ---- dead letters ---- */
	.j24-dead {
		margin-top: 22px;
		scroll-margin-top: 70px;
	}
	.j24-dead h2 {
		font-size: 17px;
		margin: 0 0 4px;
	}
	.j24-dead-sub {
		max-width: 80ch;
		font-size: 13px;
		margin: 0 0 10px;
	}
	.j24-reasons {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(220px, 1fr));
		gap: 10px;
		margin-bottom: 12px;
	}
	.j24-reason {
		display: grid;
		grid-template-columns: auto 1fr;
		gap: 2px 8px;
		align-items: baseline;
		padding: 9px 11px;
		border: 1px solid var(--line);
		border-left: 3px solid var(--muted);
		border-radius: var(--r);
		background: var(--panel);
		font-size: 12.5px;
	}
	.j24-reason.t-err {
		border-left-color: var(--err);
	}
	.j24-reason.t-warn {
		border-left-color: var(--warn);
	}
	.j24-reason-n {
		font: 600 18px/1 var(--mono);
	}
	.j24-reason-code {
		font-weight: 600;
	}
	.j24-reason-what,
	.j24-reason-then {
		grid-column: 1 / -1;
	}
	.j24-reason-what {
		color: var(--muted);
	}
	.j24-advice {
		padding: 8px 10px;
		border-left: 3px solid var(--muted);
		background: var(--sunk);
		border-radius: var(--r);
	}
	.j24-advice.t-err {
		border-left-color: var(--err);
	}
	.j24-advice.t-warn {
		border-left-color: var(--warn);
	}
	.entries li {
		display: block;
	}
	.entries summary {
		cursor: pointer;
		display: flex;
		flex-wrap: wrap;
		gap: 4px 8px;
		align-items: baseline;
		padding: 6px 0;
	}
	.entries summary .cell-sub {
		flex-basis: 100%;
	}

	@media (max-width: 900px) {
		.j24-flow {
			grid-template-columns: minmax(0, 1fr);
		}
		.j24-phase + .j24-phase::before {
			content: '↓';
			left: 24px;
			top: -15px;
		}
		.j24-prefix {
			margin-left: 0;
		}
	}
</style>
