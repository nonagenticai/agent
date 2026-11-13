<script lang="ts">
	// T6 routing map: every model call this tenant makes, as use → provider → model, grouped by the caller that
	// makes it (Agent itself, then the slots provisioned down to strata_rag and strata_ingest). Funding, region and
	// the fallback are shown on the route, so "who serves what, who pays, what happens when it is missing" reads at a
	// glance. Clicking a route opens the tab that edits it.
	import type { Funding } from '$lib/api/types/tenant';
	import Chip from '$lib/ui/Chip.svelte';
	import Kpi from '$lib/ui/Kpi.svelte';
	import { CALLERS, FUNDING, compact, summarize, type Route } from './logic';

	let { routes, onopen }: { routes: Route[]; onopen: (tab: Route['tab'], key: string) => void } = $props();

	const sum = $derived(summarize(routes));
	const STATE_CHIP = { ok: 'ok', warn: 'warn', err: 'err' } as const;
	const fundingOrder: Funding[] = ['local_gpu', 'platform_external', 'tenant_byo'];
</script>

<section class="card rm" aria-labelledby="rm-title" data-testid="routing-map">
	<header class="card-head">
		<div>
			<h2 id="rm-title">Routing: which model serves each call</h2>
			<p>
				Every model call this tenant makes goes <b>use → provider → model</b>. One provider per use; you
				decide who serves it, where it runs and who pays.
			</p>
		</div>
	</header>
	<div class="card-body stack">
		<div class="kpis">
			<Kpi
				label="Uses routed"
				value={`${sum.routed} of ${sum.total}`}
				note={sum.routed === sum.total ? 'every use has a provider' : 'empty uses fall back or fail'}
			/>
			<Kpi
				label="Need attention"
				value={String(sum.attention)}
				note={sum.attention ? 'empty, drifted, untested or keyless' : 'all routes healthy'}
			/>
			<Kpi
				label="Funding"
				value={fundingOrder
					.filter((f) => sum.funding[f])
					.map((f) => `${sum.funding[f]} ${FUNDING[f].short}`)
					.join(' · ') || '—'}
				note={fundingOrder
					.filter((f) => sum.funding[f])
					.map((f) => `${sum.funding[f]} × ${FUNDING[f].label}`)
					.join(', ') || 'nothing routed yet'}
			/>
		</div>

		{#each CALLERS as c (c.key)}
			{@const rows = routes.filter((r) => r.caller === c.key)}
			{#if rows.length}
				<div class="rm-group">
					<div class="rm-caller">
						<b>{c.label}</b>
						<span class="cell-sub">{c.sub}</span>
					</div>
					<ul class="rm-list">
						{#each rows as r (r.key)}
							<li>
								<button
									type="button"
									class="rm-route"
									class:is-err={r.state === 'err'}
									class:is-warn={r.state === 'warn'}
									data-route={r.key}
									aria-label={`${r.label}: ${r.provider ?? 'no provider'}${r.model ? `, ${r.model}` : ''}, ${r.status}. Edit`}
									onclick={() => onopen(r.tab, r.key)}
								>
									<span class="rm-use">
										<b>{r.label}</b>
										<span class="mono rm-key">{r.key}</span>
									</span>
									<span class="rm-arrow" aria-hidden="true">→</span>
									<span class="rm-prov">
										{#if r.provider}
											<b>{r.provider}</b>
											{#if r.funding}<Chip
													text={FUNDING[r.funding].label}
													variant={FUNDING[r.funding].variant}
													title={FUNDING[r.funding].hint}
												/>{/if}
										{:else}
											<span class="rm-none">no provider</span>
										{/if}
									</span>
									<span class="rm-arrow" aria-hidden="true">→</span>
									<span class="rm-model">
										{#if r.model}<span class="mono">{r.model}</span>{:else}<span class="rm-none">—</span>{/if}
										<span class="cell-sub"
											>{[
												r.region ?? (r.provider ? 'region not declared' : ''),
												r.tokens_30d != null ? `${compact(r.tokens_30d)} tokens / 30 d` : '',
											]
												.filter(Boolean)
												.join(' · ')}</span
										>
									</span>
									<span class="rm-state">
										<Chip text={r.status} variant={STATE_CHIP[r.state]} />
										{#if r.hint}<span class="cell-sub">{r.hint}</span>{/if}
									</span>
								</button>
							</li>
						{/each}
					</ul>
				</div>
			{/if}
		{/each}

		<p class="rm-legend">
			{#each fundingOrder as f (f)}
				<span class="rm-legend-item"
					><Chip text={FUNDING[f].label} variant={FUNDING[f].variant} />
					<span>{FUNDING[f].hint}</span></span
				>
			{/each}
		</p>
	</div>
</section>

<style>
	.rm-group {
		display: grid;
		gap: 6px;
	}
	.rm-caller {
		display: flex;
		flex-wrap: wrap;
		gap: 4px 10px;
		align-items: baseline;
		padding-top: 4px;
		border-top: 1px solid var(--line-2);
		padding: 10px 0 2px;
	}
	.rm-caller .cell-sub {
		display: inline;
	}
	.rm-list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 6px;
	}
	.rm-route {
		width: 100%;
		display: grid;
		grid-template-columns: minmax(150px, 1.2fr) 18px minmax(150px, 1.1fr) 18px minmax(150px, 1fr) minmax(
				160px,
				1.1fr
			);
		gap: 8px;
		align-items: center;
		text-align: left;
		font: inherit;
		color: var(--fg);
		background: var(--panel);
		border: 1px solid var(--line);
		border-left: 3px solid var(--ok);
		border-radius: var(--r);
		padding: 8px 12px;
		cursor: pointer;
	}
	.rm-route:hover {
		background: var(--sunk);
	}
	.rm-route:focus-visible {
		outline: 2px solid var(--accent);
		outline-offset: 2px;
	}
	.rm-route.is-warn {
		border-left-color: var(--warn);
	}
	.rm-route.is-err {
		border-left-color: var(--err);
	}
	.rm-use,
	.rm-model,
	.rm-state {
		display: grid;
		gap: 2px;
		min-width: 0;
	}
	.rm-prov {
		display: flex;
		flex-wrap: wrap;
		gap: 4px 8px;
		align-items: center;
		min-width: 0;
	}
	.rm-key {
		font-size: 12px;
		color: var(--muted);
		overflow-wrap: anywhere;
	}
	.rm-arrow {
		color: var(--faint);
		text-align: center;
	}
	.rm-none {
		color: var(--faint);
		font-style: italic;
	}
	.rm-state {
		justify-items: start;
	}
	.rm-legend {
		display: grid;
		gap: 6px;
		margin: 4px 0 0;
		font-size: 12px;
		color: var(--muted);
	}
	.rm-legend-item {
		display: flex;
		gap: 8px;
		align-items: baseline;
		flex-wrap: wrap;
	}
	@media (max-width: 900px) {
		.rm-route {
			grid-template-columns: 1fr;
			gap: 4px;
		}
		.rm-arrow {
			display: none;
		}
		.rm-prov::before {
			content: '→';
			color: var(--faint);
		}
		.rm-model {
			padding-left: 16px;
		}
	}
</style>
