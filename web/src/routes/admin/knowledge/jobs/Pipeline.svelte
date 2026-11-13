<script lang="ts">
	// T8: the strata_ingest pipeline as the page's mental model — five stages left to right, ending in a knowledge
	// store. Each stage shows the provider slot that powers it (empty / drifted slots flagged), the jobs in it now
	// and the jobs that stopped there.
	import type { IngestJob } from '$lib/api/types/tenant';
	import type { StageLoad } from '$lib/tenant/ingest-T8';

	let { load, onpick }: { load: StageLoad[]; onpick: (j: IngestJob) => void } = $props();
</script>

<ol class="t8-pipe" aria-label="Ingest pipeline" data-testid="ingest-pipeline">
	{#each load as l, i (l.stage.key)}
		<li class="t8-stage" class:stopped={l.failed.length > 0} class:busy={l.inFlight.length > 0}>
			<div class="t8-head">
				<span class="t8-num" aria-hidden="true">{i + 1}</span>
				<b>{l.stage.label}</b>
			</div>
			<p class="t8-what">{l.stage.what}</p>
			{#if l.slots.length}
				<ul class="t8-slots" aria-label="{l.stage.label} slots">
					{#each l.slots as s (s.slot)}
						<li>
							<span class="t8-slot-name">{s.label}</span>
							{#if !s.configured}
								{@const blocking = l.failed.some((j) =>
									j.steps.some((st) => st.status === 'failed' && st.slot === s.slot),
								)}
								<a
									class="chip {blocking ? 'err' : 'muted'}"
									title={blocking
										? 'A job stopped because this slot is empty'
										: 'Only needed for sources that use it'}
									href="/admin/providers?tab={s.service === 'strata_rag' ? 'knowledge' : 'ingest'}"
									>{blocking ? 'empty · blocking' : 'not set'}</a
								>
							{:else}
								<span class="mono t8-model">{s.model} v{s.version}</span>
								{#if s.drift}<a class="chip warn" href="/admin/providers?tab=ingest"
										>v{s.drift.remote_version} waiting</a
									>{/if}
							{/if}
						</li>
					{/each}
				</ul>
			{:else if l.stage.key === 'index'}
				<a class="t8-store-link" href="/admin/knowledge/stores">into a knowledge store →</a>
			{/if}
			{#if l.inFlight.length || l.failed.length}
				<div class="t8-load">
					{#each l.failed as j (j.id)}
						<button type="button" class="chip err" onclick={() => onpick(j)}>✕ {j.source_label}</button>
					{/each}
					{#each l.inFlight as j (j.id)}
						<button type="button" class="chip accent" onclick={() => onpick(j)}
							>{j.status === 'running' ? '●' : '○'} {j.source_label}</button
						>
					{/each}
				</div>
			{/if}
		</li>
	{/each}
</ol>

<style>
	.t8-pipe {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		grid-template-columns: repeat(5, minmax(0, 1fr));
		gap: 18px;
	}
	.t8-stage {
		position: relative;
		border: 1px solid var(--line);
		border-radius: 8px;
		padding: 10px 12px;
		background: var(--sunk);
		display: flex;
		flex-direction: column;
		gap: 6px;
		min-width: 0;
	}
	.t8-stage:not(:last-child)::after {
		content: '→';
		position: absolute;
		right: -15px;
		top: 12px;
		color: var(--faint);
		font-weight: 600;
	}
	.t8-stage.busy {
		border-color: color-mix(in srgb, var(--accent) 55%, var(--line));
	}
	.t8-stage.stopped {
		border-color: var(--err);
		background: color-mix(in srgb, var(--err) 6%, var(--panel));
	}
	.t8-head {
		display: flex;
		align-items: center;
		gap: 8px;
	}
	.t8-num {
		display: inline-grid;
		place-items: center;
		width: 20px;
		height: 20px;
		border-radius: 50%;
		background: var(--panel);
		border: 1px solid var(--line);
		font: 600 11px/1 var(--mono);
		color: var(--muted);
	}
	.t8-what {
		margin: 0;
		color: var(--muted);
		font-size: 12px;
		line-height: 1.4;
	}
	.t8-slots {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 4px;
	}
	.t8-slots li {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 4px 6px;
		font-size: 12px;
	}
	.t8-slot-name {
		font-weight: 600;
	}
	.t8-model {
		color: var(--muted);
		font-size: 11.5px;
	}
	.t8-store-link {
		font-size: 12px;
	}
	.t8-load {
		display: flex;
		flex-wrap: wrap;
		gap: 4px;
		margin-top: auto;
		padding-top: 4px;
	}
	.t8-load .chip {
		border: 0;
		cursor: pointer;
		max-width: 100%;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	@media (max-width: 900px) {
		.t8-pipe {
			grid-template-columns: minmax(0, 1fr);
			gap: 14px;
		}
		.t8-stage:not(:last-child)::after {
			content: '↓';
			right: auto;
			left: 20px;
			top: auto;
			bottom: -15px;
		}
	}
</style>
