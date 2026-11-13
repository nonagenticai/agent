<script lang="ts">
	// F-54 draft hint: free text that steers the next "Draft with LLM" call (never kept in the audit trail).
	// The counter turns red past the limit and Draft is held back by the parent; the server's 422 `hint_too_long`
	// is shown here too when it still arrives.
	import { HINT_MAX, hintState } from '$lib/journey/B8Generator';

	let {
		value = $bindable(''),
		error = '',
		disabled = false,
	}: { value?: string; error?: string; disabled?: boolean } = $props();

	const st = $derived(hintState(value));
</script>

<div class="p18-hint field" data-testid="draft-hint">
	<label for="p18-hint"
		>Hint for the next draft <span class="muted">(optional, not kept in the audit trail)</span></label
	>
	<textarea
		id="p18-hint"
		class="input"
		rows="2"
		{disabled}
		placeholder="e.g. this team only handles employee onboarding, not customer tickets"
		aria-invalid={st.over || !!error}
		bind:value
	></textarea>
	<div class="p18-hint-foot">
		{#if st.over}
			<span class="p18-hint-err" role="alert"
				>The hint is {-st.left} characters over the {HINT_MAX} limit.</span
			>
		{:else if error}
			<span class="p18-hint-err" role="alert">{error}</span>
		{/if}
		<span class="mono p18-hint-count" class:over={st.over} data-testid="hint-count"
			>{st.length}/{HINT_MAX}</span
		>
	</div>
</div>

<style>
	.p18-hint {
		margin: 0 0 12px;
	}
	.p18-hint-foot {
		display: flex;
		justify-content: space-between;
		gap: 8px;
		font-size: 12px;
	}
	.p18-hint-err {
		color: var(--err);
	}
	.p18-hint-count {
		margin-left: auto;
		color: var(--muted);
	}
	.p18-hint-count.over {
		color: var(--err);
	}
</style>
