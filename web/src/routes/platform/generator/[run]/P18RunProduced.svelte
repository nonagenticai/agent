<script lang="ts">
	// P18 "what this step produced", drawn as the artefact it becomes: A1–A2 → journey grid rows (stages × activities,
	// with the rule-4 in-scope switch); A3–A4 → the activity × object matrix. Only kept drafts count.
	import type { GeneratorRun, GeneratorStepKey, StageKey } from '$lib/api/types/journey';
	import { objectMatrix } from '$lib/journey/P18Run';

	let {
		run: r,
		step,
		open,
		onscope,
	}: {
		run: GeneratorRun;
		step: GeneratorStepKey;
		open: boolean;
		onscope: (stage: StageKey, outOfScope: boolean) => void;
	} = $props();

	const m = $derived(objectMatrix(r.items));
	const MARK = { pending: '?', accepted: '✓', renamed: '✓', merged: '✓', dropped: '✕' } as const;
</script>

{#if step === 'journey'}
	<div class="p18-prod">
		<div class="p18-prod-h">Journey grid this run drafts <span>— kept activities per stage</span></div>
		<div class="p18-stages">
			{#each r.stages as s, i (s.key)}
				{@const empty = !s.out_of_scope && !s.activities.length}
				<div class="p18-stage" class:oos={s.out_of_scope} class:empty>
					<div class="p18-stage-h">
						<span class="p18-sn">{i + 1}</span>
						<span>{s.label}</span>
					</div>
					{#each s.activities as a (a)}<div class="p18-act mono">{a.split(':')[1]}</div>{:else}
						<div class="p18-none">{s.out_of_scope ? 'out of scope' : 'no activity — rule 4'}</div>
					{/each}
					<label class="p18-scope">
						<input
							type="checkbox"
							aria-label="{s.label} in scope"
							checked={!s.out_of_scope}
							disabled={!open}
							onchange={(e) => onscope(s.key, !(e.currentTarget as HTMLInputElement).checked)}
						/>
						in scope
					</label>
				</div>
			{/each}
		</div>
	</div>
{:else if step === 'objects' && m.cells.length}
	<div class="p18-prod">
		<div class="p18-prod-h">
			Activity × object matrix <span
				>— ✓ kept · ? to decide · ✕ dropped · dashed = not a canonical object</span
			>
		</div>
		<div class="tbl-wrap">
			<table class="tbl dense p18-matrix" data-testid="p18-matrix">
				<thead>
					<tr>
						<th>Activity</th>
						{#each m.objects as o (o)}
							{@const isNew = m.cells.some((c) => c.object === o && c.item.new_object)}
							<th class="p18-obj" class:isnew={isNew}><span class="mono">{o}</span></th>
						{/each}
					</tr>
				</thead>
				<tbody>
					{#each m.activities as a (a)}
						<tr>
							<td class="mono p18-arow">{a}</td>
							{#each m.objects as o (o)}
								{@const c = m.cells.find((x) => x.activity === a && x.object === o)}
								<td class="p18-cell">
									{#if c}
										<span
											class="p18-m {c.item.decision}"
											title="{c.item.title}: {c.item.decision} · {c.item.detail.split(' · ')[0]}"
											>{MARK[c.item.decision]}</span
										>
										<span class="p18-role">{c.item.detail.split(' · ')[0]}</span>
									{/if}
								</td>
							{/each}
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	</div>
{/if}

<style>
	.p18-prod {
		margin-bottom: 14px;
	}
	.p18-prod-h {
		font-size: 12px;
		font-weight: 600;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		color: var(--muted);
		margin-bottom: 8px;
	}
	.p18-prod-h span {
		text-transform: none;
		letter-spacing: 0;
		font-weight: 400;
	}
	.p18-stages {
		display: grid;
		grid-template-columns: repeat(7, minmax(0, 1fr));
		gap: 6px;
	}
	.p18-stage {
		border: 1px solid var(--line-2);
		border-radius: var(--r);
		padding: 8px;
		display: flex;
		flex-direction: column;
		gap: 4px;
		min-width: 0;
		background: var(--panel);
	}
	.p18-stage.empty {
		border-color: var(--err);
		background: color-mix(in srgb, var(--err) 6%, transparent);
	}
	.p18-stage.oos {
		background: var(--sunk);
		border-style: dashed;
	}
	.p18-stage-h {
		display: flex;
		gap: 5px;
		align-items: center;
		font-weight: 600;
		font-size: 12.5px;
	}
	.p18-sn {
		font: 500 10.5px/1 var(--mono);
		color: var(--faint);
	}
	.p18-act {
		font-size: 11.5px;
		padding: 3px 5px;
		border-radius: 4px;
		background: color-mix(in srgb, var(--cust) 12%, transparent);
		color: var(--cust);
		overflow-wrap: anywhere;
	}
	.p18-none {
		font-size: 11.5px;
		color: var(--muted);
		font-style: italic;
	}
	.empty .p18-none {
		color: var(--err);
		font-weight: 600;
		font-style: normal;
	}
	.p18-scope {
		margin-top: auto;
		padding-top: 4px;
		font-size: 11px;
		color: var(--muted);
		display: flex;
		gap: 4px;
		align-items: center;
	}
	.p18-matrix th.p18-obj {
		text-align: center;
	}
	.p18-obj.isnew span {
		border: 1px dashed var(--warn);
		color: var(--warn);
		padding: 1px 4px;
		border-radius: 4px;
	}
	.p18-arow {
		font-size: 12px;
		white-space: nowrap;
	}
	.p18-cell {
		text-align: center;
		white-space: nowrap;
	}
	.p18-m {
		display: inline-grid;
		place-items: center;
		width: 20px;
		height: 20px;
		border-radius: 50%;
		font-weight: 700;
		font-size: 11.5px;
	}
	.p18-m.accepted,
	.p18-m.renamed,
	.p18-m.merged {
		background: color-mix(in srgb, var(--ok) 16%, transparent);
		color: var(--ok);
	}
	.p18-m.pending {
		background: color-mix(in srgb, var(--warn) 18%, transparent);
		color: var(--warn);
	}
	.p18-m.dropped {
		background: color-mix(in srgb, var(--dead) 14%, transparent);
		color: var(--dead);
	}
	.p18-role {
		display: block;
		font-size: 10.5px;
		color: var(--faint);
	}
	@media (max-width: 900px) {
		.p18-stages {
			grid-template-columns: repeat(2, minmax(0, 1fr));
		}
	}
</style>
