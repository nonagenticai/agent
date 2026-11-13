<script lang="ts">
	// P13 Dimensions — what a job selection picks besides business model + industry, and what each pick does in a
	// compile (D8: standard values only; insert-only because releases froze them).
	// D59: a dimension "selects or parametrises content in the layer stack". Channel is the real one. Region is a
	// placeholder (recorded, selects nothing); its four concerns moved: residency → tenant, languages → job,
	// jurisdiction → future packs, market operations → tenant wiring.
	// D60: a department = the industry_dept layer, specialising a BASE department archetype; the old "department"
	// values are AREAS (groupings of context spaces inside a department).
	import { onMount } from 'svelte';
	import { api } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import type { DepartmentArchetype, Dimension, DimensionUsage } from '$lib/api/types/platform';
	import PageHead from '$lib/ui/PageHead.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import Kpi from '$lib/ui/Kpi.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';
	import { utc } from '$lib/platform/ui';
	import { areaDepartments, oldestFrozen, roleOfDim, totals, usageOf } from '$lib/platform/P13Dimensions';
	import DimensionSection from './DimensionSection.svelte';
	import ArchetypeSection from './ArchetypeSection.svelte';
	import RegionSplit from './RegionSplit.svelte';
	import SelectionModel from './SelectionModel.svelte';
	import SelectionPath from './SelectionPath.svelte';
	import SelectionMatrix from './SelectionMatrix.svelte';

	const res = new Resource<Dimension[]>(
		() => api().platformDimensions(),
		(d) => d.length === 0,
	);
	const usageRes = new Resource<DimensionUsage>(() => api().platformDimensionUsage());
	const archRes = new Resource<DepartmentArchetype[]>(() => api().departmentArchetypes());
	const refresh = () => Promise.all([res.refresh(), usageRes.refresh(), archRes.refresh()]);
	onMount(() => void refresh());

	const usage = $derived(usageRes.value.state === 'ok' ? usageRes.value.data : null);
	const archetypes = $derived(archRes.value.state === 'ok' ? archRes.value.data : null);
	const frozen = $derived(usage ? oldestFrozen(usage) : null);
	const byKey = (dims: Dimension[], k: string) => dims.find((d) => d.key === k) ?? null;
</script>

<PageHead
	id="P13"
	sub="What a job selection picks besides business model and industry — department, areas, channel — and what each pick does in the layer stack. Platform-owned and insert-only."
/>

<LoadState resource={res} loading="Loading dimensions…" empty="No dimensions.">
	{#snippet children(dims)}
		{#if usage}
			{@const t = totals(dims, usage)}
			<div class="kpis" data-testid="dim-kpis">
				<Kpi label="Standard values" value={String(t.values)} note={`across ${dims.length} dimensions`} />
				<Kpi label="Picked by a selection" value={String(t.in_use)} note={`${t.selections} job selections`} />
				<Kpi label="Frozen in releases" value={String(t.in_releases)} note="must resolve on every re-cut" />
				<Kpi label="Tenant-only values" value="0" note="not allowed (D8)" />
			</div>
			{#if usage.unknown.length}
				<Callout kind="err" title="Selections name values that are not standard">
					{usage.unknown.map((u) => `${u.tenant_name} · ${u.job}: ${u.dimension}=${u.value}`).join('; ')}. The
					job page shows these as a selection error until the value is added here.
				</Callout>
			{/if}
		{:else if usageRes.value.state === 'unavailable'}
			<Unavailable reason={usageRes.value.reason} retry={usageRes.retry} />
		{/if}

		<div class="grid concept">
			<Card
				title="What selects content in the layer stack"
				sub="A selection = business model + industry + department + areas + channels. Region is recorded only."
			>
				<SelectionModel {dims} {archetypes} />
			</Card>
			<Card title="Why values are insert-only" sub="Releases freeze the values they were cut with.">
				<div class="why" data-testid="insert-only">
					{#if frozen}
						<p>
							<strong class="mono">{frozen.id}</strong> ({frozen.tenant_name}, {frozen.state}{frozen.at
								? `, ${utc(frozen.at)}`
								: ''}) froze
							{#each frozen.values.slice(0, 4) as v, i (v)}<code>{v}</code>{i <
								Math.min(frozen.values.length, 4) - 1
									? ', '
									: ''}{/each}{frozen.values.length > 4 ? ` and ${frozen.values.length - 4} more` : ''}.
							Rolling back to it, or re-cutting it, must find every one of those values again.
						</p>
					{/if}
					<ul class="rules">
						<li>
							<strong>Added for everyone.</strong> A value one tenant needs is added here, for all tenants (D8).
						</li>
						<li>
							<strong>Never renamed.</strong> The key is what selections and releases store; the label is for people.
						</li>
						<li>
							<strong>Never removed.</strong> An unused value stays — even a placeholder region value. A selection
							naming a missing value is flagged on its job page and cannot be cut.
						</li>
					</ul>
					<p class="note">
						See it compile: <a href="/platform/compile">Compile preview</a> · archetypes, departments and
						channel-qualified layers live in
						<a href="/platform/layers">Layers</a>.
					</p>
				</div>
			</Card>
		</div>

		{@const channel = byKey(dims, 'channel')}
		{@const area = byKey(dims, 'area')}
		{@const region = byKey(dims, 'region')}
		{@const others = dims.filter((d) => !['channel', 'area', 'region'].includes(d.key))}
		<div class="dims">
			{#if channel}
				<DimensionSection
					dim={channel}
					title="Channel · a real dimension"
					role={roleOfDim(channel, usage)}
					usage={usage ? usageOf(usage, channel.key) : null}
					selectionKey={usage?.selection_keys[channel.key] ?? channel.key}
					onadded={() => void refresh()}
				/>
			{/if}

			{#if archetypes}
				<ArchetypeSection {archetypes} />
			{:else if archRes.value.state === 'unavailable'}
				<Unavailable reason={archRes.value.reason} retry={archRes.retry} />
			{/if}
			{#if area}
				<DimensionSection
					dim={area}
					title="Areas · within departments"
					role={roleOfDim(area, usage)}
					usage={usage ? usageOf(usage, area.key) : null}
					selectionKey={usage?.selection_keys[area.key] ?? area.key}
					groupOf={areaDepartments(archetypes)}
					onadded={() => void refresh()}
				/>
			{/if}

			{#if region}
				<section class="card" aria-labelledby="region-h" data-testid="region-placeholder">
					<header class="card-head">
						<div>
							<h2 id="region-h">Region · a placeholder, split into four concerns</h2>
							<p>
								Region used to mean residency, law, language and local operations at once. Each concern now
								lives where it is decided; the region values stay recorded in releases and select nothing.
							</p>
						</div>
					</header>
					<div class="card-body">
						<RegionSplit concerns={region.concerns ?? []} />
					</div>
				</section>
				<DimensionSection
					dim={region}
					title="Region values (placeholder)"
					role={roleOfDim(region, usage)}
					usage={usage ? usageOf(usage, region.key) : null}
					selectionKey={usage?.selection_keys[region.key] ?? region.key}
					onadded={() => void refresh()}
				/>
			{/if}
			{#each others as d (d.key)}
				<DimensionSection
					dim={d}
					role={roleOfDim(d, usage)}
					usage={usage ? usageOf(usage, d.key) : null}
					selectionKey={usage?.selection_keys[d.key] ?? d.key}
					onadded={() => void refresh()}
				/>
			{/each}
		</div>

		<Card
			title="How one selection compiles"
			sub="Pick a running job: each layer says which pick put it there, or why a channel layer was skipped."
		>
			{#if usage}
				<SelectionPath {dims} {usage} {archetypes} />
			{:else}
				<p class="muted">Loading selections…</p>
			{/if}
		</Card>

		{#if usage}
			<Card
				title="Which selection picks which value"
				sub="Each row is one job's current selection. An empty column is a value no job picks yet — it stays available."
			>
				<SelectionMatrix {dims} {usage} />
			</Card>
		{/if}
	{/snippet}
</LoadState>

<style>
	.kpis {
		margin-bottom: 16px;
	}
	.concept {
		grid-template-columns: minmax(0, 1.6fr) minmax(0, 1fr);
		margin-bottom: 16px;
	}
	.dims {
		display: grid;
		gap: 16px;
		margin-bottom: 16px;
	}
	.dims + :global(.card) {
		margin-bottom: 16px;
	}
	.why p {
		margin: 0 0 10px;
		font-size: 13px;
	}
	.rules {
		margin: 0;
		padding-left: 18px;
		display: grid;
		gap: 6px;
		font-size: 13px;
	}
	@media (max-width: 960px) {
		.concept {
			grid-template-columns: minmax(0, 1fr);
		}
	}
</style>
