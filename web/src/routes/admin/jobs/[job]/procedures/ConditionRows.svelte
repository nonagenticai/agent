<script lang="ts" module>
	import type { CondOp } from '$lib/api/types/ontology';
	/** One editable condition: the value as typed (parsed on submit with `parseValue`). */
	export type CondDraft = { id: string; fact: string; op: CondOp; value: string };
</script>

<script lang="ts">
	// J12 (B5 T7a, F-20): rows of `fact · op · value` conditions, one list (a step's pre- or postconditions, or the
	// version's success criteria). Facts are suggested from B4's FACT_PATHS over the step names and slots the
	// caller passes; the server validates every list with B4's validator (422 `invalid_condition`).
	import { COND_OPS, COND_OP_LABEL } from '$lib/ontology/conditions';

	let {
		rows = $bindable(),
		label,
		facts,
		idPrefix,
	}: {
		rows: CondDraft[];
		/** What the list is, for screen readers (`Step 2 preconditions`). */
		label: string;
		/** Fact paths to suggest (`slots.order_id`, `steps.order.outcome`, …). */
		facts: string[];
		idPrefix: string;
	} = $props();

	const noValue = (op: CondOp) => op === 'exists' || op === 'absent';
	function add() {
		let n = rows.length + 1;
		while (rows.some((r) => r.id === `c${n}`)) n++;
		rows.push({ id: `c${n}`, fact: '', op: 'eq', value: '' });
	}
</script>

<div class="cond" role="group" aria-label={label}>
	<datalist id="{idPrefix}-facts">
		{#each facts as f (f)}<option value={f}></option>{/each}
	</datalist>
	{#each rows as r, i (i)}
		<div class="cond-row">
			<input class="input mono id" aria-label="{label} {i + 1} id" bind:value={r.id} />
			<input
				class="input mono fact"
				aria-label="{label} {i + 1} fact"
				list="{idPrefix}-facts"
				placeholder="steps.order.outcome"
				bind:value={r.fact}
			/>
			<select class="input op" aria-label="{label} {i + 1} operator" bind:value={r.op}>
				{#each COND_OPS as op (op)}<option value={op}>{COND_OP_LABEL[op]}</option>{/each}
			</select>
			{#if !noValue(r.op)}
				<input
					class="input mono val"
					aria-label="{label} {i + 1} value"
					placeholder={r.op === 'in' ? 'gold, silver' : 'ok'}
					bind:value={r.value}
				/>
			{/if}
			<button
				type="button"
				class="btn sm"
				aria-label="Remove {label} {i + 1}"
				onclick={() => rows.splice(i, 1)}>Remove</button
			>
		</div>
	{/each}
	<button type="button" class="btn sm" onclick={add}>Add condition</button>
</div>

<style>
	.cond {
		display: grid;
		gap: 4px;
		justify-items: start;
	}
	.cond-row {
		display: flex;
		gap: 4px;
		flex-wrap: wrap;
		align-items: center;
	}
	.cond .input {
		padding: 3px 6px;
	}
	.id {
		width: 96px;
	}
	.fact {
		width: 210px;
	}
	.val {
		width: 140px;
	}
</style>
