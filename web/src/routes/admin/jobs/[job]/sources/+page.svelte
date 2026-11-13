<script lang="ts">
	// T9 Sources. Design intent: the tenant_admin answers "is everything my agent needs to know actually being fed
	// in, and kept fresh?". The page reads as the chain source → ingest job → store → knowledge kind → journey stage:
	// first how the job's required knowledge kinds are fed (live feed / by hand / broken / nothing), then what to fix,
	// then the feed map, then the sources themselves (editor + field → ontology mapping + runs in the drawer).
	// Credentials are write-only. Sources are data, not design: a completed run is live without a release. The
	// ontology and kind bindings the data lands in are design (D54): draft → test chat → publish.
	import { onMount } from 'svelte';
	import { api, app } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { canWrite as canWriteFor, writeAttrs } from '$lib/auth/roles';
	import { Resource } from '$lib/load.svelte';
	import { jobPath } from '$lib/scope';
	import { fmt } from '$lib/format';
	import type { Paged } from '$lib/api/types/core';
	import type {
		IngestJob,
		KnowledgeStore,
		MappingTarget,
		Source,
		SourceMapping,
	} from '$lib/api/types/tenant';
	import type { KnowledgeKinds } from '$lib/api/types/ontology';
	import { RUN_LABELS, SOURCE_TYPES, structured, unknownTarget } from '$lib/tenant/sources';
	import { errorText } from '$lib/tenant/ingest-files';
	import { LANGUAGE_LABEL } from '$lib/api/types/core';
	import { ago, kindCoverage } from '$lib/tenant/knowledge-T7';
	import {
		FEED,
		HEALTH,
		attention,
		feedSummary,
		scheduleText,
		sourceHealth,
		storeFeeds,
		type KindFeed,
	} from '$lib/tenant/sources-T9';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LiveNote from '$lib/release/LiveNote.svelte';
	import Freshness from '$lib/ui/Freshness.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';
	import SourceEditor from './SourceEditor.svelte';
	import RunHistory from './RunHistory.svelte';
	import FeedMapT9 from './FeedMapT9.svelte';

	const list = new Resource<Paged<Source>>(() => api().sources());
	const stores = new Resource<Paged<KnowledgeStore>>(() => api().knowledgeStores());
	const targets = new Resource<MappingTarget[]>(
		() => api().sourceMappingTargets(),
		() => false,
	);
	const kinds = new Resource<KnowledgeKinds>(
		() => api().knowledgeKinds(),
		() => false,
	);
	const jobs = new Resource<Paged<IngestJob>>(() => api().ingestJobs());
	onMount(() => {
		void list.refresh();
		void stores.refresh();
		void targets.refresh();
		void kinds.refresh();
		void jobs.refresh();
	});
	const now = Date.now();
	const canWrite = $derived(canWriteFor(session, 'knowledge_manager'));
	const storeItems = $derived(stores.value.state === 'ok' ? stores.value.data.items : []);
	const targetItems = $derived(targets.value.state === 'ok' ? targets.value.data : []);
	const kindRows = $derived(kinds.value.state === 'ok' ? kinds.value.data.rows : []);
	const jobItems = $derived(jobs.value.state === 'ok' ? jobs.value.data.items : []);
	const sourceItems = $derived(list.value.state === 'ok' ? list.value.data.items : []);
	const cov = $derived(kindCoverage(kindRows, storeItems, now));
	const feeds = $derived(storeFeeds(storeItems, sourceItems, jobItems, cov, targetItems, now));
	const sum = $derived(feedSummary(cov, feeds));
	const kindsHref = $derived(jobPath(app.job, '/knowledge-kinds'));
	const todo = $derived(attention(sourceItems, targetItems, sum, now, kindsHref));
	const stagesUsed = $derived(new Set(kindRows.map((k) => k.stage)).size);
	const sourceRuns = $derived(jobItems.filter((j) => j.source_id).length);

	const storeKey = (id: string) => storeItems.find((s) => s.id === id)?.key ?? id;
	const typeLabel = (t: string) => SOURCE_TYPES.find(([k]) => k === t)?.[1] ?? t;
	const runVariant = (s: string): 'ok' | 'err' | 'warn' | 'muted' =>
		s === 'completed' ? 'ok' : s === 'failed' ? 'err' : s === 'running' ? 'warn' : 'muted';
	const ORDER: KindFeed[] = ['synced', 'manual', 'broken', 'gap'];

	let open = $state(false);
	let editorDirty = $state(false);
	let editedMapping = $state<SourceMapping[] | null>(null);
	let current = $state<Source | null>(null);
	function edit(s: Source | null) {
		current = s;
		open = true;
	}
	function editId(id: string) {
		const s = sourceItems.find((x) => x.id === id);
		if (s) edit(s);
	}
	async function saved(s: Source | null) {
		if (s) current = s;
		else open = false;
		await Promise.all([list.refresh(), jobs.refresh()]);
	}
	async function changed() {
		await Promise.all([list.refresh(), jobs.refresh(), stores.refresh()]);
		if (current && list.value.state === 'ok')
			current = list.value.data.items.find((x) => x.id === current!.id) ?? current;
	}
</script>

<PageHead
	id="T9"
	sub="Where this job's knowledge comes from and whether it stays fresh: each source fills one store, and each store answers the knowledge kinds the journey needs."
>
	{#snippet actions()}
		<Freshness resource={list} />
		<button
			class="btn primary"
			type="button"
			{...writeAttrs(session, 'knowledge_manager')}
			onclick={() => edit(null)}>Add source</button
		>
	{/snippet}
</PageHead>

<LiveNote
	what="Sources, column mappings and documents are the data plane: ingestion writes data, not agent design."
/>

<!-- The chain this page is about, with live counts; every step links to the page that owns it. -->
<nav class="t9-chain" aria-label="Knowledge chain">
	<div class="t9-step here">
		<span class="t9-step-n">1</span>
		<span class="t9-step-t">Sources</span>
		<span class="t9-step-d">{sourceItems.length} feeds + uploads</span>
	</div>
	<a class="t9-step" href="/admin/knowledge/jobs">
		<span class="t9-step-n">2</span>
		<span class="t9-step-t">Ingest jobs</span>
		<span class="t9-step-d">{sourceRuns} from sources · fetch → index</span>
	</a>
	<a class="t9-step" href="/admin/knowledge/stores">
		<span class="t9-step-n">3</span>
		<span class="t9-step-t">Stores</span>
		<span class="t9-step-d">{feeds.length} in use</span>
	</a>
	<a class="t9-step" href={kindsHref}>
		<span class="t9-step-n">4</span>
		<span class="t9-step-t">Knowledge kinds</span>
		<span class="t9-step-d">{sum.required} required</span>
	</a>
	<a class="t9-step" href={jobPath(app.job, '/journey')}>
		<span class="t9-step-n">5</span>
		<span class="t9-step-t">Journey stages</span>
		<span class="t9-step-d">{stagesUsed} answer from them</span>
	</a>
</nav>

{#if stores.value.state === 'unavailable'}
	<Unavailable reason={`Stores: ${stores.value.reason}`} retry={stores.retry} />
{/if}
{#if kinds.value.state === 'unavailable'}
	<Unavailable reason={`Knowledge kinds: ${kinds.value.reason}`} retry={kinds.retry} />
{/if}

<LoadState resource={list} empty="No sources yet. Add a website crawl, an API, a file or a datatable feed.">
	{#snippet children(page)}
		{#if sum.required}
			<section
				class="card t9-cover"
				aria-label="How the required knowledge is fed"
				data-testid="feed-summary"
			>
				<div class="t9-cover-head">
					<h2>How the {sum.required} required knowledge kinds are fed</h2>
					<p class="muted">
						A live feed keeps a store current on a schedule. "By hand" means someone has to remember to upload
						the new version.
					</p>
				</div>
				<div
					class="t9-bar"
					role="img"
					aria-label={ORDER.map((f) => `${sum.byFeed[f].length} ${FEED[f].text}`).join(', ')}
				>
					{#each ORDER as f (f)}
						{#if sum.byFeed[f].length}
							<span class="t9-seg {FEED[f].variant}" style:flex-grow={sum.byFeed[f].length}></span>
						{/if}
					{/each}
				</div>
				<ul class="t9-legend">
					{#each ORDER as f (f)}
						<li data-feed={f}>
							<span class="t9-dot {FEED[f].variant}"></span>
							<strong>{sum.byFeed[f].length}</strong>
							{FEED[f].text}
							{#if sum.byFeed[f].length}<span class="mono t9-legend-k">{sum.byFeed[f].join(' · ')}</span>{/if}
						</li>
					{/each}
				</ul>
			</section>
		{/if}

		{#if todo.length}
			<section class="t9-todo" aria-label="Needs attention" data-testid="attention">
				{#each todo as t (t.key)}
					<div class="t9-todo-item {t.severity}">
						<div class="t9-todo-text">
							<strong>{t.title}</strong>
							<span class="muted">{t.detail}</span>
						</div>
						{#if t.source}
							<button class="btn sm" type="button" onclick={() => editId(t.source!)}>{t.cta}</button>
						{:else if t.href}
							<a class="btn sm" href={t.href}>{t.cta}</a>
						{/if}
					</div>
				{/each}
			</section>
		{/if}

		<div class="t9-stack">
			<Card
				title="Feed map"
				sub="What fills each store, and what the agent answers from it. Click a source to edit it, its mapping and its runs."
				cls="flush"
			>
				{#if feeds.length}
					<FeedMapT9 {feeds} targets={targetItems} {now} onopen={edit} />
				{:else}
					<p class="muted t9-pad">Loading stores…</p>
				{/if}
			</Card>

			<Card
				title="Sources"
				sub="Every feed of this job. Credentials are write-only: set, replaced or removed, never shown."
				cls="flush"
			>
				<div class="tbl-wrap">
					<table class="tbl clickable" data-testid="sources">
						<thead>
							<tr
								><th>Source</th><th>Fills store</th><th>Mapping</th><th>Last run</th><th class="r"
									>Documents</th
								><th>Schedule</th></tr
							>
						</thead>
						<tbody>
							{#each page.items as s (s.id)}
								{@const bad = s.mapping.filter((m) => unknownTarget(m, targetItems))}
								{@const h = sourceHealth(s, targetItems, now)}
								<tr data-source={s.id} onclick={() => edit(s)}>
									<td
										><button
											class="table-action"
											type="button"
											onclick={(e) => (e.stopPropagation(), edit(s))}>{s.name}</button
										><span class="cell-sub"
											>{typeLabel(s.type)} · {LANGUAGE_LABEL[s.language] ?? s.language}{s.has_credentials
												? ' · credentials set'
												: ''}{!s.enabled ? ' · disabled' : ''}</span
										>
										{#if h !== 'ok'}<span class="cell-sub"
												><Chip
													text={HEALTH[h].text}
													variant={HEALTH[h].variant}
													title={HEALTH[h].hint}
												/></span
											>{/if}</td
									>
									<td class="mono">{storeKey(s.target_store_id)}</td>
									<td>
										{#if structured(s.type)}
											{s.mapping.length} field{s.mapping.length === 1 ? '' : 's'}
											{#if bad.length}<span class="cell-sub"
													><Chip text={`${bad.length} unknown attribute`} variant="err" /></span
												>{/if}
										{:else}<span class="muted"
												>{s.type === 'file' ? 'uploaded files' : 'pages'}, no mapping</span
											>{/if}
									</td>
									<td>
										{#if s.last_run}
											<Chip text={RUN_LABELS[s.last_run.status]} variant={runVariant(s.last_run.status)} />
											<span class="cell-sub">{ago(s.last_run.started_at, now)}</span>
											{#each s.last_run.ingest_job_ids.slice(0, 2) as id (id)}<a
													class="cell-sub"
													href={`/admin/knowledge/jobs?id=${encodeURIComponent(id)}`}
													onclick={(e) => e.stopPropagation()}>ingest job {id}</a
												>{/each}
											{#if s.last_run.ingest_job_ids.length > 2}<span class="cell-sub"
													>+{s.last_run.ingest_job_ids.length - 2} more</span
												>{/if}
											{#if s.last_run.error}<span class="cell-sub t9-err">{errorText(s.last_run.error)}</span
												>{/if}
										{:else}<span class="muted">Not synced yet</span>{/if}
									</td>
									<td class="r">{s.last_run ? fmt.n(s.last_run.documents) : '—'}</td>
									<td>{scheduleText(s.schedule)}</td>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
			</Card>
		</div>

		<section class="t9-how" aria-label="How a change here reaches customers">
			<h2>How a change here reaches customers</h2>
			<ul>
				<li>
					<strong>Sources and uploads are data, not design.</strong> When a run's ingest job completes, the store
					is searchable at once: no release needed. A failed run leaves the last good documents in place.
				</li>
				<li>
					<strong>The mapping points into the job's ontology</strong> (template objects plus your refinements,
					<a href={jobPath(app.job, '/ontology')}>Ontology</a>). Ontology changes are design: saved to draft,
					tried in the <a href="/chat">test chat</a>, live once you
					<a href={jobPath(app.job, '/release')}>publish</a>; a mapped attribute that disappears blocks the
					next re-sync until you remap it.
				</li>
				<li>
					<strong>Which store answers which kind</strong> is set on
					<a href={kindsHref}>Knowledge kinds</a>: that binding is design too — saved to draft, live once
					published.
				</li>
			</ul>
		</section>
	{/snippet}
</LoadState>

<Drawer bind:open title={current?.name ?? 'Add source'} wide dirty={editorDirty}>
	<SourceEditor
		bind:dirty={editorDirty}
		bind:editedMapping
		source={current}
		stores={storeItems}
		targets={targetItems}
		{canWrite}
		onsaved={saved}
	/>
	{#if current}
		<RunHistory source={current} {canWrite} {editedMapping} onchange={changed} />
	{/if}
</Drawer>

<style>
	.t9-chain {
		display: grid;
		grid-template-columns: repeat(5, minmax(0, 1fr));
		gap: 0;
		margin: 0 0 16px;
		border: 1px solid var(--line);
		border-radius: 8px;
		background: var(--panel);
		overflow: hidden;
	}
	.t9-step {
		position: relative;
		display: flex;
		flex-direction: column;
		gap: 2px;
		padding: 10px 14px 10px 22px;
		color: inherit;
		text-decoration: none;
		border-left: 1px solid var(--line-2);
		min-width: 0;
	}
	.t9-step:first-child {
		border-left: 0;
		padding-left: 14px;
	}
	.t9-step:not(:first-child)::before {
		content: '›';
		position: absolute;
		left: 6px;
		top: 50%;
		transform: translateY(-50%);
		color: var(--faint);
		font-size: 16px;
	}
	a.t9-step:hover {
		background: var(--sunk);
	}
	.t9-step.here {
		background: color-mix(in srgb, var(--accent) 9%, transparent);
		box-shadow: inset 0 -2px 0 var(--accent);
	}
	.t9-step-n {
		font: 600 10.5px/1 var(--mono);
		color: var(--faint);
	}
	.t9-step-t {
		font-weight: 600;
		font-size: 13.5px;
	}
	.t9-step-d {
		font-size: 12px;
		color: var(--muted);
	}
	.t9-cover {
		padding: 14px 16px;
		margin-bottom: 16px;
		gap: 10px;
	}
	.t9-cover-head h2,
	.t9-how h2 {
		margin: 0;
		font: 600 15px/1.3 var(--cond);
	}
	.t9-cover-head p {
		margin: 2px 0 0;
		font-size: 12.5px;
	}
	.t9-bar {
		display: flex;
		height: 12px;
		border-radius: 6px;
		overflow: hidden;
		gap: 2px;
		background: var(--sunk);
	}
	.t9-seg,
	.t9-dot {
		background: var(--muted);
	}
	.t9-seg.ok,
	.t9-dot.ok {
		background: var(--ok);
	}
	.t9-seg.warn,
	.t9-dot.warn {
		background: var(--warn);
	}
	.t9-seg.err,
	.t9-dot.err {
		background: var(--err);
	}
	.t9-seg {
		flex-basis: 0;
	}
	.t9-legend {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(200px, 1fr));
		gap: 6px 16px;
		font-size: 13px;
	}
	.t9-dot {
		display: inline-block;
		width: 9px;
		height: 9px;
		border-radius: 50%;
		margin-right: 4px;
	}
	.t9-legend-k {
		display: block;
		color: var(--muted);
		font-size: 11.5px;
		margin-left: 13px;
		overflow-wrap: anywhere;
	}
	.t9-todo {
		display: flex;
		flex-direction: column;
		gap: 8px;
		margin-bottom: 16px;
	}
	.t9-todo-item {
		display: flex;
		gap: 12px;
		align-items: center;
		justify-content: space-between;
		padding: 10px 14px;
		border: 1px solid var(--line);
		border-left: 3px solid var(--err);
		border-radius: var(--r);
		background: var(--panel);
	}
	.t9-todo-item.gap {
		border-left-color: var(--warn);
	}
	.t9-todo-text {
		display: flex;
		flex-direction: column;
		gap: 2px;
		min-width: 0;
		font-size: 13px;
	}
	.t9-err {
		color: var(--err);
	}
	.t9-pad {
		padding: 0 16px 16px;
	}
	.t9-how {
		margin-top: 16px;
		padding: 12px 16px;
		border: 1px dashed var(--line);
		border-radius: 8px;
	}
	.t9-how ul {
		margin: 6px 0 0;
		padding-left: 18px;
		display: flex;
		flex-direction: column;
		gap: 4px;
		font-size: 13px;
		color: var(--muted);
	}
	.t9-how strong {
		color: var(--fg);
	}
	.t9-stack {
		display: flex;
		flex-direction: column;
		gap: 16px;
	}
	@media (max-width: 760px) {
		.t9-chain {
			grid-template-columns: minmax(0, 1fr);
		}
		.t9-step {
			border-left: 0;
			border-top: 1px solid var(--line-2);
			padding: 8px 14px 8px 26px;
		}
		.t9-step:first-child {
			border-top: 0;
			padding-left: 26px;
		}
		.t9-step:not(:first-child)::before {
			content: '↓';
			left: 10px;
		}
		.t9-step-n {
			position: absolute;
			left: 10px;
			top: 11px;
		}
		.t9-step:not(:first-child) .t9-step-n {
			display: none;
		}
		.t9-todo-item {
			flex-direction: column;
			align-items: flex-start;
		}
	}
</style>
