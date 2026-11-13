<script lang="ts">
	import { untrack } from 'svelte';
	import { api } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { writeAttrs } from '$lib/auth/roles';
	import type { BusinessRule, RuleDefinition, RuleProgram } from '$lib/api/types/release';
	import Drawer from '$lib/ui/Drawer.svelte';
	import { toast } from '$lib/ui/toaster.svelte';
	import RuleConditionEditor from './RuleConditionEditor.svelte';
	let {
		open = $bindable(),
		rule = null,
		onchanged,
	}: { open: boolean; rule?: BusinessRule | null; onchanged: () => void | Promise<void> } = $props();
	const blank = (): RuleDefinition => ({
		label: '',
		applies_to: [],
		inputs: [],
		outcome: '',
		values: [],
		facts: [],
		evaluation: {
			condition: {
				kind: 'compare',
				left: { kind: 'fact', key: '' },
				op: 'eq',
				right: { kind: 'literal', value: '' },
			},
			eligible_reason: '',
			ineligible_reason: '',
		},
	});
	let form = $state(blank());
	let id = $state('');
	let operations = $state('');
	let start = $state('');
	let busy = $state(false);
	let error = $state('');
	let removing = $state(false);
	const wa = $derived(writeAttrs(session, 'job_designer'));
	const dirty = $derived(JSON.stringify({ form, id, operations }) !== start);
	$effect(() => {
		if (!open) return;
		untrack(() => {
			form = rule?.evaluation
				? {
						label: rule.label,
						applies_to: [...rule.applies_to],
						inputs: [...rule.inputs],
						outcome: rule.outcome,
						values: rule.values.map(({ value, ...v }) => ({ ...v })),
						facts: structuredClone($state.snapshot(rule.facts ?? [])),
						evaluation: structuredClone($state.snapshot(rule.evaluation)) as RuleProgram,
						statement: rule.statement,
						uses: rule.uses,
						outcomes: rule.outcomes,
						examples: rule.examples,
					}
				: blank();
			id = rule?.id ?? '';
			operations = rule?.applies_to.join(', ') ?? '';
			error = '';
			removing = false;
			start = JSON.stringify({ form, id, operations });
		});
	});
	async function save() {
		if (busy || wa.disabled) return;
		error = '';
		busy = true;
		const key = id.trim();
		const body = {
			...$state.snapshot(form),
			label: form.label.trim(),
			applies_to: operations
				.split(',')
				.map((s) => s.trim())
				.filter(Boolean),
			inputs: form.facts.map((f) => f.key),
			outcome: `eligible | ineligible:${key}`,
		};
		try {
			if (rule) await api().saveRuleDefinition(key, body);
			else await api().createBusinessRule({ id: key, ...body });
			open = false;
			toast(`${key} saved to draft.`);
			await onchanged();
		} catch (e) {
			error = e instanceof Error ? e.message : 'Could not save this rule.';
		} finally {
			busy = false;
		}
	}
	async function remove() {
		if (!rule || busy || wa.disabled) return;
		busy = true;
		error = '';
		try {
			await api().deleteBusinessRule(rule.id);
			open = false;
			toast(`${rule.id} removed from draft.`);
			await onchanged();
		} catch (e) {
			error = e instanceof Error ? e.message : 'Could not remove this rule.';
		} finally {
			busy = false;
		}
	}
</script>

<Drawer bind:open title={rule ? `Edit rule ${rule.id}` : 'Create business rule'} {dirty}>
	<form
		class="stack"
		onsubmit={(e) => {
			e.preventDefault();
			void save();
		}}
	>
		<p>
			Define the facts and conditions the agent checks with <code
				>rule.evaluate{'{'}{id || 'RULE-ID'}{'}'}</code
			>. Save to draft, try it, then publish a release to use it in Production.
		</p>
		<label
			>Rule id<input
				class="input"
				bind:value={id}
				disabled={!!rule}
				required
				pattern={'[A-Z][A-Z0-9-]{0,31}'}
				maxlength="32"
				placeholder="CREDIT-LIMIT"
			/></label
		>
		<label>Label<input class="input" bind:value={form.label} required /></label>
		<label
			>Guarded operations<input
				class="input"
				bind:value={operations}
				required
				placeholder="case.create, order.return"
			/></label
		>
		<p class="muted">
			Read these facts in your procedure before calling the rule. Add a rule step to the procedure to enforce
			it.
		</p>
		<fieldset>
			<legend>Facts</legend>
			{#each form.facts as f, i (i)}
				<div class="row">
					<label
						>Fact {i + 1} key<input
							class="input"
							bind:value={f.key}
							required
							placeholder="order.total"
						/></label
					>
					<label>Fact {i + 1} label<input class="input" bind:value={f.label} required /></label>
					<label
						>Fact {i + 1} type<select class="input" bind:value={f.kind}
							><option value="number">Number</option><option value="text">Text</option><option value="date"
								>Date</option
							><option value="bool">True or false</option></select
						></label
					>
					<button class="btn sm" type="button" onclick={() => form.facts.splice(i, 1)}
						>Remove fact {i + 1}</button
					>
				</div>
			{/each}
			<button
				class="btn sm"
				type="button"
				disabled={form.facts.length >= 32}
				onclick={() => form.facts.push({ key: '', label: '', kind: 'text' })}>Add fact</button
			>
		</fieldset>
		<fieldset>
			<legend>Thresholds (optional)</legend>
			<p class="muted">
				A threshold can change through Set values without changing the condition. An unset threshold makes the
				rule a placeholder.
			</p>
			{#each form.values as v, i (i)}
				<div class="row">
					<label
						>Threshold {i + 1} key<input
							class="input"
							bind:value={v.key}
							required
							pattern="[a-z][a-z0-9_]*"
						/></label
					>
					<label>Threshold {i + 1} label<input class="input" bind:value={v.label} required /></label>
					<label
						>Threshold {i + 1} unit<select class="input" bind:value={v.unit}
							><option value="number">Number</option><option value="days">Days</option><option value="months"
								>Months</option
							><option value="text">Text</option><option value="list">Comma-separated list</option></select
						></label
					>
					<button class="btn sm" type="button" onclick={() => form.values.splice(i, 1)}
						>Remove threshold {i + 1}</button
					>
				</div>
			{/each}
			<button
				class="btn sm"
				type="button"
				disabled={form.values.length >= 32}
				onclick={() => form.values.push({ key: '', label: '', unit: 'number', hint: '' })}
				>Add threshold</button
			>
		</fieldset>
		<RuleConditionEditor bind:condition={form.evaluation.condition} facts={form.facts} values={form.values} />
		<p class="muted">
			Numeric comparisons require numbers. List comparisons use comma-separated values and ignore letter case.
			Missing or invalid facts send the decision to a person.
		</p>
		<label
			>Eligible reason<textarea
				class="input"
				bind:value={form.evaluation.eligible_reason}
				required
				maxlength="1000"
			></textarea></label
		>
		<label
			>Ineligible reason<textarea
				class="input"
				bind:value={form.evaluation.ineligible_reason}
				required
				maxlength="1000"
			></textarea></label
		>
		{#if error}<p role="alert">{error}</p>{/if}
		<div class="row">
			<button class="btn primary" type="submit" disabled={busy || wa.disabled} title={wa.title}
				>{busy ? 'Saving…' : 'Save rule draft'}</button
			>
			<button class="btn" type="button" onclick={() => (open = false)}>Cancel</button>
			{#if rule}<button
					class="btn"
					type="button"
					disabled={busy || wa.disabled}
					onclick={() => (removing = true)}>Remove rule</button
				>{/if}
		</div>
		{#if removing}<div role="alert">
				<p>Remove {rule?.id} from the draft? Update any procedure steps that call it before publishing.</p>
				<button class="btn danger" type="button" disabled={busy} onclick={() => void remove()}
					>Confirm removal</button
				>
			</div>{/if}
	</form>
</Drawer>

<style>
	label {
		display: grid;
		gap: 4px;
	}
	fieldset {
		display: grid;
		gap: 10px;
		border: 1px solid var(--line);
		border-radius: var(--r);
		padding: 12px;
		min-width: 0;
	}
	.row {
		display: flex;
		gap: 8px;
		flex-wrap: wrap;
		align-items: end;
	}
	.row label {
		flex: 1;
		min-width: 110px;
	}
</style>
