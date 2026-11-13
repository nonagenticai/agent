<script lang="ts">
	// P18 Discovery generator: the owner's use-case generator as a guided flow (11§7, 10§8). The LLM drafts a new
	// industry / job step by step; a person decides every step; Validate gates Package; only Package publishes.
	// Platform runs output template layers; tenant runs output refinements only (D8) and their tokens are
	// that tenant's usage (D56: same usage pipeline + funding lanes as turns, never a separate bill).
	import { onMount } from 'svelte';
	import { page as appPage } from '$app/state';
	import { api } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import type { GeneratorRunSummary, IndustryJourneySummary } from '$lib/api/types/journey';
	import type { TenantSummary } from '$lib/api/types/core';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Kpi from '$lib/ui/Kpi.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { writeAttrsOrSuperAdmin } from '$lib/auth/roles';
	import { FUNDING_LABEL, tokens, when } from '$lib/journey/format';
	import {
		nextAction,
		runOutputs,
		runSegments,
		runTotals,
		splitRuns,
		stepMeta,
		type P18GenSeed,
	} from '$lib/journey/P18GenPipeline';
	import P18GenPipeline from './P18GenPipeline.svelte';
	import P18GenStart from './P18GenStart.svelte';

	const res = new Resource(() => api().generatorRuns());
	let tenants = $state<TenantSummary[]>([]);
	let industries = $state<IndustryJourneySummary[]>([]);
	onMount(() => {
		res.refresh();
		// Deep link from P8 / the "not started" chips: `?new=<industry>` opens New run prefilled.
		const want = appPage.url.searchParams.get('new');
		if (want) newRun(seedFor(want));
		api()
			.tenants()
			.then((p) => (tenants = p.items))
			.catch(() => (tenants = []));
		api()
			.journeyIndustries()
			.then((p) => (industries = p.items))
			.catch(() => (industries = []));
	});

	const STATUS: Record<GeneratorRunSummary['status'], string> = {
		running: 'accent',
		blocked: 'err',
		packaged: 'ok',
		abandoned: 'dead',
	};

	const gridOf = (industry: string) =>
		industries.find((i) => i.industry === industry && i.status !== 'not_started') ?? null;
	const notStarted = $derived(industries.filter((i) => i.status === 'not_started'));

	const seedFor = (industry: string): P18GenSeed => ({
		industry,
		department: 'customer_service',
		job: `${industry}_care_agent`,
	});

	let open = $state(false);
	let seed = $state<P18GenSeed | null>(null);
	function newRun(s: P18GenSeed | null = null) {
		seed = s;
		open = true;
	}
</script>

<PageHead
	id="P18"
	sub="Drafts a new industry, department or job — journey grid, objects, operations, example data — with an LLM. A person decides every step; nothing is published until Validate is green and Package tags it."
>
	{#snippet actions()}
		<button
			class="btn primary"
			type="button"
			{...writeAttrsOrSuperAdmin(session, 'job_designer')}
			onclick={() => newRun()}>New run</button
		>
	{/snippet}
</PageHead>

<LoadState
	resource={res}
	empty="No generator runs yet. Start one to draft a journey for a new industry or job."
>
	{#snippet children(page)}
		{@const t = runTotals(page.items)}
		{@const groups = splitRuns(page.items)}
		<div class="kpis p18-kpis">
			<Kpi label="Open runs" value={String(t.open)} note="{t.blocked} blocked by red validation items" />
			<Kpi label="Published" value={String(t.packaged)} note="runs tagged or saved as refinements" />
			<Kpi
				label="Tenant-run usage"
				value="{tokens(t.tenantTokens)} tokens"
				note="metered as each tenant's usage, like turns"
			/>
			<Kpi
				label="Platform-run usage"
				value="{tokens(t.platformTokens)} tokens"
				note="the platform's own usage"
			/>
		</div>

		<Card
			title="How a run works"
			sub="Each step: the LLM drafts, you accept / merge / rename / drop. Re-accepting a step makes the later ones stale."
		>
			<P18GenPipeline runs={page.items} />
			<div class="p18-lanes">
				<div class="p18-lane">
					<span class="chip tool">platform run</span>
					<p>
						Super-admin. Writes <b>template layers</b> (industry / department / job), provisional
						<span class="mono">@v0</span> until Package tags <span class="mono">@vN</span>. Tenants get it
						when they pin the tag.
					</p>
				</div>
				<div class="p18-lane">
					<span class="chip cust">tenant run</span>
					<p>
						Tenant admin or super-admin for a tenant. Writes <b>refinements only</b> (extension / hidden
						activities, D8) and skips platform-only steps. Its tokens are <b>that tenant's usage</b> on the same
						funding lanes as conversation turns — not a separate bill.
					</p>
				</div>
			</div>
		</Card>

		<div class="p18-runs" data-testid="runs">
			<h2 class="p18-h">
				Needs a decision <span class="muted">· {groups.open.length}</span>
			</h2>
			{#if groups.open.length}
				{#each groups.open as r (r.id)}{@render runCard(r)}{/each}
			{:else}
				<p class="muted p18-none">Every run is finished. Start one for an industry below.</p>
			{/if}

			<h2 class="p18-h">History <span class="muted">· {groups.done.length}</span></h2>
			{#each groups.done as r (r.id)}{@render runCard(r)}{/each}
		</div>

		{#if notStarted.length}
			<Card
				title="No journey grid yet"
				sub="Industries the platform has not drafted. A platform run starts their industry layer."
			>
				<div class="p18-next">
					{#each notStarted as i (i.industry)}
						<button
							class="btn sm"
							type="button"
							{...writeAttrsOrSuperAdmin(session, 'job_designer')}
							onclick={() => newRun(seedFor(i.industry))}
							>{i.label} <span aria-hidden="true">→ run</span></button
						>
					{/each}
				</div>
			</Card>
		{/if}
	{/snippet}
</LoadState>

{#snippet runCard(r: GeneratorRunSummary)}
	{@const grid = gridOf(r.industry)}
	<article class="p18-run" class:blocked={r.status === 'blocked'}>
		<div class="p18-run-head">
			<a href="/platform/generator/{r.id}" class="p18-run-id mono">#{r.id}</a>
			<span class="chip {r.scope === 'tenant' ? 'cust' : 'tool'}">{r.scope}</span>
			<span class="chip {STATUS[r.status]}">{r.status}</span>
			<span class="p18-run-title mono">{r.industry} / {r.department} / {r.job}</span>
			<span class="p18-run-who muted"
				>{r.started_by_name ?? r.started_by}{r.tenant_name ? ` · for ${r.tenant_name}` : ''} · updated {when(
					r.updated_at,
				)}</span
			>
		</div>

		<div class="p18-run-body">
			<div class="p18-run-prog">
				<div class="p18-segs" aria-label="Step {stepMeta(r.current_step).label}">
					{#each runSegments(r) as s (s.key)}
						<span class="p18-seg {s.state}" title="{stepMeta(s.key).label}: {s.state}"></span>
					{/each}
				</div>
				<p class="p18-next-line" class:red={r.red > 0}>
					{#if r.status === 'running' || r.status === 'blocked'}<b
							>At {stepMeta(r.current_step).code} · {stepMeta(r.current_step).label}.</b
						>{' '}{/if}{nextAction(r)}
				</p>
			</div>

			<div class="p18-run-out">
				<span class="p18-k">Output</span>
				<ul>
					{#each runOutputs(r) as o (o.label)}
						<li>
							<span class="chip {o.state === 'tagged' ? 'ok' : 'proposed'}">{o.state}</span>
							{#if o.href}<a class="mono" href={o.href}>{o.label}</a>{:else}<span class="mono">{o.label}</span
								>{/if}
							<small class="muted">{o.where}</small>
						</li>
					{/each}
				</ul>
				{#if r.scope === 'platform' && grid}
					<a class="p18-link" href="/platform/journeys/{r.industry}">Journey grid: {grid.label} →</a>
				{/if}
			</div>

			<div class="p18-run-cost">
				<span class="p18-k">Usage</span>
				<b>{tokens(r.cost.tokens)} tokens</b>
				<small
					>{FUNDING_LABEL[r.cost.funding]} · {r.cost.attributed_to === 'tenant'
						? `${r.tenant_name}'s usage`
						: 'platform usage'}</small
				>
			</div>

			<a class="btn sm p18-open" href="/platform/generator/{r.id}"
				>{r.status === 'running' || r.status === 'blocked' ? 'Continue' : 'Open'}</a
			>
		</div>
	</article>
{/snippet}

<P18GenStart bind:open {seed} {tenants} />

<style>
	.p18-kpis {
		margin-bottom: 14px;
	}
	.p18-lanes {
		display: grid;
		grid-template-columns: 1fr 1fr;
		gap: 10px;
		margin-top: 12px;
	}
	.p18-lane {
		display: flex;
		gap: 8px;
		align-items: flex-start;
		font-size: 12.5px;
		line-height: 1.45;
	}
	.p18-lane p {
		margin: 0;
	}
	.p18-runs {
		margin: 16px 0;
		display: grid;
		gap: 8px;
	}
	.p18-h {
		font-size: 14px;
		margin: 8px 0 0;
	}
	.p18-none {
		margin: 0;
		font-size: 13px;
	}
	.p18-run {
		background: var(--panel);
		border: 1px solid var(--line);
		border-radius: var(--r);
		padding: 10px 12px;
	}
	.p18-run.blocked {
		border-left: 3px solid var(--err);
	}
	.p18-run-head {
		display: flex;
		gap: 8px;
		align-items: center;
		flex-wrap: wrap;
	}
	.p18-run-id {
		font-weight: 600;
	}
	.p18-run-title {
		font-size: 13px;
		font-weight: 600;
		overflow-wrap: anywhere;
	}
	.p18-run-who {
		font-size: 12px;
		margin-left: auto;
	}
	.p18-run-body {
		display: grid;
		grid-template-columns: minmax(0, 1.2fr) minmax(0, 1.5fr) minmax(0, 0.8fr) auto;
		gap: 14px;
		margin-top: 8px;
		align-items: start;
	}
	.p18-segs {
		display: grid;
		grid-template-columns: repeat(9, 1fr);
		gap: 3px;
	}
	.p18-seg {
		height: 7px;
		border-radius: 2px;
		background: var(--line);
	}
	.p18-seg.done {
		background: var(--ok);
	}
	.p18-seg.current {
		background: var(--accent);
	}
	.p18-seg.blocked {
		background: var(--err);
	}
	.p18-seg.skipped {
		background: transparent;
		border: 1px dashed var(--line);
	}
	.p18-next-line {
		margin: 6px 0 0;
		font-size: 12.5px;
		line-height: 1.4;
	}
	.p18-next-line.red {
		color: var(--err);
	}
	.p18-k {
		display: block;
		font-size: 10.5px;
		text-transform: uppercase;
		letter-spacing: 0.05em;
		color: var(--muted);
		margin-bottom: 3px;
	}
	.p18-run-out ul {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 3px;
		font-size: 12.5px;
	}
	.p18-run-out li {
		display: flex;
		gap: 6px;
		align-items: baseline;
		flex-wrap: wrap;
		min-width: 0;
	}
	.p18-run-out li a,
	.p18-run-out li .mono {
		overflow-wrap: anywhere;
	}
	.p18-link {
		display: inline-block;
		margin-top: 4px;
		font-size: 12px;
	}
	.p18-run-cost {
		font-size: 13px;
	}
	.p18-run-cost small {
		display: block;
		color: var(--muted);
		font-size: 11.5px;
	}
	.p18-next {
		display: flex;
		gap: 6px;
		flex-wrap: wrap;
	}
	@media (max-width: 900px) {
		.p18-run-body {
			grid-template-columns: 1fr 1fr;
		}
		.p18-run-prog {
			grid-column: 1 / -1;
		}
		.p18-run-who {
			margin-left: 0;
			flex-basis: 100%;
		}
	}
	@media (max-width: 640px) {
		.p18-lanes,
		.p18-run-body {
			grid-template-columns: 1fr;
		}
	}
</style>
