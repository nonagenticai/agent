<script lang="ts">
	// P9 inspector: the selected object in one glance — where it comes from (layers), its lifecycle, what
	// operations can do to it, how it relates to other objects, and which concepts are folded into it.
	import type { FoldedConcept, ObjectMapNode, ObjectRelation } from '$lib/api/types/model';
	import Chip from '$lib/ui/Chip.svelte';
	import { SCOPE_LABEL, TIER_VARIANT } from './ui';
	import { MODALITIES, modalityCounts, relationsOf } from './objectsP9';

	let {
		node,
		relations,
		folded,
		hint,
	}: {
		node: ObjectMapNode;
		relations: ObjectRelation[];
		folded: FoldedConcept[];
		hint: Record<string, string>;
	} = $props();

	const rels = $derived(relationsOf(node.object, relations));
	const mc = $derived(modalityCounts(node));
	const maxOps = $derived(Math.max(1, ...MODALITIES.map((m) => mc[m])));
</script>

<section class="card insp" aria-label="Selected object" data-testid="object-inspector">
	<header>
		<div class="top">
			<Chip text={node.tier} variant={TIER_VARIANT[node.tier]} />
			{#if node.in_retail_job}<Chip text="retail job" variant="cust" />{/if}
			<span class="origin mono">{node.origin}</span>
		</div>
		<h2>{node.label} <span class="mono key">{node.object}</span></h2>
		<p>{node.description}</p>
		{#if node.storage_shape_of}
			<p class="note">Stored as a <span class="mono">{node.storage_shape_of}</span> specialisation.</p>
		{/if}
	</header>

	<div class="sec">
		<h3>Brought in by</h3>
		{#if node.layers.length}
			<ol class="layers">
				{#each node.layers as l (l.layer_key)}
					<li>
						<span class="scope">{SCOPE_LABEL[l.scope]} layer</span>
						<a class="mono" href="/platform/templates">{l.layer_key}</a>
						<span class="since">since @v{l.since}</span>
					</li>
				{/each}
			</ol>
		{:else}
			<p class="muted small">No template layer carries it yet — no job can use it.</p>
		{/if}
	</div>

	<div class="sec">
		<h3>
			Lifecycle <span class="count">{node.states.length} states · {node.transition_count} transitions</span>
		</h3>
		{#if node.states.length}
			<ol class="states" data-testid="inspector-states">
				{#each node.states as s (s.status)}
					<li class:ini={s.initial} class:term={s.terminal}>
						{s.status}{#if s.initial}<span class="tag">start</span>{/if}{#if s.terminal}<span class="tag"
								>end</span
							>{/if}
					</li>
				{/each}
			</ol>
		{:else}
			<p class="muted small">Reference data: no states, operations only read or update it.</p>
		{/if}
	</div>

	<div class="sec">
		<h3>Operations <span class="count">{node.operations.length}</span></h3>
		<ul class="mods">
			{#each MODALITIES as m (m)}
				<li>
					<span class="mname">{m}</span>
					<span class="bar"><i class={m} style:width="{(mc[m] / maxOps) * 100}%"></i></span>
					<span class="n">{mc[m]}</span>
					<span class="h">{hint[m]}</span>
				</li>
			{/each}
		</ul>
		{#if node.operations.length}
			<div class="opkeys">
				{#each node.operations as o (o.key)}
					<span class="mono opk {o.modality}" title="{o.modality} · side effect {o.side_effect}">{o.key}</span
					>
				{/each}
			</div>
		{/if}
	</div>

	<div class="sec">
		<h3>Relations <span class="count">{rels.length}</span></h3>
		{#if rels.length}
			<ul class="rels" data-testid="inspector-relations">
				{#each rels as r, i (i)}
					<li>
						<span class="mono" class:me={r.subject === node.object}>{r.subject}</span>
						<span class="pred">{r.predicate}</span>
						<span class="mono" class:me={r.object === node.object && r.subject !== node.object}
							>{r.object}</span
						>
						<span class="cardn mono">{r.cardinality}</span>
					</li>
				{/each}
			</ul>
		{:else}
			<p class="muted small">Stands alone: no relation declared.</p>
		{/if}
	</div>

	{#if folded.length}
		<div class="sec">
			<h3>Also covers</h3>
			<ul class="fold">
				{#each folded as f (f.concept)}
					<li><strong>{f.concept}</strong> — {f.becomes}</li>
				{/each}
			</ul>
		</div>
	{/if}

	<footer>
		<a class="btn primary" href="/platform/objects/{node.object}">Open {node.object}</a>
		<a class="btn" href="/platform/operations?object={node.object}">Operations</a>
	</footer>
</section>

<style>
	.insp {
		position: sticky;
		top: 12px;
		padding: 16px;
		display: flex;
		flex-direction: column;
		gap: 14px;
	}
	header h2 {
		margin: 6px 0 4px;
		font-size: 18px;
	}
	header p {
		margin: 0;
		font-size: 13px;
		color: var(--muted);
	}
	header .note {
		margin-top: 4px;
		color: var(--fg);
	}
	.top {
		display: flex;
		gap: 6px;
		align-items: center;
		flex-wrap: wrap;
	}
	.origin {
		color: var(--faint);
		font-size: 11px;
		margin-left: auto;
	}
	.key {
		font-weight: 400;
		color: var(--muted);
		font-size: 12px;
	}
	.sec {
		border-top: 1px solid var(--line-2);
		padding-top: 10px;
	}
	h3 {
		margin: 0 0 8px;
		font: 600 11px/1.2 var(--cond);
		letter-spacing: 0.07em;
		text-transform: uppercase;
		color: var(--muted);
		display: flex;
		justify-content: space-between;
	}
	.count {
		text-transform: none;
		letter-spacing: 0;
		font-family: var(--sans);
		font-weight: 400;
	}
	.small {
		font-size: 12.5px;
		margin: 0;
	}
	.layers {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 4px;
	}
	.layers li {
		display: grid;
		grid-template-columns: 78px 1fr auto;
		gap: 6px;
		align-items: baseline;
		font-size: 12px;
	}
	.scope {
		color: var(--muted);
	}
	.since {
		color: var(--faint);
		font-size: 11px;
	}
	.states {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-wrap: wrap;
		gap: 4px;
	}
	.states li {
		font: 12px var(--mono);
		padding: 2px 8px;
		border-radius: 12px;
		border: 1px solid var(--line);
		background: var(--sunk);
	}
	.states li.ini {
		border-color: var(--accent);
		color: var(--accent);
		background: color-mix(in srgb, var(--accent) 8%, var(--panel));
	}
	.states li.term {
		border-style: dashed;
		color: var(--muted);
	}
	.tag {
		font: 600 9.5px var(--cond);
		text-transform: uppercase;
		letter-spacing: 0.05em;
		margin-left: 5px;
	}
	.mods {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 5px;
	}
	.mods li {
		display: grid;
		grid-template-columns: 56px 1fr 22px;
		gap: 8px;
		align-items: center;
		font-size: 12px;
	}
	.mods .h {
		grid-column: 2 / 4;
		color: var(--faint);
		font-size: 11px;
		margin-top: -4px;
	}
	.mname {
		font-weight: 600;
	}
	.bar {
		height: 7px;
		background: var(--sunk);
		border-radius: 4px;
		overflow: hidden;
	}
	.bar i {
		display: block;
		height: 100%;
		background: var(--muted);
	}
	.bar i.do {
		background: var(--accent);
	}
	.bar i.request {
		background: var(--work);
	}
	.n {
		text-align: right;
		font-variant-numeric: tabular-nums;
	}
	.opkeys {
		display: flex;
		flex-wrap: wrap;
		gap: 4px;
		margin-top: 8px;
	}
	.opk {
		font-size: 11px;
		padding: 1px 6px;
		border-radius: 4px;
		border: 1px solid var(--line-2);
	}
	.opk.do {
		color: var(--accent);
	}
	.opk.request {
		color: var(--work);
	}
	.rels {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 4px;
		font-size: 12.5px;
	}
	.rels li {
		display: flex;
		gap: 6px;
		align-items: baseline;
		flex-wrap: wrap;
	}
	.rels .me {
		font-weight: 600;
		color: var(--accent);
	}
	.pred {
		color: var(--muted);
	}
	.rels .cardn {
		margin-left: auto;
		color: var(--faint);
		font-size: 11px;
	}
	.fold {
		margin: 0;
		padding-left: 16px;
		font-size: 12.5px;
	}
	footer {
		display: flex;
		gap: 8px;
		flex-wrap: wrap;
	}
	@media (max-width: 1100px) {
		.insp {
			position: static;
		}
	}
</style>
