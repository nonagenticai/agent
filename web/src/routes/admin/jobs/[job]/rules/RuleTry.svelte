<script lang="ts">
	// J9 "Try it": fill in the facts a rule reads (or pick an example) and see what `rule.evaluate` returns with the
	// tenant's saved values. A dry run — stores nothing, so every member may use it (not a write).
	import { onMount } from 'svelte';
	import { api } from '$lib/app.svelte';
	import type { BusinessRule, RuleEvaluation } from '$lib/api/types/release';
	import Chip from '$lib/ui/Chip.svelte';
	import { outcomeVariant } from './J9-view';

	let { rule }: { rule: BusinessRule } = $props();

	const facts0 = () =>
		rule.examples?.[0]?.facts ?? Object.fromEntries((rule.facts ?? []).map((f) => [f.key, '']));
	let facts = $state<Record<string, string>>({ ...facts0() });
	// Seeded once per card (the card is keyed by rule id).
	const label0 = () => rule.examples?.[0]?.label ?? null;
	let picked = $state<string | null>(label0());
	let result = $state<RuleEvaluation | null>(null);
	let error = $state('');
	let running = $state(false);

	function pick(label: string, f: Record<string, string>) {
		picked = label;
		facts = { ...f };
		void run();
	}
	async function run() {
		running = true;
		error = '';
		try {
			result = await api().evaluateRule(rule.id, { ...facts });
		} catch (e) {
			result = null;
			error = e instanceof Error ? e.message : 'Evaluation unavailable.';
		}
		running = false;
	}
	onMount(() => {
		if (picked) void run();
	});
	const agentDoes = $derived(rule.outcomes?.find((o) => o.code === result?.code)?.agent_does ?? null);
</script>

<div class="j9try" data-testid="rule-try" data-try={rule.id}>
	<div class="j9try-head">
		<b>Try it</b>
		<span class="muted small">with your saved values, as of today · stores nothing</span>
	</div>
	{#if rule.examples?.length}
		<div class="j9try-examples" role="group" aria-label="Example situations">
			{#each rule.examples as ex (ex.label)}
				<button
					type="button"
					class="j9try-ex"
					class:on={picked === ex.label}
					aria-pressed={picked === ex.label}
					onclick={() => pick(ex.label, ex.facts)}>{ex.label}</button
				>
			{/each}
		</div>
	{/if}
	<form
		class="j9try-form"
		novalidate
		onsubmit={(e) => {
			e.preventDefault();
			picked = null;
			void run();
		}}
	>
		{#each rule.facts ?? [] as f (f.key)}
			<label class="j9try-field">
				<span class="small">{f.label} <span class="mono muted">{f.key}</span></span>
				{#if f.kind === 'choice'}
					<select class="input" bind:value={facts[f.key]}>
						{#each f.options ?? [] as o (o)}<option value={o}>{o}</option>{/each}
					</select>
				{:else}
					<input
						class="input"
						type={f.kind === 'date' ? 'date' : 'text'}
						inputmode={f.kind === 'number' ? 'decimal' : undefined}
						bind:value={facts[f.key]}
					/>
				{/if}
			</label>
		{/each}
		<div>
			<button class="btn sm" type="submit" disabled={running}>{running ? 'Evaluating…' : 'Evaluate'}</button>
		</div>
	</form>
	{#if error}
		<p class="err small" role="alert">{error}</p>
	{:else if result}
		<div class="j9try-result o-{result.outcome}" data-testid="rule-try-result" aria-live="polite">
			<div class="j9try-says">
				<span class="muted small">{rule.id} says</span>
				<Chip text={result.code} variant={outcomeVariant(result.outcome)} />
			</div>
			<p class="j9try-reason">{result.reason}</p>
			{#if result.trace.length}
				<ol class="j9try-trace mono">
					{#each result.trace as t, i (i)}<li>{t}</li>{/each}
				</ol>
			{/if}
			{#if agentDoes}<p class="small"><b>Then the agent:</b> {agentDoes}</p>{/if}
			{#if result.outcome === 'placeholder'}<p class="small">
					<b>Then the agent:</b> hands the case to a person, because the rule has no values.
				</p>{/if}
		</div>
	{/if}
</div>

<style>
	.j9try {
		display: grid;
		gap: 10px;
		padding: 12px;
		border: 1px dashed var(--line);
		border-radius: var(--r);
		background: var(--sunk);
		min-width: 0;
	}
	.j9try-head {
		display: flex;
		flex-wrap: wrap;
		gap: 4px 8px;
		align-items: baseline;
	}
	.small {
		font-size: 12px;
	}
	.j9try-examples {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
	}
	.j9try-ex {
		font: inherit;
		font-size: 12px;
		padding: 3px 9px;
		border-radius: 999px;
		border: 1px solid var(--line);
		background: var(--panel);
		color: var(--fg);
		cursor: pointer;
	}
	.j9try-ex.on {
		border-color: var(--accent);
		color: var(--accent);
	}
	.j9try-form {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(160px, 1fr));
		gap: 8px;
		align-items: end;
	}
	.j9try-field {
		display: grid;
		gap: 3px;
		min-width: 0;
	}
	.j9try-result {
		border-left: 3px solid var(--line);
		padding: 6px 0 6px 10px;
		background: var(--panel);
		border-radius: 0 var(--r) var(--r) 0;
	}
	.j9try-result.o-eligible {
		border-left-color: var(--ok);
	}
	.j9try-result.o-ineligible {
		border-left-color: var(--err);
	}
	.j9try-result.o-escalate,
	.j9try-result.o-placeholder {
		border-left-color: var(--warn);
	}
	.j9try-says {
		display: flex;
		gap: 8px;
		align-items: center;
	}
	.j9try-reason {
		margin: 4px 0;
		font-weight: 600;
	}
	.j9try-trace {
		margin: 0 0 4px;
		padding-left: 18px;
		color: var(--muted);
	}
	.err {
		color: var(--err);
		margin: 0;
	}
</style>
