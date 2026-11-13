<script lang="ts">
	// P8 index: every industry's journey grid with its status, red findings and departments, and the industries the
	// platform has not drafted yet (they open a generator run). Super-admin; each grid opens the designer.
	import { onMount } from 'svelte';
	import { api } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Kpi from '$lib/ui/Kpi.svelte';
	import { groupJourneys } from './logic';

	const res = new Resource(() => api().journeyIndustries());
	onMount(() => res.refresh());

	const STATUS_CHIP: Record<string, string> = {
		tagged: 'ok',
		partial: 'warn',
		draft: 'work',
		not_started: 'muted',
	};
</script>

<PageHead
	id="P8"
	sub="Every industry's customer journey grid. Open one to edit its stages, activities and object bindings; an industry without a grid starts with a generator run."
>
	{#snippet actions()}
		<a class="btn" href="/platform/industries">Manage industries</a>
	{/snippet}
</PageHead>

<LoadState resource={res} empty="No industries yet: create one in Industries.">
	{#snippet children(page)}
		{@const g = groupJourneys(page.items)}
		<div class="kpis idx-kpis">
			<Kpi
				label="Industries with a grid"
				value={String(g.totals.industries)}
				note="drafted, partial or tagged"
			/>
			<Kpi
				label="Red findings"
				value={String(g.totals.red)}
				note={g.totals.red ? 'block tagging until fixed' : 'nothing blocks tagging'}
			/>
			<Kpi label="Not started" value={String(g.totals.notStarted)} note="no grid drafted yet" />
		</div>

		<Card title="Industries" sub="Status, tag and red findings of each grid.">
			{#if g.started.length}
				<div class="tbl-wrap">
					<table class="tbl" data-testid="journey-index">
						<thead
							><tr
								><th>Industry</th><th>Status</th><th>Tag</th><th>Departments</th><th class="r">Activities</th
								><th class="r">Red</th></tr
							></thead
						>
						<tbody>
							{#each g.started as i (i.industry)}
								<tr data-testid="industry-{i.industry}">
									<td><a href={i.href}>{i.label}</a></td>
									<td><span class="chip {STATUS_CHIP[i.status]}">{i.status}</span></td>
									<td class="mono">{i.tag ?? '—'}</td>
									<td>
										{#if i.grids.some((d) => d.department)}
											{#each i.grids as d (d.department ?? '')}
												<a
													class="chip {STATUS_CHIP[d.status]}"
													href={d.href}
													title="{d.status}{d.red ? ` · ${d.red} red` : ''}">{d.label}</a
												>{' '}
											{/each}
										{:else}<span class="muted">—</span>{/if}
									</td>
									<td class="r">{i.activities}</td>
									<td class="r" class:red={i.red > 0} data-testid="red-{i.industry}">{i.red || '—'}</td>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
			{:else}
				<p class="muted">No industry has a grid yet. Start a generator run to draft the first one.</p>
			{/if}
		</Card>

		{#if g.notStarted.length}
			<Card
				title="Not started"
				sub="Industries the platform has not drafted. A platform run starts their industry layer."
			>
				<div class="idx-next" data-testid="not-started">
					{#each g.notStarted as n (n.industry)}
						<a class="btn sm" href={n.href}>{n.label} <span aria-hidden="true">→ run</span></a>
					{/each}
				</div>
			</Card>
		{/if}
	{/snippet}
</LoadState>

<style>
	.idx-kpis {
		margin-bottom: 14px;
	}
	.idx-next {
		display: flex;
		gap: 6px;
		flex-wrap: wrap;
	}
	td.red {
		color: var(--err);
		font-weight: 600;
	}
</style>
