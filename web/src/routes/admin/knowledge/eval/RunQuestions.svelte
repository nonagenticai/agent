<script lang="ts">
	// One run's per-question outcome (strata_rag `GET /api/eval/runs/{id}`: the question snapshot and `results`): per
	// mode, the rank of the first matching passage (miss = not in the top 10) and whether the first passage came from
	// the expected leaf. The runs list carries summaries only, so the shown run is read on its own.
	import { Latest } from '$lib/load.svelte';
	import type { EvalRunDetail, EvalRunSummary } from '$lib/api/types/retrieval';

	let { run, detail }: { run: EvalRunSummary; detail: (id: string) => Promise<EvalRunDetail> } = $props();

	let full = $state<EvalRunDetail | null>(null);
	let error = $state<string | null>(null);
	const latest = new Latest();

	$effect(() => {
		const id = run.id;
		const ticket = latest.next();
		full = null;
		error = null;
		detail(id)
			.then((d) => {
				if (!latest.stale(ticket)) full = d;
			})
			.catch((e: unknown) => {
				if (!latest.stale(ticket)) error = e instanceof Error ? e.message : String(e);
			});
	});

	const results = $derived(full?.results ?? null);
	const cell = (index: number, mode: string) =>
		results?.find((r) => r.index === index && r.mode === mode) ?? null;
	/** Questions without a hit in the top 10 in the run's first routed mode (hybrid when it ran). */
	const focus = $derived(run.modes.includes('hybrid') ? 'hybrid' : run.modes[0]);
	const missed = $derived(results ? results.filter((r) => r.mode === focus && r.rank == null).length : null);
</script>

<details class="rq" data-testid="ev-run-questions">
	<summary
		>Per question{missed != null ? ` · ${missed} of ${run.question_count} missed in ${focus}` : ''}</summary
	>
	{#if error}
		<p class="rq-err">Per-question results not loaded: {error}</p>
	{:else if !full || !results}
		<p class="muted">Loading…</p>
	{:else}
		<div class="tbl-wrap ph-stack">
			<table class="tbl">
				<thead>
					<tr>
						<th>Question</th>
						{#each run.modes as m (m)}<th class="r">{m}</th>{/each}
					</tr>
				</thead>
				<tbody>
					{#each full.questions as q, i (i)}
						<tr>
							<td>{q.query}</td>
							{#each run.modes as m (m)}
								{@const c = cell(i, m)}
								<td class="r mono" data-label={m}>
									{#if !c}—{:else if c.rank == null}<span class="s-err">miss</span>{:else}<span
											class={c.rank === 1 ? 's-ok' : ''}>#{c.rank}</span
										>{/if}
									{#if c?.leaf_hit != null}<span
											class="rq-leaf {c.leaf_hit ? 's-ok' : 's-err'}"
											title={c.leaf_hit
												? 'first passage from the expected leaf'
												: 'first passage from another store'}>{c.leaf_hit ? 'leaf ✓' : 'leaf ✗'}</span
										>{/if}
								</td>
							{/each}
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	{/if}
</details>

<style>
	.rq {
		margin-top: 10px;
	}
	.rq summary {
		cursor: pointer;
		font-weight: 600;
		font-size: 13px;
	}
	.rq-leaf {
		font-size: 11px;
		margin-left: 4px;
	}
	.rq-err {
		color: var(--err);
		font-size: 12.5px;
	}
</style>
