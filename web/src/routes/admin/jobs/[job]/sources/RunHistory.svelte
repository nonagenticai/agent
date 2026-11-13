<script lang="ts">
	// Runs of one source, newest first, and Re-sync — with the stored or the edited mapping; a file source takes its
	// files here (Agent does not keep them). Each run opens a detail drawer: its ingest jobs, documents, replace report.
	import { api, guard } from '$lib/app.svelte';
	import { writeHint } from '$lib/auth/roles';
	import { Resource } from '$lib/load.svelte';
	import { fmt } from '$lib/format';
	import type { Paged } from '$lib/api/types/core';
	import type { Source, SourceMapping, SourceRun } from '$lib/api/types/tenant';
	import { pagesSummary, RUN_LABELS, resyncToast, structured } from '$lib/tenant/sources';
	import { checkUploads, errorText, replaceSummary } from '$lib/tenant/ingest-files';
	import { when } from '$lib/tenant/ui';
	import Chip from '$lib/ui/Chip.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';
	import UploadDrop from './UploadDrop.svelte';
	import RunDetail from './RunDetail.svelte';

	let {
		source,
		canWrite,
		editedMapping,
		onchange,
	}: {
		source: Source;
		canWrite: boolean;
		/** The form's mapping when it differs from the stored one, else null. */
		editedMapping: SourceMapping[] | null;
		onchange: () => void;
	} = $props();
	const runs = new Resource<Paged<SourceRun>>(() => api().sourceRuns(source.id));
	let files = $state<File[]>([]);
	const sid = $derived(source.id);
	$effect(() => {
		void sid;
		files = [];
		void runs.refresh();
	});
	let busy = $state(false);
	const isFile = $derived(source.type === 'file');
	const check = $derived(checkUploads(files));
	const blocked = $derived(
		!canWrite
			? writeHint('knowledge_manager')
			: !source.enabled
				? 'Enable the source first'
				: busy
					? 'A re-sync is running'
					: isFile && files.length === 0
						? 'Choose the files to ingest'
						: isFile && check.errors.length
							? check.errors[0]!
							: '',
	);
	const variant = (s: string): 'ok' | 'err' | 'warn' | 'muted' =>
		s === 'completed' ? 'ok' : s === 'failed' ? 'err' : s === 'running' ? 'warn' : 'muted';
	const filesText = (r: SourceRun) => {
		if (!r.uploads) return '';
		const bad = r.uploads.filter((u) => u.error).length;
		return `${r.uploads.length} file${r.uploads.length === 1 ? '' : 's'}${bad ? ` · ${bad} refused` : ''}`;
	};

	let detailOpen = $state(false);
	let detailRun = $state<SourceRun | null>(null);
	function openRun(r: SourceRun) {
		detailRun = r;
		detailOpen = true;
	}

	async function resync(withEdited: boolean) {
		busy = true;
		const box: { run: SourceRun | null } = { run: null };
		const opts: { mapping?: SourceMapping[]; files?: File[] } = {};
		if (withEdited && editedMapping) opts.mapping = editedMapping;
		if (isFile) opts.files = files;
		const ok = await guard(
			async () => {
				box.run = await api().resyncSource(source.id, Object.keys(opts).length ? opts : undefined);
			},
			() => (box.run ? resyncToast(box.run) : 'Re-sync started.'),
			'Re-sync refused',
		);
		busy = false;
		if (ok) files = [];
		await runs.refresh();
		onchange();
	}
</script>

<section class="runs">
	<div class="row">
		<h3>Runs</h3>
		<span class="muted runs-hint"
			>{isFile
				? 'Each re-sync uploads the full set of files: one ingest job per file; when every file is ingested, their documents replace the previous run’s.'
				: 'Each run hands one ingest job to strata_ingest; its documents replace the previous run’s in the store.'}</span
		>
		<button
			class="btn primary"
			type="button"
			disabled={blocked !== ''}
			title={blocked || undefined}
			onclick={() => resync(false)}
			>{isFile
				? `Upload and re-sync${files.length ? ` (${files.length})` : ''}`
				: structured(source.type)
					? 'Re-sync with the saved mapping'
					: 'Re-sync'}</button
		>
		{#if structured(source.type) && editedMapping}
			<button
				class="btn"
				type="button"
				disabled={blocked !== ''}
				title={blocked || undefined}
				onclick={() => resync(true)}>Re-sync with the edited mapping</button
			>
		{/if}
		<button class="btn" type="button" onclick={() => runs.refresh()}>Refresh</button>
	</div>
	{#if isFile}
		<UploadDrop
			bind:files
			disabled={!canWrite || busy}
			why={canWrite ? '' : writeHint('knowledge_manager')}
		/>
	{/if}
	{#if runs.value.state === 'unavailable'}
		<Unavailable reason={runs.value.reason} retry={runs.retry} />
	{:else if runs.value.state === 'empty'}
		<div class="empty">Not synced yet.</div>
	{:else if runs.value.state === 'ok'}
		<div class="tbl-wrap">
			<table class="tbl dense" data-testid="source-runs">
				<thead
					><tr
						><th>Started</th><th>Status</th><th class="r">Documents</th><th>{isFile ? 'Files' : 'Mapping'}</th
						><th>Details</th></tr
					></thead
				>
				<tbody>
					{#each runs.value.data.items as r (r.id)}
						<tr data-run={r.id}>
							<td class="mono nowrap"
								><button class="table-action" type="button" onclick={() => openRun(r)}
									>{when(r.started_at)}</button
								><span class="cell-sub">{r.started_by}</span></td
							>
							<td><Chip text={RUN_LABELS[r.status]} variant={variant(r.status)} /></td>
							<td class="r">{r.status === 'running' ? '—' : fmt.n(r.documents)}</td>
							<td>{isFile ? filesText(r) || '—' : (r.mapping ?? '—')}</td>
							<td class="details">
								{#if r.pages}<span class="cell-sub">{pagesSummary(r.pages)}</span>{/if}
								{#if r.error}<span class="cell-sub err-text">{errorText(r.error)}</span>{/if}
								{#if r.replace_report}<span class="cell-sub">{replaceSummary(r.replace_report)}</span>{/if}
								{#each r.ingest_job_ids.slice(0, 3) as id (id)}<a
										class="cell-sub"
										href={`/admin/knowledge/jobs?id=${encodeURIComponent(id)}`}>ingest job {id}</a
									>{/each}
								{#if r.ingest_job_ids.length > 3}<span class="cell-sub"
										>+{r.ingest_job_ids.length - 3} more ingest jobs</span
									>{/if}
								<button class="btn sm" type="button" onclick={() => openRun(r)}>Run details</button>
							</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	{:else}
		<p class="muted">Loading…</p>
	{/if}
</section>

<Drawer bind:open={detailOpen} title={detailRun ? `Run ${when(detailRun.started_at)}` : 'Run'}>
	{#if detailRun && detailOpen}
		<RunDetail sourceId={source.id} runId={detailRun.id} />
	{/if}
</Drawer>

<style>
	.runs-hint {
		flex-basis: 100%;
		font-size: 12.5px;
		order: 9;
	}
	.runs {
		margin-top: 16px;
		display: flex;
		flex-direction: column;
		gap: 10px;
	}
	.err-text {
		color: var(--err);
	}
	.details {
		min-width: 180px;
	}
	.details :global(.cell-sub) {
		overflow-wrap: anywhere;
	}
</style>
