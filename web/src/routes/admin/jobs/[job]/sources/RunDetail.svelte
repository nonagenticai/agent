<script lang="ts">
	// One run of a source: its uploads (file sources), the ingest jobs it handed to strata_ingest (each opens in T8),
	// the documents they produced, and what the run did to the earlier runs' documents (the replace report).
	import { api } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import { fmt } from '$lib/format';
	import { LANGUAGE_LABEL } from '$lib/api/types/core';
	import type { SourceRunDetail } from '$lib/api/types/tenant';
	import { pagesSummary, RUN_LABELS } from '$lib/tenant/sources';
	import { errorText, fileSize, REPLACE_PARTS, replaceCount } from '$lib/tenant/ingest-files';
	import { when } from '$lib/tenant/ui';
	import Chip from '$lib/ui/Chip.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';

	let { sourceId, runId }: { sourceId: string; runId: string } = $props();
	const detail = new Resource<SourceRunDetail>(
		() => api().sourceRun(sourceId, runId),
		() => false,
	);
	$effect(() => {
		void runId;
		void detail.refresh();
	});
	const variant = (s: string): 'ok' | 'err' | 'warn' | 'muted' =>
		s === 'completed'
			? 'ok'
			: s === 'failed' || s === 'rejected'
				? 'err'
				: s === 'running' || s === 'queued' || s === 'cancelling' || s === 'deleting'
					? 'warn'
					: 'muted';
	const jobHref = (id: string) => `/admin/knowledge/jobs?id=${encodeURIComponent(id)}`;
</script>

<div class="run-detail" data-testid="run-detail">
	{#if detail.value.state === 'unavailable'}
		<Unavailable reason={detail.value.reason} retry={detail.retry} />
	{:else if detail.value.state === 'ok'}
		{@const d = detail.value.data}
		{@const r = d.run}
		<dl class="facts">
			<div>
				<dt>Status</dt>
				<dd><Chip text={RUN_LABELS[r.status]} variant={variant(r.status)} /></dd>
			</div>
			<div>
				<dt>Started</dt>
				<dd class="mono">{when(r.started_at)} · {r.started_by}</dd>
			</div>
			<div>
				<dt>Finished</dt>
				<dd class="mono">{r.finished_at ? when(r.finished_at) : '—'}</dd>
			</div>
			<div>
				<dt>Documents</dt>
				<dd>{fmt.n(r.documents)}</dd>
			</div>
			{#if r.mapping}<div>
					<dt>Mapping</dt>
					<dd>{r.mapping}</dd>
				</div>{/if}
			{#if r.pages}<div>
					<dt>Pages</dt>
					<dd>{pagesSummary(r.pages)}</dd>
				</div>{/if}
		</dl>
		{#if r.error}
			<p class="err-line" role="alert" data-testid="run-error">
				<span class="mono">{r.error.code}</span>: {r.error.message}
			</p>
		{/if}

		{#if r.uploads}
			<h4>Files uploaded</h4>
			<div class="tbl-wrap">
				<table class="tbl dense" data-testid="run-uploads">
					<thead><tr><th>File</th><th class="r">Size</th><th>Outcome</th></tr></thead>
					<tbody>
						{#each r.uploads as u (u.file_name)}
							<tr>
								<td class="mono wrap">{u.file_name}</td>
								<td class="r nowrap">{fileSize(u.bytes)}</td>
								<td>
									{#if u.error}<span class="err-text">{errorText(u.error)}</span>
									{:else if u.ingest_job_id}<a href={jobHref(u.ingest_job_id)}>{u.ingest_job_id}</a>{/if}
								</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		{/if}

		<h4>Ingest jobs <span class="muted">({d.ingest_jobs.length})</span></h4>
		{#if d.ingest_jobs.length}
			<ul class="list" data-testid="run-jobs">
				{#each d.ingest_jobs as j (j.id)}
					<li>
						<a class="mono" href={jobHref(j.id)}>{j.id}</a>
						<Chip text={j.status} variant={variant(j.status)} />
						<span class="cell-sub">{j.source_label} · {fmt.n(j.documents)} documents</span>
						{#if j.error}<span class="cell-sub err-text">{errorText(j.error)}</span>{/if}
					</li>
				{/each}
			</ul>
		{:else}
			<p class="muted">No ingest job: the run stopped before strata_ingest was called.</p>
		{/if}

		<h4>Documents produced <span class="muted">({d.documents.length})</span></h4>
		{#if d.documents.length}
			<ul class="list" data-testid="run-documents">
				{#each d.documents.slice(0, 25) as doc (doc.id)}
					<li>
						<span>{doc.title}</span>
						<Chip text={doc.language} title={LANGUAGE_LABEL[doc.language] ?? doc.language} />
						<span class="cell-sub mono">{doc.source} · {fmt.n(doc.tokens)} tokens</span>
					</li>
				{/each}
			</ul>
			{#if d.documents.length > 25}<p class="muted">
					and {d.documents.length - 25} more in the store.
				</p>{/if}
		{:else}
			<p class="muted">
				{r.documents
					? `${fmt.n(r.documents)} documents counted by strata_ingest; none are listed here.`
					: 'None.'}
			</p>
		{/if}

		<h4>Earlier documents</h4>
		{#if r.replace_report}
			{@const rep = r.replace_report}
			<ul class="replace" data-testid="replace-report">
				{#each REPLACE_PARTS as p (p.key)}
					{@const n = replaceCount(rep, p.key)}
					<li class:warn={p.key === 'failed' && n > 0} title={p.hint}>
						<strong>{fmt.n(n)}</strong><span>{p.label}</span>
					</li>
				{/each}
			</ul>
			{#if rep.failed.length}<p class="cell-sub mono">Not deleted yet: {rep.failed.join(', ')}</p>{/if}
		{:else}
			<p class="muted">
				{r.status === 'running'
					? 'Replaced once the run completes.'
					: 'Nothing replaced: only a completed run replaces the earlier documents, so they are kept.'}
			</p>
		{/if}
	{:else}
		<p class="muted">Loading…</p>
	{/if}
</div>

<style>
	.run-detail {
		display: flex;
		flex-direction: column;
		gap: 8px;
		min-width: 0;
	}
	h4 {
		margin: 8px 0 0;
		font-size: 13.5px;
	}
	.facts {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(150px, 1fr));
		gap: 8px 16px;
		margin: 0;
	}
	.facts dt {
		font-size: 11.5px;
		color: var(--muted);
	}
	.facts dd {
		margin: 0;
		overflow-wrap: anywhere;
	}
	.err-line,
	.err-text {
		color: var(--err);
	}
	.err-line {
		margin: 0;
		overflow-wrap: anywhere;
	}
	.wrap {
		overflow-wrap: anywhere;
	}
	.list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 6px;
	}
	.list li {
		display: flex;
		flex-wrap: wrap;
		gap: 4px 8px;
		align-items: center;
		min-width: 0;
	}
	.replace {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(130px, 1fr));
		gap: 8px;
	}
	.replace li {
		display: flex;
		flex-direction: column;
		padding: 8px 10px;
		border: 1px solid var(--line);
		border-radius: 8px;
		font-size: 12.5px;
	}
	.replace li strong {
		font-size: 18px;
	}
	.replace li.warn {
		border-color: var(--warn);
	}
</style>
