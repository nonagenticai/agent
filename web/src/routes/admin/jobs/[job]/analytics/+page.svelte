<script lang="ts">
	// J2 Analytics — "is the agent doing its job, and did the last release make it better?". Leads with the
	// north-star verdict, then the release comparison, then where (journey stage / activity) and why (handover reason,
	// grouped by who can fix it) it falls short, then systems + gates, then volume and cost. Reads jobAnalytics,
	// operationStats (30 d) and jobGates (7 d). Read-only page: changes are made on the linked pages and reach
	// customers through the next release.
	import { onMount } from 'svelte';
	import { api, app } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import { fmt } from '$lib/format';
	import { jobPath } from '$lib/scope';
	import { when } from '$lib/runtime/labels';
	import { isEmptyWindow, kpiDelta, toSeries, totals } from './series';
	import {
		OWNER_LABEL,
		compareReleases,
		headline,
		reasonGroups,
		releaseMarkers,
		rollingRates,
		stageRows,
		systemRows,
		targetState,
		tokensPerCase,
		topGaps,
		typicalAnswerMs,
		type MetricCompare,
		type Verdict,
	} from '$lib/runtime/J2-insights';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Freshness from '$lib/ui/Freshness.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Seg from '$lib/ui/Seg.svelte';
	import LineChart from '$lib/ui/LineChart.svelte';
	import Meter from '$lib/ui/Meter.svelte';
	import Kpi from '$lib/ui/Kpi.svelte';
	import TrendChart from './TrendChart.svelte';
	import KnowledgePanel from './KnowledgePanel.svelte';

	let range = $state('30');
	const days = $derived(range === '7' ? 7 : 30);
	const data = new Resource(() => api().jobAnalytics(range === '7' ? 7 : 30), isEmptyWindow);
	const ops = new Resource(
		() => api().operationStats(),
		(p) => p.items.length === 0,
	);
	const gates = new Resource(
		() => api().jobGates(),
		(g) => g.questions.length === 0,
	);
	$effect(() => {
		void range;
		void data.refresh();
	});
	onMount(() => {
		void ops.refresh();
		void gates.refresh();
	});
	const href = (sub: string) => jobPath(app.job, sub);
	type GateQ = { key: string; stats: { below_threshold: number; verdicts_7d: number } };
	/** The five gates that most often land below threshold (asked again or handed over). */
	const worstGates = <Q extends GateQ>(qs: Q[]) =>
		qs
			.slice()
			.sort(
				(x, y) =>
					y.stats.below_threshold / y.stats.verdicts_7d - x.stats.below_threshold / x.stats.verdicts_7d,
			)
			.slice(0, 5);

	const TARGET_CHIP = {
		met: { text: 'On target', variant: 'ok' },
		close: { text: 'Close to target', variant: 'warn' },
		below: { text: 'Below target', variant: 'err' },
		unmeasured: { text: 'Not measured', variant: 'muted' },
	} as const;
	const VERDICT_CHIP: Record<Verdict, { text: string; variant: 'ok' | 'err' | 'muted' | 'warn' }> = {
		better: { text: 'Better', variant: 'ok' },
		worse: { text: 'Worse', variant: 'err' },
		flat: { text: 'No real change', variant: 'muted' },
		early: { text: 'Too early to call', variant: 'warn' },
		none: { text: 'No comparison', variant: 'muted' },
	};
	function show(m: MetricCompare, v: number | null): string {
		if (v == null) return '—';
		if (m.key === 'containment' || m.key === 'handover') return fmt.pct(v, 0);
		if (m.key === 'csat') return `${v.toFixed(1)} / 5`;
		if (m.key === 'p50') return fmt.ms(v);
		return fmt.n(v);
	}
	function change(m: MetricCompare): string {
		if (m.before == null || m.after == null) return '';
		const d = m.after - m.before;
		const sign = d > 0 ? '+' : d < 0 ? '−' : '±';
		const a = Math.abs(d);
		if (m.key === 'containment' || m.key === 'handover') return `${sign}${Math.round(a * 100)} pts`;
		if (m.key === 'csat') return `${sign}${a.toFixed(1)}`;
		if (m.key === 'p50') return `${sign}${fmt.ms(a)}`;
		return `${sign}${fmt.n(a)}`;
	}
</script>

<PageHead
	id="J2"
	sub="Is the agent doing its job, where does it fall short, and did the last release make it better?"
>
	{#snippet actions()}
		<Freshness resource={data} />
		<Seg
			label="Window"
			items={[
				['7', '7 days'],
				['30', '30 days'],
			]}
			bind:active={range}
		/>
	{/snippet}
</PageHead>

<LoadState resource={data}>
	{#snippet empty()}
		No conversations for this job yet. Once a release is applied and customers write in, this page answers:
		are we on target, did the release help, and why do people take over. Try the <a href="/chat">test chat</a>
		meanwhile, or check what is still missing on <a href={href('/readiness')}>Readiness</a>.
	{/snippet}
	{#snippet children(a)}
		{@const t = totals(a)}
		{@const ns = a.north_star}
		{@const state = targetState(ns.value, ns.target)}
		{@const cmp = compareReleases(a.release_windows ?? [], a.as_of ?? new Date().toISOString())}
		{@const stages = stageRows(a, ns.target)}
		{@const gaps = topGaps(stages)}
		{@const groups = reasonGroups(a)}
		{@const handovers = a.escalation_reasons.reduce((n, r) => n + r.count, 0)}
		{@const span = days === 7 ? 1 : 7}

		<!-- 1 · Are we on target? -->
		<section class="j2-hero" data-testid="j2-verdict" aria-label="North star">
			<div class="j2-ns">
				<div class="j2-ns-head">
					<span class="j2-eyebrow">North star · {ns.label}</span>
					<Chip text={TARGET_CHIP[state].text} variant={TARGET_CHIP[state].variant} />
				</div>
				<div class="j2-ns-row">
					<b class="j2-ns-v">{ns.value == null ? '—' : fmt.pct(ns.value, 0)}</b>
					{#if ns.value != null}
						<div class="j2-ns-track" aria-hidden="true">
							<span class="j2-ns-fill j2-{state}" style:width="{ns.value * 100}%"></span>
							{#if ns.target != null}
								<span class="j2-ns-tgt" style:left="{ns.target * 100}%"
									><i>target {fmt.pct(ns.target, 0)}</i></span
								>
							{/if}
						</div>
					{/if}
				</div>
				<p class="j2-ns-say">{headline(ns.value, ns.target, t.containmentPrev)}</p>
				<p class="j2-src muted">
					Production traffic only. Metric comes from the job template; the target is set on this job. Last {days}
					days vs the {days}
					before.
				</p>
			</div>
			<div class="j2-kpis">
				<Kpi
					label="Handed to staff"
					value={t.escalation == null ? null : fmt.pct(t.escalation, 0)}
					delta={kpiDelta(t.escalation, t.escalationPrev)}
					good="down"
					note="{fmt.n(handovers)} handovers"
				/>
				<Kpi
					label="Customer rating"
					value={a.csat.score == null ? null : `${a.csat.score.toFixed(1)} / 5`}
					delta={kpiDelta(a.csat.score, a.csat.previous)}
					unavailable={a.csat.score == null ? 'no survey answers yet' : ''}
					note={a.csat.responses ? `${fmt.n(a.csat.responses)} answers` : ''}
				/>
				<Kpi label="Typical answer time" value={fmt.ms(typicalAnswerMs(a))} note="case-weighted median" />
				<Kpi
					label="Tokens per case"
					value={fmt.n(tokensPerCase(a, t.cases))}
					note="{fmt.n(t.cases)} cases · {fmt.n(t.turns)} turns"
				/>
			</div>
		</section>

		<Card
			title="Is it moving the right way?"
			sub={span === 1
				? 'Daily share of cases. The dotted lines mark a release going live.'
				: '7-day rolling share of cases, so a quiet Sunday does not read as a dip. Dotted lines mark a release going live.'}
		>
			<TrendChart
				points={rollingRates(a.days, span)}
				target={ns.target}
				markers={releaseMarkers(a.days, a.release_windows ?? [])}
				{span}
			/>
		</Card>

		<!-- 2 · Did the last release help? -->
		<Card
			title="Did the last release help?"
			sub="Production changes only through a release: template updates and your refinements are drafted, tried in the test environment, then published together. Each release is measured from the moment it was applied; all figures on this page are production traffic."
		>
			{#snippet actions()}
				<a class="btn sm" href={href('/release')}>Releases</a>
			{/snippet}
			<div data-testid="j2-release">
				{#if !cmp}
					<div class="empty">No release is live yet, so there is nothing to compare.</div>
				{:else}
					<div class="j2-rel-say">
						<Chip text={VERDICT_CHIP[cmp.overall].text} variant={VERDICT_CHIP[cmp.overall].variant} />
						<span>
							<a class="mono" href={href(`/release/${encodeURIComponent(cmp.current.release_id)}`)}
								>{cmp.current.release_id}</a
							>
							has been live {cmp.liveDays < 2
								? `${cmp.liveDays.toFixed(1)} days`
								: `${Math.round(cmp.liveDays)} days`} ({fmt.n(cmp.current.cases)} cases){#if cmp.previous},
								compared with
								<a class="mono" href={href(`/release/${encodeURIComponent(cmp.previous.release_id)}`)}
									>{cmp.previous.release_id}</a
								>
								({fmt.n(cmp.previous.cases)} cases, {when(cmp.previous.applied_at)} – {when(
									cmp.previous.until,
								)}){/if}.
							{#if cmp.overall === 'early'}
								Early read: a verdict needs at least 100 cases on each release.
							{/if}
						</span>
					</div>
					{#if cmp.previous}
						<div class="tbl-wrap">
							<table class="tbl dense j2-rel-tbl">
								<thead>
									<tr>
										<th>Measure</th>
										<th class="r"><span class="mono">{cmp.previous.release_id.replace('cfg-', '')}</span></th>
										<th class="r"><span class="mono">{cmp.current.release_id.replace('cfg-', '')}</span></th>
										<th class="r">Change</th>
										<th>Verdict</th>
									</tr>
								</thead>
								<tbody>
									{#each cmp.metrics as m (m.key)}
										<tr>
											<td>{m.label}</td>
											<td class="r muted">{show(m, m.before)}</td>
											<td class="r"><b>{show(m, m.after)}</b></td>
											<td class="r mono">{change(m)}</td>
											<td>
												<Chip
													text={m.key === 'csat' && m.verdict === 'early'
														? 'Too few answers'
														: VERDICT_CHIP[m.verdict].text}
													variant={VERDICT_CHIP[m.verdict].variant}
												/>
											</td>
										</tr>
									{/each}
								</tbody>
							</table>
						</div>
					{:else}
						<p class="muted">
							Only one release has carried traffic so far. The comparison starts once the next release is
							applied.
						</p>
					{/if}
					<div class="grid g2 j2-rel-more">
						<div>
							<h4 class="j2-h4">What this release changed</h4>
							<ul class="j2-bullets">
								{#each cmp.current.changes as c (c)}<li>{c}</li>{/each}
							</ul>
							{#if cmp.current.refinements?.length}
								<h4 class="j2-h4 j2-h4-gap">Your refinements in this release</h4>
								<ul class="j2-bullets" data-testid="j2-refinements">
									{#each cmp.current.refinements as r (r.rev)}
										<li><span class="mono muted">rev {r.rev}</span> {r.summary}</li>
									{/each}
								</ul>
								<a class="j2-small" href={href('/refinements')}>All refinements →</a>
							{/if}
						</div>
						{#if cmp.current.moves.length}
							<div>
								<h4 class="j2-h4">Activities that moved most</h4>
								<ul class="list-plain">
									{#each cmp.current.moves as mv (mv.activity)}
										<li>
											<a href={href(`/journey/${encodeURIComponent(mv.activity)}`)}
												>{mv.activity.split(':')[1]?.replaceAll('_', ' ')}</a
											>
											<span class="mono">
												{fmt.pct(mv.from, 0)} → <b>{fmt.pct(mv.to, 0)}</b>
												<span class={mv.to >= mv.from ? 's-ok' : 's-err'}
													>{mv.to >= mv.from ? '▲' : '▼'}
													{Math.round(Math.abs(mv.to - mv.from) * 100)} pts</span
												>
												<span class="muted">· {mv.cases} cases</span>
											</span>
										</li>
									{/each}
								</ul>
							</div>
						{/if}
					</div>
				{/if}
				{#if a.test_env || cmp?.waiting}
					{@const te = a.test_env}
					<div class="j2-next" data-testid="j2-test-env">
						<h4 class="j2-h4">Not in production yet</h4>
						{#if cmp?.waiting}
							<p class="j2-waiting">
								<Chip text="Approved, not applied" variant="accent" />
								<span>
									<a class="mono" href={href(`/release/${encodeURIComponent(cmp.waiting.release_id)}`)}
										>{cmp.waiting.release_id}</a
									>
									publishes the draft it was cut from. Apply it on the
									<a href={href('/release')}>Release page</a> to start measuring it here.
								</span>
							</p>
						{/if}
						{#if te}
							<p class="j2-waiting">
								<Chip text="Test environment" variant="work" />
								<span>
									<b>{fmt.n(te.chats)}</b> staff test chats against draft rev {te.draft_rev} since
									{when(te.since)}: {fmt.pct(te.chats ? te.contained / te.chats : null, 0)} resolved, {fmt.n(
										te.escalated,
									)} handed over. Not counted anywhere else on this page.
									<a href={href('/cases?env=test')}>Test cases</a> · <a href="/chat">Test chat</a>
								</span>
							</p>
							{#if te.trying.length}
								<ul class="j2-bullets j2-trying">
									{#each te.trying as t (t)}<li>{t}</li>{/each}
								</ul>
							{/if}
						{/if}
					</div>
				{/if}
			</div>
		</Card>

		<!-- 3 · Where does it fall short? -->
		<Card
			title="Where along the journey does it fall short?"
			sub="Share of cases resolved without a person, per journey stage and activity. “Gap” = handovers you would have avoided at the {ns.target ==
			null
				? '70%'
				: fmt.pct(ns.target, 0)} target."
			cls="flush"
		>
			{#if gaps.length}
				<div class="j2-gaps" data-testid="j2-gaps">
					<span class="j2-eyebrow">Biggest wins</span>
					{#each gaps as g, i (g.activity)}
						<a class="j2-gap" href={href(`/journey/${encodeURIComponent(g.activity)}`)}>
							<span class="j2-gap-n">{i + 1}</span>
							<span
								><b>{g.activity.split(':')[0]} · {g.name}</b><span class="cell-sub"
									>{fmt.pct(g.rate, 0)} resolved · {g.gap} handovers to win back</span
								></span
							>
						</a>
					{/each}
				</div>
			{/if}
			<div class="tbl-wrap">
				<table class="tbl dense j2-act">
					<thead>
						<tr>
							<th>Stage · activity</th>
							<th class="r">Cases</th>
							<th>Resolved without a person</th>
							<th class="r">To staff</th>
							<th class="r">Gap</th>
							<th class="r">Median time</th>
						</tr>
					</thead>
					{#each stages as s (s.stage)}
						<tbody>
							<tr class="j2-stage">
								<th scope="rowgroup">{s.label}</th>
								<td class="r">{fmt.n(s.cases)}</td>
								<td><span class="j2-rate mono">{fmt.pct(s.rate, 0)}</span></td>
								<td class="r">{fmt.n(s.escalated)}</td>
								<td></td>
								<td></td>
							</tr>
							{#each s.activities as r (r.activity)}
								<tr>
									<td class="j2-actname"
										><a href={href(`/journey/${encodeURIComponent(r.activity)}`)}>{r.name}</a><span
											class="cell-sub mono">{r.activity}</span
										></td
									>
									<td class="r">{fmt.n(r.cases)}</td>
									<td>
										<span class="j2-meter"
											><Meter
												value={r.rate}
												cls={ns.target != null && r.rate >= ns.target
													? 's-ok'
													: r.rate >= 0.4
														? 's-warn'
														: 's-err'}
											/><span class="mono">{fmt.pct(r.rate, 0)}</span></span
										>
									</td>
									<td class="r">{fmt.n(r.escalated)}</td>
									<td class="r">{r.gap ? fmt.n(r.gap) : '—'}</td>
									<td class="r">{fmt.ms(r.p50_ms)}</td>
								</tr>
							{/each}
						</tbody>
					{/each}
				</table>
			</div>
		</Card>

		<!-- 4 · Why do people take over? -->
		<Card
			title="Why do people take over?"
			sub="{fmt.n(handovers)} handovers in the last {days} days, grouped by who can fix the cause."
		>
			{#snippet actions()}
				<a class="btn sm" href={href('/handover')}>Handover queue</a>
				<a class="btn sm" href={href('/cases?status=handover')}>Cases with staff</a>
			{/snippet}
			{#if groups.length === 0}
				<div class="empty">No handovers in this window.</div>
			{:else}
				<div class="j2-owners" data-testid="j2-reasons">
					{#each groups as g (g.owner)}
						<div class="j2-owner j2-o-{g.owner}">
							<div class="j2-owner-head">
								<b>{OWNER_LABEL[g.owner].title}</b>
								<span class="mono">{fmt.n(g.count)} · {fmt.pct(g.share, 0)}</span>
							</div>
							<p class="muted j2-owner-hint">{OWNER_LABEL[g.owner].hint}</p>
							<ul class="j2-reasons">
								{#each g.reasons as r (r.reason)}
									<li>
										<span class="j2-r-label">{r.label}</span>
										<span class="j2-r-bar" aria-hidden="true"
											><span style:width="{handovers ? (r.count / handovers) * 100 : 0}%"></span></span
										>
										<span class="mono j2-r-n">{fmt.n(r.count)}</span>
										<a class="j2-r-fix" href={href(r.page)}>{r.pageLabel} →</a>
									</li>
								{/each}
							</ul>
						</div>
					{/each}
				</div>
			{/if}
		</Card>

		<!-- 4b · Does it answer from knowledge? -->
		<KnowledgePanel job={app.job} {days} />

		<!-- 5 · Systems and gates -->
		<div class="grid g2">
			<Card title="Are the systems holding up?" sub="Operation calls per backend system, last 30 days.">
				{#snippet actions()}
					<a class="btn sm" href={href('/operation-calls')}>Operation calls</a>
				{/snippet}
				<LoadState resource={ops} empty="No operation calls yet.">
					{#snippet children(p)}
						<ul class="list-plain j2-sys" data-testid="j2-systems">
							{#each systemRows(p.items) as s (s.system)}
								<li>
									<span>
										<b class="mono">{s.system}</b>
										<span class="cell-sub"
											>{fmt.n(s.calls)} calls{s.denied
												? ` · ${fmt.n(s.denied)} refused by policy`
												: ''}{s.worst ? ` · worst: ${s.worst.op} (${s.worst.errors} errors)` : ''}</span
										>
									</span>
									<span class="j2-sys-r">
										<Chip
											text="{fmt.pct(s.success, 1)} ok"
											variant={s.success >= 0.99 ? 'ok' : s.success >= 0.97 ? 'warn' : 'err'}
										/>
										<span class="mono muted">p95 {fmt.ms(s.p95_ms)}</span>
									</span>
								</li>
							{/each}
						</ul>
						<p class="muted j2-foot">
							Refused-by-policy calls are the guard working, not errors. Fix failing systems on <a
								href="/admin/connections/systems">Systems</a
							>.
						</p>
					{/snippet}
				</LoadState>
			</Card>
			<Card
				title="Are the gates deciding well?"
				sub="Pre- and post-answer checks, last 7 days. Below threshold = the agent asked again or handed over."
			>
				{#snippet actions()}
					<a class="btn sm" href={href('/gates')}>Gates</a>
				{/snippet}
				<LoadState resource={gates} empty="This job asks no gates yet.">
					{#snippet children(g)}
						{@const qs = g.questions.filter((q) => q.stats.verdicts_7d > 0)}
						{@const v = qs.reduce((n, q) => n + q.stats.verdicts_7d, 0)}
						{@const below = qs.reduce((n, q) => n + q.stats.below_threshold, 0)}
						{@const closed = qs.reduce((n, q) => n + q.stats.fail_closed, 0)}
						{@const open = qs.reduce((n, q) => n + q.stats.fail_open, 0)}
						{#if !qs.length}
							<div class="empty">No gate verdicts in the last 7 days.</div>
						{:else}
							<div class="j2-gate-sum" data-testid="j2-gates">
								<span><b>{fmt.n(v)}</b> verdicts</span>
								<span><b>{fmt.pct(v ? below / v : 0, 0)}</b> below threshold</span>
								<span><b>{fmt.n(closed)}</b> failed closed · <b>{fmt.n(open)}</b> failed open</span>
							</div>
							<ul class="list-plain">
								{#each worstGates(qs) as q (q.key)}
									<li>
										<span
											>{q.question}<span class="cell-sub mono"
												>{q.key}{q.safety ? ' · safety' : ''} · {q.layer}</span
											></span
										>
										<span class="mono"
											>{fmt.pct(q.stats.below_threshold / q.stats.verdicts_7d, 0)} below · {fmt.n(
												q.stats.verdicts_7d,
											)}</span
										>
									</li>
								{/each}
							</ul>
						{/if}
					{/snippet}
				</LoadState>
			</Card>
		</div>

		<!-- 6 · Volume and cost -->
		<Card title="Volume and cost" sub="How much work the agent did and who paid for the compute.">
			{#snippet actions()}
				<a class="btn sm" href="/admin/usage">Tenant usage</a>
			{/snippet}
			<div class="kpis">
				<Kpi
					label="Cases"
					value={fmt.n(t.cases)}
					delta={kpiDelta(t.cases, a.previous.cases)}
					note="vs previous {days} days"
				/>
				<Kpi label="Turns" value={fmt.n(t.turns)} delta={kpiDelta(t.turns, a.previous.turns)} />
				<Kpi label="Tokens" value={fmt.k(a.tokens)} note="{fmt.n(tokensPerCase(a, t.cases))} per case" />
				<Kpi
					label="Funded by"
					value={a.funding.local_gpu >= 0.995
						? 'Internal GPU'
						: a.funding.tenant_byo >= 0.995
							? 'Your own provider'
							: 'Mixed'}
					note="internal {fmt.pct(a.funding.local_gpu, 0)} · platform external {fmt.pct(
						a.funding.platform_external,
						0,
					)} · your key {fmt.pct(a.funding.tenant_byo, 0)}"
				/>
			</div>
			<details class="j2-vol">
				<summary>Cases per day by outcome</summary>
				<LineChart
					labels={a.days.map((d) => d.day.slice(5))}
					series={toSeries(a)}
					ariaLabel="Cases per day by outcome"
				/>
			</details>
		</Card>
	{/snippet}
</LoadState>

<style>
	.j2-hero {
		display: grid;
		grid-template-columns: minmax(0, 1.2fr) minmax(0, 1fr);
		gap: 16px;
		margin-bottom: 16px;
	}
	.j2-ns {
		background: var(--panel);
		border: 1px solid var(--line);
		border-radius: var(--r);
		padding: 16px 18px;
		display: grid;
		gap: 10px;
		align-content: start;
	}
	.j2-ns-head {
		display: flex;
		gap: 10px;
		align-items: center;
		justify-content: space-between;
		flex-wrap: wrap;
	}
	.j2-eyebrow {
		font: 600 11px/1.2 var(--cond);
		letter-spacing: 0.06em;
		text-transform: uppercase;
		color: var(--muted);
	}
	.j2-ns-row {
		display: flex;
		gap: 18px;
		align-items: center;
	}
	.j2-ns-v {
		font-size: 44px;
		line-height: 1;
		font-weight: 600;
	}
	.j2-ns-track {
		position: relative;
		flex: 1;
		height: 10px;
		border-radius: 999px;
		background: var(--sunk);
		margin-top: 14px;
	}
	.j2-ns-fill {
		position: absolute;
		inset: 0 auto 0 0;
		border-radius: 999px;
		background: var(--accent);
	}
	.j2-ns-fill.j2-met {
		background: var(--ok);
	}
	.j2-ns-fill.j2-below {
		background: var(--err);
	}
	.j2-ns-fill.j2-close {
		background: var(--warn);
	}
	.j2-ns-tgt {
		position: absolute;
		top: -6px;
		bottom: -6px;
		width: 0;
		border-left: 2px dashed var(--fg);
	}
	.j2-ns-tgt i {
		position: absolute;
		bottom: calc(100% + 2px);
		transform: translateX(-50%);
		font-style: normal;
		font-size: 11px;
		color: var(--muted);
		white-space: nowrap;
	}
	.j2-ns-say {
		margin: 0;
		font-size: 15px;
	}
	.j2-src {
		margin: 0;
		font-size: 12px;
	}
	.j2-kpis {
		display: grid;
		grid-template-columns: repeat(2, minmax(0, 1fr));
		gap: 12px;
	}
	.j2-rel-say {
		display: flex;
		gap: 10px;
		align-items: baseline;
		margin-bottom: 12px;
		line-height: 1.5;
	}
	.j2-rel-tbl {
		margin-bottom: 12px;
	}
	.j2-rel-more {
		margin-top: 4px;
	}
	.j2-h4 {
		margin: 0 0 6px;
		font-size: 13px;
	}
	.j2-h4-gap {
		margin-top: 12px;
	}
	.j2-small {
		display: inline-block;
		margin-top: 6px;
		font-size: 12px;
	}
	.j2-bullets {
		margin: 0;
		padding-left: 18px;
		display: grid;
		gap: 4px;
		font-size: 13px;
	}
	.j2-next {
		margin-top: 14px;
		padding-top: 12px;
		border-top: 1px solid var(--line-2);
	}
	.j2-trying {
		margin: 6px 0 0;
		font-size: 12.5px;
	}
	.j2-waiting {
		display: flex;
		gap: 10px;
		align-items: baseline;
		margin: 6px 0 0;
		font-size: 13px;
	}
	.j2-gaps {
		display: flex;
		flex-wrap: wrap;
		gap: 10px;
		align-items: center;
		padding: 12px 16px;
		border-bottom: 1px solid var(--line-2);
	}
	.j2-gap {
		display: flex;
		gap: 10px;
		align-items: center;
		padding: 8px 12px;
		border: 1px solid var(--line);
		border-radius: var(--r);
		background: var(--sunk);
		color: inherit;
		text-decoration: none;
		font-size: 13px;
		text-transform: none;
	}
	.j2-gap:hover {
		border-color: var(--accent);
	}
	.j2-gap b {
		text-transform: capitalize;
	}
	.j2-gap-n {
		display: inline-grid;
		place-items: center;
		width: 22px;
		height: 22px;
		border-radius: 50%;
		background: var(--panel);
		border: 1px solid var(--line);
		font: 600 12px/1 var(--mono);
	}
	.j2-stage th,
	.j2-stage td {
		background: var(--sunk);
		font-weight: 600;
	}
	.j2-stage th {
		text-align: left;
	}
	.j2-actname {
		padding-left: 22px;
	}
	.j2-actname a {
		text-transform: capitalize;
	}
	.j2-meter {
		display: inline-flex;
		gap: 8px;
		align-items: center;
		min-width: 140px;
	}
	.j2-owners {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(260px, 1fr));
		gap: 12px;
	}
	.j2-owner {
		border: 1px solid var(--line);
		border-left: 3px solid var(--line);
		border-radius: var(--r);
		padding: 12px 14px;
	}
	.j2-o-customer {
		border-left-color: var(--cust);
	}
	.j2-o-agent {
		border-left-color: var(--work);
	}
	.j2-o-system {
		border-left-color: var(--err);
	}
	.j2-o-safety {
		border-left-color: var(--dead);
	}
	.j2-owner-head {
		display: flex;
		justify-content: space-between;
		gap: 8px;
	}
	.j2-owner-hint {
		margin: 2px 0 8px;
		font-size: 12px;
	}
	.j2-reasons {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 8px;
	}
	.j2-reasons li {
		display: grid;
		grid-template-columns: minmax(0, 1fr) auto;
		gap: 2px 10px;
		font-size: 13px;
	}
	.j2-r-bar {
		grid-column: 1 / -1;
		grid-row: 2;
		height: 6px;
		border-radius: 999px;
		background: var(--sunk);
		overflow: hidden;
	}
	.j2-r-bar span {
		display: block;
		height: 100%;
		border-radius: 999px;
		background: var(--muted);
	}
	.j2-r-n {
		grid-column: 2;
		grid-row: 1;
	}
	.j2-r-fix {
		grid-column: 1 / -1;
		grid-row: 3;
		font-size: 12px;
		white-space: nowrap;
	}
	.j2-sys-r {
		display: inline-flex;
		gap: 10px;
		align-items: center;
	}
	.j2-foot {
		font-size: 12px;
		margin: 8px 0 0;
	}
	.j2-gate-sum {
		display: flex;
		flex-wrap: wrap;
		gap: 6px 18px;
		font-size: 13px;
		margin-bottom: 6px;
	}
	.j2-vol {
		margin-top: 12px;
	}
	.j2-vol summary {
		cursor: pointer;
		color: var(--muted);
		font-size: 13px;
		margin-bottom: 8px;
	}
	@media (max-width: 860px) {
		.j2-hero {
			grid-template-columns: minmax(0, 1fr);
		}
		.j2-ns-v {
			font-size: 36px;
		}
		.j2-rel-say,
		.j2-waiting {
			flex-direction: column;
			gap: 6px;
		}
	}
</style>
