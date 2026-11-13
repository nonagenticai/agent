<script lang="ts">
	// PK-8 "Replaces platform topics": the platform topic keys a source's or an upload's documents replace in searches
	// (strata hides platform text with those topic keys whenever this tenant's text is searched with it). A tag input:
	// Enter or comma adds, × removes; the rules are the API's (≤ 32 keys of 1–128 characters, sorted, unique).
	import { MAX_SUPERSEDES, normalizeTopics, splitTopics } from './pk';

	let {
		value = $bindable([]),
		disabled = false,
		id = 'supersedes',
	}: { value?: string[]; disabled?: boolean; id?: string } = $props();

	let text = $state('');
	let error = $state('');

	function commit() {
		const add = splitTopics(text);
		if (!add.length) return;
		const t = normalizeTopics([...value, ...add]);
		if (t.error !== null) {
			error = t.error;
			return;
		}
		value = t.keys;
		text = '';
		error = '';
	}
	function key(e: KeyboardEvent) {
		if (e.key === 'Enter' || e.key === ',') {
			e.preventDefault();
			commit();
		} else if (e.key === 'Backspace' && !text && value.length) {
			value = value.slice(0, -1);
		}
	}
	const remove = (k: string) => (value = value.filter((x) => x !== k));
</script>

<div class="field span sup" data-testid="supersedes-input">
	<label class="label" for={id}>Replaces platform topics</label>
	<div class="sup-box" class:off={disabled}>
		{#each value as k (k)}
			<span class="chip accent sup-tag mono"
				>{k}<button
					type="button"
					class="sup-x"
					aria-label="Remove topic {k}"
					{disabled}
					onclick={() => remove(k)}>×</button
				></span
			>
		{/each}
		<input
			{id}
			class="sup-in mono"
			bind:value={text}
			onkeydown={key}
			onblur={commit}
			{disabled}
			placeholder={value.length ? '' : 'returns.window, eu.withdrawal'}
			aria-label="Replaces platform topics"
		/>
	</div>
	<span class="cell-sub"
		>Platform text with these topic keys is left out when this text is searched with it: up to {MAX_SUPERSEDES}
		keys of a-z, 0-9 and _ . / -.</span
	>
	{#if error}<span class="cell-sub sup-err" role="alert">{error}</span>{/if}
</div>

<style>
	.sup-box {
		display: flex;
		flex-wrap: wrap;
		gap: 4px;
		align-items: center;
		padding: 4px 6px;
		border: 1px solid var(--line);
		border-radius: 6px;
		background: var(--bg);
		min-height: 34px;
	}
	.sup-box.off {
		opacity: 0.6;
	}
	.sup-tag {
		display: inline-flex;
		gap: 4px;
		align-items: center;
	}
	.sup-x {
		border: 0;
		background: none;
		cursor: pointer;
		color: inherit;
		padding: 0 2px;
	}
	.sup-in {
		flex: 1 1 140px;
		min-width: 0;
		border: 0;
		outline: 0;
		background: transparent;
		color: var(--fg);
		font-size: 13px;
	}
	.sup-err {
		color: var(--err);
	}
</style>
