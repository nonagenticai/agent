<script lang="ts">
	// T7 search preview: ask what a customer would ask and see what knowledge.search would cite, with strata_rag's
	// working shown — per-hit dense / lexical / rerank scores, the laya routing taken (beam, fallback) and where the
	// time went. `?q=<question>` prefills and runs it (the retrieval eval links its questions here); any question
	// can be promoted to a golden question on the eval page.
	import { onMount } from 'svelte';
	import { api, app } from '$lib/app.svelte';
	import { fmt, pages } from '$lib/format';
	import type { KnowledgeHit, KnowledgeQueryResult, KnowledgeStore } from '$lib/api/types/tenant';
	import { errText } from '$lib/tenant/ui';
	import { BEAM_MAX } from '$lib/tenant/retrieval';
	import Card from '$lib/ui/Card.svelte';
	import Seg from '$lib/ui/Seg.svelte';
	import Meter from '$lib/ui/Meter.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import type { KnowledgeKindRow } from '$lib/api/types/ontology';
	import { layerChip, provenanceHref, provenanceText } from '$lib/pk/pk';

	/** D91: the tenant's root stores; with more than one the search needs a tree (else 422 `root_required`). */
	let { roots = [] }: { roots?: Pick<KnowledgeStore, 'id' | 'key' | 'label'>[] } = $props();
	let root = $state('');
	const needRoot = $derived(roots.length > 1);
	// PK (T5): search one knowledge kind of the active job as its draft resolves it (its tenant store + pinned
	// platform packs, minus exclusions and hidden pins) — no store tree needed.
	let scope = $state<'tree' | 'kind'>('tree');
	let kinds = $state<KnowledgeKindRow[] | null>(null);
	let kindKey = $state('');
	async function loadKinds() {
		if (kinds) return;
		try {
			kinds = (await api().knowledgeKinds()).rows;
		} catch {
			kinds = [];
		}
	}
	$effect(() => {
		if (scope === 'kind') void loadKinds();
	});

	let q = $state('How long does a refund take?');
	let mode = $state('hybrid');
	let topK = $state(8);
	let beam = $state(3);
	let minScore = $state(0.35);
	let result = $state<KnowledgeQueryResult | null>(null);
	let queryErr = $state('');
	let running = $state(false);

	async function search() {
		running = true;
		queryErr = '';
		result = null;
		try {
			result = await api().knowledgeQuery({
				query: q,
				mode: mode as 'embed' | 'laya' | 'hybrid',
				top_k: Math.min(50, Math.max(1, Math.round(Number(topK) || 8))),
				min_score: Math.min(1, Math.max(0, Number(minScore) || 0)),
				beam: Math.min(BEAM_MAX, Math.max(1, Math.round(Number(beam) || 3))),
				...(scope === 'kind' && kindKey
					? { job: app.job, stage: kindKey.split('|')[0], kind: kindKey.split('|')[1] }
					: root
						? { store_id: root }
						: {}),
			});
		} catch (err) {
			queryErr = errText(err);
		}
		running = false;
	}
	function run(e: SubmitEvent) {
		e.preventDefault();
		void search();
	}

	onMount(() => {
		const want = new URLSearchParams(window.location.search).get('q');
		if (want?.trim()) {
			q = want.trim();
			void search();
			requestAnimationFrame(() =>
				document.getElementById('t7-playground')?.scrollIntoView?.({ block: 'start' }),
			);
		}
	});

	const cite = (h: KnowledgeHit) => [h.document, pages(h.page_start, h.page_end)].filter(Boolean).join(' · ');
	const score = (v: number | null) => (v == null ? '—' : v.toFixed(2));
	const STAGES = [
		['embed', 'Embed', 's-accent'],
		['route', 'Route (laya)', 's-tool'],
		['search', 'Search', 's-work'],
		['rerank', 'Rerank', 's-ok'],
	] as const;
	const promoteHref = $derived(`/admin/knowledge/eval?add=${encodeURIComponent(q.trim())}`);
</script>

<Card
	title="Search preview"
	sub="Ask what a customer would ask: these are the passages the agent's knowledge.search would cite (POST /api/knowledge/query → strata_rag)."
>
	{#snippet actions()}<a class="btn sm" href="/admin/knowledge/eval">Retrieval eval</a>{/snippet}
	<form class="sp-form" onsubmit={run}>
		<label class="field sp-q"
			><span class="label">Question</span><input class="input" bind:value={q} aria-label="Question" /></label
		>
		<div class="field">
			<span class="label">Search</span>
			<Seg
				label="Search scope"
				items={[
					['tree', 'Store tree'],
					['kind', 'As a knowledge kind'],
				]}
				bind:active={scope}
			/>
		</div>
		{#if scope === 'kind'}
			<label class="field"
				><span class="label">Kind of {app.job}</span><select
					class="input"
					aria-label="Knowledge kind"
					bind:value={kindKey}
					required
				>
					<option value="">{kinds === null ? 'Loading kinds…' : 'Pick a kind…'}</option>
					{#each kinds ?? [] as k (`${k.stage}|${k.kind}`)}<option value={`${k.stage}|${k.kind}`}
							>{k.stage} · {k.kind}{k.packs?.length
								? ` (+${k.packs.length} pack${k.packs.length === 1 ? '' : 's'})`
								: ''}</option
						>{/each}
				</select></label
			>
		{:else if needRoot}
			<label class="field"
				><span class="label">Store tree</span><select
					class="input"
					aria-label="Store tree"
					bind:value={root}
					required
				>
					<option value="">Pick a tree…</option>
					{#each roots as r (r.id)}<option value={r.id}>{r.label} ({r.key})</option>{/each}
				</select></label
			>
		{/if}
		<div class="field">
			<span class="label">Routing</span>
			<Seg
				label="Routing"
				items={[
					['embed', 'Embedding'],
					['laya', 'Laya routing'],
					['hybrid', 'Hybrid'],
				]}
				bind:active={mode}
			/>
		</div>
		<label class="field"
			><span class="label">Results</span><input
				class="input num"
				type="number"
				min="1"
				max="50"
				bind:value={topK}
			/></label
		>
		<label
			class="field"
			title="How many branches of the store tree laya follows (1–8). Embedding search ignores it."
			><span class="label">Branches (beam)</span><input
				class="input num"
				type="number"
				min="1"
				max={BEAM_MAX}
				disabled={mode === 'embed'}
				bind:value={beam}
			/></label
		>
		<label class="field"
			><span class="label">Min score</span><input
				class="input num"
				type="number"
				min="0"
				max="1"
				step="0.05"
				bind:value={minScore}
			/></label
		>
		<button class="btn primary" type="submit" disabled={running || !q.trim() || (needRoot && !root)}
			>{running ? 'Searching…' : 'Search'}</button
		>
	</form>
	{#if queryErr}
		<Callout kind="err" title="Search refused"><span>{queryErr}</span></Callout>
	{/if}
	{#if result}
		{@const r = result.routing}
		{@const t = result.timings_ms}
		{#if result.degraded?.length}
			<Callout kind="gap" title="Searched without some platform packs">
				<span data-testid="query-degraded"
					>{result.degraded.join(', ')} — excluded, hidden, retired or unreadable right now. See the job's Knowledge
					kinds.</span
				>
			</Callout>
		{/if}
		<div class="sp-routing" data-testid="query-routing">
			<span><span class="label">Mode</span> <b>{r.mode_used}</b></span>
			<span
				><span class="label">Searched</span>
				<span class="mono">{r.leaves.join(', ') || '—'}</span>
				<span class="muted">({r.leaves.length} of {result.searched.length} stores)</span></span
			>
			<span
				><span class="label">Laya</span>
				{#if !r.laya_consulted}not consulted{:else if r.laya_fallback}<span class="s-warn"
						>fell back to every store ({r.laya_fallback.replace(/_/g, ' ')})</span
					>{:else}routed with {Number(beam)} branch{Number(beam) === 1 ? '' : 'es'}{/if}</span
			>
			{#if result.skipped_unbound.length}
				<span class="muted">skipped (not in strata_rag): {result.skipped_unbound.join(', ')}</span>
			{/if}
		</div>
		<div class="sp-time" data-testid="query-timings">
			<span
				class="sp-track"
				role="img"
				aria-label={`Time: ${STAGES.map(([k, l]) => `${l} ${t[k]} ms`).join(', ')}`}
			>
				{#each STAGES as [k, l, cls] (k)}
					{#if t[k] > 0}<i
							class={cls}
							style="width:{(t[k] / Math.max(1, t.total)) * 100}%"
							title="{l}: {t[k]} ms"
						></i>{/if}
				{/each}
			</span>
			<span class="mono">{fmt.ms(t.total)}</span>
			<span class="legend sp-legend">
				{#each STAGES as [k, l, cls] (k)}
					<span class={cls}><i class="sw"></i><span class="muted">{l} {fmt.ms(t[k])}</span></span>
				{/each}
			</span>
		</div>
		{#if result.hits.length === 0}
			<div class="empty">
				Nothing matched closely enough. Only passages scoring at least {minScore} are shown: try other words or
				lower the minimum score. If a whole topic is missing, look at the coverage gaps above.
			</div>
		{:else}
			<ol class="list-plain sp-hits" data-testid="query-hits">
				{#each result.hits as h (h.chunk_id)}
					<li>
						<div class="sp-hit">
							<span class="mono muted sp-path"
								>{h.layer === 'platform' ? '' : h.store_path.join(' / ')}
								<span
									class="chip {h.layer === 'platform' ? 'accent' : 'muted'}"
									data-testid="hit-layer"
									title={h.layer === 'platform' ? 'A pinned platform pack' : 'One of your stores'}
									>{layerChip(h)}</span
								>{#if h.layer === 'platform' && provenanceHref(h.metadata?.provenance)}
									<a
										class="sp-prov"
										href={provenanceHref(h.metadata?.provenance)}
										target="_blank"
										rel="noopener noreferrer">source</a
									>
									<span class="muted">{provenanceText(h.metadata?.provenance)}</span>{/if}</span
							>
							<b class="cite">{cite(h)}</b>
							<span class="cell-sub">{h.text}</span>
							<span class="sp-scores mono" data-testid="hit-scores">
								<span title="Embedding similarity (your embed slot)"
									>dense {score(h.scores.dense)} <Meter value={h.scores.dense ?? 0} /></span
								>
								<span title="Word match (hybrid only)"
									>lexical {score(h.scores.lexical)}
									<Meter value={h.scores.lexical ?? 0} cls="s-work" /></span
								>
								<span title="Rerank score (your rerank slot; the first candidates only)"
									>rerank {score(h.scores.rerank)} <Meter value={h.scores.rerank ?? 0} cls="s-ok" /></span
								>
							</span>
						</div>
						<span class="mono sp-final" title="Final score">{h.score.toFixed(2)}</span>
					</li>
				{/each}
			</ol>
		{/if}
		<p class="note">
			Is this the passage a customer should get? <a href={promoteHref}>Add the question to the golden set</a> so
			every eval run checks it.
		</p>
	{/if}
</Card>

<style>
	.sp-prov {
		font-size: 11.5px;
	}
	.sp-form {
		display: flex;
		flex-wrap: wrap;
		gap: 10px;
		align-items: end;
	}
	.sp-q {
		flex: 1 1 260px;
		min-width: 0;
	}
	.num {
		width: 90px;
	}
	.sp-routing {
		display: flex;
		flex-wrap: wrap;
		gap: 6px 16px;
		margin: 14px 0 8px;
		font-size: 13px;
		overflow-wrap: anywhere;
	}
	.sp-routing .label {
		margin-right: 4px;
	}
	.sp-time {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 4px 10px;
		margin-bottom: 10px;
	}
	.sp-track {
		display: flex;
		flex: 1 1 200px;
		height: 8px;
		border-radius: 4px;
		overflow: hidden;
		background: var(--sunk);
	}
	.sp-track i {
		display: block;
		height: 100%;
		background: currentColor;
	}
	.sp-legend {
		flex: 1 1 100%;
		margin-top: 0;
	}
	.sp-hits li {
		display: flex;
		justify-content: space-between;
		gap: 10px;
		align-items: start;
	}
	.sp-hit {
		display: grid;
		gap: 3px;
		min-width: 0;
	}
	.sp-path {
		overflow-wrap: anywhere;
	}
	.cite {
		display: block;
	}
	.sp-scores {
		display: flex;
		flex-wrap: wrap;
		gap: 4px 14px;
		font-size: 11.5px;
		color: var(--muted);
	}
	.sp-scores :global(.meter) {
		width: 48px;
	}
	.sp-final {
		font-weight: 600;
	}
</style>
