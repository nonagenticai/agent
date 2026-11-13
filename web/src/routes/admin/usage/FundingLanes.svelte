<script lang="ts">
	// T5: the three funding lanes (memory usage-funding, D38) — who pays for the compute behind the tokens. D56: every
	// metered source shares these lanes — customer turns, staff test chats and generator runs — so the lanes sum all
	// sources, and the source split below shows which lane each one ran on.
	import { fmt } from '$lib/format';
	import type { Funding, UsageReport } from '$lib/api/types/tenant';
	import { pct } from '$lib/tenant/ui';
	import { laneTotals } from './usage';

	let { report }: { report: UsageReport } = $props();

	const LANES: {
		key: Funding;
		name: string;
		cls: string;
		who: string;
		idle: string;
		href?: string;
		link?: string;
	}[] = [
		{
			key: 'local_gpu',
			name: 'Internal GPU',
			cls: 's-ok',
			who: 'nonagentic’s own tenant only (D70): the pgx vLLM and mlx_api. Customer tenants use their own providers.',
			idle: 'No turns ran on the internal GPUs.',
		},
		{
			key: 'platform_external',
			name: 'Platform external',
			cls: 's-work',
			who: 'An external model the platform pays for on your behalf.',
			idle: 'No platform-paid external model in use.',
		},
		{
			key: 'tenant_byo',
			name: 'Your own key (BYO)',
			cls: 's-cust',
			who: 'Your provider URL and token: you pay the provider directly, we only count.',
			idle: 'No turns on your own provider yet.',
			href: '/admin/providers',
			link: 'Add a provider',
		},
	];
	const lanes = $derived(laneTotals(report));
	const total = $derived(lanes.local_gpu + lanes.platform_external + lanes.tenant_byo);
	const sources = $derived((report.sources ?? []).filter((s) => s.tokens > 0));
</script>

<ul class="t5-lanes">
	{#each LANES as l (l.key)}
		{@const v = lanes[l.key]}
		<li class="t5-lane" class:idle={v === 0} data-funding={l.key}>
			<div class="t5-lane-head">
				<span class="t5-lane-name"><i class="sw {l.cls}"></i> {l.name}</span>
				<span class="mono t5-lane-val">{v > 0 ? `${fmt.k(v)} tokens · ${pct(v, total)}` : '0'}</span>
			</div>
			<span class="t5-track {l.cls}" role="img" aria-label="{l.name}: {pct(v, total)}"
				><i style="width:{total > 0 ? (v / total) * 100 : 0}%"></i></span
			>
			<p class="t5-lane-who">
				{v > 0 ? l.who : l.idle}
				{#if v === 0 && l.href}<a href={l.href}>{l.link} →</a>{/if}
			</p>
		</li>
	{/each}
</ul>
{#if sources.length}
	<!-- the lane chips are nowrap: on phones the table scrolls inside its own wrapper, not the page -->
	<div class="tbl-wrap t5-src">
		<table class="tbl dense" data-testid="usage-sources">
			<caption class="t5-src-cap">By source — one ledger, one set of lanes</caption>
			<tbody>
				{#each sources as s (s.key)}
					<tr data-source={s.key}>
						<td
							>{s.label}{#if s.turns !== null}<span class="cell-sub">{fmt.n(s.turns)} turns</span>{:else}<span
									class="cell-sub">activity generator · no turns</span
								>{/if}</td
						>
						<td class="r mono">{fmt.k(s.tokens)}</td>
						<td class="r">
							{#each LANES.filter((l) => s.funding[l.key] > 0) as l (l.key)}<span class="chip ok"
									>{l.name} {pct(s.funding[l.key], s.tokens)}</span
								>{/each}
						</td>
					</tr>
				{/each}
			</tbody>
		</table>
	</div>
{/if}
{#if report.plan}
	<p class="note t5-plan">
		Plan <strong>{report.plan.name}</strong> · licence {report.plan.licence} · no usage allowance is set on plans
		yet, so nothing here is capped or priced (K52).
	</p>
{/if}

<style>
	.t5-lanes {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 12px;
	}
	.t5-lane-head {
		display: flex;
		justify-content: space-between;
		gap: 8px;
		flex-wrap: wrap;
		margin-bottom: 4px;
	}
	.t5-lane-name {
		font-weight: 600;
		display: inline-flex;
		gap: 6px;
		align-items: center;
	}
	.t5-lane-val {
		font-size: 12px;
	}
	.t5-lane.idle .t5-lane-name,
	.t5-lane.idle .t5-lane-val {
		color: var(--muted);
		font-weight: 500;
	}
	.t5-track {
		display: block;
		height: 8px;
		border-radius: 4px;
		background: var(--sunk);
		overflow: hidden;
	}
	.t5-track i {
		display: block;
		height: 100%;
		background: currentColor;
	}
	.t5-lane-who {
		margin: 4px 0 0;
		font-size: 12px;
		color: var(--muted);
	}
	.t5-lane-who a {
		margin-left: 4px;
	}
	.t5-plan {
		margin-top: 12px;
	}
	.t5-src {
		margin-top: 12px;
	}
	.t5-src-cap {
		text-align: left;
		font-size: 12px;
		color: var(--muted);
		padding-bottom: 4px;
	}
</style>
