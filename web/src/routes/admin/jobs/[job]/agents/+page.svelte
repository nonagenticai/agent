<script lang="ts">
	// J23 Agents — the processes that run this job on the bus. Question this page answers: "is my job being served
	// right now, by which agents, on which config — and if one is behind, what do I do?". Leads with the verdict, then
	// the message path (each step = an agent role, 12 §2.1 / P15 vocabulary), then Production vs Test env config
	// (D54: Production serves the published release, the Test env serves the draft), then the numbers per agent.
	import { onMount } from 'svelte';
	import { api, app, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { writeAttrs } from '$lib/auth/roles';
	import { Resource } from '$lib/load.svelte';
	import { jobPath } from '$lib/scope';
	import { ApiError } from '$lib/api/errors';
	import type { AgentFleet, AgentServing, BusAgent } from '$lib/api/types/runtime';
	import { busVariant, streamLabel, when } from '$lib/runtime/labels';
	import {
		BEHIND,
		ROLES,
		STEPS,
		ago,
		configState,
		healthOf,
		heartbeatAge,
		plural,
		roleOf,
		stepOf,
		totalsOf,
		verdictOf,
		type BehindKind,
	} from '$lib/runtime/J23-agents';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Freshness from '$lib/ui/Freshness.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import KeyValues from '$lib/ui/KeyValues.svelte';
	import Kpi from '$lib/ui/Kpi.svelte';
	import MilestoneChip from '$lib/ui/MilestoneChip.svelte';
	import AgentFlow from './AgentFlow.svelte';

	const agents = new Resource(async () => {
		const [page, fleet] = await Promise.all([
			api().busAgents(),
			api()
				.agentFleet()
				.catch(() => null),
		]);
		return { ...page, fleet };
	});
	onMount(() => void agents.refresh());

	let sel = $state<BusAgent | null>(null);
	let open = $state(false);
	let busy = $state(false);
	function pick(a: BusAgent) {
		sel = a;
		open = true;
	}

	const KIND: Record<BusAgent['kind'], string> = {
		front: 'Front — talks to the customer',
		gate: 'Gate — classifies or checks',
		coordination: 'Coordination — runs the procedure',
		worker: 'Worker — prepares one kind of work',
		runner: 'Runner — calls the systems',
	};
	const STEP_LABEL = Object.fromEntries(STEPS.map((s) => [s.key, s.label]));
	const streamVariant = (s: string) =>
		busVariant(
			s.startsWith('bus:customer')
				? 'customer'
				: s.startsWith('bus:worker')
					? 'worker'
					: s.startsWith('bus:tool')
						? 'tool'
						: 'other',
		);
	const cfgText = (s: AgentServing | null | undefined, env: 'prod' | 'test') =>
		!s ? 'not running' : env === 'prod' ? `${s.release ?? '—'}` : `draft · rev ${s.rev ?? '—'}`;
	const CFG_LABEL = { current: 'current', catching_up: 're-reading', off: 'off' } as const;

	/** Hand stuck entries (idle ≥ 30 s) of this agent's group to a healthy instance, on every stream it reads. */
	async function claim(a: BusAgent) {
		busy = true;
		let n = 0;
		const ok = await guard(
			async () => {
				for (const st of a.streams) {
					try {
						// The fleet's pending counts are Production's (J23): claim on the prod bus.
						n += (await api().claimStuck(st, a.group, 'prod')).claimed;
					} catch (e) {
						if (!(e instanceof ApiError && e.status === 404)) throw e;
					}
				}
			},
			() =>
				n === 0
					? `Nothing to take over for ${a.name}: no entry has been idle 30 s or more.`
					: `Took over ${plural(n, 'stuck entry', 'stuck entries')} for ${a.name}.`,
			'Not claimed',
		);
		busy = false;
		if (ok) await agents.refresh();
	}
</script>

<PageHead
	id="J23"
	sub="The processes that run this job on the bus. Each agent runs as one agent role, reads its streams, does one step of the message path and hands the result on. Production runs the published release; the Test environment runs your draft."
>
	{#snippet actions()}
		<Freshness resource={agents} />
	{/snippet}
</PageHead>

<LoadState resource={agents}>
	{#snippet empty()}
		<div class="j23-empty">
			<b>No agents serve this job yet.</b>
			<span
				>Agents start when the job has a published release and is put on the bus. Until then customer messages
				are not answered.</span
			>
			<a class="btn sm" href={jobPath(app.job, '/release')}>Go to Release</a>
		</div>
	{/snippet}
	{#snippet children(p)}
		{@const items = p.items}
		{@const fleet = p.fleet}
		{@const v = verdictOf(items, fleet)}
		{@const t = totalsOf(items, fleet)}
		{@const prodOn = items.filter((a) => configState(a.prod, fleet!, 'prod') === 'current').length}
		{@const testCopies = items.filter((a) => a.test && a.test.consumers > 0)}
		{@const testOn = testCopies.filter((a) => configState(a.test, fleet!, 'test') === 'current').length}

		<!-- 1. Verdict: is the job being served? -->
		<section class="j23-verdict j23-v-{v.tone}" data-testid="agents-verdict">
			<div class="j23-v-text">
				<b>{v.title}</b>
				<span>{v.detail}</span>
				{#if v.attention.length}
					<ul class="j23-v-list">
						{#each v.attention as a (a.id)}
							{@const h = healthOf(a, fleet)}
							<li>
								<button type="button" class="linkish" onclick={() => pick(a)}>{a.name}</button>
								— {h.reasons.join(' ')}
								{#if h.kinds[0]}<span class="muted">{BEHIND[h.kinds[0]].short}</span>{/if}
							</li>
						{/each}
					</ul>
				{/if}
			</div>
			{#if v.attention.length}
				{@const first = v.attention[0]!}
				<div class="j23-v-actions">
					{#if first.pending}
						<button
							type="button"
							class="btn sm"
							disabled={busy || writeAttrs(session, 'operator').disabled}
							title={writeAttrs(session, 'operator').title}
							data-testid="claim-stuck"
							onclick={() => claim(first)}>Claim stuck entries</button
						>
					{/if}
					<a class="btn sm" href={jobPath(app.job, '/operation-calls')}>Operation calls</a>
					<a class="btn sm" href="#behind">What “behind” means</a>
				</div>
			{/if}
		</section>

		<!-- 2. The numbers that back the verdict -->
		<div class="kpis j23-kpis">
			<Kpi label="Keeping up" value="{t.keeping_up} of {t.agents}" note="agents" />
			<Kpi label="Instances running" value={String(t.instances)} note="processes in Production" />
			<Kpi label="Handled now" value="{t.per_min}/min" note="messages acknowledged" />
			<Kpi label="Waiting" value={String(t.waiting)} note="{t.unacked} picked up, not finished" />
			<Kpi label="Errors · 1 h" value={String(t.errors_1h)} note="retried, then dead-lettered" />
			<Kpi label="Restarts · 24 h" value={String(t.restarts_24h)} note="all instances" />
		</div>

		<!-- 3. The message path: steps, roles, agents -->
		<Card title="The message path">
			<p class="j23-lead">
				A customer message passes left to right. Every step is done by agents running as one
				<a href="/platform/agent-roles">agent role</a> — the role decides which operations they may call. Click
				an agent for its instances, config and what to do.
			</p>
			<AgentFlow {items} {fleet} onpick={pick} />
		</Card>

		<!-- 4. Which config is running where (D54) -->
		{#if fleet}
			<div class="grid g2 j23-envs" data-testid="agents-envs">
				<section class="j23-env">
					<header class="split">
						<b>Production</b>
						<span class="j23-pill">customers</span>
					</header>
					<div class="j23-env-cfg">
						{#if fleet.production.release}
							<span class="mono">{fleet.production.release.id}</span>
							<span class="muted">· published {when(fleet.production.released_at)}</span>
						{:else}
							<span class="muted">No published release.</span>
						{/if}
					</div>
					<div class="j23-meter" aria-label="{prodOn} of {items.length} agents serve it">
						{#each items as a (a.id)}
							<span
								class="j23-seg j23-seg-{configState(a.prod, fleet, 'prod')}"
								title="{a.name}: {cfgText(a.prod, 'prod')}"
							></span>
						{/each}
					</div>
					<span class="j23-env-sub"
						><b>{prodOn} of {items.length}</b> agents serve the published release.</span
					>
					{#if fleet.approved_waiting}
						<p class="j23-env-note">
							<span class="mono">{fleet.approved_waiting}</span> is approved but not applied. After you apply
							it, every agent switches within {fleet.follow_s} s; cases already running finish on the release they
							started on.
						</p>
					{/if}
					<a class="btn sm" href={jobPath(app.job, '/release')}>Release</a>
				</section>
				<section class="j23-env j23-env-test">
					<header class="split">
						<b>Test environment</b>
						<span class="j23-pill">test chat · draft</span>
					</header>
					<div class="j23-env-cfg">
						<span class="mono">draft · rev {fleet.test.rev}</span>
						<span class="muted"
							>· {fleet.test.unpublished
								? plural(fleet.test.unpublished, 'unpublished change')
								: 'same as Production (no unpublished changes)'}</span
						>
					</div>
					{#if testCopies.length}
						<div class="j23-meter" aria-label="{testOn} of {testCopies.length} test copies serve the draft">
							{#each items as a (a.id)}
								<span
									class="j23-seg j23-seg-{configState(a.test, fleet, 'test')}"
									title="{a.name}: {cfgText(a.test, 'test')}"
								></span>
							{/each}
						</div>
						<span class="j23-env-sub"
							><b>{testOn} of {testCopies.length}</b> test copies serve the draft{#if testOn < testCopies.length};
								{testCopies.length - testOn} re-reading (≤ {fleet.follow_s} s){/if}.</span
						>
					{:else}
						<span class="j23-env-sub muted">No test copies are running.</span>
					{/if}
					<p class="j23-env-note">
						Saved changes reach the test copies, never customers. Production changes only when you publish.
					</p>
					<span class="j23-env-links">
						<a class="btn sm" href="/chat">Test in test chat</a>
						<a class="btn sm" href={jobPath(app.job, '/release')}>Review &amp; publish</a>
					</span>
				</section>
			</div>
		{/if}

		<!-- 5. Every agent, the numbers -->
		<Card title="Every agent · {items.length}">
			<div class="tbl-wrap">
				<table class="tbl dense j23-tbl" data-testid="agents-table">
					<thead>
						<tr>
							<th>Agent</th>
							<th>Step · role</th>
							<th class="r">Inst.</th>
							<th class="r">/min</th>
							<th class="r">Waiting</th>
							<th class="r">Unfinished</th>
							<th class="r">Err 1 h</th>
							<th class="r">Restarts</th>
							<th>Heartbeat</th>
							<th>Production</th>
							<th>Test env</th>
						</tr>
					</thead>
					<tbody>
						{#each items as a (a.id)}
							{@const h = healthOf(a, fleet)}
							{@const ps = fleet ? configState(a.prod, fleet, 'prod') : 'off'}
							{@const ts = fleet ? configState(a.test, fleet, 'test') : 'off'}
							<tr class:j23-row-warn={h.word === 'warn'} class:j23-row-err={h.word === 'err'}>
								<td>
									<button type="button" class="linkish" onclick={() => pick(a)}>{a.name}</button>
									<span class="cell-sub j23-h-{h.word}">{h.label}</span>
								</td>
								<td>{STEP_LABEL[stepOf(a)]}<span class="cell-sub mono">{a.role}</span></td>
								<td class="r">{a.consumers}</td>
								<td class="r">{a.per_min ?? '—'}</td>
								<td class="r" class:j23-warn-num={a.lag > 5}>{a.lag}</td>
								<td class="r" class:j23-warn-num={a.pending > 3}
									>{a.pending}{#if a.oldest_pending_s}<span class="cell-sub"
											>oldest {a.oldest_pending_s} s</span
										>{/if}</td
								>
								<td class="r" class:j23-warn-num={(a.errors_1h ?? 0) > 0}>{a.errors_1h ?? '—'}</td>
								<td class="r">{a.restarts_24h ?? '—'}</td>
								<td>{fleet ? ago(heartbeatAge(a.last_heartbeat, fleet.as_of)) : when(a.last_heartbeat)}</td>
								<td class="mono"
									>{cfgText(a.prod, 'prod')}<span class="cell-sub j23-cfg-{ps}">{CFG_LABEL[ps]}</span></td
								>
								<td class="mono"
									>{cfgText(a.test, 'test')}<span class="cell-sub j23-cfg-{ts}">{CFG_LABEL[ts]}</span></td
								>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		</Card>

		<!-- 6. What "behind" means, and what to do -->
		<div id="behind">
			<Card title="What “behind” means — and what to do">
				<div class="j23-behind">
					{#each Object.entries(BEHIND) as [k, b] (k)}
						{@const n = items.filter((a) => healthOf(a, fleet).kinds.includes(k as BehindKind)).length}
						<section class="j23-b" class:on={n > 0}>
							<header class="split">
								<b>{b.label}</b>
								<span class="j23-b-n">{n ? plural(n, 'agent') + ' now' : 'none now'}</span>
							</header>
							<p>{b.means}</p>
							<p class="j23-todo"><span class="label">Do</span> {b.todo}</p>
						</section>
					{/each}
				</div>
			</Card>
		</div>
	{/snippet}
</LoadState>

<Drawer bind:open title={sel?.name ?? ''} wide>
	{#if sel}
		{@const fleet = agents.value.state === 'ok' ? agents.value.data.fleet : null}
		{@const h = healthOf(sel, fleet)}
		{@const role = ROLES[roleOf(sel)]}
		<div class="j23-d-head j23-v-{h.word === 'idle' ? 'idle' : h.word}">
			<b>{h.label}</b>
			{#if h.reasons.length}<span>{h.reasons.join(' ')}</span>{:else}<span
					>Picks up every message as it arrives.</span
				>{/if}
		</div>
		{#each h.kinds as k (k)}
			<p class="j23-todo"><span class="label">{BEHIND[k].label}</span> {BEHIND[k].todo}</p>
		{/each}
		{#if sel.pending}
			<button
				type="button"
				class="btn sm"
				disabled={busy || writeAttrs(session, 'operator').disabled}
				title={writeAttrs(session, 'operator').title}
				onclick={() => claim(sel!)}>Claim stuck entries</button
			>
		{/if}
		{#if sel.note}<p class="note">{sel.note}</p>{/if}

		<h3>Role: <span class="mono">{sel.role}</span></h3>
		<p class="j23-d-role">{role.grant} <span class="muted">Max effect: {role.effect}.</span></p>

		<KeyValues
			pairs={[
				['Step', `${STEP_LABEL[stepOf(sel)]} — ${KIND[sel.kind]}`],
				['Consumer group', sel.group],
				['Audit actor', `${sel.role}@${sel.group}`],
				['Waiting (lag)', `${sel.lag} — published, not picked up yet`],
				['Unfinished (pending)', `${sel.pending} — picked up, not acknowledged`],
				['Handled', `${sel.per_min ?? 0}/min now · ${sel.handled_1h ?? 0} in the last hour`],
				['Errors · restarts', `${sel.errors_1h ?? 0} in 1 h · ${sel.restarts_24h ?? 0} in 24 h`],
				['Build', `v${sel.version}`],
			]}
		/>

		<h3>Config it serves</h3>
		<div class="tbl-wrap">
			<table class="tbl dense">
				<thead
					><tr><th>Environment</th><th>Serving</th><th>Re-read</th><th class="r">Inst.</th><th></th></tr
					></thead
				>
				<tbody>
					{#each [['Production', sel.prod, 'prod'], ['Test env', sel.test, 'test']] as const as [label, s, env] (env)}
						{@const st = fleet ? configState(s, fleet, env) : 'off'}
						<tr>
							<td>{label}</td>
							<td class="mono">{cfgText(s, env)}</td>
							<td>{s && fleet ? ago(heartbeatAge(s.read_at, fleet.as_of)) : '—'}</td>
							<td class="r">{s?.consumers ?? 0}</td>
							<td class="j23-cfg-{st}">{CFG_LABEL[st]}</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>

		<h3>Instances · {sel.instances?.length ?? sel.consumers}</h3>
		{#if sel.instances?.length}
			<div class="tbl-wrap">
				<table class="tbl dense" data-testid="agent-instances">
					<thead
						><tr
							><th>Process</th><th>State</th><th>Started</th><th class="r">Restarts</th><th>Heartbeat</th></tr
						></thead
					>
					<tbody>
						{#each sel.instances as i (i.name)}
							<tr>
								<td class="mono">{i.name}</td>
								<td>{i.state}</td>
								<td>{when(i.started_at)}</td>
								<td class="r" class:j23-warn-num={i.restarts > 0}>{i.restarts}</td>
								<td>{fleet ? ago(heartbeatAge(i.last_heartbeat, fleet.as_of)) : when(i.last_heartbeat)}</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		{:else}
			<p class="muted">No instance details.</p>
		{/if}

		<h3>Streams it reads</h3>
		<div class="chips">
			{#each sel.streams as st (st)}<Chip
					text={streamLabel(st)}
					title={st}
					variant={streamVariant(st)}
				/>{/each}
		</div>
		<h3>Model routes</h3>
		{#if sel.model_routes.length === 0}
			<p class="muted">None. This agent is deterministic.</p>
		{:else}
			<div class="chips">
				{#each sel.model_routes as r (r)}<Chip text={r} variant="work" />{/each}
			</div>
		{/if}
		{#if sel.kind === 'runner'}<p><MilestoneChip milestone="M4" note="operation runner" /></p>{/if}
		<h3>Related</h3>
		<div class="chips">
			<a class="btn sm" href={jobPath(app.job, '/buses')}>Buses & topics</a>
			{#if sel.kind === 'gate'}<a class="btn sm" href={jobPath(app.job, '/gates')}>Gates</a>{/if}
			{#if sel.kind === 'runner' || sel.kind === 'worker'}<a
					class="btn sm"
					href={jobPath(app.job, '/operation-calls')}>Operation calls</a
				>{/if}
			{#if sel.model_routes.length}<a class="btn sm" href="/admin/providers">Providers</a>{/if}
			<a class="btn sm" href="/platform/agent-roles">Agent roles</a>
		</div>
	{/if}
</Drawer>

<style>
	.j23-empty {
		display: grid;
		gap: 8px;
		justify-items: start;
	}
	.j23-verdict {
		display: flex;
		flex-wrap: wrap;
		justify-content: space-between;
		gap: 12px;
		padding: 14px 16px;
		margin-bottom: 14px;
		border: 1px solid var(--line);
		border-left: 4px solid var(--ok);
		border-radius: 8px;
		background: var(--panel);
	}
	.j23-v-text {
		display: grid;
		gap: 4px;
		min-width: 0;
		flex: 1 1 420px;
	}
	.j23-v-text > b {
		font-size: 16px;
	}
	.j23-v-warn {
		border-left-color: var(--warn);
		background: color-mix(in srgb, var(--warn) 7%, var(--panel));
	}
	.j23-v-err {
		border-left-color: var(--err);
		background: color-mix(in srgb, var(--err) 7%, var(--panel));
	}
	.j23-v-idle {
		border-left-color: var(--faint);
	}
	.j23-v-list {
		margin: 4px 0 0;
		padding-left: 18px;
		font-size: 13px;
	}
	.j23-v-actions {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		align-items: flex-start;
	}
	.linkish {
		all: unset;
		cursor: pointer;
		color: var(--accent);
		font-weight: 600;
	}
	.linkish:hover {
		text-decoration: underline;
	}
	.linkish:focus-visible {
		outline: 2px solid var(--accent);
	}
	.j23-kpis {
		margin-bottom: 14px;
	}
	.j23-lead {
		margin: 0 0 12px;
		color: var(--muted);
		font-size: 13px;
	}
	.j23-envs {
		margin: 14px 0;
	}
	.j23-env {
		display: grid;
		gap: 8px;
		align-content: start;
		justify-items: start;
		padding: 14px 16px;
		border: 1px solid var(--line);
		border-radius: 8px;
		background: var(--panel);
		min-width: 0;
	}
	.j23-env header {
		width: 100%;
	}
	.j23-env-test {
		border-style: dashed;
	}
	.j23-pill {
		font-size: 11px;
		padding: 1px 8px;
		border-radius: 10px;
		background: var(--sunk);
		color: var(--muted);
	}
	.j23-env-cfg {
		font-size: 14px;
	}
	.j23-env-sub {
		font-size: 13px;
	}
	.j23-env-note {
		margin: 0;
		font-size: 12px;
		color: var(--muted);
	}
	.j23-env-links {
		display: flex;
		gap: 6px;
		flex-wrap: wrap;
	}
	.j23-meter {
		display: flex;
		gap: 3px;
		width: 100%;
	}
	.j23-seg {
		flex: 1;
		height: 8px;
		border-radius: 2px;
		background: var(--line);
	}
	.j23-seg-current {
		background: var(--ok);
	}
	.j23-seg-catching_up {
		background: var(--warn);
	}
	.j23-cfg-current {
		color: var(--ok);
	}
	.j23-cfg-catching_up {
		color: var(--warn);
		font-weight: 600;
	}
	.j23-cfg-off {
		color: var(--faint);
	}
	.j23-tbl td.mono {
		white-space: nowrap;
	}
	.j23-row-warn td:first-child {
		box-shadow: inset 3px 0 0 var(--warn);
	}
	.j23-row-err td:first-child {
		box-shadow: inset 3px 0 0 var(--err);
	}
	.j23-h-ok {
		color: var(--ok);
	}
	.j23-h-warn {
		color: var(--warn);
		font-weight: 600;
	}
	.j23-h-err {
		color: var(--err);
		font-weight: 600;
	}
	.j23-warn-num {
		color: var(--warn);
		font-weight: 600;
	}
	.j23-behind {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(220px, 1fr));
		gap: 10px;
	}
	.j23-b {
		padding: 10px 12px;
		border: 1px solid var(--line);
		border-radius: 8px;
		font-size: 13px;
	}
	.j23-b.on {
		border-color: var(--warn);
		background: color-mix(in srgb, var(--warn) 6%, var(--panel));
	}
	.j23-b p {
		margin: 6px 0 0;
		color: var(--muted);
	}
	.j23-b-n {
		font-size: 12px;
		color: var(--faint);
	}
	.j23-b.on .j23-b-n {
		color: var(--warn);
		font-weight: 600;
	}
	.j23-todo {
		margin: 6px 0;
		font-size: 13px;
	}
	.j23-b .j23-todo {
		color: var(--fg);
	}
	.j23-d-head {
		display: grid;
		gap: 2px;
		padding: 10px 12px;
		border-left: 4px solid var(--ok);
		border-radius: 6px;
		background: var(--sunk);
		margin-bottom: 8px;
	}
	.j23-d-role {
		margin: 0 0 10px;
		font-size: 13px;
	}
	@media (max-width: 640px) {
		.j23-envs {
			grid-template-columns: 1fr;
		}
		.j23-kpis {
			grid-template-columns: repeat(2, minmax(0, 1fr));
		}
	}
</style>
