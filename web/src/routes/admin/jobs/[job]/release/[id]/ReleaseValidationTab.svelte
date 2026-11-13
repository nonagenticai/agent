<script lang="ts">
	// Release detail › Validation (B8 T12a, F-06): the knowledge questions graded on THIS release's cut, and the latest
	// grid run — queued by the approve, then progress, then done. Polls `releaseValidation` every few seconds while a run
	// is queued or running, and only while this tab is open (a page poll; the server runs nothing on a schedule).
	import { onDestroy, onMount } from 'svelte';
	import { api, app } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import { jobPath } from '$lib/scope';
	import type { RejectReason, ReleaseValidation } from '$lib/api/types/journey';
	import { when } from '$lib/journey/format';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Kpi from '$lib/ui/Kpi.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Meter from '$lib/ui/Meter.svelte';
	import {
		gridActive,
		gridProgress,
		gridStatusLine,
		releaseValidationUi,
		triggerLabel,
	} from './validation-tab';

	let { id }: { id: string } = $props();

	const res = new Resource<ReleaseValidation>(
		() => api().releaseValidation(id),
		() => false,
	);
	let timer: ReturnType<typeof setTimeout> | undefined;
	let closed = false;
	function schedule() {
		clearTimeout(timer);
		const v = res.value;
		if (closed || v.state !== 'ok' || !gridActive(v.data.grid)) return;
		timer = setTimeout(async () => {
			await res.refresh();
			schedule();
		}, releaseValidationUi.pollMs);
	}
	onMount(async () => {
		await res.refresh();
		schedule();
	});
	onDestroy(() => {
		closed = true;
		clearTimeout(timer);
	});

	const REASON: Record<Exclude<RejectReason, null>, string> = {
		missing_citation: 'missing citation',
		wrong_citation: 'wrong citation',
		incorrect: 'incorrect answer',
	};
	const count = (v: ReleaseValidation, x: string) => v.rows.filter((r) => r.verdict === x).length;
	const STATUS_VARIANT = { queued: 'muted', running: 'accent', done: 'ok', failed: 'err' } as const;
</script>

<div data-testid="release-validation">
	<LoadState resource={res}>
		{#snippet children(v)}
			{@const g = v.grid}
			<Card
				title="Validation run"
				sub="The grid grades every knowledge question on this cut when a person approves the release. It never runs on a schedule, and applying the release does not wait for it."
			>
				<div class="rv-status" data-testid="grid-status" data-state={g?.status ?? 'none'}>
					{#if g}<Chip text={g.status} variant={STATUS_VARIANT[g.status]} />{/if}
					<b>{gridStatusLine(g)}</b>
					{#if g}<span class="muted">· {triggerLabel(g.trigger)}</span>{/if}
				</div>
				{#if g}
					<div class="rv-bar" aria-label="Progress">
						<Meter value={gridProgress(g)} cls={g.status === 'failed' ? 's-err' : 's-accent'} />
						<span class="mono small" data-testid="grid-progress">{g.done} / {g.total}</span>
					</div>
					<dl class="kvs rv-times">
						<dt>Queued</dt>
						<dd>{when(g.queued_at)}</dd>
						<dt>Started</dt>
						<dd>{g.started_at ? when(g.started_at) : '—'}</dd>
						<dt>Finished</dt>
						<dd>{g.finished_at ? when(g.finished_at) : '—'}</dd>
					</dl>
				{:else}
					<p class="muted">
						Nothing has graded this cut yet. Approving the release queues a run; a person can also run it on
						the
						<a href={jobPath(app.job, '/validation')}>Validation</a> page.
					</p>
				{/if}
			</Card>

			<div class="grid g4 rv-kpis">
				<Kpi label="Pass" value={String(count(v, 'pass'))} note="of {v.rows.length} questions" />
				<Kpi label="Rejected" value={String(count(v, 'rejected'))} />
				<Kpi label="Not graded" value={String(count(v, 'pending'))} />
				<Kpi
					label="Last graded"
					value={v.ran_at ? when(v.ran_at).slice(5) : null}
					unavailable={v.ran_at ? '' : 'not graded yet'}
				/>
			</div>

			<Card
				cls="flush"
				title="Grid rows"
				sub="Question, expected answer, reference citation and the verdict on this cut."
			>
				{#if !v.rows.length}
					<div class="empty" data-testid="grid-no-rows">
						This cut has no questions. Add them on the <a href={jobPath(app.job, '/validation')}>Validation</a
						> page.
					</div>
				{:else}
					<div class="tbl-wrap">
						<table class="tbl dense" data-testid="grid-rows">
							<thead
								><tr><th>Question</th><th>Expected answer</th><th>Reference</th><th>Verdict</th></tr></thead
							>
							<tbody>
								{#each v.rows as r (r.id)}
									<tr data-testid="grid-row-{r.id}" class:rej={r.verdict === 'rejected'}>
										<td
											>{r.question}
											{#if r.activity}<div class="cell-sub mono">{r.activity}</div>{/if}</td
										>
										<td>{r.expected}</td>
										<td class="mono small">{r.reference}</td>
										<td>
											<span
												class="chip {r.verdict === 'pass'
													? 'ok'
													: r.verdict === 'rejected'
														? 'err'
														: 'muted'}">{r.verdict === 'pending' ? 'not graded' : r.verdict}</span
											>
											{#if r.reason}<div class="cell-sub err-text">{REASON[r.reason]}</div>{/if}
										</td>
									</tr>
								{/each}
							</tbody>
						</table>
					</div>
				{/if}
			</Card>
		{/snippet}
	</LoadState>
</div>

<style>
	.rv-status {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 8px;
		font-size: 14px;
	}
	.rv-bar {
		display: flex;
		align-items: center;
		gap: 10px;
		margin: 10px 0;
		max-width: 420px;
	}
	.rv-bar :global(.meter) {
		flex: 1;
	}
	.rv-times {
		margin: 8px 0 0;
	}
	.rv-kpis {
		margin: 14px 0;
	}
	.small {
		font-size: 12px;
	}
	.err-text {
		color: var(--err);
	}
	tr.rej td {
		background: color-mix(in srgb, var(--err) 5%, transparent);
	}
</style>
