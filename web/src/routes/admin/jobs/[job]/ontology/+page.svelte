<script lang="ts">
	// J11 Ontology: the things this job's agent reasons over (entities, attributes, relations). Leads with how it is
	// assembled — inherited template layers (locked) + this tenant's refinements (saved to draft, published by a release — D54) — then the entity
	// map in one lane per layer plus an "Added by you" lane, or a relations list. The drawer says what each entity is
	// for (activities, procedures, platform operations) and what the tenant may change on it.
	// Each entity links to the platform object it is a view of (owner call #8). Split from cb's 888-LOC page:
	// filters, the entity drawer, the two create drawers and the relations list live next to this file.
	// from cb:web/src/routes/admin/jobs/[job]/ontology/+page.svelte@45ee128
	import { onMount } from 'svelte';
	import { api, app } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { writeAttrs, canWrite as canWriteFn, isSuperAdmin, writeHint } from '$lib/auth/roles';
	import { Resource } from '$lib/load.svelte';
	import PageHead from '$lib/ui/PageHead.svelte';
	import Freshness from '$lib/ui/Freshness.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';
	import OntologyGraph from '$lib/ontology/OntologyGraph.svelte';
	import { distinct, heading, layout, visibleEntities, type Filter } from '$lib/ontology/layout';
	import type { OntologyEntity } from '$lib/api/types/ontology';
	import OntologyFilters from './OntologyFilters.svelte';
	import EntityDrawer from './EntityDrawer.svelte';
	import AddEntityDrawer from './AddEntityDrawer.svelte';
	import AddRelationDrawer from './AddRelationDrawer.svelte';
	import RelationsTable from './RelationsTable.svelte';
	import J11LayerStack from './J11LayerStack.svelte';
	import DraftBar from '$lib/release/DraftBar.svelte';
	import { layerBands, platformObjects, tenantChanges, unusedEntities } from '$lib/ontology/J11usage';
	import { draftMarks } from '$lib/ontology/J11drafts';

	const spaces = new Resource(() => api().contextSpaces());
	// include_hidden: hidden entities are absent from the effective list; this page lists them under "Show hidden".
	const entities = new Resource(() => api().ontologyEntities({ include_hidden: true }));
	const relations = new Resource(() => api().ontologyRelations());
	// D54: the job's unpublished changes, to mark each row the draft touches with a "Draft" chip.
	const drafts = new Resource(() => api().draftDiff());
	onMount(() => {
		drafts.refresh();
		spaces.refresh();
		entities.refresh();
		relations.refresh();
		// `?entity=Order` deep-links to one entity (other pages link here); the drawer opens on it.
		const want = new URLSearchParams(window.location.search).get('entity');
		if (want) {
			selName = want;
			open = true;
		}
	});
	const fresh = {
		get refreshing() {
			return entities.refreshing || relations.refreshing || spaces.refreshing;
		},
		get updatedAt() {
			const t = [entities.updatedAt, relations.updatedAt, spaces.updatedAt].filter((d): d is Date => !!d);
			return t.length ? new Date(Math.min(...t.map((d) => d.getTime()))) : null;
		},
		refresh: async () => {
			await Promise.all([spaces.refresh(), entities.refresh(), relations.refresh(), drafts.refresh()]);
		},
	};

	const canWrite = $derived(canWriteFn(session, 'job_designer'));
	// A phone cannot read a four-lane graph; it starts on the list (the toggle still offers the graph).
	let view = $state(typeof window !== 'undefined' && window.innerWidth < 640 ? 'table' : 'graph');
	let filter = $state<Filter>({ industry: 'all', department: 'all', space: 'all', showHidden: false });

	const allEntities = $derived(entities.value.state === 'ok' ? entities.value.data.items : []);
	const allRelations = $derived(relations.value.state === 'ok' ? relations.value.data.items : []);
	const allSpaces = $derived(spaces.value.state === 'ok' ? spaces.value.data.items : []);
	const marks = $derived(draftMarks(drafts.value.state === 'ok' ? drafts.value.data : []));
	const industries = $derived(distinct(allEntities, 'industry'));
	const shown = $derived(visibleEntities(allEntities, filter, allSpaces));
	const graph = $derived(layout(shown, allRelations));
	const head = $derived(heading(filter));
	const changes = $derived(tenantChanges(allEntities, allRelations));
	const bands = $derived(layerBands(allEntities));
	const visibleAll = $derived(allEntities.filter((e) => !e.hidden));
	const effectiveRelations = $derived(
		allRelations.filter(
			(r) => visibleAll.some((e) => e.name === r.subject) && visibleAll.some((e) => e.name === r.object),
		),
	);
	const counts = $derived({ hidden: changes.hidden, conflicts: changes.conflicts });

	// ---- drawers ----
	let open = $state(false);
	let selName = $state<string | null>(null);
	const sel = $derived(allEntities.find((e) => e.name === selName) ?? null);
	let addingEntity = $state(false);
	let addingRel = $state(false);
	let relSubject = $state('');

	function select(e: OntologyEntity) {
		selName = e.name;
		open = true;
	}
	function startAddRelation(subject = '') {
		relSubject = subject;
		addingRel = true;
	}
	async function created(name: string) {
		await Promise.all([entities.refresh(), relations.refresh(), drafts.refresh()]);
		selName = name;
		open = true;
	}
</script>

<PageHead
	id="J11"
	sub="The things your agent knows about and how they connect — what it looks up, asks for and acts on. Most of it is inherited from templates; you refine it for your business."
>
	{#snippet actions()}
		<Freshness resource={fresh} />
		<button
			type="button"
			class="btn primary"
			{...writeAttrs(session, 'job_designer')}
			onclick={() => (addingEntity = true)}>Add entity</button
		>
		<button
			type="button"
			class="btn"
			{...writeAttrs(session, 'job_designer')}
			onclick={() => startAddRelation()}>Add relation</button
		>
	{/snippet}
</PageHead>
<DraftBar page="J11" tick={[entities.value, relations.value]} />
{#if !canWrite}<p class="note">{writeHint('job_designer')} to add entities or relations.</p>{/if}

{#if entities.value.state === 'ok'}
	<J11LayerStack
		{bands}
		{changes}
		total={visibleAll.length}
		relations={effectiveRelations.length}
		objects={platformObjects(allEntities)}
		unused={unusedEntities(allEntities, allSpaces).length}
		superAdmin={isSuperAdmin(session)}
	/>
	{#if counts.conflicts}
		<div class="callout gap" role="status">
			<strong
				>{counts.conflicts} refinement{counts.conflicts === 1 ? '' : 's'} lost its base parent (marked
				<span class="j11-bang">!</span> on the map)</strong
			>
			<span
				>An attribute this tenant added sits on a base row that changed. Resolve it on <a
					href="/admin/jobs/{app.job}/release#conflicts">Release → Conflicts</a
				>.</span
			>
		</div>
	{/if}
{/if}

<OntologyFilters
	bind:filter
	bind:view
	entities={allEntities}
	spaces={allSpaces}
	{industries}
	{head}
	nodeCount={graph.nodes.length}
	edgeCount={graph.edges.length}
	hiddenCount={counts.hidden}
/>

{#if entities.value.state === 'unavailable'}
	<Unavailable reason={entities.value.reason} retry={entities.retry} />
{:else if entities.value.state === 'empty'}
	<div class="empty" data-testid="load-empty">
		This job has no ontology yet. It appears when a template with entities is pinned, or add the first entity
		above.
	</div>
{:else if entities.value.state === 'loading'}
	<p class="muted" role="status">Loading the ontology…</p>
{:else if view === 'graph'}
	{#if relations.value.state === 'unavailable'}
		<Unavailable reason={relations.value.reason} retry={relations.retry} />
	{/if}
	{#if shown.length === 0}
		<div class="empty">No entities match these filters.</div>
	{:else}
		<div class="canvas card">
			<OntologyGraph
				entities={shown}
				relations={allRelations}
				selected={open ? selName : null}
				onselect={select}
			/>
		</div>
		<p class="note legend">
			<span class="sw generic"></span> Base <span class="sw industry"></span> Industry
			<span class="sw industry_dept"></span> Specialized — inherited, read-only
			<span class="sw tenant"></span> Added by you (cards, <b class="plus">+</b> attributes, dashed relations) ·
			click an entity to see what uses it; its neighbours stay lit.
		</p>
	{/if}
{:else}
	<RelationsTable
		edges={graph.edges.map((e) => e.data!.relation)}
		entities={allEntities}
		{relations}
		{spaces}
		{marks}
		onpick={(name) => {
			const e = allEntities.find((x) => x.name === name);
			if (e) select(e);
		}}
	/>
{/if}

<EntityDrawer
	bind:open
	entity={sel}
	entities={allEntities}
	relations={allRelations}
	spaces={allSpaces}
	{marks}
	{canWrite}
	onpick={(name) => (selName = name)}
	onaddrelation={(s) => startAddRelation(s)}
	onchanged={() =>
		Promise.all([entities.refresh(), relations.refresh(), drafts.refresh()]).then(() => undefined)}
/>
<AddEntityDrawer bind:open={addingEntity} entities={allEntities} {industries} {filter} oncreated={created} />
<AddRelationDrawer
	bind:open={addingRel}
	subject={relSubject}
	entities={allEntities}
	relations={allRelations}
	oncreated={() => Promise.all([relations.refresh(), drafts.refresh()]).then(() => undefined)}
/>

<style>
	.j11-bang {
		display: inline-block;
		width: 14px;
		text-align: center;
		border-radius: 50%;
		font-size: 10px;
		color: var(--warn);
		background: color-mix(in srgb, var(--warn) 18%, transparent);
	}
	.plus {
		color: var(--accent);
	}
	.canvas {
		padding: 0;
		overflow: hidden;
		height: 680px;
	}
	.legend {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		align-items: center;
	}
	.sw {
		display: inline-block;
		width: 10px;
		height: 10px;
		border-radius: 3px;
		margin-left: 6px;
	}
	.sw.generic {
		background: var(--cust);
	}
	.sw.industry {
		background: var(--tool);
	}
	.sw.industry_dept {
		background: var(--work);
	}
	.sw.tenant {
		background: var(--accent);
	}
</style>
