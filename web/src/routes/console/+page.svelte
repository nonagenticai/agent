<script lang="ts">
	// L1 Console — the live operator console. Default view (Operator.svelte): the job's conversations as they
	// happen across channels (Telegram = production on the live release, staff test chat = test env on the draft,
	// D54), what the qualifier / procedure / post-gate did on each turn, which conversations need a person now
	// (unknown outcomes, failures, handovers), each linking into the workspace. Second tab: the raw bus stream (from
	// cb:web/src/routes/console/+page.svelte@45ee128, admin SSE `GET /api/jobs/{job}/events`, K38) with lanes,
	// filters and super_admin fault injection on the test stub (07 §3a). Pause and Drop connection act on both.
	import { superAdminAttrs } from '$lib/auth/roles';
	import './console.css';
	import { onDestroy, onMount, tick } from 'svelte';
	import { api, app, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { Resource } from '$lib/load.svelte';
	import type { BusEvent, LiveAgent } from '$lib/api/types/live';
	import { createFeed, FEED_LABEL, type FeedStatus } from '$lib/live/feed';
	import { ConsoleStore, MAX_PER_LANE, busKey, direction, type Lane } from '$lib/live/console/store.svelte';
	import {
		activity,
		agentCards,
		clock,
		contextOf,
		elapsed,
		envOf,
		eventOf,
		isBad,
		matches,
		plain,
		summary,
		type ConsoleFilter,
	} from '$lib/live/console/bus';
	import PageHead from '$lib/ui/PageHead.svelte';
	import Tabs from '$lib/ui/Tabs.svelte';
	import AgentStrip from './AgentStrip.svelte';
	import Operator from './Operator.svelte';
	import './L1-operator.css';
	import Inspector from './Inspector.svelte';

	const store = new ConsoleStore();
	let agentViews = $state<LiveAgent[] | null>(null);
	let selectedAgent = $state<string | null>('orchestrator');
	let hoverCid = $state<string | null>(null);
	let feedStatus = $state<FeedStatus>('stopped');
	let paused = $state(false);
	let held = $state<BusEvent[]>([]);
	type Tab = 'conversations' | 'bus';
	let tab = $state<Tab>('conversations');
	let operator = $state<ReturnType<typeof Operator> | null>(null);
	let convStatus = $state<FeedStatus>('stopped');
	/** One status for both streams: the worse of the two. */
	const streamStatus = $derived<FeedStatus>(feedStatusOf(feedStatus, convStatus));
	function feedStatusOf(a: FeedStatus, b: FeedStatus): FeedStatus {
		for (const s of ['retrying', 'connecting', 'stopped'] as const) if (a === s || b === s) return s;
		return 'open';
	}
	function dropAll() {
		feed.drop();
		operator?.drop();
	}
	let filter = $state<ConsoleFilter>({ caseId: '', env: '', text: '', badOnly: false });
	/** Narrow layouts (container < 700px) show one panel at a time; ignored by the CSS on wider layouts. */
	type View = 'controls' | 'activity' | 'details';
	let view = $state<View>('activity');
	const VIEWS: [View, string][] = [
		['controls', 'Controls'],
		['activity', 'Activity'],
		['details', 'Details'],
	];
	let now = $state(Date.now());
	const nowTimer = setInterval(() => (now = Date.now()), 1000);
	let follow = $state<Record<Lane, boolean>>({ customer: true, op: true, result: true, dead: true });
	let seenAt = $state<Record<Lane, number>>({ customer: 0, op: 0, result: 0, dead: 0 });
	let listEls: Partial<Record<Lane, HTMLElement>> = {};

	let cursor = 0;
	const feed = createFeed<BusEvent>({
		pull: async () => {
			const r = await api().busBacklog({ after_ms: cursor });
			agentViews = r.agents;
			const last = r.items.at(-1);
			if (last) cursor = last.envelope.published_at_ms;
			return r.items;
		},
		onEvents: (events) => {
			if (paused) held = [...held, ...events];
			else for (const e of events) store.addBus(e);
		},
		onStatus: (s) => (feedStatus = s),
		tickMs: 900,
		connectMs: 500,
		retryMs: 2600,
	});

	const LANES = [
		{
			id: 'customer',
			title: 'customer-bus',
			cls: '--cust',
			flow: 'bus:customer:{area} · only fully qualified requests',
			legend: ['request.qualified → orchestrator', 'request.resolved → interaction'],
			empty: 'Nothing yet. A request appears once the interaction agent has every required field.',
		},
		{
			id: 'op',
			title: 'op-bus',
			cls: '--work',
			flow: 'bus:op · calls to the operation runner (ids + codes only)',
			legend: ['op.call → runner', 'retries carry attempt 2'],
			empty: 'Operation calls from the orchestrator go down to the runner.',
		},
		{
			id: 'result',
			title: 'op-results',
			cls: '--tool',
			flow: 'bus:op:results · outcome code per call',
			legend: ['runs on the routed instance', 'op.result → orchestrator'],
			empty: 'Each call comes back with ok, an error code or unknown_outcome.',
		},
		{
			id: 'dead',
			title: 'dead-letters',
			cls: '--err',
			flow: 'bus:dead · entries no consumer could process',
			legend: ['retries exhausted / scope mismatch', 'inspect in Job › Buses'],
			empty: 'No dead letters.',
		},
	] as const;

	const events = $derived(store.all);
	const cards = $derived(agentCards(events, agentViews));
	const agentCard = $derived(store.selected ? null : (cards.find((c) => c.id === selectedAgent) ?? null));
	const act = $derived(activity(events, filter.caseId));
	const lanes = $derived(LANES.filter((l) => l.id !== 'dead' || store.dead.length > 0));
	/** Env per case, from its request.qualified (results inherit it). */
	const envOfCase = $derived(
		new Map(
			events.filter((e) => envOf(e)).map((e) => [e.envelope.correlation_id, envOf(e)] as [string, string]),
		),
	);
	const filtering = $derived(!!(filter.caseId || filter.env || filter.text.trim() || filter.badOnly));
	const shown = (lane: Lane) =>
		filtering ? store[lane].filter((e) => matches(e, filter, envOfCase)) : store[lane];
	const fresh = (lane: Lane) => Math.max(0, store.added[lane] - seenAt[lane]);
	const badCount = $derived(events.filter(isBad).length);

	function toEnd(lane: Lane) {
		const el = listEls[lane];
		if (el) el.scrollTop = el.scrollHeight;
	}
	function onScroll(lane: Lane) {
		const el = listEls[lane];
		if (!el) return;
		follow[lane] = el.scrollHeight - el.scrollTop - el.clientHeight < 24;
		if (follow[lane]) seenAt[lane] = store.added[lane];
	}
	function jump(lane: Lane) {
		follow[lane] = true;
		seenAt[lane] = store.added[lane];
		toEnd(lane);
	}
	$effect(() => {
		for (const lane of ['customer', 'op', 'result', 'dead'] as const) {
			const n = store.added[lane];
			void filter.caseId;
			if (follow[lane]) {
				seenAt[lane] = n;
				void tick().then(() => toEnd(lane));
			}
		}
	});

	function togglePause() {
		if (paused) {
			for (const e of held) store.addBus(e);
			held = [];
		}
		paused = !paused;
	}
	function clearView() {
		store.reset();
		held = [];
		follow = { customer: true, op: true, result: true, dead: true };
		seenAt = { customer: 0, op: 0, result: 0, dead: 0 };
	}
	function selectAgent(id: string) {
		store.selected = null;
		selectedAgent = id;
		view = 'details';
	}
	onMount(() => {
		feed.start();
	});
	onDestroy(() => {
		feed.stop();
		clearInterval(nowTimer);
	});
</script>

<PageHead
	id="L1"
	sub="What is happening right now in {app.job}, and where you need to step in. Production conversations run the live release; staff test chats run the draft."
>
	{#snippet actions()}
		<span
			class="chip"
			class:ok={streamStatus === 'open' && !paused}
			class:warn={streamStatus === 'retrying' || streamStatus === 'connecting' || paused}
			role="status"
			data-testid="feed-status">{paused ? 'paused' : FEED_LABEL[streamStatus]}</span
		>
		<button class="btn sm" type="button" onclick={togglePause} aria-pressed={paused}
			>{paused ? `Resume${held.length ? ` (${held.length})` : ''}` : 'Pause'}</button
		>
		<button
			class="btn sm"
			type="button"
			onclick={dropAll}
			disabled={streamStatus !== 'open'}
			title="Simulate a dropped connection">Drop connection</button
		>
	{/snippet}
</PageHead>

{#if streamStatus === 'retrying'}
	<div class="callout gap" role="status">
		<strong>Reconnecting to the event stream…</strong>
		<span>Entries published meanwhile are not lost: the stream resumes from the last entry seen.</span>
	</div>
{/if}

<div class="l1-tabs">
	<Tabs
		items={[
			['conversations', 'Conversations'],
			['bus', 'Bus stream'],
		]}
		bind:active={tab}
		label="Console view"
	/>
	<span class="l1-tabs-note"
		>{tab === 'bus'
			? 'Every bus entry, for engineers: operation calls show ids and outcome codes only (K50).'
			: 'Turns, agents and step-in. The raw bus entries are under Bus stream.'}</span
	>
</div>

<div hidden={tab !== 'conversations'}>
	<Operator bind:this={operator} {paused} onstatus={(s) => (convStatus = s)} />
</div>

{#if tab === 'bus'}
	<div class="console-host">
		<div class="console app" data-view={view}>
			<div class="ctabs" data-pane="nav">
				<Tabs items={VIEWS} bind:active={view} label="Console view" />
			</div>
			<section class="panel side" aria-label="Console controls" data-pane="controls">
				<header>
					<span class="label">{app.job} · live</span>
					<h1>Bus console</h1>
					<p class="sub">
						{events.length} entries in view · {store.caseIds.length} cases · {badCount} failure{badCount === 1
							? ''
							: 's'}
					</p>
				</header>
				<section aria-label="Stream">
					<span class="label">Stream</span>
					<div class="row">
						<button class="btn sm" type="button" onclick={togglePause}>{paused ? 'Resume' : 'Pause'}</button>
						<button class="btn sm" type="button" onclick={clearView}>Clear view</button>
					</div>
					{#if paused}
						<span class="held" role="status"
							>Paused · {held.length} new entr{held.length === 1 ? 'y' : 'ies'} held. Resume to show them.</span
						>
					{/if}
				</section>
				<section aria-label="Filters">
					<span class="label">Filter</span>
					<select class="input" bind:value={filter.caseId} aria-label="Filter by case">
						<option value="">All cases ({store.caseIds.length})</option>
						{#each store.caseIds as c (c)}<option value={c}>{c}</option>{/each}
					</select>
					<select class="input" bind:value={filter.env} aria-label="Filter by env">
						<option value="">prod and test</option>
						<option value="prod">prod only (Telegram)</option>
						<option value="test">test only (staff chat)</option>
					</select>
					<input
						class="input"
						bind:value={filter.text}
						placeholder="Operation, code or activity"
						aria-label="Filter by text"
					/>
					<label class="chk"><input type="checkbox" bind:checked={filter.badOnly} /> Failures only</label>
					{#if filtering}
						<button
							class="btn sm"
							type="button"
							onclick={() => (filter = { caseId: '', env: '', text: '', badOnly: false })}
							>Clear filters</button
						>
					{/if}
				</section>
				<section aria-label="Legend">
					<span class="label">Legend</span>
					<p class="k50">
						↓ forward · ↑ return path · <span class="ack">pending</span> →
						<span class="ack done">acked</span>. Op envelopes are mirrored with ids and outcome codes only
						(K50); inputs and customer data never reach the console.
					</p>
				</section>
			</section>

			<div class="main">
				<div class="tracebar" data-pane="activity">
					<span>Agent status is derived from the entries seen here and consumer-group metrics.</span>
					{#if store.trimmed > 0}
						<span role="status"
							>Showing the last {MAX_PER_LANE} entries per lane; {store.trimmed} older not shown (open the case
							for its full trace).</span
						>
					{/if}
				</div>
				<section class="panel activity" aria-label="Case activity" data-pane="activity">
					<span class="label">Observed from bus entries</span>
					{#if act}
						<dl>
							<div>
								<dt>Case</dt>
								<dd class="mono">
									<a href="/admin/jobs/{encodeURIComponent(app.job)}/cases/{encodeURIComponent(act.caseId)}"
										>{act.caseId}</a
									>{filter.caseId ? '' : ' (latest seen)'}
								</dd>
							</div>
							<div>
								<dt>Last observed step</dt>
								<dd>
									{act.step || 'No entries for this case yet.'}{#if act.stepAt !== null}
										<span class="time">{clock(act.stepAt)}</span>{/if}
								</dd>
							</div>
							<div>
								<dt>Outcome</dt>
								<dd class:bad={act.bad}>
									{#if act.outcome}{#each act.outcome.split('.') as part, i (i)}{i ? '.' : ''}<wbr
											/>{part}{/each}{:else}No resolution observed yet.{/if}
								</dd>
							</div>
							<div>
								<dt>{act.endedAt !== null ? 'Took' : 'Elapsed'}</dt>
								<dd class="mono">
									{act.startedAt === null
										? '—'
										: act.endedAt !== null
											? `${elapsed(act.endedAt - act.startedAt)} (first to resolution)`
											: `${elapsed(now - act.startedAt)} since the first entry`}
								</dd>
							</div>
						</dl>
					{:else if feedStatus === 'connecting'}
						<p role="status">Connecting to the event stream…</p>
					{:else}
						<p>No case on the bus yet. Activity appears once a request is fully qualified.</p>
					{/if}
				</section>
				<div class="strip" data-pane="activity">
					<AgentStrip {cards} selected={store.selected ? null : selectedAgent} onselect={selectAgent} />
				</div>
				<div class="buses">
					{#each lanes as lane (lane.id)}
						{@const list = shown(lane.id)}
						<section
							class="panel bus"
							class:dead={lane.id === 'dead'}
							style="--c:var({lane.cls})"
							aria-label={lane.title}
							data-pane="activity"
							data-testid="lane-{lane.id}"
						>
							<header>
								<h2>
									{lane.title}
									<span class="count">{filtering ? `${list.length}/` : ''}{store[lane.id].length}</span>
								</h2>
								<span class="flow">{lane.flow}</span>
								<span class="legend"
									><span><b>↓</b> {lane.legend[0]}</span><span><b>↑</b> {lane.legend[1]}</span></span
								>
							</header>
							<div class="list" bind:this={listEls[lane.id]} onscroll={() => onScroll(lane.id)}>
								{#if store[lane.id].length === 0}<div class="empty">{lane.empty}</div>
								{:else if filtering && list.length === 0}<div class="empty">
										No entries match the filter in this lane.
									</div>{/if}
								{#each list as e (busKey(e))}
									{@const dir = direction(e.envelope.topic)}
									{@const ack = store.ackOf(e)}
									{@const ev = eventOf(e.envelope.topic)}
									<button
										type="button"
										class="msg fresh {dir}"
										class:bad={isBad(e)}
										class:rel={hoverCid !== null && hoverCid === e.envelope.correlation_id}
										class:sel={store.selected !== null && busKey(store.selected) === busKey(e)}
										data-testid="bus-entry"
										onclick={() => {
											store.selected = e;
											view = 'details';
										}}
										onmouseenter={() => (hoverCid = e.envelope.correlation_id)}
										onmouseleave={() => (hoverCid = null)}
									>
										<span class="r1"
											><span class="topic"
												><b class="dir" title={dir === 'down' ? 'forward' : 'return path'}
													>{dir === 'down' ? '↓' : '↑'}</b
												>{#if ev}<span class="ctx">{contextOf(e.envelope.topic)}</span>{ev}{:else}{e.envelope
														.topic}{/if}</span
											><span class="time">{clock(e.envelope.published_at_ms)}</span></span
										>
										{#if plain(e)}<span class="plain">{plain(e)}</span>{/if}
										<span class="route"
											>{e.envelope.from} → {e.consumer_group ?? '—'} · {e.envelope.correlation_id}</span
										>
										{#if summary(e)}<span class="sum">{summary(e)}</span>{/if}
										<span class="meta"
											><span class="skey">{e.stream}</span><span>{e.entry_id}</span>{#if ack}<span
													class="ack"
													class:done={ack === 'acked'}
													title={ack === 'acked'
														? `XACKed by ${e.consumer_group}`
														: `In ${e.consumer_group}: not acknowledged yet`}>{ack}</span
												>{/if}</span
										>
									</button>
								{/each}
								{#if fresh(lane.id) > 0 && !follow[lane.id]}
									<button type="button" class="newev" onclick={() => jump(lane.id)}
										>{fresh(lane.id)} new entr{fresh(lane.id) === 1 ? 'y' : 'ies'} ↓</button
									>
								{/if}
							</div>
						</section>
					{/each}
					<div class="ipane" data-pane="details">
						<Inspector
							event={store.selected}
							agent={agentCard}
							ack={store.selected ? store.ackOf(store.selected) : null}
						/>
					</div>
				</div>
			</div>
		</div>
	</div>
{/if}
