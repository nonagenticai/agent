<script lang="ts">
	// J10 Knowledge kinds. Design intent: the tenant_admin answers "at each stage of the customer journey, which
	// knowledge must my agent be able to cite, and which of my stores backs it?" Model (11 §4.3.3, D34): the job
	// template's stages declare knowledge kinds (required/optional, inherited, read-only) → the tenant binds each
	// (stage, kind) to one of its stores (a refinement, saved to the job's draft — D54: tried in the test chat, live once published) → the store is created in
	// strata_rag and filled on T7 → the post-gate (R1, F2/F7) checks every policy claim against passages of exactly
	// these kinds. A required kind without a usable store is a J3 go-live gap (enforced at apply from
	// M4; apply does not check it today). Stores come from the tenant
	// store tree (T7), so coverage here and there is the same truth.
	import { onMount } from 'svelte';
	import { api, app, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { canWrite as canWriteFn, writeAttrs, writeHint } from '$lib/auth/roles';
	import { Resource } from '$lib/load.svelte';
	import { jobPath } from '$lib/scope';
	import { fmt } from '$lib/format';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Freshness from '$lib/ui/Freshness.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import MilestoneChip from '$lib/ui/MilestoneChip.svelte';
	import DraftBar from '$lib/release/DraftBar.svelte';
	import CoverageMatrix from './CoverageMatrix.svelte';
	import PackPins from './PackPins.svelte';
	import type { Paged } from '$lib/api/types/core';
	import type { KnowledgeStore } from '$lib/api/types/tenant';
	import type { KnowledgeKindRow, KnowledgeKinds, PlatformPack } from '$lib/api/types/ontology';
	import { STAGES } from '$lib/ontology/catalogue';
	import {
		COVERAGE,
		ago,
		kindCoverage,
		pathOf,
		storeState,
		type KindCoverage,
	} from '$lib/tenant/knowledge-T7';
	import { J10_POST_GATE, fixOf, j10Summary, storeUse } from '$lib/ontology/knowledge-J10';

	const res = new Resource(
		() => api().knowledgeKinds(),
		(d: KnowledgeKinds) => d.rows.length === 0,
	);
	// The store tree is optional context (state, passages): the page binds without it.
	const tree = new Resource<Paged<KnowledgeStore>>(() => api().knowledgeStores());
	// R10: the platform packs this tenant may pin (empty when platform knowledge is off; a failure only hides the picker).
	const packs = new Resource<PlatformPack[]>(() => api().platformPacks());
	onMount(() => Promise.all([res.refresh(), tree.refresh(), packs.refresh()]));
	const listing = $derived(packs.value.state === 'ok' ? packs.value.data : null);
	const now = Date.now();
	const canWrite = $derived(canWriteFn(session, 'job_designer'));
	// An empty tree is a known state (the tenant has no stores yet), not a failed read: only 'unavailable' is.
	const treeKnown = $derived(tree.value.state === 'ok' || tree.value.state === 'empty');
	const all = $derived(
		tree.value.state === 'ok' || tree.value.state === 'empty' ? tree.value.data.items : [],
	);
	let saving = $state<string | null>(null);

	const stageLabel = (s: string) => STAGES.find(([k]) => k === s)?.[1] ?? s;
	/** Where an inherited pack comes from, in fold order (base → industry → department → the job template). */
	const LAYER_ORDER = ['base', 'industry', 'department', 'job'] as const;
	function layerLevel(layer: string): (typeof LAYER_ORDER)[number] {
		if (layer === 'base' || layer.startsWith('base/') || layer.startsWith('base_dept/')) return 'base';
		if (layer.startsWith('industry_dept/')) return 'department';
		if (layer.startsWith('industry/')) return 'industry';
		return 'job';
	}
	function layerName(layer: string): string {
		const parts = layer.split('/');
		switch (layerLevel(layer)) {
			case 'base':
				return layer.startsWith('base_dept/') ? `All ${parts[1] ?? ''} departments` : 'Platform base';
			case 'industry':
				return `Industry · ${parts[1] ?? ''}`;
			case 'department':
				return `Department · ${parts[2] ?? ''}`;
			default:
				return 'Job template';
		}
	}
	const keyOf = (r: KnowledgeKindRow) => `${r.stage}|${r.kind}`;
	const storesHref = '/admin/knowledge/stores';
	const readinessHref = $derived(jobPath(app.job, '/readiness'));
	const refinementsHref = $derived(jobPath(app.job, '/refinements'));
	const journeyHref = $derived(jobPath(app.job, '/journey'));

	function stagesOf(cov: KindCoverage[]) {
		return STAGES.map(([stage, label, hint]) => {
			const kinds = cov
				.filter((c) => c.row.stage === stage)
				.sort(
					(a, b) => Number(b.row.required) - Number(a.row.required) || a.row.kind.localeCompare(b.row.kind),
				);
			const gaps = kinds.filter((k) => k.row.required && COVERAGE[k.state].gap).length;
			return { stage, label, hint, kinds, gaps };
		}).filter((s) => s.kinds.length);
	}
	async function bind(r: KnowledgeKindRow, store: string) {
		saving = keyOf(r);
		await guard(
			() => api().bindKnowledgeKind(r.stage, r.kind, { store: store || null }),
			store
				? `${r.kind} (${stageLabel(r.stage)}) now reads ${store}. Saved to draft — try it in the test chat; customers get it when you publish.`
				: `${r.kind} (${stageLabel(r.stage)}) is unbound. Saved to draft.`,
		);
		saving = null;
		// Re-read either way: a refused bind must put the select back on the stored value.
		await Promise.all([res.refresh(), tree.refresh()]);
	}
	/** R10: pin these packs (an explicit save = a draft change; a re-pin to a newer version is this too). */
	async function pin(r: KnowledgeKindRow, pins: string[], done: string) {
		saving = keyOf(r);
		await guard(() => api().bindKnowledgeKind(r.stage, r.kind, { packs: pins }), done);
		saving = null;
		await res.refresh();
	}
</script>

<PageHead
	id="J10"
	sub="What your agent must be able to cite at each stage of the customer journey, and which of your knowledge stores backs each kind."
>
	{#snippet actions()}
		<Freshness resource={res} />
		<a class="btn" href={storesHref}>Knowledge stores</a>
		<a class="btn" href={readinessHref}>Readiness</a>
	{/snippet}
</PageHead>

<DraftBar page="J10" tick={res.value} />

<LoadState
	resource={res}
	empty="This job declares no knowledge kinds yet. They come with the job template’s journey stages."
>
	{#snippet children(d)}
		{@const cov = kindCoverage(d.rows, all, now)}
		{@const sum = j10Summary(cov)}
		{@const stages = stagesOf(cov)}
		{@const used = storeUse(cov)}
		{@const unused = d.stores.filter((s) => !used.some((u) => u.path === s.path))}
		{@const treeOk = treeKnown}

		<!-- The chain from template to reply: who owns each hop and how far this job gets along it. -->
		<ol class="j10-chain" aria-label="How a knowledge kind reaches a reply" data-testid="j10-chain">
			<li class="j10-hop">
				<span class="label">1 · Template needs <span class="j10-owner">inherited</span></span>
				<b>{sum.required} required · {sum.pairs - sum.required} optional</b>
				<span class="muted"
					>{sum.kinds} kinds over {sum.stages} <a href={journeyHref}>journey stages</a>{sum.tenantKinds
						? ` · ${sum.tenantKinds} added by you`
						: ''}</span
				>
			</li>
			<li class="j10-hop" class:bad={sum.toBind.length > 0}>
				<span class="label">2 · You bind a store <span class="j10-owner here">this page</span></span>
				<b>{sum.requiredBound} / {sum.required} required bound</b>
				<span class="muted"
					>{sum.toBind.length
						? `${sum.toBind.length} to bind below`
						: 'every required kind has a store'}</span
				>
			</li>
			<li class="j10-hop" class:wait={treeOk && sum.toFill.length > 0}>
				<span class="label">3 · Store holds passages <MilestoneChip milestone="M3" /></span>
				{#if treeOk}
					<b>{sum.requiredCited} / {sum.required} citable</b>
					<span class="muted" data-testid="j10-store-state"
						>{all.length === 0
							? 'no stores yet'
							: sum.toFill.length
								? `${sum.toFill.length} bound to a store not ready yet`
								: 'in strata_rag, filled'} · <a href={storesHref}>stores</a></span
					>
				{:else}
					<b>—</b><span class="muted" data-testid="j10-store-state"
						>store state unavailable · <a href={storesHref}>stores</a></span
					>
				{/if}
			</li>
			<li class="j10-hop">
				<span class="label">4 · Post-gate cites <MilestoneChip milestone="M4" /></span>
				<b>R1 · F2/F7</b>
				<span class="muted">every policy claim in a reply must cite a passage of its stage’s kinds</span>
			</li>
		</ol>
		<p class="j10-rule muted">
			Kinds and “required” come from the job template and change only with a new release. A binding is your
			refinement: it is saved to draft, runs in the <a href="/chat">test chat</a> at once, and reaches
			customers when you publish (<a href={refinementsHref}>refinements</a>).
		</p>

		{#if !canWrite}<p class="note">{writeHint('job_designer')} to bind kinds to stores.</p>{/if}

		{#if sum.toBind.length}
			<Callout
				kind="err"
				title={`Go-live gap: ${sum.toBind.length} required kind${sum.toBind.length === 1 ? '' : 's'} without a store`}
			>
				<span data-testid="j10-to-bind">
					{#each sum.toBind as c, i (keyOf(c.row))}
						{#if i}{', '}{/if}<a class="mono" href="#j10-{c.row.stage}">{c.row.kind}</a> ({stageLabel(
							c.row.stage,
						)}){/each}. A release still applies, but the agent has nothing to cite there and hands the
					customer over — <a href={readinessHref}>see readiness</a>.
				</span>
			</Callout>
		{:else}
			<Callout kind="info" title="Every required kind is bound">
				<span>The post-gate has a store for every policy it may cite.</span>
			</Callout>
		{/if}
		{#if treeOk && sum.toFill.length}
			<Callout
				kind="gap"
				title={`${sum.toFill.length} required kind${sum.toFill.length === 1 ? ' is' : 's are'} bound to a store that cannot answer yet`}
			>
				<span>
					{sum.toFill.map((c) => `${c.row.kind} → ${COVERAGE[c.state].text}`).join(', ')}. Fix on
					<a href={storesHref}>Knowledge stores</a> (create it in strata_rag, add a source); the binding here is
					already right.
				</span>
			</Callout>
		{/if}
		{#if d.stores.length === 0}
			<Callout kind="gap" title="No knowledge stores yet">
				<span
					>Lay out a store per kind on <a href={storesHref}>Knowledge stores</a> and add a source (website crawl,
					file, datatable); then bind it here.</span
				>
			</Callout>
		{/if}

		<div class="j10-main">
			<div class="j10-stages" data-testid="j10-stages">
				{#each stages as st (st.stage)}
					<section class="j10-stage card" class:gap={st.gaps > 0} id="j10-{st.stage}" aria-label={st.label}>
						<header class="j10-stage-head">
							<div>
								<h3>{st.label}</h3>
								<span class="muted">{st.hint}</span>
							</div>
							{#if st.gaps}<Chip
									text={`${st.gaps} gap${st.gaps === 1 ? '' : 's'}`}
									variant="err"
								/>{:else}<Chip text="ready" variant="ok" />{/if}
						</header>
						{#if J10_POST_GATE[st.stage]}
							<p class="j10-gate"><span class="label">R1 checks</span> {J10_POST_GATE[st.stage]}</p>
						{/if}
						<ul class="j10-kinds">
							{#each st.kinds as c (c.row.kind)}
								{@const r = c.row}
								{@const fix = fixOf(c)}
								{@const cand = c.candidate ? pathOf(all, c.candidate) : null}
								<li class="j10-kind" class:miss={r.required && fix === 'bind'}>
									<div class="j10-what">
										<span class="j10-name">
											<b class="mono">{r.kind}</b>
											{#if r.required}<Chip
													text="required"
													variant={fix === 'bind' ? 'err' : 'ok'}
												/>{:else}<span class="muted j10-opt">optional</span>{/if}
											{#if r.origin === 'tenant'}<Chip
													text="Tenant"
													variant="accent"
													title="Added by your tenant (extension activity)"
												/>{/if}
										</span>
										<span class="j10-desc">{r.description}</span>
									</div>
									<div class="j10-bind">
										<select
											class="input mono sm"
											aria-label="Store for {r.kind} in {st.label}"
											value={r.store ?? ''}
											disabled={!canWrite || saving === keyOf(r)}
											title={canWrite ? undefined : writeHint('job_designer')}
											onchange={(e) => bind(r, e.currentTarget.value)}
										>
											<option value="">— no store —</option>
											{#each d.stores as s (s.path)}<option value={s.path}>{s.path}</option>{/each}
											{#if r.store && !d.stores.some((s) => s.path === r.store)}
												<option value={r.store}>{r.store} (gone)</option>
											{/if}
										</select>
										{#if !r.store && cand}
											<button
												type="button"
												class="btn sm"
												{...writeAttrs(session, 'job_designer')}
												disabled={!canWrite || saving === keyOf(r)}
												onclick={() => bind(r, cand)}>Use {cand}</button
											>
										{:else if !r.store && r.required}
											<a class="j10-create" href={storesHref}
												>No matching store — create one on Knowledge stores</a
											>
										{/if}
										<PackPins
											row={r}
											{listing}
											disabled={!canWrite || saving === keyOf(r)}
											onpin={(pins, done) => pin(r, pins, done)}
											onchanged={() => res.refresh()}
										/>
									</div>
									<div class="j10-cov">
										{#if !r.store}
											<Chip text={COVERAGE[c.state].text} variant={r.required ? 'err' : 'muted'} />
										{:else if treeOk}
											<Chip
												text={COVERAGE[c.state].text}
												variant={COVERAGE[c.state].variant}
												title={COVERAGE[c.state].hint}
											/>
											{#if c.store}
												<span class="muted j10-meta"
													>{fmt.n(c.store.documents)} docs · {fmt.n(c.store.chunks)} passages · {ago(
														c.store.updated_at,
														now,
													)}</span
												>
											{/if}
										{/if}
									</div>
								</li>
							{/each}
						</ul>
					</section>
				{/each}
			</div>

			<aside class="j10-side">
				{#if d.inherited?.length}
					<Card
						title="Platform knowledge this job inherits"
						sub="Published by the platform for every job of this industry and department. Read-only: your own stores replace it by topic."
					>
						<ul class="j10-uses" data-testid="j10-inherited">
							{#each LAYER_ORDER as lv (lv)}
								{#each d.inherited.filter((p) => layerLevel(p.layer) === lv) as p (`${p.layer}|${p.key}`)}
									<li>
										<span class="j10-use-head">
											<b class="mono">{p.pin}</b>
											<Chip text={layerName(p.layer)} variant="muted" />
											{#if !p.applies}<Chip
													text="No matching kind"
													variant="warn"
													title="It matches none of this job's knowledge kinds, so no search reads it."
												/>{/if}
										</span>
										<span class="muted"
											>{p.stage === '*' ? 'Every stage' : stageLabel(p.stage)} · {p.kind === '*'
												? 'every kind'
												: p.kind}</span
										>
									</li>
								{/each}
							{/each}
						</ul>
					</Card>
				{/if}
				<Card
					title="Stores this job reads"
					sub="One store can answer several stages; the binding is per stage, so a stage can read a different store."
				>
					{#if used.length === 0}
						<div class="empty">No kind is bound yet.</div>
					{:else}
						<ul class="j10-uses" data-testid="j10-store-use">
							{#each used as u (u.path)}
								{@const st = u.store && treeOk ? COVERAGE[storeState(u.store, now)] : null}
								<li>
									<span class="j10-use-head">
										<b class="mono">{u.path}</b>
										{#if st}<Chip text={st.text} variant={st.variant} title={st.hint} />{/if}
									</span>
									<span class="muted">{u.store?.label ?? ''}</span>
									<span class="j10-reads">
										{#each u.reads as r (keyOf(r))}<a class="j10-read" href="#j10-{r.stage}"
												>{stageLabel(r.stage)} · <span class="mono">{r.kind}</span></a
											>{/each}
									</span>
								</li>
							{/each}
						</ul>
					{/if}
					{#if unused.length}
						<p class="muted j10-unused">
							Not read by this job: <span class="mono">{unused.map((s) => s.path).join(', ')}</span>
						</p>
					{/if}
				</Card>
			</aside>
		</div>
		<CoverageMatrix job={app.job} tick={d} />
	{/snippet}
</LoadState>

<style>
	.j10-chain {
		list-style: none;
		margin: 0 0 6px;
		padding: 0;
		display: grid;
		grid-template-columns: repeat(4, minmax(0, 1fr));
		gap: 22px;
	}
	.j10-hop {
		position: relative;
		display: grid;
		gap: 2px;
		align-content: start;
		padding: 10px 12px;
		background: var(--panel);
		border: 1px solid var(--line);
		border-top: 3px solid var(--ok);
		border-radius: 8px;
		min-width: 0;
	}
	.j10-hop.wait {
		border-top-color: var(--warn);
	}
	.j10-hop.bad {
		border-top-color: var(--err);
	}
	.j10-hop b {
		font-size: 16px;
	}
	.j10-hop .muted {
		font-size: 12.5px;
	}
	.j10-hop:not(:last-child)::after {
		content: '→';
		position: absolute;
		right: -17px;
		top: 50%;
		transform: translateY(-50%);
		color: var(--muted);
	}
	.j10-owner {
		font-size: 10.5px;
		padding: 0 5px;
		border-radius: 4px;
		border: 1px solid var(--line);
		color: var(--muted);
		text-transform: none;
		letter-spacing: 0;
	}
	.j10-owner.here {
		border-color: var(--accent);
		color: var(--accent);
	}
	.j10-rule {
		font-size: 12.5px;
		margin: 0 0 14px;
	}
	.j10-main {
		display: grid;
		grid-template-columns: minmax(0, 1fr) 300px;
		gap: 16px;
		align-items: start;
		margin-top: 14px;
	}
	.j10-stages {
		display: grid;
		gap: 12px;
		min-width: 0;
	}
	.j10-stage {
		padding: 12px 14px;
		border-left: 3px solid var(--ok);
		min-width: 0;
		scroll-margin-top: 80px;
	}
	.j10-stage.gap {
		border-left-color: var(--err);
	}
	.j10-stage-head {
		display: flex;
		justify-content: space-between;
		align-items: flex-start;
		gap: 10px;
	}
	.j10-stage-head h3 {
		margin: 0;
		font-size: 15px;
	}
	.j10-stage-head .muted {
		font-size: 12.5px;
	}
	.j10-gate {
		margin: 6px 0 8px;
		font-size: 12.5px;
		color: var(--muted);
	}
	.j10-gate .label {
		margin-right: 6px;
	}
	.j10-kinds {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.j10-kind {
		display: grid;
		grid-template-columns: minmax(0, 1.3fr) minmax(0, 1.2fr) minmax(0, 1fr);
		gap: 6px 14px;
		align-items: center;
		padding: 8px 6px;
		border-top: 1px solid var(--line);
	}
	.j10-kind.miss {
		background: color-mix(in srgb, var(--err) 7%, transparent);
	}
	.j10-what {
		display: grid;
		gap: 2px;
		min-width: 0;
	}
	.j10-name {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		align-items: center;
	}
	.j10-desc,
	.j10-opt,
	.j10-meta {
		font-size: 12.5px;
	}
	.j10-bind {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		min-width: 0;
	}
	.j10-bind .input.sm {
		padding: 4px 8px;
		font-size: 12.5px;
		width: 100%;
		min-width: 0;
	}
	.j10-create {
		font-size: 12px;
	}
	.j10-cov {
		display: flex;
		flex-wrap: wrap;
		gap: 4px 8px;
		align-items: center;
	}
	.j10-uses {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 10px;
	}
	.j10-uses li {
		display: grid;
		gap: 3px;
		padding-bottom: 10px;
		border-bottom: 1px solid var(--line);
		min-width: 0;
	}
	.j10-use-head {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		align-items: center;
		word-break: break-all;
	}
	.j10-use-head b {
		font-size: 12.5px;
	}
	.j10-reads {
		display: flex;
		flex-wrap: wrap;
		gap: 4px;
	}
	.j10-read {
		font-size: 11.5px;
		padding: 1px 6px;
		border: 1px solid var(--line);
		border-radius: 10px;
		text-decoration: none;
		color: inherit;
	}
	.j10-unused {
		font-size: 12.5px;
		margin: 10px 0 0;
	}
	@media (max-width: 1100px) {
		.j10-main {
			grid-template-columns: 1fr;
		}
	}
	@media (max-width: 760px) {
		.j10-chain {
			grid-template-columns: 1fr;
			gap: 14px;
		}
		.j10-hop:not(:last-child)::after {
			content: '↓';
			right: auto;
			left: 50%;
			top: auto;
			bottom: -15px;
			transform: translateX(-50%);
			line-height: 1;
		}
		.j10-kind {
			grid-template-columns: 1fr;
		}
	}
</style>
