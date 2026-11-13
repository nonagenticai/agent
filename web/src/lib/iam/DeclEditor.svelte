<script lang="ts">
	// "Inputs & state" editor shared by a job's tool page (A2) and the steps of a tenant identity procedure (A3).
	// Each input comes from the conversation (the model) or from a state key, which is already there, is the output of
	// another tool, or is asked of the user. Each output may be written to a state key. All conversion lives in
	// `$lib/iam/policy`; this component only edits the rows.
	import type { DeclForm, InputMode, ToolChoice } from './policy';

	let {
		form = $bindable(),
		tools,
		disabled = false,
		idPrefix = 'decl',
		identity = false,
	}: {
		form: DeclForm;
		tools: ToolChoice[];
		disabled?: boolean;
		idPrefix?: string;
		/** A tenant identity procedure: an output may also write the person's identity. A job's tool policy may not. */
		identity?: boolean;
	} = $props();

	const MODES: [InputMode, string][] = [
		['model', 'From the conversation (model)'],
		['state', 'From state: already there'],
		['tool', 'From state: output of another tool'],
		['ask', 'From state: ask the user'],
	];
	const outputsOf = (op: string) => tools.find((t) => t.key === op)?.output_fields ?? [];
</script>

<div class="stack decl" data-testid="{idPrefix}-editor">
	<section>
		<h4>Inputs</h4>
		{#if form.inputs.length === 0}<p class="muted small">This tool has no inputs.</p>{/if}
		{#each form.inputs as r, i (r.field)}
			<div class="decl-row" data-testid="{idPrefix}-input-{r.field}">
				<span class="mono decl-field">{r.field}</span>
				<select
					class="input"
					aria-label="Where {r.field} comes from"
					{disabled}
					bind:value={form.inputs[i]!.mode}
				>
					{#each MODES as [v, label] (v)}<option value={v}>{label}</option>{/each}
				</select>
				{#if r.mode !== 'model'}
					<label class="field inline"
						><span class="label small">State key</span>
						<input
							class="input mono"
							aria-label="State key for {r.field}"
							placeholder="customer_id"
							{disabled}
							bind:value={form.inputs[i]!.key}
						/></label
					>
				{/if}
				{#if r.mode === 'tool'}
					<label class="field inline"
						><span class="label small">Tool</span>
						<select
							class="input"
							aria-label="Tool that provides {r.field}"
							{disabled}
							bind:value={form.inputs[i]!.tool}
							onchange={() => (form.inputs[i]!.output = '')}
						>
							<option value="" disabled>Choose a tool…</option>
							{#each tools as t (t.key)}<option value={t.key}>{t.key}</option>{/each}
						</select></label
					>
					<label class="field inline"
						><span class="label small">Output field</span>
						<select
							class="input"
							aria-label="Output field that provides {r.field}"
							{disabled}
							bind:value={form.inputs[i]!.output}
						>
							<option value="" disabled>Choose a field…</option>
							{#each outputsOf(r.tool) as o (o)}<option value={o}>{o}</option>{/each}
						</select></label
					>
				{:else if r.mode === 'ask'}
					<label class="field inline wide"
						><span class="label small">What the agent says</span>
						<input
							class="input"
							aria-label="Prompt for {r.field}"
							placeholder="Please enter the code we just sent you"
							{disabled}
							bind:value={form.inputs[i]!.prompt}
						/></label
					>
					<label class="opt inline"
						><input type="checkbox" {disabled} bind:checked={form.inputs[i]!.secret} /> Secret (hidden from the
						transcript)</label
					>
					<label class="field inline"
						><span class="label small">Retries</span>
						<input
							class="input narrow"
							type="number"
							min="0"
							max="9"
							aria-label="Retries for {r.field}"
							{disabled}
							bind:value={form.inputs[i]!.retries}
						/></label
					>
				{/if}
			</div>
		{/each}
	</section>

	<section>
		<h4>Outputs → state</h4>
		{#if identity}
			<p class="muted small" data-testid="{idPrefix}-identity-hint">
				The person’s identity: write <span class="mono">subject</span> (their id) — and optionally
				<span class="mono">email</span> and <span class="mono">name</span> — to make the conversation known. One
				step of the procedure writes the subject.
			</p>
		{/if}
		{#if form.outputs.length === 0}<p class="muted small">This tool returns no fields.</p>{/if}
		{#each form.outputs as r, i (r.field)}
			<div class="decl-row" data-testid="{idPrefix}-output-{r.field}">
				<label class="opt inline"
					><input type="checkbox" {disabled} bind:checked={form.outputs[i]!.write} />
					Write <span class="mono">{r.field}</span> to state</label
				>
				{#if r.write}
					<label class="field inline"
						><span class="label small">State key</span>
						<input
							class="input mono"
							aria-label="State key written from {r.field}"
							placeholder="auth_token"
							{disabled}
							bind:value={form.outputs[i]!.key}
						/></label
					>
					<select class="input" aria-label="Kind of {r.field}" {disabled} bind:value={form.outputs[i]!.kind}>
						<option value="value">Value</option>
						<option value="secret">Secret (sealed, never shown to the model)</option>
					</select>
					<label class="field inline"
						><span class="label small">Expires after (seconds)</span>
						<input
							class="input narrow"
							inputmode="numeric"
							aria-label="Expiry in seconds of {r.field}"
							placeholder="never"
							{disabled}
							bind:value={form.outputs[i]!.ttl}
						/></label
					>
					<label class="opt inline"
						><input type="checkbox" {disabled} bind:checked={form.outputs[i]!.persist} /> Keep for this customer's
						next conversations</label
					>
				{/if}
			</div>
		{/each}
	</section>
</div>

<style>
	.decl-row {
		display: flex;
		flex-wrap: wrap;
		gap: 0.5rem 0.75rem;
		align-items: end;
		padding: 0.5rem 0;
		border-bottom: 1px solid var(--line, #e5e5e5);
	}
	.decl-field {
		min-width: 8rem;
		align-self: center;
	}
	.field.inline {
		display: flex;
		flex-direction: column;
		gap: 0.15rem;
	}
	.field.wide {
		flex: 1 1 16rem;
	}
	.narrow {
		width: 6rem;
	}
</style>
