<script lang="ts">
	// J11 scope bar: graph vs list, industry / department chips (only when the job spans more than one), narrowing
	// to one activity's context space, "Show hidden".
	// from cb:web/src/routes/admin/jobs/[job]/ontology/+page.svelte@45ee128 (scope bar section)
	import Seg from '$lib/ui/Seg.svelte';
	import type { ContextSpace, OntologyEntity } from '$lib/api/types/ontology';
	import { distinct, title, type Filter, type Scope } from '$lib/ontology/layout';

	let {
		filter = $bindable(),
		view = $bindable(),
		entities,
		spaces,
		industries,
		head,
		nodeCount,
		edgeCount,
		hiddenCount,
	}: {
		filter: Filter;
		view: string;
		entities: OntologyEntity[];
		spaces: ContextSpace[];
		industries: string[];
		head: { text: string; level: Scope | 'all' };
		nodeCount: number;
		edgeCount: number;
		hiddenCount: number;
	} = $props();

	const departments = $derived(distinct(entities, 'department', filter.industry));

	function pickIndustry(v: string) {
		filter.industry = v;
		if (v === 'all' || !distinct(entities, 'department', v).includes(filter.department))
			filter.department = 'all';
	}
</script>

<section class="scopebar card">
	<div class="scope-title">
		<h2>Entity map</h2>
		{#if head.level !== 'all'}<span class="level {head.level}">{head.text}</span>{/if}
		<span class="muted counts"><b>{nodeCount}</b> entities · <b>{edgeCount}</b> relations</span>
		<span class="spacer"></span>
		<Seg
			label="View"
			items={[
				['graph', 'Graph'],
				['table', 'Relations list'],
			]}
			bind:active={view}
		/>
	</div>
	<div class="filters">
		{#if industries.length > 1}
			<div class="frow" role="group" aria-label="Industry">
				<span class="flabel">Industry</span>
				<button
					type="button"
					class="chip"
					class:accent={filter.industry === 'all'}
					aria-pressed={filter.industry === 'all'}
					onclick={() => pickIndustry('all')}>All</button
				>
				{#each industries as ind (ind)}
					<button
						type="button"
						class="chip"
						class:accent={filter.industry === ind}
						aria-pressed={filter.industry === ind}
						onclick={() => pickIndustry(ind)}>{title(ind)}</button
					>
				{/each}
			</div>
		{/if}
		{#if departments.length > 1}
			<div class="frow" role="group" aria-label="Department">
				<span class="flabel">Department</span>
				<button
					type="button"
					class="chip"
					class:accent={filter.department === 'all'}
					aria-pressed={filter.department === 'all'}
					onclick={() => (filter.department = 'all')}>All</button
				>
				{#each departments as d (d)}
					<button
						type="button"
						class="chip"
						class:accent={filter.department === d}
						aria-pressed={filter.department === d}
						onclick={() => (filter.department = d)}>{title(d)}</button
					>
				{/each}
			</div>
		{/if}
		<div class="frow">
			<label class="flabel" for="onto-space">Activity</label>
			<select id="onto-space" class="input sm" bind:value={filter.space}>
				<option value="all">Every activity (whole ontology)</option>
				{#each spaces as s (s.key)}<option value={s.key}>{s.label} ({s.key})</option>{/each}
			</select>
			<label class="check">
				<input type="checkbox" bind:checked={filter.showHidden} disabled={hiddenCount === 0} /> Show hidden ({hiddenCount})
			</label>
		</div>
		{#if filter.space !== 'all'}
			<p class="fnote">Showing only the entities the agent brings into this activity (its context space).</p>
		{/if}
	</div>
</section>

<style>
	.scopebar {
		display: grid;
		gap: 10px;
		padding: 12px 14px;
		margin-bottom: 12px;
	}
	.scope-title {
		display: flex;
		align-items: center;
		gap: 10px;
		flex-wrap: wrap;
	}
	.scope-title h2 {
		margin: 0;
		font: 600 18px/1.2 var(--cond);
	}
	.counts b {
		color: var(--fg);
		font-variant-numeric: tabular-nums;
	}
	.level {
		--lv: var(--accent);
		font: 600 10px/1.4 var(--sans);
		letter-spacing: 0.07em;
		text-transform: uppercase;
		padding: 2px 7px;
		border-radius: 999px;
		color: var(--lv);
		background: color-mix(in srgb, var(--lv) 14%, transparent);
	}
	.level.generic {
		--lv: var(--cust);
	}
	.level.industry {
		--lv: var(--tool);
	}
	.level.industry_dept {
		--lv: var(--work);
	}
	.filters {
		display: grid;
		gap: 8px;
	}
	.frow {
		display: flex;
		align-items: center;
		gap: 6px;
		flex-wrap: wrap;
	}
	.flabel {
		min-width: 92px;
		font: 600 11px/1.2 var(--cond);
		letter-spacing: 0.07em;
		text-transform: uppercase;
		color: var(--muted);
	}
	button.chip {
		cursor: pointer;
		border: 1px solid var(--line);
		background: var(--panel);
		color: var(--fg);
	}
	button.chip.accent {
		border-color: color-mix(in srgb, var(--accent) 40%, var(--line));
		background: color-mix(in srgb, var(--accent) 13%, transparent);
		color: var(--accent);
	}
	.input.sm {
		width: auto;
		max-width: 100%;
		padding: 4px 8px;
		font-size: 12.5px;
	}
	.check {
		display: inline-flex;
		gap: 5px;
		align-items: center;
		font-size: 12.5px;
		color: var(--muted);
	}
	.spacer {
		flex: 1;
	}
	.fnote {
		margin: 0;
		font-size: 12px;
		color: var(--muted);
	}
</style>
