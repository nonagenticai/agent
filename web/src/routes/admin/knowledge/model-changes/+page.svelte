<script lang="ts">
	// U-06 Model changes (D83, D92, D93; B3 T4a/T14). A provider slot save (any of the six capabilities) never
	// changes a job by itself: every job whose pinned store generation runs the old model gets a model change here.
	// Per job a person starts the rebuild — Re-embed (embed), Copy (rerank / chat: vectors copied, no GPU embed) or
	// Re-ingest (ocr / vision / transcribe, from the kept originals) — and, once every store is ready, accepts it into
	// that job's draft (D54); customers get it when the job publishes. Documents strata_ingest kept no original of are
	// listed: accepting then needs an explicit acknowledgement, and they must be re-uploaded or re-synced.
	// `?id=<change>` opens a change.
	import { onMount } from 'svelte';
	import { page } from '$app/state';
	import { api, guardOutcome } from '$lib/app.svelte';
	import { ApiError } from '$lib/api/errors';
	import { session } from '$lib/auth/session.svelte';
	import { canWrite as canWriteFor, writeAttrs, writeHint } from '$lib/auth/roles';
	import { Resource } from '$lib/load.svelte';
	import { jobPath } from '$lib/scope';
	import { fmt } from '$lib/format';
	import { when } from '$lib/tenant/ui';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Freshness from '$lib/ui/Freshness.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import {
		MODEL_REBUILD_LABEL,
		type MissingOriginal,
		type ModelChange,
		type ModelChangeJob,
		type ModelChangeState,
		type ModelChangeSummary,
	} from '$lib/api/types/tenant';

	const list = new Resource<ModelChangeSummary[]>(
		() => api().modelChanges(),
		(d) => d.length === 0,
	);
	let openId = $state<string | null>(null);
	let detail = $state<ModelChange | null>(null);
	let detailErr = $state('');
	let busy = $state<string | null>(null);
	/** A refused accept's missing originals, per job: shown with an "Accept anyway" acknowledgement. */
	let missing = $state<Record<string, MissingOriginal[]>>({});
	let notReady = $state<Record<string, string[]>>({});
	const canWrite = $derived(canWriteFor(session, 'knowledge_manager'));

	onMount(async () => {
		await list.refresh();
		const want = page.url.searchParams.get('id');
		if (want) await show(want);
	});

	async function show(id: string) {
		openId = id;
		detailErr = '';
		try {
			detail = await api().modelChange(id);
		} catch (e) {
			detail = null;
			detailErr = e instanceof ApiError ? `${e.message} (${e.code})` : String(e);
		}
	}
	async function reload() {
		await list.refresh();
		if (openId) await show(openId);
	}

	const STATE: Record<
		ModelChangeState,
		{ text: string; variant: 'ok' | 'warn' | 'err' | 'muted' | 'accent' }
	> = {
		pending: { text: 'waiting for a rebuild', variant: 'warn' },
		reembedding: { text: 'rebuilding', variant: 'accent' },
		ready: { text: 'ready to accept', variant: 'ok' },
		accepted: { text: 'in the draft', variant: 'accent' },
		published: { text: 'published', variant: 'ok' },
		dismissed: { text: 'dismissed', variant: 'muted' },
	};
	const model = (m: string | null) => m ?? 'deployment default';
	const open = (ch: ModelChangeSummary) => ch.jobs.filter((j) => j.state !== 'published').length;

	async function rebuild(ch: ModelChange, j: ModelChangeJob) {
		busy = `${ch.id}:${j.job}`;
		await guardOutcome(
			() => api().reembedJobStore(ch.id, j.job),
			`${j.job}: ${MODEL_REBUILD_LABEL[ch.rebuild].toLowerCase()} started.`,
			'Not started',
		);
		busy = null;
		await reload();
	}
	async function accept(ch: ModelChange, j: ModelChangeJob, acknowledge = false) {
		const key = `${ch.id}:${j.job}`;
		busy = key;
		const out = await guardOutcome(
			() => api().acceptModelChange(ch.id, j.job, acknowledge ? { acknowledge_missing: true } : {}),
			`${j.job}: the new ${ch.slot} model is in its draft. Test it, then publish.`,
			'Not accepted',
		);
		busy = null;
		if (out.kind !== 'ok' && out.error instanceof ApiError) {
			const d = out.error.details;
			if (out.error.code === 'originals_not_retained' && Array.isArray(d.documents))
				missing = { ...missing, [key]: d.documents as MissingOriginal[] };
			if (out.error.code === 'generation_not_ready' && Array.isArray(d.stores))
				notReady = { ...notReady, [key]: d.stores as string[] };
		} else {
			const { [key]: _m, ...restM } = missing;
			const { [key]: _n, ...restN } = notReady;
			missing = restM;
			notReady = restN;
		}
		await reload();
	}
</script>

<PageHead
	id="T7"
	title="Model changes"
	sub="A new model for a knowledge capability never changes a job by itself. Rebuild each job's stores with the new model, then accept it into that job's draft; customers get it when the job publishes."
>
	{#snippet actions()}
		<Freshness resource={list} />
		<a class="btn" href="/admin/providers">Providers</a>
		<a class="btn" href="/admin/knowledge/stores">Knowledge stores</a>
	{/snippet}
</PageHead>

{#if !canWrite}<p class="note">{writeHint('knowledge_manager')} to rebuild or accept a model change.</p>{/if}

<LoadState
	resource={list}
	empty="No model changes. Changing a provider slot's model creates one for every job that uses the old model."
>
	{#snippet children(changes)}
		<div class="mc-grid">
			<Card
				cls="flush"
				title="Changes"
				sub="Newest first. Each change lists the jobs still on the old model."
			>
				<div class="tbl-wrap">
					<table class="tbl" data-testid="model-changes">
						<thead
							><tr><th>Capability</th><th>From → to</th><th>Rebuild</th><th>Jobs</th><th>Created</th></tr
							></thead
						>
						<tbody>
							{#each changes as ch (ch.id)}
								<tr
									data-change={ch.id}
									class:sel={openId === ch.id}
									onclick={() => show(ch.id)}
									style="cursor:pointer"
								>
									<td><b class="mono">{ch.slot}</b></td>
									<td class="mono">{model(ch.from_model)} → {model(ch.to_model)}</td>
									<td>{MODEL_REBUILD_LABEL[ch.rebuild]}</td>
									<td>
										<span class="chips">
											{#each ch.jobs as j (j.job)}<Chip
													text={`${j.job}: ${STATE[j.state].text}`}
													variant={STATE[j.state].variant}
												/>{/each}
										</span>
										{#if open(ch)}<span class="cell-sub">{open(ch)} open</span>{/if}
									</td>
									<td
										><span class="cell-sub">{when(ch.created_at)}</span><span class="cell-sub"
											>by {ch.created_by}</span
										></td
									>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
			</Card>

			{#if detailErr}
				<Callout kind="err" title="Could not open the change"><span>{detailErr}</span></Callout>
			{:else if detail}
				{@const ch = detail}
				<Card
					title={`${ch.slot}: ${model(ch.from_model)} → ${model(ch.to_model)}`}
					sub={`${MODEL_REBUILD_LABEL[ch.rebuild]} per job. ${
						ch.rebuild === 'reembed'
							? 'Re-embedding runs the new embed model over every passage (GPU).'
							: ch.rebuild === 'copy'
								? 'Copy keeps the vectors and binds the new model: no embed call.'
								: 'Re-ingest reads every kept original again with the new model.'
					}`}
				>
					<div class="mc-jobs" data-testid="model-change-jobs">
						{#each ch.jobs as j (j.job)}
							{@const key = `${ch.id}:${j.job}`}
							<section class="mc-job" data-job={j.job}>
								<header class="mc-job-head">
									<a class="mono" href={jobPath(j.job)}><b>{j.job}</b></a>
									<Chip text={STATE[j.state].text} variant={STATE[j.state].variant} />
									<span class="cell-sub"
										>{j.live_release ? `live: ${j.live_release}` : 'never published'} · draft rev {j.draft_rev}</span
									>
								</header>
								<table class="tbl mc-stores">
									<thead><tr><th>Store</th><th>Now</th><th>New</th><th>Passages</th></tr></thead>
									<tbody>
										{#each j.stores as st (st.store_id)}
											<tr>
												<td class="mono">{st.store ?? '(store gone)'}</td>
												<td class="mono"
													>{st.from_generation
														? `${model(st.from_generation.models[ch.slot])} · ${st.from_generation.build}`
														: '—'}</td
												>
												<td>
													{#if st.to_generation}
														<span class="mono">{model(st.to_generation.models[ch.slot])}</span>
														<Chip
															text={st.to_generation.state}
															variant={st.to_generation.state === 'ready'
																? 'ok'
																: st.to_generation.state === 'failed'
																	? 'err'
																	: 'accent'}
														/>
														{#if st.to_generation.error}<span class="cell-sub err-text"
																>{st.to_generation.error}</span
															>{/if}
													{:else}<span class="muted">not rebuilt yet</span>{/if}
												</td>
												<td>{st.chunks == null ? '—' : fmt.n(st.chunks)}</td>
											</tr>
											{#if st.missing_originals.length}
												<tr
													><td colspan="4"
														><span class="cell-sub warn-text" data-testid="missing-originals"
															>No original kept for {st.missing_originals.length} document{st
																.missing_originals.length === 1
																? ''
																: 's'}: {st.missing_originals
																.map((d) => d.title ?? d.document_id)
																.join(', ')}. Re-upload or re-sync them after accepting.</span
														></td
													></tr
												>
											{/if}
										{/each}
									</tbody>
								</table>
								{#if notReady[key]}
									<p class="cell-sub err-text" role="alert">
										Not ready yet: {notReady[key]!.join(', ')}. Rebuild first.
									</p>
								{/if}
								{#if missing[key]}
									<div class="callout gap" role="alertdialog" aria-label="Accept without originals">
										<strong>{missing[key]!.length} document(s) have no kept original</strong>
										<span
											>Accepting leaves them out of the new generation until you re-upload or re-sync them:
											{missing[key]!.map((d) => d.title ?? d.document_id).join(', ')}.</span
										>
										<span
											><button
												class="btn sm"
												type="button"
												{...writeAttrs(session, 'knowledge_manager')}
												disabled={!canWrite || busy === key}
												onclick={() => accept(ch, j, true)}>Accept anyway</button
											></span
										>
									</div>
								{/if}
								<div class="mc-actions">
									{#if j.state === 'pending'}
										<button
											class="btn primary sm"
											type="button"
											{...writeAttrs(session, 'knowledge_manager')}
											disabled={!canWrite || busy === key}
											onclick={() => rebuild(ch, j)}>{MODEL_REBUILD_LABEL[ch.rebuild]}</button
										>
									{:else if j.state === 'reembedding'}
										<button class="btn sm" type="button" onclick={() => show(ch.id)}>Refresh</button>
									{/if}
									{#if j.state === 'pending' || j.state === 'reembedding' || j.state === 'ready'}
										<button
											class="btn sm"
											class:primary={j.state === 'ready'}
											type="button"
											{...writeAttrs(session, 'knowledge_manager')}
											disabled={!canWrite || busy === key || j.state !== 'ready'}
											title={j.state === 'ready' ? undefined : 'Rebuild first: every store must be ready.'}
											onclick={() => accept(ch, j)}>Accept into draft</button
										>
									{:else if j.state === 'accepted'}
										<a class="btn sm" href={jobPath(j.job, '/release')}>Publish the job</a>
									{/if}
								</div>
							</section>
						{/each}
					</div>
				</Card>
			{:else}
				<div class="empty pad">Pick a change to see its jobs.</div>
			{/if}
		</div>
	{/snippet}
</LoadState>

<style>
	.mc-grid {
		display: grid;
		gap: 16px;
	}
	.chips {
		display: flex;
		flex-wrap: wrap;
		gap: 4px;
	}
	tr.sel {
		background: color-mix(in srgb, var(--accent) 8%, transparent);
	}
	.mc-jobs {
		display: grid;
		gap: 14px;
	}
	.mc-job {
		display: grid;
		gap: 8px;
		padding-top: 10px;
		border-top: 1px solid var(--line);
		min-width: 0;
	}
	.mc-job-head {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
		align-items: center;
	}
	.mc-stores {
		width: 100%;
	}
	.mc-actions {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
	}
	.err-text {
		color: var(--err);
	}
	.warn-text {
		color: var(--warn);
	}
	.pad {
		padding: 16px;
	}
</style>
