<script lang="ts">
	// P13: how ONE job selection's dimension values become the layer stack a cut compiles (05 §2.1). Pick a live
	// selection; each step says which value put the layer there, or why a dimension-qualified layer was skipped.
	import type { DepartmentArchetype, Dimension, DimensionUsage } from '$lib/api/types/platform';
	import { selectionId, selectionPath } from '$lib/platform/P13Dimensions';

	let {
		dims,
		usage,
		archetypes = null,
	}: { dims: Dimension[]; usage: DimensionUsage; archetypes?: DepartmentArchetype[] | null } = $props();

	let picked = $state('');
	const sel = $derived(
		usage.selections.find((s) => selectionId(s) === picked) ?? usage.selections[0] ?? null,
	);
	const steps = $derived(sel ? selectionPath(sel, dims, usage, archetypes) : []);
	const labelOf = (dim: string, v: string) =>
		dims.find((d) => d.key === dim)?.values.find((x) => x.key === v)?.label ?? v;
</script>

{#if !sel}
	<p class="empty">No job has a selection yet. A selection is made per job by its tenant_admin.</p>
{:else}
	<label class="field pick">
		<span class="label">Selection</span>
		<select
			class="input"
			aria-label="Selection to explain"
			value={selectionId(sel)}
			onchange={(e) => (picked = e.currentTarget.value)}
		>
			{#each usage.selections as s (selectionId(s))}
				<option value={selectionId(s)}>{s.tenant_name} · {s.job_label}</option>
			{/each}
		</select>
	</label>

	<div class="picks" data-testid="selection-values">
		<div class="pick-row">
			<span class="dim">Departments</span>
			<span class="chips">
				{#each sel.selection.departments as d, i (d)}<span class="val"
						>{d}{#if i === 0}<span class="muted"> · primary</span>{/if}</span
					>{/each}
			</span>
		</div>
		{#each dims as d (d.key)}
			<div class="pick-row">
				<span class="dim">{d.label}</span>
				<span class="chips">
					{#each sel.values[d.key] ?? [] as v (v)}
						<span class="val">{labelOf(d.key, v)}</span>
					{:else}
						<span class="muted small">none</span>
					{/each}
				</span>
			</div>
		{/each}
	</div>

	<ol class="stack" data-testid="selection-path" aria-label="Compile order for this selection">
		{#each steps as s, i (s.layer + i)}
			<li class="step {s.status}" data-status={s.status}>
				<span class="dot" aria-hidden="true"></span>
				<div class="body">
					<div class="top">
						{#if s.href}
							<a class="mono path" href={s.href}>{s.layer}</a>
						{:else}
							<span class="mono path">{s.layer}</span>
						{/if}
						<span class="why">
							{#if s.status === 'skipped'}skipped ·
							{/if}{s.because}
						</span>
					</div>
					{#if s.detail.length}
						<div class="detail">
							{#each s.detail as d (d)}<span class="mono">{d}</span>{/each}
						</div>
					{/if}
				</div>
			</li>
		{/each}
	</ol>
	<p class="note">
		Later layers override earlier ones. The department layer specialises its base archetype; areas narrow it
		to their context spaces; a job layer qualified by a channel compiles only when the selection names it;
		region is recorded in the release and selects nothing.
	</p>
{/if}

<style>
	.pick {
		max-width: 360px;
		margin-bottom: 10px;
	}
	.picks {
		display: grid;
		gap: 4px;
		padding: 8px 10px;
		background: var(--sunk);
		border-radius: var(--r);
		margin-bottom: 14px;
	}
	.pick-row {
		display: grid;
		grid-template-columns: 92px minmax(0, 1fr);
		gap: 8px;
		align-items: baseline;
	}
	.dim {
		font: 600 11px/1.2 var(--cond);
		letter-spacing: 0.07em;
		text-transform: uppercase;
		color: var(--muted);
	}
	.val {
		font-size: 12.5px;
		padding: 1px 8px;
		border-radius: 999px;
		border: 1px solid var(--accent);
		color: var(--accent);
	}
	.small {
		font-size: 12px;
	}
	.stack {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
	}
	.step {
		display: grid;
		grid-template-columns: 18px minmax(0, 1fr);
		gap: 8px;
		position: relative;
		padding-bottom: 10px;
	}
	.step::before {
		content: '';
		position: absolute;
		left: 6px;
		top: 14px;
		bottom: 0;
		border-left: 2px solid var(--line);
	}
	.step:last-child::before {
		display: none;
	}
	.dot {
		width: 14px;
		height: 14px;
		border-radius: 50%;
		margin-top: 3px;
		background: var(--accent);
		border: 2px solid var(--accent);
		position: relative;
		z-index: 1;
	}
	.skipped .dot {
		background: var(--panel);
		border-style: dashed;
		border-color: var(--faint);
	}
	.recorded .dot {
		background: var(--panel);
		border-color: var(--muted);
		border-radius: 3px;
	}
	.top {
		display: flex;
		flex-wrap: wrap;
		gap: 4px 10px;
		align-items: baseline;
	}
	.path {
		font-size: 12.5px;
		overflow-wrap: anywhere;
	}
	.skipped .path {
		color: var(--faint);
		text-decoration: line-through;
	}
	.why {
		font-size: 12px;
		color: var(--muted);
	}
	.detail {
		display: flex;
		flex-wrap: wrap;
		gap: 4px;
		margin-top: 4px;
	}
	.detail span {
		font-size: 11.5px;
		padding: 1px 6px;
		border-radius: 4px;
		background: var(--sunk);
		color: var(--muted);
	}
</style>
