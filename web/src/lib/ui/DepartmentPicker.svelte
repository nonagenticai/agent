<script lang="ts">
	// D115: a job's departments — an ORDERED multi-select within the chosen industry, primary first. Adding appends,
	// "Make primary"/"Move up" reorder, removing drops one. The order is the job's declared order (it decides only
	// section order, never which sibling wins a key); `value[0]` is the primary department. Option keys are the
	// department layer keys `industry_dept/<industry>/<dept>`.
	type Option = { key: string; label: string; note?: string; extends?: string };

	let {
		options,
		value,
		onchange,
		testid = 'department-picker',
	}: {
		options: Option[];
		value: string[];
		onchange: (next: string[]) => void;
		testid?: string;
	} = $props();

	const chosen = $derived(value.map((k) => options.find((o) => o.key === k) ?? { key: k, label: k }));
	const available = $derived(options.filter((o) => !value.includes(o.key)));

	function moveUp(i: number) {
		if (i <= 0) return;
		const next = [...value];
		[next[i - 1], next[i]] = [next[i]!, next[i - 1]!];
		onchange(next);
	}
	function makePrimary(i: number) {
		if (i <= 0) return;
		onchange([value[i]!, ...value.filter((_, n) => n !== i)]);
	}
</script>

<div class="picker" data-testid={testid}>
	{#if chosen.length}
		<ol class="chosen" data-testid={`${testid}-chosen`}>
			{#each chosen as d, i (d.key)}
				<li data-key={d.key}>
					<span class="pos">{i + 1}</span>
					<span class="name" title={d.note}
						>{d.label}{#if d.extends}<span class="muted small"> · extends {d.extends}</span>{/if}</span
					>
					{#if i === 0}<span class="primary-tag">primary</span>{/if}
					<span class="actions">
						{#if i > 0}
							<button type="button" class="btn small" onclick={() => makePrimary(i)}>Make primary</button>
							<button
								type="button"
								class="btn small"
								aria-label={`Move ${d.label} up`}
								onclick={() => moveUp(i)}>↑</button
							>
						{/if}
						<button
							type="button"
							class="btn small"
							aria-label={`Remove ${d.label}`}
							onclick={() => onchange(value.filter((k) => k !== d.key))}>Remove</button
						>
					</span>
				</li>
			{/each}
		</ol>
	{:else}
		<p class="muted small none" data-testid={`${testid}-empty`}>
			No department yet — a job needs at least one department of its industry.
		</p>
	{/if}
	{#if available.length}
		<div class="add">
			{#each available as d (d.key)}
				<button
					type="button"
					class="btn small"
					title={d.note}
					data-key={d.key}
					onclick={() => onchange([...value, d.key])}>+ {d.label}</button
				>
			{/each}
		</div>
	{:else if !options.length}
		<span class="muted small">This industry has no department yet.</span>
	{/if}
</div>

<style>
	.picker {
		display: grid;
		gap: 6px;
	}
	.chosen {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 4px;
	}
	.chosen li {
		display: flex;
		align-items: center;
		gap: 8px;
		flex-wrap: wrap;
	}
	.pos {
		font: 600 11px/1 var(--mono, monospace);
		color: var(--muted);
		min-width: 1.5em;
	}
	.primary-tag {
		font: 600 10px/1.2 var(--cond);
		letter-spacing: 0.07em;
		text-transform: uppercase;
		border: 1px solid var(--line);
		border-radius: 4px;
		padding: 1px 5px;
	}
	.actions {
		display: inline-flex;
		gap: 4px;
		margin-left: auto;
	}
	.add {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
	}
	.btn.small {
		padding: 2px 8px;
		font-size: 12px;
	}
	.small {
		font-size: 12px;
	}
	.none {
		margin: 0;
	}
</style>
