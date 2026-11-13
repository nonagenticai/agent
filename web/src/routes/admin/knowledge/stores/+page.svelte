<script lang="ts">
	// T7 Knowledge stores. Design intent: the tenant_admin answers "does my agent know enough to answer customers,
	// stage by stage, and where is knowledge missing?". Knowledge is provisioned down: the job's journey stages need
	// knowledge kinds (J10) → kinds are bound to stores laid out here → each store is created in strata_rag (pinned to
	// a region; `store_ensure`, K45, M3) → sources/uploads fill it with documents split into passages → the agent's
	// knowledge.search cites those passages. The page leads with that chain and the coverage gaps, then the store tree,
	// one store's contents (with direct uploads), a search preview showing strata_rag's scores/routing/timings, and the
	// platform-fixed search models. Retrieval quality over a golden question set lives on /admin/knowledge/eval.
	import { onMount } from 'svelte';
	import { api, app, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { canWrite as canWriteFor, writeAttrs } from '$lib/auth/roles';
	import { Latest, Resource } from '$lib/load.svelte';
	import { jobPath } from '$lib/scope';
	import { fmt } from '$lib/format';
	import { toast } from '$lib/ui/toaster.svelte';
	import type { Paged } from '$lib/api/types/core';
	import type { KnowledgeDocument, KnowledgeStore } from '$lib/api/types/tenant';
	import type { KnowledgeKinds } from '$lib/api/types/ontology';
	import { when } from '$lib/tenant/ui';
	import { storeTree } from '$lib/tenant/sources';
	import { STAGES } from '$lib/ontology/catalogue';
	import {
		COVERAGE,
		REGION_LABEL,
		ago,
		byStage,
		draftFor,
		kindCoverage,
		neededBy,
		pathOf,
		storeState,
		summarize,
		type KindCoverage,
	} from '$lib/tenant/knowledge-T7';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LiveNote from '$lib/release/LiveNote.svelte';
	import Freshness from '$lib/ui/Freshness.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import KeyValues from '$lib/ui/KeyValues.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';
	import MilestoneChip from '$lib/ui/MilestoneChip.svelte';
	import SearchPreview from './SearchPreview.svelte';
	import ModelCard from './ModelCard.svelte';
	import UploadDocs from './UploadDocs.svelte';

	const stores = new Resource<Paged<KnowledgeStore>>(() => api().knowledgeStores());
	const kinds = new Resource<KnowledgeKinds>(
		() => api().knowledgeKinds(),
		(d) => d.rows.length === 0,
	);
	const now = Date.now();
	const canWrite = $derived(canWriteFor(session, 'knowledge_manager'));
	const items = $derived(stores.value.state === 'ok' ? stores.value.data.items : []);
	const unbound = $derived(items.filter((s) => !s.strata_store_id && !s.ensuring));
	const groups = $derived(items.filter((s) => s.kind === 'group'));
	const kindRows = $derived(
		kinds.value.state === 'ok' || kinds.value.state === 'empty' ? kinds.value.data.rows : [],
	);
	const cov = $derived(kindCoverage(kindRows, items, now));
	const stages = $derived(byStage(cov, STAGES).filter((s) => s.kinds.length));
	const sum = $derived(summarize(items, cov, now));
	/** Gaps to fix, required first; one row per kind (a kind needed by two stages is one fix). */
	const fixes = $derived.by(() => {
		const seen = new Map<string, KindCoverage & { stages: string[] }>();
		for (const c of cov) {
			if (!COVERAGE[c.state].gap && c.state !== 'stale') continue;
			const prev = seen.get(c.row.kind);
			if (prev) {
				prev.stages.push(c.row.stage);
				if (c.row.required) prev.row = c.row;
			} else seen.set(c.row.kind, { ...c, stages: [c.row.stage] });
		}
		return [...seen.values()].sort(
			(a, b) => Number(b.row.required) - Number(a.row.required) || a.row.kind.localeCompare(b.row.kind),
		);
	});
	/** Required gaps and stale stores: the agent answers worse today. Optional kinds without a store come after. */
	const mustFix = $derived(fixes.filter((f) => f.row.required || f.state === 'stale'));
	const niceToHave = $derived(fixes.filter((f) => !(f.row.required || f.state === 'stale')));
	const jobLabel = $derived(app.jobs.find((j) => j.key === app.job)?.label ?? app.job);
	const stageLabel = (k: string) => STAGES.find(([s]) => s === k)?.[1] ?? k;

	let sel = $state<KnowledgeStore | null>(null);
	let docs = $state<{
		s: 'idle' | 'loading' | 'ok' | 'err';
		items: KnowledgeDocument[];
		next: string | null;
		err: string;
	}>({ s: 'idle', items: [], next: null, err: '' });
	let loadingMore = $state(false);
	let confirmStore = $state(false);
	let confirmDoc = $state<KnowledgeDocument | null>(null);
	let form = $state({ key: '', label: '', parent_id: '' as string, kind: 'leaf' as 'leaf' | 'group' });
	let addOpen = $state(false);

	onMount(async () => {
		await Promise.all([stores.refresh(), kinds.refresh()]);
		// `?store=<id>` (or its key) deep-links to one store (T9 Sources links here); otherwise open on the store
		// that needs attention first, so the right-hand panel never starts empty.
		const want = new URLSearchParams(window.location.search).get('store');
		const linked = want ? items.find((s) => s.id === want || s.key === want) : undefined;
		if (linked) {
			await pick(linked);
			document.getElementById('t7-store-detail')?.scrollIntoView?.({ block: 'start' });
			return;
		}
		const leaves = items.filter((s) => s.kind === 'leaf');
		const first = leaves.find((s) => storeState(s, now) !== 'ok') ?? leaves[0];
		if (first && !sel) await pick(first);
	});

	const latestDocs = new Latest();
	async function pick(n: KnowledgeStore) {
		const ticket = latestDocs.next();
		sel = n;
		confirmStore = false;
		confirmDoc = null;
		docs = { s: 'loading', items: [], next: null, err: '' };
		try {
			const p = await api().knowledgeDocuments(n.id);
			if (!latestDocs.stale(ticket)) docs = { s: 'ok', items: p.items, next: p.next, err: '' };
		} catch (e) {
			if (!latestDocs.stale(ticket)) docs = { s: 'err', items: [], next: null, err: (e as Error).message };
		}
	}
	function show(s: KnowledgeStore) {
		void pick(s);
		document.getElementById('t7-store-detail')?.scrollIntoView({ behavior: 'smooth', block: 'start' });
	}
	async function more() {
		if (!sel || !docs.next || loadingMore) return;
		loadingMore = true;
		try {
			const p = await api().knowledgeDocuments(sel.id, docs.next);
			docs = { s: 'ok', items: [...docs.items, ...p.items], next: p.next, err: '' };
		} catch (e) {
			toast(`Could not load more documents: ${(e as Error).message}`, 'err');
		}
		loadingMore = false;
	}
	async function refreshAll() {
		await Promise.all([stores.refresh(), kinds.refresh()]);
		if (sel) sel = items.find((x) => x.id === sel!.id) ?? null;
	}
	async function ensureMissing() {
		const box: { v: { enqueued: number; stores: string[] } | null } = { v: null };
		await guard(
			async () => {
				box.v = await api().ensureMissingStores();
			},
			() =>
				box.v && box.v.enqueued
					? `Queued ${box.v.enqueued} store_ensure row${box.v.enqueued === 1 ? '' : 's'}: ${box.v.stores.join(', ')}.`
					: 'Every store is already bound in strata_rag.',
			'Not queued',
		);
		await refreshAll();
	}
	/** A gap's "Create store": prefill the form with the kind as key, under the tenant's first group. */
	function draftStore(kind: string) {
		const d = draftFor(kind);
		form = { key: d.key, label: d.label, parent_id: groups[0]?.id ?? '', kind: 'leaf' };
		addOpen = true;
		requestAnimationFrame(() => document.getElementById('t7-add-store')?.scrollIntoView({ block: 'center' }));
	}
	async function create(e: SubmitEvent) {
		e.preventDefault();
		const ok = await guard(
			() =>
				api().createKnowledgeStore({
					key: form.key.trim(),
					label: form.label.trim(),
					parent_id: form.parent_id || null,
					kind: form.kind,
				}),
			`Store ${form.key} created; strata_rag creates it next (store_ensure queued). Bind it to its kind on Knowledge kinds.`,
			'Not created',
		);
		if (ok) {
			form = { key: '', label: '', parent_id: form.parent_id, kind: 'leaf' };
			await refreshAll();
		}
	}
	async function removeStore() {
		if (!sel) return;
		const s = sel;
		confirmStore = false;
		const ok = await guard(() => api().deleteKnowledgeStore(s.id), `Store ${s.key} deleted.`, 'Not deleted');
		if (ok) {
			sel = null;
			docs = { s: 'idle', items: [], next: null, err: '' };
			await refreshAll();
		}
	}
	async function removeDoc(d: KnowledgeDocument) {
		confirmDoc = null;
		const ok = await guard(
			() => api().deleteKnowledgeDocument(d.id),
			'Document deleted from strata_rag.',
			'Not deleted',
		);
		if (ok && sel) {
			await refreshAll();
			const fresh = items.find((x) => x.id === sel!.id);
			if (fresh) await pick(fresh);
		}
	}

	const selState = $derived(sel ? storeState(sel, now) : 'ok');
	const selNeeded = $derived(sel ? neededBy(cov, sel.id) : []);
	const kindsHref = $derived(jobPath(app.job, '/knowledge-kinds'));
	const sourcesHref = $derived(jobPath(app.job, '/sources'));
	const regionText = $derived(sum.regions.map((r) => REGION_LABEL[r]).join(' + ') || 'not pinned');
</script>

<PageHead
	id="T7"
	sub="What your agent can know: the stores it searches in strata_rag, what each one holds, and which journey stages still lack knowledge."
>
	{#snippet actions()}
		<Freshness resource={stores} />
		<a class="btn" href="/admin/knowledge/jobs">Ingest jobs</a>
		<a class="btn" href={sourcesHref}>Sources</a>
		<button
			class="btn primary"
			type="button"
			{...writeAttrs(session, 'knowledge_manager')}
			onclick={ensureMissing}>Create missing stores</button
		>
	{/snippet}
</PageHead>

<LiveNote what="Stores and documents are the data plane: uploads and deletions apply at once." />

{#if stores.value.state === 'unavailable'}
	<Unavailable reason={stores.value.reason} retry={stores.retry} />
{:else}
	<!-- The provision-down chain, one tile per hop, each with its count and state. -->
	<ol class="t7-flow" aria-label="How knowledge reaches the agent" data-testid="t7-flow">
		<li class="t7-hop" class:bad={sum.gapKinds.length > 0}>
			<span class="label">1 · Job needs</span>
			<b>{sum.required} required kinds</b>
			<span class="muted"
				>{sum.requiredCovered} covered{sum.gapKinds.length ? ` · ${sum.gapKinds.length} missing` : ''} ·
				<a href={kindsHref}>{jobLabel}</a></span
			>
		</li>
		<li class="t7-hop">
			<span class="label">2 · Store layout</span>
			<b>{sum.leaves} stores</b>
			<span class="muted"
				>in {sum.groups} group{sum.groups === 1 ? '' : 's'} · laid out here <MilestoneChip
					milestone="M1"
				/></span
			>
		</li>
		<li class="t7-hop" class:bad={unbound.length > 0} class:wait={sum.ensuring > 0}>
			<span class="label">3 · strata_rag</span>
			<b>{sum.bound} of {items.length} created</b>
			<span class="muted"
				><span title="Your tenant's data residency, set at provisioning (D59)"
					>region {regionText} (tenant residency)</span
				>{sum.ensuring ? ` · ${sum.ensuring} being created` : ''}
				<MilestoneChip milestone="M3" /></span
			>
		</li>
		<li class="t7-hop" class:wait={sum.stale.length > 0}>
			<span class="label">4 · Content</span>
			<b>{fmt.n(sum.documents)} documents</b>
			<span class="muted"
				>{fmt.n(sum.passages)} passages · from <a href={sourcesHref}>sources</a> and uploads</span
			>
		</li>
		<li class="t7-hop">
			<span class="label">5 · Agent search</span>
			<b class="mono">knowledge.<wbr />search</b>
			<span class="muted">cites passages in replies · <a href="#t7-playground">try it</a></span>
		</li>
	</ol>

	{#if unbound.length}
		<Callout
			kind="gap"
			title={`${unbound.length} store${unbound.length === 1 ? '' : 's'} laid out but not created in strata_rag`}
			><span
				><span class="mono">{unbound.map((s) => s.key).join(', ')}</span>: search skips
				{unbound.length === 1 ? 'it' : 'them'} until strata_rag has created {unbound.length === 1
					? 'it'
					: 'them'}. “Create missing stores” queues one <span class="mono">store_ensure</span> each; progress
				shows on the Knowledge row of the <a href="/admin">overview</a>.</span
			></Callout
		>
	{/if}

	<Card
		title={`Coverage by journey stage · ${jobLabel}`}
		sub="Each stage of the customer journey needs certain kinds of knowledge. A kind is covered when its store exists in strata_rag, holds documents and is fresh."
	>
		{#snippet actions()}<a class="btn sm" href={kindsHref}>Bind kinds to stores</a>{/snippet}
		{#if kinds.value.state === 'unavailable'}
			<Unavailable reason={kinds.value.reason} retry={kinds.retry} />
		{:else if kinds.value.state === 'loading'}
			<p class="muted">Loading…</p>
		{:else if kinds.value.state === 'empty'}
			<div class="empty">
				This job lists no knowledge kinds yet, so nothing here is required. Kinds come with the job's journey
				template.
			</div>
		{:else}
			<div class="t7-stages" data-testid="t7-coverage">
				{#each stages as st (st.stage)}
					<section class="t7-stage" class:gap={st.gaps > 0} aria-label={st.label}>
						<header>
							<b>{st.label}</b>
							{#if st.gaps}<Chip
									text={`${st.gaps} gap${st.gaps === 1 ? '' : 's'}`}
									variant="err"
								/>{:else}<Chip text="ready" variant="ok" />{/if}
						</header>
						<ul>
							{#each st.kinds as k (k.row.kind)}
								<li title={`${k.row.description} ${COVERAGE[k.state].hint}`}>
									{#if k.store}
										<button type="button" class="t7-kind mono" onclick={() => show(k.store!)}
											>{k.row.kind}</button
										>
									{:else}
										<span class="t7-kind mono">{k.row.kind}</span>
									{/if}
									<span class="t7-kind-meta">
										<Chip text={COVERAGE[k.state].text} variant={COVERAGE[k.state].variant} />
										{#if !k.row.required}<span class="muted t7-opt">optional</span>{/if}
									</span>
								</li>
							{/each}
						</ul>
					</section>
				{/each}
			</div>

			{#if fixes.length}
				<h3 class="t7-h3">What to fix</h3>
				{#snippet fixList(list: typeof fixes, testid: string)}
					<ul class="t7-fixes" data-testid={testid}>
						{#each list as f (f.row.kind)}
							<li>
								<span>
									<Chip text={COVERAGE[f.state].text} variant={COVERAGE[f.state].variant} />
									<b class="mono">{f.row.kind}</b>
									<span class="muted"
										>{f.row.required ? 'required' : 'optional'} in {f.stages.map(stageLabel).join(', ')}
										· {COVERAGE[f.state].hint}</span
									>
								</span>
								<span class="t7-fix-act">
									{#if f.state === 'no_store' || f.state === 'missing_store'}
										{#if f.candidate}
											<a class="btn sm" href={kindsHref}>Bind to {f.candidate.key}</a>
										{:else}
											<button
												class="btn sm"
												type="button"
												{...writeAttrs(session, 'knowledge_manager')}
												onclick={() => draftStore(f.row.kind)}>Create store</button
											>
										{/if}
									{:else if f.state === 'unbound'}
										<span class="muted">Create missing stores ↑</span>
									{:else if f.state === 'ensuring'}
										<a class="btn sm" href="/admin">Watch progress</a>
									{:else if f.state === 'empty' || f.state === 'stale'}
										<a class="btn sm" href={sourcesHref}
											>{f.state === 'empty' ? 'Add a source' : 'Re-sync its source'}</a
										>
									{/if}
								</span>
							</li>
						{/each}
					</ul>
				{/snippet}
				{#if mustFix.length}
					{@render fixList(mustFix, 't7-fixes')}
				{:else}
					<p class="muted t7-allgood">Every required kind is covered.</p>
				{/if}
				{#if niceToHave.length}
					<details class="t7-optional">
						<summary
							>{niceToHave.length} optional kind{niceToHave.length === 1 ? '' : 's'} without a store
							<span class="muted mono">{niceToHave.map((f) => f.row.kind).join(', ')}</span></summary
						>
						{@render fixList(niceToHave, 't7-fixes-optional')}
					</details>
				{/if}
			{:else}
				<p class="muted t7-allgood">
					Every kind this job needs has a fresh store with documents. The agent can cite all of them.
				</p>
			{/if}
		{/if}
	</Card>

	<div class="grid g2 t7-main">
		<Card title="Stores" sub="Groups route a question to the stores below them; leaves hold the documents.">
			{#if stores.value.state === 'empty'}
				<div class="empty">No stores yet. Add the first one below, or bind kinds once stores exist.</div>
			{:else if stores.value.state === 'loading'}
				<p class="muted">Loading…</p>
			{:else}
				<div data-testid="store-tree" class="t7-tree">
					{#each storeTree(items) as { node, depth } (node.id)}
						{@const st = storeState(node, now)}
						{@const uses = neededBy(cov, node.id)}
						<button
							type="button"
							class="kn-node"
							class:group={node.kind === 'group'}
							class:on={sel?.id === node.id}
							style="--d:{depth}"
							aria-pressed={sel?.id === node.id}
							onclick={() => pick(node)}
						>
							<span class="kn-name"
								>{#if node.kind === 'group'}<span class="t7-glyph" aria-hidden="true">▾</span>{/if}
								<span>{node.label}</span>
								<span class="mono muted">{node.key}</span>
								{#if node.kind === 'group'}<span class="muted t7-small">group</span>{/if}</span
							>
							<span class="meta">
								{#if node.ensuring}<Chip
										text="ensuring"
										variant="warn"
										title={COVERAGE.ensuring.hint}
									/>{:else if !node.strata_store_id}<Chip
										text="unbound"
										variant="err"
										title={COVERAGE.unbound.hint}
									/>{:else if st === 'empty' || st === 'stale'}<Chip
										text={COVERAGE[st].text}
										variant="warn"
										title={COVERAGE[st].hint}
									/>{/if}
								{#if node.kind === 'leaf'}
									<span class="muted mono">{fmt.n(node.documents)} docs</span>
									<span class="muted t7-small t7-age">{ago(node.updated_at, now)}</span>
								{/if}
							</span>
							{#if uses.length}
								<span class="kn-uses muted t7-small"
									>answers {[...new Set(uses.map((u) => stageLabel(u.stage)))].join(' · ')}</span
								>
							{:else if node.kind === 'leaf' && kindRows.length}
								<span class="kn-uses muted t7-small">not bound to a kind in this job</span>
							{/if}
						</button>
					{/each}
				</div>
			{/if}
			<details class="t7-add" id="t7-add-store" bind:open={addOpen}>
				<summary>Add a store</summary>
				<p class="muted t7-small">
					Its key is the knowledge kind it holds (e.g. <span class="mono">policy.gift_cards</span>). It lands
					in strata_rag via <span class="mono">store_ensure</span>, then fills from a source.
				</p>
				<form class="form-grid new-store" onsubmit={create}>
					<label class="field"
						><span class="label">Key</span><input
							class="input mono"
							bind:value={form.key}
							placeholder="policy.gift_cards"
						/></label
					>
					<label class="field"
						><span class="label">Label</span><input
							class="input"
							bind:value={form.label}
							placeholder="Gift cards"
						/></label
					>
					<label class="field"
						><span class="label">Kind</span>
						<select class="input" bind:value={form.kind}>
							<option value="leaf">Leaf (holds documents)</option>
							<option value="group">Group (holds stores)</option>
						</select></label
					>
					<label class="field"
						><span class="label">Parent</span>
						<select class="input" bind:value={form.parent_id}>
							<option value="">None (top level)</option>
							{#each groups as g (g.id)}<option value={g.id}>{g.key}</option>{/each}
						</select></label
					>
					<button
						class="btn"
						type="submit"
						{...writeAttrs(session, 'knowledge_manager')}
						disabled={!canWrite || !form.key.trim() || !form.label.trim()}>Create store</button
					>
				</form>
			</details>
		</Card>

		<div id="t7-store-detail">
			<Card title={sel ? sel.label : 'Store'} sub={sel ? pathOf(items, sel) : undefined}>
				{#if sel}
					<ol class="t7-chain" aria-label="This store's chain">
						<li class="ok"><span class="label">Layout</span><span>{sel.kind}</span></li>
						<li class={sel.strata_store_id ? 'ok' : sel.ensuring ? 'wait' : 'bad'}>
							<span class="label">strata_rag</span><span class="mono"
								>{sel.strata_store_id ?? (sel.ensuring ? 'being created' : 'not created')}</span
							>
						</li>
						<li class={sel.region ? 'ok' : 'wait'}>
							<span class="label">Region</span><span
								>{sel.region ? `${REGION_LABEL[sel.region]} · tenant data residency` : 'not pinned'}</span
							>
						</li>
						{#if sel.kind === 'leaf'}
							<li class={sel.documents ? (selState === 'stale' ? 'wait' : 'ok') : 'bad'}>
								<span class="label">Content</span><span
									>{fmt.n(sel.documents)} docs · {fmt.n(sel.chunks)} passages</span
								>
							</li>
						{/if}
					</ol>
					<KeyValues
						pairs={[
							[
								'Needed by',
								selNeeded.length
									? selNeeded
											.map((u) => `${stageLabel(u.stage)} (${u.kind}${u.required ? '' : ', optional'})`)
											.join(' · ')
									: sel.kind === 'group'
										? 'Routes questions to its stores'
										: 'No kind of this job is bound to it',
							],
							['Journey areas', sel.areas.join(', ') || '—'],
							['Size', `${fmt.n(sel.chunks)} passages · ${fmt.n(sel.tokens)} tokens`],
							['Updated', `${when(sel.updated_at)} (${ago(sel.updated_at, now)})`],
						]}
					/>
					<div class="row">
						{#if sel.kind === 'leaf'}<a class="btn sm" href={sourcesHref}>Add documents via a source</a>{/if}
						<button
							class="btn sm danger"
							type="button"
							{...writeAttrs(session, 'knowledge_manager')}
							onclick={() => (confirmStore = true)}>Delete store</button
						>
					</div>
					{#if sel.kind === 'leaf'}
						{#key sel.id}<UploadDocs store={sel} ondone={refreshAll} />{/key}
					{/if}
					{#if confirmStore}
						<div class="callout err" role="alertdialog" aria-label="Delete store">
							<strong>Delete {sel.key}?</strong>
							<span
								>The store and its documents are deleted from strata_rag{selNeeded.length
									? `, and ${selNeeded.length} journey kind${selNeeded.length === 1 ? '' : 's'} lose their source`
									: ''}. A group must be empty first.</span
							>
							<span
								><button class="btn sm danger" type="button" onclick={removeStore}>Delete store</button>
								<button class="btn sm" type="button" onclick={() => (confirmStore = false)}>Keep it</button
								></span
							>
						</div>
					{/if}
				{/if}
				{#if confirmDoc}
					<div class="callout err" role="alertdialog" aria-label="Delete document">
						<strong>Delete {confirmDoc.title}?</strong>
						<span>It is removed from strata_rag and no longer found by search.</span>
						<span
							><button class="btn sm danger" type="button" onclick={() => removeDoc(confirmDoc!)}
								>Delete document</button
							>
							<button class="btn sm" type="button" onclick={() => (confirmDoc = null)}>Keep it</button></span
						>
					</div>
				{/if}
				<h3 class="t7-h3">Documents</h3>
				{#if docs.s === 'idle'}
					<div class="empty">Pick a store to see what it holds.</div>
				{:else if docs.s === 'loading'}
					<p class="muted">Loading…</p>
				{:else if docs.s === 'err'}
					<Unavailable reason={docs.err} retry={() => sel && pick(sel)} />
				{:else if docs.items.length === 0}
					<div class="empty">
						{sel?.kind === 'group'
							? 'A group holds no documents: it routes a question on to the stores below it.'
							: !sel?.strata_store_id
								? 'Nothing yet: strata_rag has not created this store, so no source can fill it.'
								: 'This store has no documents yet. Point a source at it or upload a file.'}
					</div>
				{:else}
					<ul class="list-plain" data-testid="store-docs">
						{#each docs.items as d (d.id)}
							<li>
								<span
									>{d.title}<span class="cell-sub mono">{d.source} · added {ago(d.added_at, now)}</span></span
								>
								<span class="muted mono">{fmt.n(d.chunks)} passages</span>
								<button
									class="btn sm"
									type="button"
									{...writeAttrs(session, 'knowledge_manager')}
									onclick={() => (confirmDoc = d)}>Delete</button
								>
							</li>
						{/each}
					</ul>
					{#if docs.next}
						<button class="btn sm" type="button" disabled={loadingMore} onclick={more}
							>{loadingMore ? 'Loading…' : `Load more (${fmt.n(sel?.documents ?? 0)} in total)`}</button
						>
					{/if}
				{/if}
			</Card>
		</div>
	</div>

	<div class="grid t7-search">
		<div id="t7-playground">
			<SearchPreview roots={items.filter((s) => !s.parent_id)} />
		</div>
		<ModelCard />
	</div>
{/if}

<style>
	/* ---- provision-down chain ---- */
	.t7-flow {
		list-style: none;
		margin: 0 0 16px;
		padding: 0;
		display: grid;
		grid-template-columns: repeat(5, minmax(0, 1fr));
		gap: 22px;
		min-width: 0;
	}
	.t7-hop {
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
		overflow-wrap: anywhere;
	}
	.t7-hop .label {
		white-space: normal;
	}
	.t7-hop b.mono {
		font-size: 14px;
	}
	.t7-hop.wait {
		border-top-color: var(--warn);
	}
	.t7-hop.bad {
		border-top-color: var(--err);
	}
	.t7-hop b {
		font-size: 16px;
	}
	.t7-hop .muted {
		font-size: 12.5px;
	}
	.t7-hop:not(:last-child)::after {
		content: '→';
		position: absolute;
		right: -18px;
		top: 50%;
		transform: translateY(-50%);
		color: var(--muted);
	}

	/* ---- coverage ---- */
	.t7-stages {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(165px, 1fr));
		gap: 10px;
	}
	.t7-stage {
		border: 1px solid var(--line);
		border-radius: 8px;
		padding: 8px 10px;
		background: var(--bg);
		min-width: 0;
	}
	.t7-stage.gap {
		border-color: color-mix(in srgb, var(--err) 45%, var(--line));
	}
	.t7-stage header {
		display: flex;
		justify-content: space-between;
		align-items: center;
		gap: 6px;
		margin-bottom: 6px;
	}
	.t7-stage ul {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 6px;
	}
	.t7-stage li {
		display: grid;
		gap: 2px;
	}
	.t7-kind {
		overflow-wrap: anywhere;
	}
	button.t7-kind {
		all: unset;
		cursor: pointer;
		font-family: var(--mono);
		font-size: 12.5px;
		color: var(--accent);
		overflow-wrap: anywhere;
	}
	button.t7-kind:focus-visible {
		outline: 2px solid var(--accent);
	}
	.t7-kind-meta {
		display: inline-flex;
		gap: 6px;
		align-items: center;
		flex-wrap: wrap;
	}
	.t7-opt,
	.t7-small {
		font-size: 11.5px;
	}
	.t7-h3 {
		font-size: 13px;
		margin: 16px 0 6px;
	}
	.t7-fixes {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.t7-fixes li {
		display: flex;
		justify-content: space-between;
		align-items: center;
		gap: 10px;
		flex-wrap: wrap;
		padding: 7px 0;
		border-bottom: 1px solid var(--line-2);
	}
	.t7-fixes li > span:first-child {
		display: inline-flex;
		gap: 8px;
		align-items: baseline;
		flex-wrap: wrap;
		min-width: 0;
	}
	.t7-fixes .muted {
		font-size: 12.5px;
	}
	.t7-optional {
		margin-top: 8px;
	}
	.t7-optional summary {
		cursor: pointer;
		font-size: 13px;
	}
	.t7-allgood {
		margin: 12px 0 0;
	}
	.t7-main {
		margin-top: 16px;
		margin-bottom: 16px;
	}

	/* ---- tree ---- */
	.kn-node {
		all: unset;
		box-sizing: border-box;
		cursor: pointer;
		display: grid;
		grid-template-columns: minmax(0, 1fr) auto;
		gap: 2px 8px;
		align-items: center;
		width: 100%;
		padding: 6px 8px 6px calc(8px + var(--d) * 18px);
		border-radius: 6px;
	}
	.kn-node.group .kn-name > span:nth-child(2) {
		font-weight: 600;
	}
	.kn-node:hover,
	.kn-node.on {
		background: var(--sunk);
	}
	.kn-node.on {
		box-shadow: inset 3px 0 0 var(--accent);
	}
	.kn-node:focus-visible {
		outline: 2px solid var(--accent);
	}
	.kn-name {
		display: inline-flex;
		gap: 6px;
		align-items: baseline;
		flex-wrap: wrap;
		min-width: 0;
	}
	.t7-glyph {
		color: var(--muted);
	}
	.kn-uses {
		grid-column: 1 / -1;
	}
	.meta {
		display: inline-flex;
		gap: 8px;
		align-items: center;
	}
	.t7-age {
		min-width: 72px;
		text-align: right;
	}
	.t7-add {
		margin-top: 12px;
		border-top: 1px solid var(--line-2);
		padding-top: 10px;
	}
	.t7-add summary {
		cursor: pointer;
		font-weight: 600;
	}
	.new-store {
		margin-top: 10px;
		align-items: end;
	}

	/* ---- one store's chain ---- */
	.t7-chain {
		list-style: none;
		margin: 0 0 12px;
		padding: 0;
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
	}
	.t7-chain li {
		display: grid;
		gap: 1px;
		padding: 5px 9px;
		border-radius: 6px;
		background: var(--sunk);
		border-left: 3px solid var(--ok);
		font-size: 12.5px;
	}
	.t7-chain li.wait {
		border-left-color: var(--warn);
	}
	.t7-chain li.bad {
		border-left-color: var(--err);
	}

	.t7-search {
		grid-template-columns: minmax(0, 2fr) minmax(0, 1fr);
		align-items: start;
	}

	@media (max-width: 900px) {
		.t7-flow {
			grid-template-columns: 1fr;
			gap: 14px;
		}
		.t7-hop:not(:last-child)::after {
			content: '↓';
			right: auto;
			left: 50%;
			top: auto;
			bottom: -15px;
			transform: translateX(-50%);
			line-height: 1;
		}
		.t7-age {
			min-width: 0;
		}
		.t7-search {
			grid-template-columns: minmax(0, 1fr);
		}
	}
</style>
