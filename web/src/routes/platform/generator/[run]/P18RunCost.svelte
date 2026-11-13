<script lang="ts">
	// P18 usage: the LLM tokens this run's drafting calls used, per step. D56: not a bill — generator tokens are metered
	// through the same usage pipeline and funding lane as customer-engagement turns. Tenant runs are that tenant's usage
	// (project = tenant, activity `generator`, on T5 Usage); platform runs are the platform's own usage.
	// The `cost-line` test-id is the e2e key of this page.
	import type { GeneratorRun } from '$lib/api/types/journey';
	import Card from '$lib/ui/Card.svelte';
	import { FUNDING_LABEL, costLine, tokens } from '$lib/journey/format';
	import { costByStep, stepName } from '$lib/journey/P18Run';

	let { run: r }: { run: GeneratorRun } = $props();

	const rows = $derived(costByStep(r));
	const max = $derived(Math.max(1, ...rows.map((x) => x.tokens)));
	const tenant = $derived(r.cost.attributed_to === 'tenant');
</script>

<Card
	title="LLM usage"
	sub={tenant
		? `Metered as ${r.tenant_name}'s usage, like conversation turns`
		: "Platform run — the platform's own usage"}
>
	<div class="p18-usage">
		<span class="p18-big">{tokens(r.cost.tokens)}</span>
		<span class="muted">tokens</span>
		<span class="chip ok">{FUNDING_LABEL[r.cost.funding]}</span>
		<span class="chip {tenant ? 'cust' : 'tool'}"
			>{tenant ? `usage of ${r.tenant_name}` : 'platform usage'}</span
		>
	</div>
	<p class="p18-costline" data-testid="cost-line"><b>Usage:</b> {costLine(r)}</p>
	{#if rows.length}
		<table class="tbl dense p18-cost-tbl">
			<thead><tr><th>Step</th><th class="r">Tokens</th><th class="r">Calls</th></tr></thead>
			<tbody>
				{#each rows as row (row.step)}
					<tr>
						<td>
							{stepName(row.step)}
							<span class="p18-cbar" aria-hidden="true"
								><span style:width="{(row.tokens / max) * 100}%"></span></span
							>
						</td>
						<td class="r mono">{tokens(row.tokens)}</td>
						<td class="r mono">{row.calls}</td>
					</tr>
				{/each}
			</tbody>
		</table>
		<p class="muted p18-small">
			{(r.cost_lines ?? []).length} drafting call(s) on <span class="mono">{r.cost_lines?.[0]?.model}</span>,
			one usage row each.
			{#if tenant}They show on the tenant's Usage page under the generator activity — not as a separate bill.{/if}
		</p>
	{:else}
		<p class="muted p18-small">No LLM calls yet — usage accrues when a step is drafted.</p>
	{/if}
</Card>

<style>
	.p18-usage {
		display: flex;
		align-items: baseline;
		gap: 8px;
		flex-wrap: wrap;
	}
	.p18-big {
		font: 600 22px/1.1 var(--cond);
	}
	.p18-costline {
		margin: 8px 0;
		font-size: 12.5px;
		color: var(--muted);
	}
	.p18-cost-tbl td {
		vertical-align: middle;
	}
	.p18-cbar {
		display: block;
		height: 3px;
		background: var(--line-2);
		border-radius: 2px;
		margin-top: 3px;
	}
	.p18-cbar span {
		display: block;
		height: 100%;
		background: var(--accent);
		border-radius: 2px;
	}
	.p18-small {
		font-size: 12px;
		margin: 6px 0 0;
	}
</style>
