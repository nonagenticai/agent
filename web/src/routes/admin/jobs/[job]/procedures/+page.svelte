<script lang="ts">
	// J12 Procedures: what the agent does for each request type. Leads with the path a request takes (RequestPath),
	// then one card per context space: required slots, the running version's step shape, and where every version
	// stands (running / shadowed base / in review / drafts). The flat version table is the second tab. Base
	// versions come from the template and ship in a release; a tenant PUBLISHED version shadows the base one and is
	// not live until a release carries it (D54: draft → test → publish via release → live). One procedure's flow, tests and reviews are on `procedures/[name]`.
	// from cb:web/src/routes/admin/jobs/[job]/procedures/+page.svelte@45ee128
	import { onMount } from 'svelte';
	import { api, app } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { writeAttrs } from '$lib/auth/roles';
	import { Resource } from '$lib/load.svelte';
	import PageHead from '$lib/ui/PageHead.svelte';
	import Freshness from '$lib/ui/Freshness.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Kpi from '$lib/ui/Kpi.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Tabs from '$lib/ui/Tabs.svelte';
	import type { JobSummary } from '$lib/api/types/core';
	import RequestPath from './RequestPath.svelte';
	import { liveOf, unpublished } from './view';
	import ContextSpaces from './ContextSpaces.svelte';
	import ProcedureTable from './ProcedureTable.svelte';
	import NewVersionDrawer from './NewVersionDrawer.svelte';

	const spaces = new Resource(() => api().contextSpaces());
	const procs = new Resource(() => api().procedures());
	const job = new Resource<JobSummary>(() => api().job(app.job));
	onMount(() => {
		spaces.refresh();
		procs.refresh();
		job.refresh();
	});
	const release = $derived(job.value.state === 'ok' ? (job.value.data.current_release?.id ?? null) : null);
	let view = $state('map');
	const fresh = {
		get refreshing() {
			return spaces.refreshing || procs.refreshing;
		},
		get updatedAt() {
			return procs.updatedAt;
		},
		refresh: async () => {
			await Promise.all([spaces.refresh(), procs.refresh()]);
		},
	};

	const all = $derived(procs.value.state === 'ok' ? procs.value.data.items : []);
	const spaceList = $derived(spaces.value.state === 'ok' ? spaces.value.data.items : []);
	const names = $derived([...new Set(all.map((p) => p.name))]);
	const changes = $derived(unpublished(all, release));
	const kpi = $derived({
		published: names.filter((n) =>
			liveOf(
				all.filter((p) => p.name === n),
				release,
			),
		).length,
		review: all.filter((p) => p.status === 'CANARY').length,
		drafts: all.filter((p) => p.status === 'DRAFT').length,
		outside: all.filter((p) => p.outside_grant.length && p.status !== 'ARCHIVED' && p.status !== 'DISCARDED')
			.length,
	});
	let creating = $state(false);
	let startFrom = $state('');
	function newVersion(from = '') {
		startFrom = from;
		creating = true;
	}
</script>

<PageHead
	id="J12"
	sub={'What the agent does when a customer asks for something. Each request type has a context space (what the agent must know first) and a procedure: versioned steps, each one operation inside the activity’s grant.'}
>
	{#snippet actions()}
		<Freshness resource={fresh} />
		<button
			type="button"
			class="btn primary"
			{...writeAttrs(session, 'job_designer')}
			onclick={() => newVersion()}>New procedure version</button
		>
	{/snippet}
</PageHead>

<RequestPath />

<section class="pr-pipe" data-testid="procedure-publish" aria-label="Draft, test, publish">
	<div class="pr-pipe-live">
		<span class="label">Live in production</span>
		{#if release}
			<span
				><b>{kpi.published}</b> procedure{kpi.published === 1 ? '' : 's'} from release
				<a class="mono" href="/admin/jobs/{app.job}/release/{release}">{release}</a></span
			>
		{:else if job.value.state === 'ok'}
			<span>Nothing yet: this job has no release.</span>
		{/if}
	</div>
	<div class="pr-pipe-draft">
		<span class="label">Draft</span>
		<span
			>{#if changes}<Chip
					text="{changes} unpublished change{changes === 1 ? '' : 's'}"
					variant="warn"
				/>{:else}<span class="muted">No unpublished changes</span>{/if}</span
		>
	</div>
	<p class="muted pr-pipe-how">
		Every change here is saved to draft. Test it in the test chat, then publish it with a release (approve →
		apply to Production); customers keep the live version until then.
	</p>
	<div class="pr-pipe-acts">
		<a class="btn" href="/chat">Test in test chat</a>
		<a class="btn" href="/admin/jobs/{app.job}/release">Review &amp; publish</a>
	</div>
</section>

{#if procs.value.state === 'ok'}
	<div class="kpis pr-kpis">
		<Kpi label="Live" value={String(kpi.published)} note="one live version per procedure" />
		<Kpi
			label="Awaiting review"
			value={String(kpi.review)}
			note={kpi.review ? 'a Release manager publishes, then it rides the next release' : 'nothing in review'}
		/>
		<Kpi label="Drafts" value={String(kpi.drafts)} note="not live; test in the test chat" />
		<Kpi
			label="Outside activity grant"
			value={String(kpi.outside)}
			note={kpi.outside ? 'refused at review and at release cut (409)' : 'every step inside its grant'}
		/>
	</div>
{/if}

<Tabs
	items={[
		['map', 'By request type', spaceList.length],
		['versions', 'All versions', all.length],
	]}
	bind:active={view}
	label="Procedure views"
/>

<div class="pr-view">
	{#if view === 'map'}
		<ContextSpaces {spaces} procedures={all} {release} />
	{:else}
		<LoadState
			resource={procs}
			empty="No procedures yet. They appear when a template with context spaces is pinned."
		>
			{#snippet children(page)}
				<ProcedureTable rows={page.items} {release} onstartfrom={(key) => newVersion(key)} />
			{/snippet}
		</LoadState>
	{/if}
</div>

<NewVersionDrawer
	bind:open={creating}
	from={startFrom}
	procedures={all}
	spaces={spaceList}
	oncreated={() => procs.refresh()}
/>

<style>
	.pr-kpis {
		margin-bottom: 10px;
	}
	.pr-pipe {
		display: grid;
		grid-template-columns: auto auto minmax(0, 1fr) auto;
		gap: 8px 20px;
		align-items: center;
		margin: 0 0 12px;
		padding: 10px 14px;
		border: 1px solid var(--line);
		border-left: 3px solid var(--warn);
		border-radius: var(--r);
		background: var(--panel);
		font-size: 13px;
	}
	.pr-pipe > div {
		display: grid;
		gap: 2px;
	}
	.pr-pipe-how {
		margin: 0;
		font-size: 12.5px;
	}
	.pr-pipe-acts {
		display: flex !important;
		gap: 8px;
		flex-wrap: wrap;
	}
	@media (max-width: 1000px) {
		.pr-pipe {
			grid-template-columns: 1fr 1fr;
		}
		.pr-pipe-how,
		.pr-pipe-acts {
			grid-column: 1 / -1;
		}
	}
	.pr-view {
		margin-top: 12px;
	}
</style>
