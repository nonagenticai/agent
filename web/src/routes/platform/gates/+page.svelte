<script lang="ts">
	// P14 Gate library (super_admin). Gates are the fast yes/no / pick-one / score checks laya answers around every
	// conversation turn (spec 10 §6, 11 §4.3.4, 03 §3): P1 before the qualifier, P2 before a case opens, R1 before a
	// reply leaves. The library holds the reusable questions per template layer (base → industry → job); jobs ask a
	// subset, tenants refine them per job (J7). Safety gates fail closed to a person (D29); options live in the DB
	// (D30), some generated at the release cut. The page leads with that model, then the questions grouped by layer.
	import { onMount } from 'svelte';
	import { api } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Freshness from '$lib/ui/Freshness.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Seg from '$lib/ui/Seg.svelte';
	import Lock from '$lib/ontology/Lock.svelte';
	import type { GateGroup, GatePoint, GateQuestion, GateUsage } from '$lib/api/types/ontology';
	import GateQuestionDrawer from './GateQuestionDrawer.svelte';
	import GateFlow from './GateFlow.svelte';
	import { GROUPS, optionsSummary, ruleText, usageSummary } from './gates';

	const res = new Resource(() => api().gateLibrary());
	const usage = new Resource(() => api().gateLibraryUsage());
	onMount(() => {
		res.refresh();
		usage.refresh();
	});

	let group = $state('all');
	let point = $state<GatePoint | 'all'>('all');
	let open = $state(false);
	let selKey = $state<string | null>(null);
	const sel = $derived(
		res.value.state === 'ok' ? (res.value.data.items.find((g) => g.key === selKey) ?? null) : null,
	);
	const usageOf = $derived.by(() => {
		const m = new Map<string, GateUsage>();
		if (usage.value.state === 'ok') for (const u of usage.value.data) m.set(u.gate, u);
		return m;
	});
	function show(g: GateQuestion) {
		selKey = g.key;
		open = true;
	}
	async function changed() {
		await Promise.all([res.refresh(), usage.refresh()]);
	}
	const count = (items: GateQuestion[], g: GateGroup) => items.filter((q) => q.group === g).length;
	const visible = (q: GateQuestion, k: GateGroup) => q.group === k && (point === 'all' || q.point === point);
</script>

<PageHead
	id="P14"
	sub="The reusable checks laya answers in milliseconds around every conversation turn. Template layers pick questions from this library, every job asks its set, and tenants refine labels and thresholds per job — the question itself is written once, here."
>
	{#snippet actions()}
		<Freshness resource={res} />
	{/snippet}
</PageHead>

<LoadState resource={res} empty="The gate library is empty.">
	{#snippet children(page)}
		<section class="gl-model" data-testid="gate-flow">
			<header class="gl-model-head">
				<h2>Where gates run in one turn</h2>
				<p class="muted">Pick a gate point to see only its questions.</p>
			</header>
			<GateFlow items={page.items} bind:point />
			<div class="gl-legend">
				<div class="gl-leg">
					<h3>Answer types</h3>
					<p>
						<b class="mono">noul</b> true/false, no confidence — acts on any true.
						<b class="mono">choice</b> picks one of 2–255 options with a confidence — acts above a threshold.
						<b class="mono">score</b> an ordered 1–5 level — acts at or above a level.
					</p>
				</div>
				<div class="gl-leg">
					<h3>When laya cannot answer</h3>
					<p>
						<span class="gl-closed"><Lock /> Fails closed</span>: the turn goes to a person (safety, D29).
						<b>Fails open</b>: the turn continues without the verdict (routing and quality).
					</p>
				</div>
				<div class="gl-leg">
					<h3>Who changes what</h3>
					<p>
						Here: question text, threshold, authored options. <Chip text="generated" variant="tool" /> options
						are rebuilt from each job's grid at the release cut. Tenants relabel, hide and tune per job on the
						job's Gates page.
					</p>
				</div>
			</div>
		</section>

		<div class="gl-bar">
			<Seg
				label="Template layer"
				items={[
					['all', `All layers (${page.items.length})`],
					...GROUPS.map((g): [string, string] => [g.key, `${g.label} (${count(page.items, g.key)})`]),
				]}
				bind:active={group}
			/>
			{#if point !== 'all'}
				<button type="button" class="btn sm" onclick={() => (point = 'all')}
					>Showing {point} only · show all points</button
				>
			{/if}
		</div>

		{#each GROUPS.filter((g) => group === 'all' || group === g.key) as g (g.key)}
			{@const rows = page.items.filter((q) => visible(q, g.key))}
			{#if rows.length}
				<Card title={g.label} sub={g.sub} cls="flush">
					<ul class="gl-list">
						{#each rows as q (q.key)}
							{@const u = usageOf.get(q.key)}
							<li>
								<button
									type="button"
									class="gl-row"
									class:safety={q.safety}
									onclick={() => show(q)}
									aria-label="Open {q.key}: {q.question}"
								>
									<span class="gl-key mono"
										>{q.key}{#if q.safety}<Lock />{/if}</span
									>
									<span class="gl-main">
										<span class="gl-q">{q.question}</span>
										<span class="gl-rule"
											><span class="mono gl-th">{ruleText(q)}</span><span class="gl-arrow" aria-hidden="true"
												>→</span
											>{q.effect}</span
										>
									</span>
									<span class="gl-meta">
										<span class="chips">
											<Chip text={q.point} variant="accent" />
											<Chip text={q.type} />
											{#if q.fail === 'closed'}<Chip text="fails closed" variant="err" />{:else}<Chip
													text="fails open"
												/>{/if}
										</span>
										<span class="gl-opts">
											{#if q.generated_from}<Chip
													text="generated"
													variant="tool"
													title={`From ${q.generated_from}`}
												/>{/if}
											<span class="muted">{optionsSummary(q)}</span>
										</span>
										<span class="gl-use muted">{u ? usageSummary(u) : ''}</span>
									</span>
								</button>
							</li>
						{/each}
					</ul>
				</Card>
			{/if}
		{/each}
		{#if !GROUPS.some((g) => (group === 'all' || group === g.key) && page.items.some( (q) => visible(q, g.key), ))}
			<p class="muted gl-none">
				No {point} questions in this layer.
				<button type="button" class="linkish" onclick={() => (point = 'all')}>Show every gate point</button>
			</p>
		{/if}
	{/snippet}
</LoadState>

<GateQuestionDrawer
	bind:open
	gate={sel}
	usage={sel ? (usageOf.get(sel.key) ?? null) : null}
	onchanged={changed}
/>

<style>
	.gl-model {
		background: var(--panel);
		border: 1px solid var(--line);
		border-radius: 8px;
		padding: 14px 16px 16px;
		margin-bottom: 14px;
		display: grid;
		gap: 12px;
	}
	.gl-model-head {
		display: flex;
		flex-wrap: wrap;
		align-items: baseline;
		gap: 4px 12px;
	}
	.gl-model-head h2 {
		margin: 0;
		font: 600 15px/1.3 var(--cond);
	}
	.gl-model-head p {
		margin: 0;
		font-size: 12.5px;
	}
	.gl-legend {
		display: grid;
		grid-template-columns: repeat(3, minmax(0, 1fr));
		gap: 10px;
		border-top: 1px solid var(--line-2);
		padding-top: 12px;
	}
	.gl-leg h3 {
		margin: 0 0 3px;
		font: 600 11.5px/1.3 var(--cond);
		letter-spacing: 0.06em;
		text-transform: uppercase;
		color: var(--muted);
	}
	.gl-leg p {
		margin: 0;
		font-size: 12.5px;
		line-height: 1.5;
	}
	.gl-closed {
		color: var(--err);
		font-weight: 600;
	}
	.gl-bar {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 10px;
		margin-bottom: 10px;
	}
	.gl-bar :global(.seg) {
		max-width: 100%;
		overflow-x: auto;
	}
	.gl-list {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.gl-list li + li {
		border-top: 1px solid var(--line-2);
	}
	.gl-row {
		display: grid;
		grid-template-columns: 52px minmax(0, 1fr) minmax(200px, 260px);
		gap: 12px;
		width: 100%;
		padding: 10px 16px;
		border: 0;
		background: transparent;
		color: var(--fg);
		font: inherit;
		text-align: left;
		cursor: pointer;
	}
	.gl-row:hover {
		background: var(--sunk);
	}
	.gl-row.safety {
		box-shadow: inset 3px 0 0 var(--err);
	}
	.gl-key {
		font-weight: 600;
		display: inline-flex;
		gap: 4px;
		align-items: flex-start;
		padding-top: 1px;
	}
	.gl-main {
		display: flex;
		flex-direction: column;
		gap: 3px;
		min-width: 0;
	}
	.gl-q {
		font-weight: 500;
	}
	.gl-rule {
		font-size: 12.5px;
		color: var(--muted);
	}
	.gl-th {
		color: var(--fg);
		font-size: 12px;
	}
	.gl-arrow {
		margin: 0 6px;
	}
	.gl-meta {
		display: flex;
		flex-direction: column;
		gap: 5px;
		font-size: 12px;
	}
	.gl-opts {
		display: flex;
		align-items: center;
		gap: 6px;
		flex-wrap: wrap;
	}
	.gl-none {
		margin-top: 8px;
	}
	.linkish {
		border: 0;
		background: none;
		padding: 0;
		color: var(--accent);
		font: inherit;
		cursor: pointer;
		text-decoration: underline;
	}
	@media (max-width: 900px) {
		.gl-legend {
			grid-template-columns: 1fr;
		}
		.gl-row {
			grid-template-columns: 44px minmax(0, 1fr);
		}
		.gl-meta {
			grid-column: 2;
		}
	}
</style>
