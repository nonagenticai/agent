<script lang="ts">
	// T8: one job's walk through the five ingest stages, as five labelled cells. State is spelled out (glyph +
	// accessible label), never colour alone.
	import type { IngestJob } from '$lib/api/types/tenant';
	import { STAGES, jobStages, type StageState } from '$lib/tenant/ingest-T8';

	let { job, labels = false }: { job: IngestJob; labels?: boolean } = $props();
	const GLYPH: Record<StageState, string> = {
		done: '✓',
		running: '●',
		queued: '○',
		failed: '✕',
		skipped: '–',
		none: '·',
	};
	const WORD: Record<StageState, string> = {
		done: 'done',
		running: 'running',
		queued: 'waiting',
		failed: 'stopped here',
		skipped: 'not run',
		none: 'not needed',
	};
	const st = $derived(jobStages(job));
	const aria = $derived(STAGES.map((s) => `${s.label} ${WORD[st[s.key]]}`).join(', '));
</script>

<span class="t8-track" class:labels role="img" aria-label={aria} data-testid="stage-track">
	{#each STAGES as s (s.key)}
		<span class="t8-cell {st[s.key]}" title="{s.label}: {WORD[st[s.key]]}">
			<span class="g" aria-hidden="true">{GLYPH[st[s.key]]}</span>
			{#if labels}<span class="l">{s.label}</span>{/if}
		</span>
	{/each}
</span>

<style>
	.t8-track {
		display: inline-flex;
		gap: 3px;
		align-items: stretch;
	}
	.t8-cell {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		gap: 4px;
		min-width: 20px;
		height: 20px;
		padding: 0 4px;
		border-radius: 4px;
		font: 600 11px/1 var(--mono);
		border: 1px solid var(--line);
		color: var(--faint);
		background: var(--sunk);
	}
	.labels .t8-cell {
		height: 24px;
		padding: 0 8px;
	}
	.l {
		font: 500 11.5px/1 var(--sans);
	}
	.done {
		color: var(--ok);
		border-color: color-mix(in srgb, var(--ok) 40%, transparent);
		background: color-mix(in srgb, var(--ok) 12%, transparent);
	}
	.running {
		color: var(--accent);
		border-color: color-mix(in srgb, var(--accent) 50%, transparent);
		background: color-mix(in srgb, var(--accent) 12%, transparent);
	}
	.queued {
		color: var(--muted);
		border-style: dashed;
	}
	.failed {
		color: var(--err);
		border-color: var(--err);
		background: color-mix(in srgb, var(--err) 14%, transparent);
	}
	.skipped,
	.none {
		opacity: 0.6;
	}
</style>
