<script lang="ts">
	// T5 Usage: what this tenant's agents consumed, what drove it and who pays for the compute. Turns and LLM tokens
	// come from usage_ledger (one row per qualifier call, 03 §metering) with the funding split local_gpu /
	// platform_external / tenant_byo. Counts, not money (K52). D56: generator runs are not a separate bill — their tokens
	// are this tenant's usage (activity `generator`) on the same funding lanes, shown per source next to the turns.
	import { goto } from '$app/navigation';
	import { api } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import { fmt } from '$lib/format';
	import type { Funding, UsageGroup, UsageReport, UsageWindow } from '$lib/api/types/tenant';
	import { day, pct, when } from '$lib/tenant/ui';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Kpi from '$lib/ui/Kpi.svelte';
	import Seg from '$lib/ui/Seg.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import LineChart from '$lib/ui/LineChart.svelte';
	import BarsH from '$lib/ui/BarsH.svelte';
	import Freshness from '$lib/ui/Freshness.svelte';
	import MeteringModel from './MeteringModel.svelte';
	import FundingLanes from './FundingLanes.svelte';
	import { delta, driverHeadline, generatorTokens, isEmptyUsage, tokensPerTurn } from './usage';

	let windowKey = $state('30');
	let group = $state<string>('job');
	const res = new Resource<UsageReport>(
		() => api().tenantUsage({ window: Number(windowKey) as UsageWindow, group: group as UsageGroup }),
		isEmptyUsage,
	);
	// Runs once on mount and again whenever the window or grouping changes.
	$effect(() => {
		void [windowKey, group];
		void res.refresh();
	});

	const FUNDING: [Funding, string][] = [
		['local_gpu', 'Internal GPU'],
		['platform_external', 'Platform external'],
		['tenant_byo', 'Your key'],
	];
	const GROUP_LABEL: Record<string, string> = {
		job: 'Job',
		user: 'Person / channel',
		project: 'Project',
		provider: 'Provider · model',
	};
	const activityHref = (job: string, activity: string): string | null =>
		activity.includes(':') ? `/admin/jobs/${job}/journey/${encodeURIComponent(activity)}` : null;
</script>

<PageHead
	id="T5"
	sub="What your agents and generator runs consumed, what drove it, and which funding lane it ran on."
>
	{#snippet actions()}
		<Seg
			label="Window"
			items={[
				['7', '7 days'],
				['30', '30 days'],
				['90', '90 days'],
			]}
			bind:active={windowKey}
		/>
		<Freshness resource={res} />
	{/snippet}
</PageHead>

<MeteringModel />

<LoadState resource={res}>
	{#snippet empty()}
		<div class="empty t5-empty" data-testid="usage-empty">
			<strong>No usage in the last {windowKey} days.</strong>
			<p>
				Turns are metered once a job is released and serving customers, and attributed to this tenant once its
				licence is on (04 §6a). Check that each job has a release and an LLM provider it can use.
			</p>
			<p class="t5-links"><a href="/admin/jobs">Jobs →</a> <a href="/admin/providers">Providers →</a></p>
		</div>
	{/snippet}
	{#snippet children(r)}
		{@const staff = r.staff_daily ?? r.daily.map(() => 0)}
		{@const staffTotal = staff.reduce((a, b) => a + b, 0)}
		{@const drivers = r.drivers ?? []}
		{@const gen = r.generator ?? []}
		{@const genTok = generatorTokens(r)}
		{@const headline = driverHeadline(drivers, r.totals)}
		<div class="kpis g4 grid">
			<Kpi
				label="Turns"
				value={fmt.n(r.totals.turns)}
				delta={delta(r.totals.turns, r.previous?.turns)}
				note={`${day(r.from)} → ${day(r.to)}`}
			/>
			<Kpi
				label="LLM tokens"
				value={fmt.k(r.totals.tokens)}
				delta={delta(r.totals.tokens, r.previous?.tokens)}
				note={`${fmt.n(r.totals.tokens)} tokens`}
			/>
			<Kpi
				label="Tokens per turn"
				value={fmt.n(tokensPerTurn(r.totals))}
				note="one qualifier call per turn"
			/>
			<Kpi
				label="Generator tokens"
				value={fmt.k(genTok)}
				note={gen.length
					? `${gen.length} run${gen.length > 1 ? 's' : ''} · same lanes as turns`
					: 'no generator runs'}
			/>
		</div>

		<div class="grid g-main">
			<Card title="Turns per day" sub="End customers vs your staff's test chats.">
				<LineChart
					ariaLabel="Turns per day"
					labels={r.daily.map((d) => day(d.day))}
					series={[
						{
							key: 'customers',
							label: 'Customers',
							cls: 's-accent',
							values: r.daily.map((d, i) => d.turns - (staff[i] ?? 0)),
						},
						{ key: 'staff', label: 'Staff test chats', cls: 's-tool', values: staff },
					]}
					height={200}
				/>
				<p class="note">
					{pct(r.totals.turns - staffTotal, r.totals.turns)} of turns came from customers. Staff test chats are
					metered the same way.
				</p>
			</Card>
			<Card
				title="Funding lanes"
				sub="Every token — turn or generator run — runs in exactly one funding lane."
			>
				<FundingLanes report={r} />
			</Card>
		</div>

		<Card title="What drives the tokens" sub="Per qualified activity. Long conversations cost more per turn.">
			{#if drivers.length}
				{#if headline}<p class="t5-headline">{headline}</p>{/if}
				<div class="t5-bars">
					<BarsH
						ariaLabel="Tokens per activity"
						items={drivers.map((d) => ({
							label: d.label,
							parts: [{ v: d.tokens, cls: 's-accent', label: 'Tokens' }],
							right: `${fmt.k(d.tokens)} · ${fmt.n(tokensPerTurn(d))}/turn`,
						}))}
						onClick={(i) => {
							const d = drivers[i]!;
							const href = activityHref(d.job, d.activity);
							if (href) void goto(href);
						}}
					/>
				</div>
				<p class="note">Select an activity to open it in the job's journey.</p>
			{:else}
				<p class="muted">No qualified turns in this window.</p>
			{/if}
		</Card>

		<Card cls="flush" title="Breakdown" sub="The ledger, grouped. Shares are of this window's turns.">
			{#snippet actions()}
				<Seg
					label="Group by"
					items={[
						['job', 'Job'],
						['user', 'Person'],
						['project', 'Project'],
						['provider', 'Provider'],
					]}
					bind:active={group}
				/>
			{/snippet}
			<div class="tbl-wrap">
				<table class="tbl" data-testid="usage-rows">
					<thead>
						<tr
							><th>{GROUP_LABEL[r.group]}</th><th class="r">Turns</th><th class="r">Share</th><th class="r"
								>Tokens</th
							><th class="r">Per turn</th><th>Funding</th></tr
						>
					</thead>
					<tbody>
						{#each r.rows as row (row.key)}
							<tr>
								<td>{row.label}<span class="cell-sub mono">{row.key}</span></td>
								<td class="r">{fmt.n(row.turns)}</td>
								<td class="r">{pct(row.turns, r.totals.turns)}</td>
								<td class="r">{fmt.n(row.tokens)}</td>
								<td class="r">{fmt.n(tokensPerTurn(row))}</td>
								<td>
									{#each FUNDING.filter(([k]) => row.funding[k] > 0) as [k, label] (k)}<span class="chip ok"
											>{label} {pct(row.funding[k], row.tokens)}</span
										>{/each}
								</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
			{#if r.group === 'project'}
				<p class="note t5-pad">
					Agent has no projects of its own: its usage is recorded under the tenant as the project.
				</p>
			{:else if r.group === 'user'}
				<p class="note t5-pad">
					End customers have no account; their turns are grouped per channel. Staff turns are test chats.
				</p>
			{/if}
		</Card>

		<Card
			title="Beyond conversation turns"
			sub="Knowledge work is counted by the knowledge services; generator runs are LLM usage like turns."
		>
			<div class="t5-beyond">
				<div class="t5-tile">
					<span class="label">Knowledge searches</span>
					<span class="kv-val">{fmt.n(r.knowledge?.searches ?? 0)}</span>
					<p>
						knowledge.search calls during turns, counted by strata_rag.
						<a href="/admin/knowledge/stores">Stores →</a>
					</p>
				</div>
				<div class="t5-tile">
					<span class="label">Ingest jobs</span>
					<span class="kv-val">{fmt.n(r.knowledge?.ingest_jobs ?? 0)}</span>
					<p>
						{fmt.n(r.knowledge?.documents ?? 0)} documents · {fmt.n(r.knowledge?.pages ?? 0)} pages
						{#if r.knowledge?.ingest_failed}· <span class="s-err">{r.knowledge.ingest_failed} failed</span
							>{/if}
						<a href="/admin/knowledge/jobs">Ingest jobs →</a>
					</p>
				</div>
				<div class="t5-tile t5-gen">
					<span class="label">Generator runs · your usage</span>
					{#if gen.length}
						<ul class="list-plain" data-testid="usage-generator">
							{#each gen as g (g.id)}
								<li>
									<span
										><a href="/admin/jobs/{g.job}/discovery">Run #{g.id}</a>
										<span class="cell-sub">{g.output} · {when(g.started_at)}</span></span
									>
									<span class="mono nowrap"
										>{fmt.k(g.tokens)} tok · {FUNDING.find(([k]) => k === g.funding)?.[1]}</span
									>
								</li>
							{/each}
						</ul>
						<p>
							Recorded under the generator activity for this tenant's project — part of your usage, not a
							bill.
						</p>
					{:else}
						<span class="kv-val">0</span>
						<p>No generator runs you started in this window.</p>
					{/if}
				</div>
			</div>
		</Card>

		<Callout kind="info" title="Counts, not prices"
			><span
				>Turns and tokens are counted, not priced: the billing model is out of scope here (K52) and
				license-authz remains the billing meter. Generator runs are no exception: their tokens are metered as
				your usage on the same funding lanes, never as a separate bill.</span
			></Callout
		>
		{#if !r.attributed}
			<Callout kind="gap" title="Not attributed yet"
				><span>Rows are attributed to this tenant once its licence is on.</span></Callout
			>
		{/if}
	{/snippet}
</LoadState>

<style>
	/* the "View data" table under the bars: long activity names wrap instead of widening the card on phones */
	.t5-bars :global(details .tbl :is(th, td)) {
		overflow-wrap: anywhere;
		white-space: normal;
	}
	.t5-headline {
		margin: 0 0 12px;
		font-size: 14px;
	}
	.t5-pad {
		padding: 0 16px 12px;
	}
	.t5-empty p {
		margin: 6px 0 0;
	}
	.t5-links a {
		margin-right: 14px;
	}
	.t5-beyond {
		display: grid;
		grid-template-columns: 1fr 1fr 2fr;
		gap: 12px;
	}
	.t5-tile {
		background: var(--sunk);
		border-radius: var(--r);
		padding: 12px;
		display: flex;
		flex-direction: column;
		gap: 4px;
		min-width: 0;
	}
	.t5-tile p {
		margin: 0;
		font-size: 12px;
		color: var(--muted);
	}
	.t5-gen li {
		display: flex;
		justify-content: space-between;
		gap: 8px;
		flex-wrap: wrap;
	}
	@media (max-width: 900px) {
		.t5-beyond {
			grid-template-columns: 1fr;
		}
	}
</style>
