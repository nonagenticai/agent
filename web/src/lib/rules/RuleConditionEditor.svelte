<script lang="ts">
	import type { RuleCondition, RuleFact, RuleDefinition, RuleOperand } from '$lib/api/types/release';
	import RuleConditionEditor from './RuleConditionEditor.svelte';
	let {
		condition = $bindable(),
		facts,
		values,
		path = 'Condition',
		depth = 0,
	}: {
		condition: RuleCondition;
		facts: RuleFact[];
		values: RuleDefinition['values'];
		path?: string;
		depth?: number;
	} = $props();
	const compare = (): RuleCondition => ({
		kind: 'compare',
		left: { kind: 'fact', key: facts[0]?.key ?? '' },
		op: 'eq',
		right: { kind: 'literal', value: '' },
	});
	function kind(next: string) {
		condition = next === 'compare' ? compare() : { kind: next as 'all' | 'any', conditions: [compare()] };
	}
	function rightKind(next: string) {
		if (condition.kind !== 'compare') return;
		condition.right =
			next === 'literal' ? { kind: 'literal', value: '' } : { kind: 'value', key: values[0]?.key ?? '' };
	}
	function leftKind(next: string) {
		if (condition.kind !== 'compare') return;
		condition.left = {
			kind: next as 'fact' | 'days_since' | 'months_since',
			key: 'key' in condition.left ? condition.left.key : (facts[0]?.key ?? ''),
		};
	}
	const keyOf = (operand: RuleOperand) => ('key' in operand ? operand.key : '');
</script>

<fieldset class="condition">
	<legend>{path}</legend>
	<label
		>Match
		<select
			class="input"
			aria-label="{path} match"
			value={condition.kind}
			onchange={(e) => kind(e.currentTarget.value)}
		>
			<option value="compare">One comparison</option>
			<option value="all" disabled={depth >= 7}>All conditions</option>
			<option value="any" disabled={depth >= 7}>Any condition</option>
		</select>
	</label>
	{#if condition.kind === 'compare'}
		<div class="row">
			<label
				>Read as<select
					class="input"
					aria-label="{path} read as"
					value={condition.left.kind}
					onchange={(e) => leftKind(e.currentTarget.value)}
				>
					<option value="fact">Fact value</option><option value="days_since">Days since date</option><option
						value="months_since">Months since date</option
					>
				</select></label
			>
			<label
				>Fact<select
					class="input"
					aria-label="{path} fact"
					value={keyOf(condition.left)}
					onchange={(e) => {
						if (condition.kind === 'compare' && 'key' in condition.left)
							condition.left.key = e.currentTarget.value;
					}}
					required
				>
					<option value="">Choose fact</option>{#each facts as f, i (i)}<option value={f.key}
							>{f.label || f.key}</option
						>{/each}
				</select></label
			>
			<label
				>Comparison<select class="input" aria-label="{path} comparison" bind:value={condition.op}>
					<option value="eq">equals</option><option value="ne">does not equal</option><option value="lt"
						>less than</option
					><option value="lte">at most</option><option value="gt">greater than</option><option value="gte"
						>at least</option
					><option value="in">is in list</option><option value="not_in">is not in list</option>
				</select></label
			>
		</div>
		<div class="row">
			<label
				>Compare with<select
					class="input"
					aria-label="{path} compare with"
					value={condition.right.kind}
					onchange={(e) => rightKind(e.currentTarget.value)}
				>
					<option value="literal">Fixed value</option><option value="value" disabled={!values.length}
						>Your threshold</option
					>
				</select></label
			>
			{#if condition.right.kind === 'literal'}
				<label
					>Value<input
						class="input"
						aria-label="{path} value"
						maxlength="280"
						bind:value={condition.right.value}
					/></label
				>
			{:else if 'key' in condition.right}
				<label
					>Threshold<select
						class="input"
						aria-label="{path} threshold"
						bind:value={condition.right.key}
						required
					>
						<option value="">Choose threshold</option>{#each values as v, i (i)}<option value={v.key}
								>{v.label || v.key}</option
							>{/each}
					</select></label
				>
			{/if}
		</div>
	{:else}
		{#each condition.conditions as child, i (i)}
			<RuleConditionEditor
				bind:condition={condition.conditions[i]!}
				{facts}
				{values}
				path="{path}.{i + 1}"
				depth={depth + 1}
			/>
			<button
				type="button"
				class="btn sm"
				disabled={condition.conditions.length === 1}
				onclick={() => {
					if (condition.kind !== 'compare') condition.conditions.splice(i, 1);
				}}>Remove {path}.{i + 1}</button
			>
		{/each}
		<button
			type="button"
			class="btn sm"
			disabled={condition.conditions.length >= 16}
			onclick={() => {
				if (condition.kind !== 'compare') condition.conditions.push(compare());
			}}>Add condition to {path}</button
		>
	{/if}
</fieldset>

<style>
	.condition {
		display: grid;
		gap: 10px;
		border: 1px solid var(--line);
		border-radius: var(--r);
		padding: 12px;
		min-width: 0;
	}
	.row {
		display: flex;
		gap: 10px;
		flex-wrap: wrap;
	}
	label {
		display: grid;
		gap: 4px;
		flex: 1;
		min-width: 120px;
	}
</style>
