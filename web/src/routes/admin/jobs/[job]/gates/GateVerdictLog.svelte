<script lang="ts">
	// J7 verdict log: the newest per-question verdicts of this job's cases (`GET /api/jobs/{job}/gates/verdicts`),
	// filterable by point, decision, question and below-the-line; each row opens its case. The 7-day aggregates stay on
	// the question rows and in the drawer.
	import { onMount } from 'svelte';
	import { api, app } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import { jobPath } from '$lib/scope';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import type { GateDecision, GatePoint, GateVerdictRow } from '$lib/api/types/ontology';
	import { DECISION_TEXT, RUN_POINT, RUN_POINT_LABEL, filterVerdicts } from '$lib/ontology/gateRules';

	let { limit = 50 }: { limit?: number } = $props();
	const res = new Resource(
		() => api().gateVerdicts(app.job, limit),
		(d: GateVerdictRow[]) => d.length === 0,
	);
	onMount(() => res.refresh());

	let point = $state<GatePoint | 'all'>('all');
	let action = $state<GateDecision | 'all'>('all');
	let gate = $state('all');
	let unsure = $state(false);
	let q = $state('');
	const DECISION_VARIANT = { allow: 'ok', block: 'warn', handover: 'err' } as const;
	const gatesOf = (rows: GateVerdictRow[]) => [...new Set(rows.map((r) => r.gate))].sort();
</script>

<section class="card vl" aria-label="Verdict log" data-testid="gate-verdict-log">
	<header class="vl-h">
		<h2>Verdict log</h2>
		<span class="muted"
			>The newest {limit} answers laya gave in this job's cases. Open a case to see the turn.</span
		>
	</header>
	<LoadState resource={res} empty="No verdicts yet: this job has no cases.">
		{#snippet children(rows)}
			{@const shown = filterVerdicts(rows, { point, action, gate, unsure, q })}
			<div class="vl-f" role="group" aria-label="Filter verdicts">
				<select class="input" aria-label="Point" bind:value={point}>
					<option value="all">All points</option>
					{#each ['P1', 'P2', 'R1'] as const as p (p)}
						<option value={p}>{p} · {RUN_POINT_LABEL[RUN_POINT[p]]}</option>
					{/each}
				</select>
				<select class="input" aria-label="Decision" bind:value={action}>
					<option value="all">All decisions</option>
					{#each ['allow', 'block', 'handover'] as const as a (a)}
						<option value={a}>{DECISION_TEXT[a]}</option>
					{/each}
				</select>
				<select class="input" aria-label="Question" bind:value={gate}>
					<option value="all">All questions</option>
					{#each gatesOf(rows) as g (g)}<option value={g}>{g}</option>{/each}
				</select>
				<label class="vl-chk"><input type="checkbox" bind:checked={unsure} /> Below the line</label>
				<input
					class="input vl-q"
					type="search"
					aria-label="Search verdicts"
					placeholder="Search"
					bind:value={q}
				/>
				<span class="muted vl-n">{shown.length} of {rows.length}</span>
			</div>
			<div class="tbl-wrap">
				<table class="tbl dense">
					<thead>
						<tr
							><th>Time</th><th>Point</th><th>Question</th><th>Answer</th><th class="r">Confidence</th><th
								>Action</th
							><th>Case</th></tr
						>
					</thead>
					<tbody>
						{#each shown as r (r.id)}
							<tr>
								<td class="mono vl-t">{r.at.slice(5, 16).replace('T', ' ')}</td>
								<td class="mono">{r.point}</td>
								<td><span class="mono vl-k">{r.gate}</span> <span class="vl-qq">{r.question}</span></td>
								<td
									>{r.answer}{#if r.below_threshold}
										<Chip text="below line" variant="warn" />{/if}</td
								>
								<td class="r mono">{Math.round(r.confidence * 100)}%</td>
								<td><Chip text={DECISION_TEXT[r.action]} variant={DECISION_VARIANT[r.action]} /></td>
								<td
									><a class="mono" href={jobPath(app.job, `/cases/${encodeURIComponent(r.case_id)}`)}
										>{r.case_id}</a
									></td
								>
							</tr>
						{:else}
							<tr><td colspan="7" class="muted">No verdict matches these filters.</td></tr>
						{/each}
					</tbody>
				</table>
			</div>
		{/snippet}
	</LoadState>
</section>

<style>
	.vl {
		padding: 14px;
		margin-top: 16px;
		min-width: 0;
	}
	.vl-h {
		display: flex;
		flex-wrap: wrap;
		gap: 2px 10px;
		align-items: baseline;
		margin-bottom: 8px;
	}
	.vl-h h2 {
		margin: 0;
		font: 600 15px/1.3 var(--cond);
	}
	.vl-f {
		display: flex;
		flex-wrap: wrap;
		gap: 6px 8px;
		align-items: center;
		margin-bottom: 8px;
	}
	.vl-f select {
		width: auto;
		max-width: 100%;
	}
	.vl-q {
		width: 160px;
		max-width: 100%;
	}
	.vl-chk {
		display: inline-flex;
		gap: 5px;
		align-items: center;
		font-size: 13px;
	}
	.vl-n {
		font-size: 12.5px;
	}
	.vl-t {
		white-space: nowrap;
	}
	.vl-k {
		font-weight: 600;
		color: var(--tool);
	}
	.vl-qq {
		font-size: 12.5px;
	}
</style>
