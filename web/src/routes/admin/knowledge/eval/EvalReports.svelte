<script lang="ts">
	// One eval run's per-mode reports (strata_rag `ModeReport`), recall@5 as the headline column, with the change
	// against the run before it, and the run's verdict (every routed mode's recall@5 at least flat's).
	import type { CompletedEvalRun } from '$lib/api/types/retrieval';
	import { fmt } from '$lib/format';
	import { when } from '$lib/tenant/ui';
	import { EVAL_METRICS, COUNT_METRICS, type EvalMetric } from '$lib/tenant/retrieval';
	import Meter from '$lib/ui/Meter.svelte';

	let { run, prev = null }: { run: CompletedEvalRun; prev?: CompletedEvalRun | null } = $props();

	const MODE_LABEL: Record<string, string> = {
		embed: 'Embedding',
		laya: 'Laya routing',
		hybrid: 'Hybrid',
		flat: 'Flat (no routing)',
	};
	const best = $derived(Math.max(...run.reports.map((r) => r.recall_at_5)));
	const was = (mode: string, k: EvalMetric) => prev?.reports.find((r) => r.mode === mode)?.[k] ?? null;
	const val = (k: EvalMetric, v: number | null) =>
		v == null ? '—' : COUNT_METRICS.includes(k) ? fmt.n(v) : v.toFixed(2);
	function change(mode: string, k: EvalMetric, v: number | null): { text: string; cls: string } | null {
		const p = was(mode, k);
		if (p == null || v == null) return null;
		const d = Math.round((v - p) * 100) / 100;
		if (d === 0) return null;
		const better = k === 'laya_fallbacks' ? d < 0 : k === 'laya_consulted' ? null : d > 0;
		return {
			text: `${d > 0 ? '+' : ''}${COUNT_METRICS.includes(k) ? d : d.toFixed(2)}`,
			cls: better == null ? 'muted' : better ? 's-ok' : 's-err',
		};
	}
</script>

<p class="muted er-meta">
	<span class="er-verdict {run.verdict === 'PASS' ? 's-ok' : 's-err'}" data-testid="ev-verdict"
		>{run.verdict}</span
	>
	Run {run.id} · {when(run.started_at)} by {run.started_by ?? '—'} · {fmt.n(run.question_count)} questions · beam
	{run.beam} · {fmt.ms(run.took_ms)}{run.root_id ? ` · scoped to ${run.root_id}` : ''}{prev
		? ` · changes against ${when(prev.started_at)}`
		: ''}
</p>
<div class="tbl-wrap ph-stack">
	<table class="tbl" data-testid="ev-reports">
		<thead>
			<tr>
				<th>Mode</th>
				{#each EVAL_METRICS as [k, label] (k)}<th class="r" class:er-head={k === 'recall_at_5'}>{label}</th
					>{/each}
			</tr>
		</thead>
		<tbody>
			{#each run.reports as r (r.mode)}
				<tr class:er-best={r.recall_at_5 === best}>
					<td
						><b>{MODE_LABEL[r.mode] ?? r.mode}</b>
						<span class="mono muted er-small">{r.mode}</span></td
					>
					{#each EVAL_METRICS as [k, label] (k)}
						{@const c = change(r.mode, k, r[k])}
						<td class="r mono" class:er-head={k === 'recall_at_5'} data-label={label}>
							{#if k === 'recall_at_5'}<Meter value={r.recall_at_5} cls="s-ok" />{/if}
							{val(k, r[k])}
							{#if c}<span class="er-small {c.cls}">{c.text}</span>{/if}
						</td>
					{/each}
				</tr>
			{/each}
		</tbody>
	</table>
</div>

<style>
	.er-meta {
		font-size: 12.5px;
		margin: 0 0 8px;
	}
	.er-small {
		font-size: 11px;
	}
	.er-head {
		font-weight: 600;
	}
	.er-verdict {
		font-weight: 700;
		margin-right: 4px;
	}
	.er-best td:first-child {
		box-shadow: inset 3px 0 0 var(--ok);
	}
</style>
