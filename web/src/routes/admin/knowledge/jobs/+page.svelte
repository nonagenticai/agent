<script lang="ts">
	// T8 Ingest jobs: how strata_ingest turns an upload, crawl or table into searchable knowledge. Every job walks
	// fetch → read → chunk → embed → index into one knowledge store; provider slots power read and embed. The page
	// leads with that pipeline (where jobs are, where they stopped), then what needs the tenant admin (a failed job:
	// its cause, the fix, Retry), slot drift that affects finished jobs, and the full history.
	// Retry (failed only), Cancel (queued/running; a step in flight makes it `cancelling` until the worker finishes it),
	// Delete (ended jobs; staged files go, indexed documents stay). A delete runs in three phases (mark `deleting`, delete
	// the staged files, delete the rows); when a file cannot be deleted (502) the job stays `deleting` and only a second
	// delete (Retry delete) finishes it. strata_ingest#11: a DELETE still working after its 5 s wait answers 202
	// `deleting`; the page keeps showing Deleting… and polls the job until it answers 404 (gone). Data actions: they
	// apply at once, no draft (D69).
	// `?id=<job>` opens a job.
	import { onDestroy, onMount } from 'svelte';
	import '$lib/phone-stack.css';
	import { page } from '$app/state';
	import { api, app, guard, guardOutcome } from '$lib/app.svelte';
	import { ApiError } from '$lib/api/errors';
	import { session } from '$lib/auth/session.svelte';
	import { canWrite as canWriteFor, writeHint } from '$lib/auth/roles';
	import { Resource } from '$lib/load.svelte';
	import { jobPath } from '$lib/scope';
	import { fmt, sentence } from '$lib/format';
	import type { Paged } from '$lib/api/types/core';
	import type {
		IngestJob,
		IngestJobDeleted,
		IngestJobDeleting,
		IngestStatus,
		KnowledgeStore,
		ProviderSlot,
	} from '$lib/api/types/tenant';
	import { isStillDeleting } from '$lib/api/types/tenant';
	import { toast } from '$lib/ui/toaster.svelte';
	import { when } from '$lib/tenant/ui';
	import { pagesSummary } from '$lib/tenant/sources';
	import {
		STAGES,
		actionBlock,
		attemptNote,
		cancelEffect,
		driftImpact,
		eur,
		failureAdvice,
		inFlight as holdsWorker,
		jobStages,
		pipelineLoad,
		stageOf,
		took,
	} from '$lib/tenant/ingest-T8';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LiveNote from '$lib/release/LiveNote.svelte';
	import Freshness from '$lib/ui/Freshness.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import Pipeline from './Pipeline.svelte';
	import StageTrack from './StageTrack.svelte';

	const STATUSES: IngestStatus[] = [
		'queued',
		'running',
		'cancelling',
		'deleting',
		'completed',
		'failed',
		'rejected',
		'cancelled',
	];
	let status = $state<IngestStatus | ''>('');
	let store = $state('');
	// The history table follows the filters; the pipeline, "needs you" and drift always see every job.
	const jobs = new Resource<Paged<IngestJob>>(() =>
		api().ingestJobs({ status, store_id: store || undefined }),
	);
	const all = new Resource<Paged<IngestJob>>(() => api().ingestJobs({}));
	const stores = new Resource<Paged<KnowledgeStore>>(() => api().knowledgeStores());
	const slots = new Resource<ProviderSlot[]>(() => api().providerSlots());
	onMount(() => {
		void stores.refresh();
		void slots.refresh();
		void all.refresh().then(openFromUrl);
	});
	$effect(() => {
		void [status, store];
		void jobs.refresh();
	});
	const canWrite = $derived(canWriteFor(session, 'knowledge_manager'));
	const storeItems = $derived(stores.value.state === 'ok' ? stores.value.data.items : []);
	const storeKey = (id: string) => storeItems.find((s) => s.id === id)?.key ?? id;
	const slotItems = $derived(slots.value.state === 'ok' ? slots.value.data : []);
	const allJobs = $derived(all.value.state === 'ok' ? all.value.data.items : []);
	const load = $derived(pipelineLoad(allJobs, slotItems));
	const failed = $derived(allJobs.filter((j) => j.status === 'failed'));
	const inFlight = $derived(allJobs.filter(holdsWorker));
	const drift = $derived(driftImpact(allJobs, slotItems).filter((d) => d.jobs.length));
	const docsIndexed = $derived(
		allJobs.filter((j) => j.status === 'completed').reduce((n, j) => n + j.documents, 0),
	);
	const cost = $derived(allJobs.reduce((n, j) => n + (j.cost_eur ?? 0), 0));

	let open = $state(false);
	let cur = $state<IngestJob | null>(null);
	let confirm = $state<'cancel' | 'delete' | null>(null);
	// `?id=` of a job that is not (or no longer) here, e.g. a deleted job linked from a source run.
	let missing = $state<string | null>(null);
	const variant = (s: string): 'ok' | 'warn' | 'err' | 'muted' | 'accent' =>
		s === 'completed'
			? 'ok'
			: s === 'failed' || s === 'rejected'
				? 'err'
				: s === 'running'
					? 'accent'
					: s === 'queued' || s === 'cancelling' || s === 'deleting'
						? 'warn'
						: 'muted';
	const active = holdsWorker;
	const advice = $derived(cur && cur.status === 'failed' ? failureAdvice(cur, slotItems) : null);
	const curStages = $derived(cur ? jobStages(cur) : null);

	function openFromUrl() {
		const id = page.url.searchParams.get('id');
		const j = id ? allJobs.find((x) => x.id === id) : undefined;
		if (j) show(j);
		else missing = id;
	}
	function show(j: IngestJob) {
		cur = j;
		confirm = null;
		open = true;
	}
	// The DELETE in flight (shows "Deleting…"), and the last one that did not finish (its error, shown with Retry delete).
	let deletingId = $state<string | null>(null);
	let deleteError = $state<{ id: string; code: string; message: string } | null>(null);
	async function remove(id: string) {
		deletingId = id;
		deleteError = null;
		let d: IngestJobDeleted | IngestJobDeleting | null = null;
		const out = await guardOutcome(
			async () => {
				d = await api().deleteIngestJob(id);
			},
			() => deletedText(d),
			'Not deleted',
			{ reconcile: true },
		);
		if (out.kind === 'ok' && isStillDeleting(d)) {
			// 202: keep "Deleting…" up and poll until the job answers 404.
			if (cur?.id === id) cur = { ...cur, status: 'deleting' };
			await Promise.all([jobs.refresh(), all.refresh()]);
			watchDelete(id);
			return;
		}
		deletingId = null;
		if (out.kind === 'ok') {
			if (cur?.id === id) open = false;
		} else {
			const e = out.error;
			deleteError = {
				id,
				code: e instanceof ApiError ? e.code : 'error',
				message: (e as Error).message,
			};
			// Re-read: a failed delete may have left the job `deleting` (rows kept, some staged files gone).
			try {
				const j = await api().ingestJob(id);
				if (cur?.id === id) cur = j;
			} catch {
				/* gone or unreachable: the refresh below shows the truth */
			}
		}
		await Promise.all([jobs.refresh(), all.refresh()]);
	}
	async function act(kind: 'retry' | 'cancel' | 'delete', id = cur?.id) {
		if (!id) return;
		confirm = null;
		if (kind === 'delete') return remove(id);
		const box: { v: IngestJob | null } = { v: null };
		const ok = await guard(
			async () => {
				box.v = kind === 'retry' ? await api().retryIngestJob(id) : await api().cancelIngestJob(id);
			},
			() =>
				kind === 'retry'
					? 'Job ran again and completed.'
					: box.v?.status === 'cancelling'
						? 'Cancelling: the step in progress finishes, then the job stops.'
						: 'Job cancelled.',
			kind === 'retry' ? 'Not retried' : 'Not cancelled',
		);
		if (ok && cur?.id === id) {
			if (box.v) cur = box.v;
			else open = false;
		}
		if (ok && box.v?.status === 'cancelling') watch(id);
		await Promise.all([jobs.refresh(), all.refresh()]);
	}
	function deletedText(d: IngestJobDeleted | IngestJobDeleting | null): string {
		if (isStillDeleting(d)) return 'Deleting: removing its staged files; the job goes when that is done.';
		const n = d?.document_ids.length ?? 0;
		return n
			? `Job deleted. Its ${fmt.n(n)} indexed document${n === 1 ? '' : 's'} stay${n === 1 ? 's' : ''} in the knowledge store.`
			: 'Job deleted.';
	}
	// Cooperative cancel: poll the job until the worker has finished its step and the job is `cancelled`.
	let poll: ReturnType<typeof setTimeout> | null = null;
	function watch(id: string) {
		if (poll) clearTimeout(poll);
		poll = setTimeout(async () => {
			poll = null;
			let j: IngestJob;
			try {
				j = await api().ingestJob(id);
			} catch {
				return; // gone (deleted elsewhere) or unreachable: the next refresh shows the truth
			}
			if (cur?.id === id) cur = j;
			if (j.status === 'cancelling') watch(id);
			else await Promise.all([jobs.refresh(), all.refresh()]);
		}, 1500);
	}
	// strata_ingest#11 202: poll the deleting job until `GET` answers 404 (gone); any other answer keeps polling, an
	// error other than 404 stops (the delete is then unfinished; Retry delete resumes it).
	let deletePoll: ReturnType<typeof setTimeout> | null = null;
	const DELETE_POLL_MS = 1500;
	function watchDelete(id: string) {
		if (deletePoll) clearTimeout(deletePoll);
		deletePoll = setTimeout(async () => {
			deletePoll = null;
			try {
				const j = await api().ingestJob(id);
				if (cur?.id === id) cur = j;
				if (j.status === 'deleting') return watchDelete(id);
				deletingId = null;
			} catch (e) {
				deletingId = null;
				if (e instanceof ApiError && e.status === 404) {
					toast('Job deleted.');
					if (cur?.id === id) open = false;
				} else {
					deleteError = {
						id,
						code: e instanceof ApiError ? e.code : 'error',
						message: (e as Error).message,
					};
				}
			}
			await Promise.all([jobs.refresh(), all.refresh()]);
		}, DELETE_POLL_MS);
	}
	onDestroy(() => {
		if (poll) clearTimeout(poll);
		if (deletePoll) clearTimeout(deletePoll);
	});
	const curStore = $derived(cur ? storeKey(cur.store_id) : '');
	const retryTitle = (j: IngestJob, ready: boolean) =>
		!canWrite
			? writeHint('knowledge_manager')
			: actionBlock(j, 'retry')
				? actionBlock(j, 'retry')!
				: !ready
					? 'Fix the cause first, or the retry fails the same way'
					: undefined;
</script>

<PageHead
	id="T8"
	sub="Ingest turns uploads, website crawls and tables into passages your agents can search. Every job walks the same five stages into one knowledge store; when a stage stops, fix its cause and retry."
>
	{#snippet actions()}
		<Freshness resource={jobs} />
		<a class="btn" href="/admin/knowledge/stores">Stores</a>
		<a class="btn" href={jobPath(app.job, '/sources')}>Sources</a>
	{/snippet}
</PageHead>

<LiveNote what="Ingest jobs are the data plane: retries, cancellations and deletions apply at once." />

{#if missing}
	<Callout kind="gap" title="Job not found"
		>No ingest job <span class="mono">{missing}</span> here; it may have been deleted. Documents it indexed stay
		in their knowledge store.</Callout
	>
{/if}

<div class="stack t8">
	{#if all.value.state === 'ok'}
		<p class="t8-summary" data-testid="ingest-summary">
			<span><b>{allJobs.length}</b> jobs</span>
			<span class:t8-bad={failed.length > 0}
				><b>{failed.length}</b> need{failed.length === 1 ? 's' : ''} you</span
			>
			<span><b>{inFlight.length}</b> in flight</span>
			<span><b>{fmt.n(docsIndexed)}</b> documents indexed</span>
			<span><b>{eur(cost)}</b> metered · <a href="/admin/usage">Usage</a></span>
		</p>
	{/if}

	<Card
		title="How a source becomes knowledge"
		sub="Slots in Read and Embed come from Providers. Click a job to see its steps."
	>
		<Pipeline {load} onpick={show} />
	</Card>

	{#if failed.length}
		<Card
			title="Needs you"
			sub="Automatic retries are used up. Fix the cause, then Retry; a blind retry fails the same way."
		>
			<ul class="t8-needs" data-testid="ingest-needs-you">
				{#each failed as j (j.id)}
					{@const a = failureAdvice(j, slotItems)}
					<li>
						<div class="t8-needs-head">
							<button type="button" class="table-action" onclick={() => show(j)}
								><b>{j.source_label}</b></button
							>
							<StageTrack job={j} />
							<span class="muted"
								>→ <span class="mono">{storeKey(j.store_id)}</span> · {when(j.created_at)}</span
							>
						</div>
						<p class="t8-cause">
							Stopped at <b>{STAGES.find((s) => s.key === a.stage)?.label ?? '—'}</b>: {a.cause}
						</p>
						<ol class="t8-fix">
							{#if a.fix}
								<li><a href={a.fix.href}>{a.fix.label}</a> on Providers.</li>
							{:else if !a.retryReady}
								<li>Fix the cause above.</li>
							{/if}
							<li>
								<button
									class="btn sm"
									type="button"
									disabled={!canWrite || !a.retryReady}
									title={retryTitle(j, a.retryReady)}
									onclick={() => act('retry', j.id)}>Retry</button
								>
								<span class="muted">re-runs only the steps that did not finish.</span>
							</li>
						</ol>
					</li>
				{/each}
			</ul>
		</Card>
	{/if}

	{#each drift as d (d.slot.slot)}
		<Callout kind="gap" title="{d.slot.label} slot moved v{d.from} → v{d.to}">
			{d.slot.drift!.changed_by} wrote v{d.to} on {when(d.slot.drift!.at)}. This tenant keeps reading with v{d.from}
			until you review it; nothing is overwritten. {d.jobs.length} finished job{d.jobs.length === 1
				? ''
				: 's'} read with v{d.from}: {d.jobs.map((j) => j.source_label).join(', ')}. Adopting v{d.to} affects new
			jobs only — re-run a source or re-upload a file to re-read it.
			<a href="/admin/providers?tab=ingest">Review on Providers</a>
		</Callout>
	{/each}

	<Card title="All jobs" sub="Newest first. Each cell of the track is one stage.">
		{#snippet actions()}
			<div class="t8-filters">
				<label class="field"
					><span class="label">Status</span>
					<select class="input" bind:value={status}>
						<option value="">All</option>
						{#each STATUSES as s (s)}<option value={s}>{s}</option>{/each}
					</select></label
				>
				<label class="field"
					><span class="label">Store</span>
					<select class="input" bind:value={store}>
						<option value="">All</option>
						{#each storeItems.filter((s) => s.kind === 'leaf') as s (s.id)}<option value={s.id}
								>{s.key}</option
							>{/each}
					</select></label
				>
			</div>
		{/snippet}
		<LoadState
			resource={jobs}
			empty={status || store
				? 'No ingest jobs match these filters.'
				: 'No ingest jobs yet. Add a source (website, file, table) on Sources, or upload a document to a knowledge store; each one becomes a job here.'}
		>
			{#snippet children(pg)}
				<div class="tbl-wrap ph-stack">
					<table class="tbl clickable t8-table" data-testid="ingest-jobs">
						<thead>
							<tr
								><th>Source</th><th>Pipeline</th><th>Status</th><th>Feeds store</th><th class="r"
									>Documents</th
								><th class="r">Cost</th><th>Started</th></tr
							>
						</thead>
						<tbody>
							{#each pg.items as j (j.id)}
								<tr data-job={j.id} onclick={() => show(j)}>
									<td
										><button
											class="table-action"
											type="button"
											onclick={(e) => (e.stopPropagation(), show(j))}>{j.source_label}</button
										><span class="cell-sub"
											>{j.source_kind}{j.pages ? ` · ${pagesSummary(j.pages)}` : ''}</span
										></td
									>
									<td data-label="Pipeline"><StageTrack job={j} /></td>
									<td data-label="Status">
										<Chip text={j.status} variant={variant(j.status)} />
										{#if j.error}<span class="cell-sub">{j.error.code}</span>{/if}
									</td>
									<td class="mono" data-label="Feeds store">{storeKey(j.store_id)}</td>
									<td class="r" data-label="Documents">{active(j) ? '—' : fmt.n(j.documents)}</td>
									<td class="r" data-label="Cost">{eur(j.cost_eur)}</td>
									<td data-label="Started"
										><span class="mono">{when(j.created_at)}</span><span class="cell-sub"
											>{j.created_by}{j.finished_at ? ` · ${took(j.created_at, j.finished_at)}` : ''}</span
										></td
									>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
			{/snippet}
		</LoadState>
	</Card>
</div>

<Drawer bind:open title={cur ? cur.source_label : ''} wide>
	{#if cur && curStages}
		<div class="stack">
			<p class="row">
				<Chip text={cur.status} variant={variant(cur.status)} />
				<span class="muted"
					>{cur.source_kind} · started {when(cur.created_at)} by {cur.created_by}{cur.finished_at
						? ` · took ${took(cur.created_at, cur.finished_at)}`
						: ''}</span
				>
			</p>
			<dl class="t8-facts">
				<div>
					<dt>Feeds</dt>
					<dd><a class="mono" href="/admin/knowledge/stores">{storeKey(cur.store_id)}</a></dd>
				</div>
				<div>
					<dt>From</dt>
					<dd>
						{#if cur.source_id}<a href={jobPath(app.job, '/sources')}>source {cur.source_id}</a>{:else}direct
							upload{/if}
					</dd>
				</div>
				<div>
					<dt>Produced</dt>
					<dd>
						{fmt.n(cur.documents)} documents{cur.chunks != null ? ` · ${fmt.n(cur.chunks)} passages` : ''}
					</dd>
				</div>
				<div>
					<dt>Cost</dt>
					<dd>{eur(cur.cost_eur)}</dd>
				</div>
			</dl>
			{#if cur.status === 'cancelling'}
				<div data-testid="ingest-cancelling">
					<Callout kind="gap" title="Cancelling"
						>The worker is finishing the step in progress ({cur.steps
							.filter((s) => s.status === 'running')
							.map((s) => s.kind)
							.join(', ') || 'last step'}); nothing after it runs. It becomes cancelled when that step ends;
						it can be deleted then.</Callout
					>
				</div>
			{/if}
			{#if deletingId === cur.id}
				<div data-testid="ingest-deleting">
					<Callout kind="gap" title="Deleting…"
						>Removing its staged files, then the job. Documents it indexed stay in <span class="mono"
							>{curStore}</span
						>.</Callout
					>
				</div>
			{:else if cur.status === 'deleting'}
				{@const err = deleteError?.id === cur.id ? deleteError : null}
				<div data-testid="ingest-delete-unfinished">
					<Callout kind="err" title="Delete did not finish"
						>{#if err}<span class="mono">{err.code}</span>: {sentence(err.message)}{' '}{/if}The job stays
						<b>deleting</b>: its record is kept, but some of its staged files may already be gone, so it can
						no longer be retried or cancelled. Retry delete to finish it; documents it indexed stay in
						<span class="mono">{curStore}</span>.</Callout
					>
				</div>
			{/if}
			{#if cur.pages}<p class="cell-sub">Page jobs: {pagesSummary(cur.pages)}</p>{/if}
			{#if cur.error}
				<Callout kind="err" title={cur.error.code}
					><span
						>{cur.error.message}
						{#if advice?.fix}<a href={advice.fix.href}>{advice.fix.label}</a>, then retry.{/if}</span
					></Callout
				>
			{/if}
			<StageTrack job={cur} labels />
			<ol class="t8-steps" aria-label="Steps by stage">
				{#each STAGES as s (s.key)}
					{@const steps = cur.steps.filter((x) => stageOf(x.kind) === s.key)}
					<li class="t8-step-stage {curStages[s.key]}">
						<div class="t8-step-head">
							<b>{s.label}</b>
							<Chip
								text={curStages[s.key] === 'none' ? 'not needed' : curStages[s.key]}
								variant={curStages[s.key] === 'done'
									? 'ok'
									: curStages[s.key] === 'failed'
										? 'err'
										: curStages[s.key] === 'running'
											? 'accent'
											: 'muted'}
							/>
						</div>
						{#each steps as st, i (i)}
							<div class="t8-step">
								<span>{st.kind}</span>
								<span class="mono muted"
									>{st.slot
										? `${st.slot} · ${st.provider ? `${st.provider} · ${st.model}` : 'no provider'}${st.slot_version ? ` v${st.slot_version}` : ''}`
										: 'built-in'}</span
								>
								<Chip text={st.status} variant={variant(st.status)} />
								{#if attemptNote(st)}<span class="cell-sub">{attemptNote(st)}</span>{/if}
								{#if st.error}<span class="cell-sub t8-err">{st.error.code}: {st.error.message}</span>{/if}
							</div>
						{:else}
							{#if s.key === 'index' && cur.status === 'completed'}
								<p class="cell-sub">Written into {storeKey(cur.store_id)}.</p>
							{:else if cur.steps.length === 0}
								<p class="cell-sub">Waiting for a strata_ingest worker.</p>
							{/if}
						{/each}
					</li>
				{/each}
			</ol>
			{#if confirm}
				<div
					class="callout err"
					role="alertdialog"
					aria-label={confirm === 'cancel' ? 'Cancel job' : 'Delete job'}
				>
					<strong>{confirm === 'cancel' ? 'Cancel this job?' : 'Delete this job?'}</strong>
					{#if confirm === 'cancel'}
						<span>{cancelEffect(cur)} Work already done stays.</span>
					{:else}
						<span
							>Removes the job, its steps and its staged files (the uploaded original, figures, page images,
							audio segments){cur.source_kind === 'crawl' || cur.source_kind === 'sitemap'
								? ', and its page jobs'
								: ''}. This cannot be undone.</span
						>
						<span
							><b>Documents it already indexed stay</b> in the knowledge store
							<span class="mono">{curStore}</span>{cur.documents
								? ` (${fmt.n(cur.documents)} document${cur.documents === 1 ? '' : 's'})`
								: ''}; delete them on <a href="/admin/knowledge/stores">Stores</a> if they should go too.</span
						>
					{/if}
					<span
						><button class="btn sm danger" type="button" onclick={() => act(confirm!)}
							>{confirm === 'cancel' ? 'Cancel job' : 'Delete job'}</button
						>
						<button class="btn sm" type="button" onclick={() => (confirm = null)}>Keep</button></span
					>
				</div>
			{/if}
			<div class="actions">
				<button
					class="btn"
					type="button"
					disabled={!canWrite || !!actionBlock(cur, 'retry') || (advice ? !advice.retryReady : false)}
					title={retryTitle(cur, advice ? advice.retryReady : true)}
					onclick={() => act('retry')}>Retry</button
				>
				<button
					class="btn"
					type="button"
					disabled={!canWrite || !!actionBlock(cur, 'cancel')}
					title={!canWrite ? writeHint('knowledge_manager') : (actionBlock(cur, 'cancel') ?? undefined)}
					onclick={() => (confirm = 'cancel')}
					>{cur.status === 'cancelling' ? 'Cancelling…' : 'Cancel job'}</button
				>
				<button
					class="btn danger"
					type="button"
					disabled={!canWrite || !!actionBlock(cur, 'delete') || deletingId === cur.id}
					title={!canWrite ? writeHint('knowledge_manager') : (actionBlock(cur, 'delete') ?? undefined)}
					onclick={() => (cur!.status === 'deleting' ? act('delete') : (confirm = 'delete'))}
					>{deletingId === cur.id
						? 'Deleting…'
						: cur.status === 'deleting'
							? 'Retry delete'
							: 'Delete'}</button
				>
			</div>
		</div>
	{/if}
</Drawer>

<style>
	.t8-table {
		min-width: 820px;
	}
	.t8-summary {
		margin: 0;
		display: flex;
		flex-wrap: wrap;
		gap: 6px 18px;
		font-size: 13px;
		color: var(--muted);
	}
	.t8-summary b {
		color: var(--fg);
		font-variant-numeric: tabular-nums;
	}
	.t8-bad,
	.t8-bad b {
		color: var(--err);
	}
	.t8-needs {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 12px;
	}
	.t8-needs > li {
		border-left: 3px solid var(--err);
		padding: 2px 0 2px 12px;
		display: grid;
		gap: 6px;
	}
	.t8-needs-head {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 6px 12px;
	}
	.t8-cause {
		margin: 0;
		font-size: 13px;
	}
	.t8-fix {
		margin: 0;
		padding-left: 20px;
		display: grid;
		gap: 6px;
		font-size: 13px;
	}
	.t8-filters {
		display: flex;
		gap: 10px;
		flex-wrap: wrap;
	}
	.t8-facts {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(140px, 1fr));
		gap: 8px 16px;
		margin: 0;
	}
	.t8-facts dt {
		font: 600 11px/1.2 var(--cond);
		letter-spacing: 0.07em;
		text-transform: uppercase;
		color: var(--muted);
	}
	.t8-facts dd {
		margin: 2px 0 0;
		font-size: 13px;
	}
	.t8-steps {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 0;
	}
	.t8-step-stage {
		border-left: 2px solid var(--line);
		padding: 6px 0 8px 12px;
		display: grid;
		gap: 4px;
	}
	.t8-step-stage.done {
		border-left-color: var(--ok);
	}
	.t8-step-stage.running {
		border-left-color: var(--accent);
	}
	.t8-step-stage.failed {
		border-left-color: var(--err);
	}
	.t8-step-head {
		display: flex;
		gap: 8px;
		align-items: center;
	}
	.t8-step {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 4px 10px;
		font-size: 13px;
	}
	.t8-step .cell-sub {
		flex-basis: 100%;
	}
	.t8-err {
		color: var(--err);
	}
</style>
