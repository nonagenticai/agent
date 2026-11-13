<script lang="ts">
	// Two eval runs side by side: per mode and metric old → new (change), and which questions were added or dropped
	// between them (a change in the set explains a change in the numbers as much as a change in search does). The
	// runs list carries summaries (reports, `question_count`), so the metrics show at once; the added / dropped
	// questions need both snapshots, read with `GET /api/eval/runs/{id}`.
	import type { CompletedEvalRun, EvalRunDetail } from '$lib/api/types/retrieval';
	import { Latest } from '$lib/load.svelte';
	import { when } from '$lib/tenant/ui';
	import { COUNT_METRICS, diffModes, diffRuns } from '$lib/tenant/retrieval';

	let { runs, detail }: { runs: CompletedEvalRun[]; detail: (id: string) => Promise<EvalRunDetail> } =
		$props();

	// svelte-ignore state_referenced_locally
	let baseId = $state(runs[1]?.id ?? '');
	// svelte-ignore state_referenced_locally
	let nextId = $state(runs[0]?.id ?? '');
	const base = $derived(runs.find((r) => r.id === baseId) ?? runs[1]);
	const next = $derived(runs.find((r) => r.id === nextId) ?? runs[0]);
	const modes = $derived(base && next ? diffModes(base, next) : null);
	const label = (r: CompletedEvalRun) => `${when(r.started_at)} · ${r.question_count} q`;

	/** Added / dropped questions of the picked pair, once both snapshots are read. */
	let set = $state<{ added: string[]; removed: string[] } | null>(null);
	let setError = $state<string | null>(null);
	const latest = new Latest();
	$effect(() => {
		const a = base?.id;
		const b = next?.id;
		const ticket = latest.next();
		set = null;
		setError = null;
		if (!a || !b || a === b) return;
		Promise.all([detail(a), detail(b)])
			.then(([da, db]) => {
				if (!latest.stale(ticket)) set = diffRuns(da, db);
			})
			.catch((e: unknown) => {
				if (!latest.stale(ticket)) setError = e instanceof Error ? e.message : String(e);
			});
	});
	const num = (k: string, v: number | null) =>
		v == null ? '—' : COUNT_METRICS.includes(k as never) ? String(v) : v.toFixed(2);
	function cls(k: string, d: number | null) {
		if (!d) return 'muted';
		if (k === 'laya_consulted') return 'muted';
		const better = k === 'laya_fallbacks' ? d < 0 : d > 0;
		return better ? 's-ok' : 's-err';
	}
	const SHOWN = ['recall_at_1', 'recall_at_5', 'recall_at_10', 'leaf_accuracy', 'laya_fallbacks'];
</script>

<div class="rd-pick">
	<label class="field"
		><span class="label">Older run</span>
		<select class="input" bind:value={baseId}>
			{#each runs as r (r.id)}<option value={r.id}>{label(r)}</option>{/each}
		</select></label
	>
	<label class="field"
		><span class="label">Newer run</span>
		<select class="input" bind:value={nextId}>
			{#each runs as r (r.id)}<option value={r.id}>{label(r)}</option>{/each}
		</select></label
	>
</div>
{#if modes && base && next}
	{#if base.id === next.id}
		<p class="muted">Pick two different runs.</p>
	{:else}
		<div class="tbl-wrap ph-stack">
			<table class="tbl" data-testid="ev-diff">
				<thead>
					<tr
						><th>Mode</th><th>Metric</th><th class="r">Older</th><th class="r">Newer</th><th class="r"
							>Change</th
						></tr
					>
				</thead>
				<tbody>
					{#each modes as m (m.mode)}
						{#each m.metrics.filter((x) => SHOWN.includes(x.key)) as x, i (x.key)}
							<tr>
								<td class="mono" class:muted={i > 0}>{m.mode}</td>
								<td data-label="Metric">{x.label}</td>
								<td class="r mono" data-label="Older">{num(x.key, x.base)}</td>
								<td class="r mono" data-label="Newer">{num(x.key, x.next)}</td>
								<td class="r mono {cls(x.key, x.delta)}" data-label="Change"
									>{x.delta == null ? '—' : `${x.delta > 0 ? '+' : ''}${num(x.key, x.delta)}`}</td
								>
							</tr>
						{/each}
					{/each}
				</tbody>
			</table>
		</div>
		<div class="rd-set" data-testid="ev-diff-set">
			{#if setError}
				<p class="rd-err">Questions of the two runs not loaded: {setError}</p>
			{:else if !set}
				<p class="muted">Loading the two runs' questions…</p>
			{:else if !set.added.length && !set.removed.length}
				<p class="muted">Both runs asked the same questions, so the change is search, not the set.</p>
			{:else}
				{#if set.added.length}
					<p>
						<b>{set.added.length} added:</b>
						<span class="muted">{set.added.join(' · ')}</span>
					</p>
				{/if}
				{#if set.removed.length}
					<p>
						<b>{set.removed.length} dropped:</b>
						<span class="muted">{set.removed.join(' · ')}</span>
					</p>
				{/if}
			{/if}
		</div>
	{/if}
{/if}

<style>
	.rd-pick {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(min(180px, 100%), 1fr));
		gap: 10px;
		margin-bottom: 10px;
	}
	.rd-set {
		font-size: 12.5px;
		margin-top: 10px;
		overflow-wrap: anywhere;
	}
	.rd-set p {
		margin: 4px 0;
	}
	.rd-err {
		color: var(--err);
	}
</style>
