<script lang="ts">
	// Gate rule editor (fire_when): gte / lt compare the answer's value with a threshold, `in` fires on
	// chosen options; an optional minimum confidence makes a weaker answer “unsure” (its fail policy applies). Used by
	// J7 (tenant refinement → D54 draft) and P14 (template rule). The parent saves; this edits a local copy.
	import { untrack } from 'svelte';
	import type { GateOption, GateRule, GateType } from '$lib/api/types/ontology';
	import { normaliseRule, ruleError, ruleSummary } from './gateRules';

	let {
		id,
		type,
		options,
		rule,
		disabled = false,
		disabledReason = null,
		busy = false,
		saveLabel = 'Save rule',
		onsave,
	}: {
		id: string;
		type: GateType;
		options: GateOption[];
		rule: GateRule;
		disabled?: boolean;
		disabledReason?: string | null;
		busy?: boolean;
		saveLabel?: string;
		onsave: (r: GateRule) => void;
	} = $props();

	let op = $state<GateRule['fire_when']>('gte');
	let threshold = $state('');
	let minConf = $state('');
	let values = $state<string[]>([]);
	$effect(() => {
		void rule;
		untrack(() => {
			op = rule.fire_when;
			threshold = rule.threshold == null ? '' : String(rule.threshold);
			minConf = rule.min_confidence == null ? '' : String(rule.min_confidence);
			values = [...(rule.fire_values ?? [])];
		});
	});
	const num = (s: unknown) => (s === null || s === undefined || String(s).trim() === '' ? null : Number(s));
	const next = $derived(
		normaliseRule({
			fire_when: op,
			threshold: num(threshold),
			fire_values: values,
			min_confidence: num(minConf),
		}),
	);
	const err = $derived(ruleError({ type, options }, next));
	const dirty = $derived(JSON.stringify(next) !== JSON.stringify(normaliseRule(rule)));
	const subject = $derived(type === 'noul' ? 'p(true)' : type === 'score' ? 'level' : 'confidence');
	function toggle(k: string, on: boolean) {
		values = on ? [...values, k] : values.filter((v) => v !== k);
	}
</script>

<div class="gre" data-testid="gate-rule-editor">
	<div class="gre-row">
		<label class="gre-f">
			<span class="label">Fires when</span>
			<select class="input" aria-label="{id} rule operator" bind:value={op} {disabled}>
				<option value="gte">{subject} ≥ threshold</option>
				<option value="lt">{subject} &lt; threshold</option>
				<option value="in" disabled={type !== 'choice'}>answer is one of… (choice)</option>
			</select>
		</label>
		{#if op !== 'in'}
			<label class="gre-f">
				<span class="label">Threshold</span>
				<input
					class="input mono gre-n"
					type="number"
					step={type === 'score' ? 1 : 0.05}
					min={type === 'score' ? 1 : 0}
					max={type === 'score' ? options.length : 1}
					aria-label="{id} rule threshold"
					bind:value={threshold}
					{disabled}
				/>
			</label>
		{/if}
		<label class="gre-f">
			<span class="label">Min confidence</span>
			<input
				class="input mono gre-n"
				type="number"
				step="0.05"
				min="0"
				max="1"
				placeholder="any"
				aria-label="{id} minimum confidence"
				bind:value={minConf}
				{disabled}
			/>
		</label>
	</div>
	{#if op === 'in'}
		<fieldset class="gre-vals" {disabled}>
			<legend class="label">Answers that fire</legend>
			{#each options as o (o.key)}
				<label class="check"
					><input
						type="checkbox"
						checked={values.includes(o.key)}
						onchange={(e) => toggle(o.key, e.currentTarget.checked)}
					/>
					{o.label}</label
				>
			{:else}
				<span class="muted">Options are built per case; “in” needs a fixed list.</span>
			{/each}
		</fieldset>
	{/if}
	<p class="gre-sum">
		<span class="mono">{ruleSummary(type, next)}</span>
		<span class="muted">
			· an answer under the minimum confidence is unsure: it does not fire and the fail policy applies.</span
		>
	</p>
	{#if err && dirty}<p class="gre-err" role="alert">{err}</p>{/if}
	<button
		type="button"
		class="btn sm primary"
		disabled={disabled || busy || !dirty || !!err}
		title={disabled ? (disabledReason ?? undefined) : undefined}
		onclick={() => onsave(next)}>{saveLabel}</button
	>
	{#if disabled && disabledReason}<span class="muted gre-why">{disabledReason}</span>{/if}
</div>

<style>
	.gre {
		display: grid;
		gap: 8px;
		justify-items: start;
	}
	.gre-row {
		display: flex;
		flex-wrap: wrap;
		gap: 8px 12px;
		align-items: end;
	}
	.gre-f {
		display: grid;
		gap: 3px;
		font-size: 12.5px;
	}
	.gre-n {
		width: 96px;
	}
	.gre-vals {
		border: 1px solid var(--line-2);
		border-radius: 6px;
		padding: 6px 10px;
		margin: 0;
		display: flex;
		flex-wrap: wrap;
		gap: 4px 14px;
		max-width: 100%;
	}
	.check {
		display: inline-flex;
		gap: 5px;
		align-items: center;
		font-size: 13px;
	}
	.gre-sum {
		margin: 0;
		font-size: 12.5px;
		overflow-wrap: anywhere;
	}
	.gre-err {
		margin: 0;
		color: var(--err);
		font-size: 12.5px;
	}
	.gre-why {
		font-size: 12px;
	}
</style>
