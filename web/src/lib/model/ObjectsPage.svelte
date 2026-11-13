<script lang="ts">
	// P9 Objects (D19–D22, D25, 11 §1–§3): the canonical business object model as a map. Objects are the nouns
	// every template, operation and connector is keyed on; template layers bring them into a job (base handful for
	// every job, industry additions selected by the business model); each object has a lifecycle (states) that
	// operations move it through. Select an object to see its lifecycle, operations and relations.
	import { onMount } from 'svelte';
	import { api } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Seg from '$lib/ui/Seg.svelte';
	import type { ObjectCatalog, ObjectModelMap } from '$lib/api/types/model';
	import { SCOPE_LABEL, TIER_VARIANT } from './ui';
	import ObjectsP9Map from './ObjectsP9Map.svelte';
	import ObjectsP9Inspector from './ObjectsP9Inspector.svelte';
	import { MODALITIES, matchesFilter, modalityCounts } from './objectsP9';
	import CreateObject from './CreateObject.svelte';

	type Data = { catalog: ObjectCatalog; map: ObjectModelMap };
	const res = new Resource<Data>(
		async () => {
			const [catalog, map] = await Promise.all([api().objectCatalog(), api().objectModelMap()]);
			return { catalog, map };
		},
		(d) => d.catalog.objects.length === 0,
	);
	onMount(() => res.refresh());

	let tier = $state('all');
	let q = $state('');
	let selected = $state<string | null>(null);

	const MODALITY_HINT: Record<string, string> = {
		observe: 'read state, change nothing',
		do: 'the customer changes the record now',
		request: 'ask the provider; opens a pending request',
	};
</script>

<PageHead
	id="P9"
	sub="The nouns of every job: order, parcel, refund, case… Template layers bring objects into a job, each object has a lifecycle of states, and operations move it between them. Platform rows — edits reach tenants only through a new cut."
>
	{#snippet actions()}
		<CreateObject oncreated={() => res.refresh()} />
		<a class="btn" href="/platform/templates">Templates</a>
		<a class="btn" href="/platform/operations">Operations catalogue</a>
	{/snippet}
</PageHead>

<LoadState resource={res} empty="No objects in the catalogue yet.">
	{#snippet children({ catalog, map })}
		{@const nodes = map.nodes}
		{@const layerKeys = new Set(nodes.flatMap((n) => n.layers.map((l) => l.layer_key)))}
		{@const stateTotal = nodes.reduce((s, n) => s + n.states.length, 0)}
		{@const transTotal = nodes.reduce((s, n) => s + n.transition_count, 0)}
		{@const opTotals = MODALITIES.map(
			(m) => [m, nodes.reduce((s, n) => s + modalityCounts(n)[m], 0)] as const,
		)}
		{@const opTotal = opTotals.reduce((s, [, c]) => s + c, 0)}
		{@const base = nodes.filter((n) => n.tier === 'base').length}
		{@const current =
			nodes.find((n) => n.object === selected) ?? nodes.find((n) => n.object === 'order') ?? nodes[0]!}

		<ol class="chain" aria-label="How the object model works">
			<li>
				<a href="/platform/templates">
					<span class="k">Template layers</span>
					<strong>{layerKeys.size}</strong>
					<span class="d">bring objects into a job</span>
				</a>
			</li>
			<li>
				<span class="k">Objects</span>
				<strong>{nodes.length}</strong>
				<span class="d">{base} base handful · {nodes.length - base} industry &amp; optional</span>
			</li>
			<li>
				<span class="k">Lifecycles</span>
				<strong>{stateTotal}</strong>
				<span class="d">states · {transTotal} transitions</span>
			</li>
			<li>
				<a href="/platform/operations">
					<span class="k">Operations</span>
					<strong>{opTotal}</strong>
					<span class="d">{opTotals.map(([m, c]) => `${c} ${m}`).join(' · ')}</span>
				</a>
			</li>
		</ol>

		<div class="layout">
			<Card
				title="Object map"
				sub="Lines are relations (11 §1.3). Upper band: the base handful every job gets. Lower band: objects an industry layer adds when the business model needs them."
			>
				{#snippet actions()}
					<Seg
						label="Tier"
						bind:active={tier}
						items={[
							['all', 'All'],
							['base', 'Base'],
							['industry', 'Industry'],
							['optional', 'Optional'],
							['retail', 'Retail job'],
						]}
					/>
					<label class="search"
						><span class="sr">Search objects</span><input
							bind:value={q}
							placeholder="Search objects"
						/></label
					>
				{/snippet}
				<ObjectsP9Map
					{nodes}
					relations={map.relations}
					selected={current.object}
					dim={(n) => !matchesFilter(n, tier, q)}
					onselect={(o) => (selected = o)}
				/>
				<div class="legend" aria-label="Map legend">
					<span><i class="sw base"></i>base handful</span>
					<span><i class="sw industry"></i>industry addition</span>
					<span><i class="sw optional"></i>optional</span>
					<span><i class="dot"></i>in the retail customer-care job</span>
					<span class="muted">Select an object to trace its relations.</span>
				</div>
			</Card>

			<ObjectsP9Inspector
				node={current}
				relations={map.relations}
				folded={catalog.folded.filter((f) => f.object === current.object)}
				hint={MODALITY_HINT}
			/>
		</div>

		<Card
			title="Catalogue"
			sub="Every object with the layer that brings it in, its lifecycle and what operations can do to it."
		>
			{@const rows = catalog.objects.filter((o) => matchesFilter(o, tier, q))}
			{#if rows.length === 0}
				<p class="empty">No object matches “{q}”{tier !== 'all' ? ` in ${tier}` : ''}.</p>
			{:else}
				<div class="tbl-wrap">
					<table class="tbl" data-testid="objects">
						<thead>
							<tr>
								<th>Object</th>
								<th>Tier</th>
								<th>Brought in by</th>
								<th>Lifecycle</th>
								<th>Operations</th>
								<th class="r">Fields</th>
								<th>Stored as</th>
							</tr>
						</thead>
						<tbody>
							{#each rows as o (o.object)}
								{@const n = nodes.find((x) => x.object === o.object)}
								{@const mc = n ? modalityCounts(n) : null}
								<tr class:sel={o.object === current.object}>
									<td>
										<a class="mono" href="/platform/objects/{o.object}">{o.object}</a>
										{#if o.in_retail_job}<Chip text="retail job" variant="cust" />{/if}
										<span class="cell-sub">{o.description}</span>
									</td>
									<td>
										<Chip text={o.tier} variant={TIER_VARIANT[o.tier]} />
										<span class="cell-sub mono">{o.origin}</span>
									</td>
									<td>
										{#each n?.layers ?? [] as l (l.layer_key)}
											<span class="layer" title="{SCOPE_LABEL[l.scope]} layer, since @v{l.since}"
												>{l.layer_key}</span
											>
										{:else}
											<span class="muted">—</span>
										{/each}
									</td>
									<td>
										{#if n && n.states.length}
											<span class="strip" title={n.states.map((s) => s.status).join(' → ')}>
												{#each n.states as s (s.status)}<i class:ini={s.initial} class:term={s.terminal}
													></i>{/each}
											</span>
											<span class="cell-sub">{n.states.length} states</span>
										{:else}
											<span class="muted">no lifecycle</span>
										{/if}
									</td>
									<td>
										{#if o.operation_count && mc}
											<a href="/platform/operations?object={o.object}" class="ops">
												{#each MODALITIES as m (m)}{#if mc[m]}<span class="op {m}">{mc[m]} {m}</span
														>{/if}{/each}
											</a>
										{:else}<span class="muted">none</span>{/if}
									</td>
									<td class="r"
										>{o.field_count}{#if o.pii_count}<span class="cell-sub"
												><Chip text="{o.pii_count} PII" variant="warn" /></span
											>{/if}</td
									>
									<td
										>{#if o.storage_shape_of}<span class="mono">{o.storage_shape_of}</span>{:else}<span
												class="muted">own table</span
											>{/if}</td
									>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
			{/if}
		</Card>

		<Card
			title="Not separate objects"
			sub="Concepts people ask for that are modelled as a status, type or role of an existing object (11 §2.1). Ask before adding an object."
		>
			<ul class="folded" data-testid="folded">
				{#each catalog.folded as f (f.concept)}
					<li data-folded={f.concept}>
						<span class="concept">{f.concept}</span>
						<span class="arrow" aria-hidden="true">→</span>
						<span class="becomes"
							>{f.becomes} <a class="mono" href="/platform/objects/{f.object}">{f.object}</a></span
						>
						<span class="cite mono">{f.cite}</span>
					</li>
				{/each}
			</ul>
		</Card>
	{/snippet}
</LoadState>

<style>
	.sr {
		position: absolute;
		width: 1px;
		height: 1px;
		overflow: hidden;
		clip: rect(0 0 0 0);
	}
	.chain {
		list-style: none;
		margin: 0 0 16px;
		padding: 0;
		display: grid;
		grid-template-columns: repeat(4, minmax(0, 1fr));
		gap: 0;
		border: 1px solid var(--line);
		border-radius: var(--r);
		background: var(--panel);
		overflow: hidden;
	}
	.chain li {
		position: relative;
		border-left: 1px solid var(--line-2);
	}
	.chain li:first-child {
		border-left: 0;
	}
	.chain li > a {
		display: flex;
		flex-direction: column;
		gap: 2px;
	}
	.chain li {
		padding: 12px 16px 12px 22px;
		display: flex;
		flex-direction: column;
		gap: 2px;
	}
	.chain li:not(:first-child)::before {
		content: '›';
		position: absolute;
		left: -7px;
		top: 50%;
		transform: translateY(-50%);
		background: var(--panel);
		color: var(--muted);
		font-size: 18px;
		line-height: 1;
		padding: 2px 0;
	}
	.chain a {
		color: inherit;
		text-decoration: none;
	}
	.chain a:hover strong {
		color: var(--accent);
	}
	.chain .k {
		font: 600 11px/1.2 var(--cond);
		letter-spacing: 0.07em;
		text-transform: uppercase;
		color: var(--muted);
	}
	.chain strong {
		font-size: 22px;
		font-weight: 600;
		font-variant-numeric: tabular-nums;
	}
	.chain .d {
		font-size: 12px;
		color: var(--muted);
	}
	.layout {
		display: grid;
		grid-template-columns: minmax(0, 1fr) 330px;
		gap: 16px;
		align-items: start;
		margin-bottom: 16px;
	}
	.legend {
		display: flex;
		flex-wrap: wrap;
		gap: 6px 16px;
		margin-top: 10px;
		font-size: 12px;
		color: var(--muted);
	}
	.legend span {
		display: inline-flex;
		align-items: center;
		gap: 6px;
	}
	.sw {
		width: 10px;
		height: 10px;
		border-radius: 2px;
		display: inline-block;
	}
	.sw.base {
		background: var(--ok);
	}
	.sw.industry {
		background: var(--tool);
	}
	.sw.optional {
		background: var(--muted);
	}
	.dot {
		width: 8px;
		height: 8px;
		border-radius: 50%;
		background: var(--cust);
		display: inline-block;
	}
	.search input {
		border: 1px solid var(--line);
		background: var(--panel);
		color: var(--fg);
		border-radius: 6px;
		padding: 5px 10px;
		font: inherit;
		font-size: 13px;
		width: 160px;
	}
	tr.sel td {
		background: color-mix(in srgb, var(--accent) 6%, transparent);
	}
	.layer {
		display: block;
		font-family: var(--mono);
		font-size: 11.5px;
		white-space: nowrap;
	}
	.strip {
		display: inline-flex;
		gap: 2px;
	}
	.strip i {
		width: 9px;
		height: 9px;
		border-radius: 50%;
		border: 1.5px solid var(--muted);
		display: inline-block;
	}
	.strip i.ini {
		background: var(--accent);
		border-color: var(--accent);
	}
	.strip i.term {
		border-radius: 2px;
		background: var(--muted);
	}
	.ops {
		display: inline-flex;
		flex-wrap: wrap;
		gap: 4px;
		text-decoration: none;
	}
	.op {
		font-size: 11.5px;
		padding: 1px 6px;
		border-radius: 10px;
		white-space: nowrap;
		border: 1px solid var(--line);
		color: var(--fg);
	}
	.op.observe {
		border-color: color-mix(in srgb, var(--muted) 60%, transparent);
	}
	.op.do {
		border-color: color-mix(in srgb, var(--accent) 60%, transparent);
		color: var(--accent);
	}
	.op.request {
		border-color: color-mix(in srgb, var(--work) 60%, transparent);
		color: var(--work);
	}
	.folded {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(300px, 1fr));
		gap: 8px;
	}
	.folded li {
		display: flex;
		flex-wrap: wrap;
		align-items: baseline;
		gap: 4px 8px;
		padding: 8px 10px;
		border: 1px solid var(--line-2);
		border-radius: var(--r);
		background: var(--sunk);
		font-size: 13px;
	}
	.concept {
		font-weight: 600;
	}
	.arrow {
		color: var(--muted);
	}
	.cite {
		margin-left: auto;
		color: var(--faint);
		font-size: 11px;
	}
	@media (max-width: 1100px) {
		.layout {
			grid-template-columns: minmax(0, 1fr);
		}
	}
	@media (max-width: 720px) {
		.chain {
			grid-template-columns: repeat(2, minmax(0, 1fr));
		}
		.chain li:nth-child(3) {
			border-left: 0;
		}
		.chain li:nth-child(3)::before {
			display: none;
		}
		.chain li:nth-child(n + 3) {
			border-top: 1px solid var(--line-2);
		}
		.folded .arrow {
			display: none;
		}
		.search input {
			width: 120px;
		}
	}
</style>
