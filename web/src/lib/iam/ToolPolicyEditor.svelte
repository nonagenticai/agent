<script lang="ts">
	// One op's full declaration on the job's tools page: the shared inputs/outputs editor plus what the tool needs
	// (`requires`), whether the customer confirms first and what happens when an input cannot be resolved.
	import { api } from '$lib/app.svelte';
	import { ApiError } from '$lib/api/errors';
	import { session } from '$lib/auth/session.svelte';
	import { canWrite, writeAttrs, writeHint } from '$lib/auth/roles';
	import type { ToolPolicy } from '$lib/api/types/iam';
	import { toast } from '$lib/ui/toaster.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import DeclEditor from './DeclEditor.svelte';
	import { explainCode, formProblem, isEmptyPolicy, toPolicy, toPolicyForm, type ToolChoice } from './policy';

	let {
		op,
		inputFields,
		outputFields,
		policy,
		tools,
		onsaved,
	}: {
		op: string;
		inputFields: string[];
		outputFields: string[];
		policy: ToolPolicy | null;
		tools: ToolChoice[];
		onsaved: () => void | Promise<void>;
	} = $props();

	const writable = $derived(canWrite(session, 'integrations_manager'));
	// svelte-ignore state_referenced_locally
	let form = $state(toPolicyForm(inputFields, outputFields, policy));
	let saving = $state(false);
	let err = $state<{ code: string; message: string } | null>(null);
	const problem = $derived(formProblem(form));

	async function run(fn: () => Promise<unknown>, done: string) {
		saving = true;
		err = null;
		try {
			await fn();
			toast(done);
			await onsaved();
		} catch (e) {
			const x = e as ApiError;
			err = { code: x.code ?? 'error', message: explainCode(x.code, x.message) };
		} finally {
			saving = false;
		}
	}
	const save = () => {
		const p = toPolicy(form);
		return run(
			() => (isEmptyPolicy(p) && policy ? api().clearToolPolicy(op) : api().saveToolPolicy(op, p)),
			`${op} saved to the draft. Try it in the test chat; customers get it when you publish a release.`,
		);
	};
	const clear = () => run(() => api().clearToolPolicy(op), `${op} is back to model-only inputs.`);
</script>

<div class="stack" data-testid="policy-editor">
	<DeclEditor
		bind:form
		tools={tools.filter((t) => t.key !== op)}
		disabled={!writable || saving}
		idPrefix="policy"
	/>
	<section>
		<h4>Needs and outcome</h4>
		<label class="field"
			><span class="label">Requires these state keys before it runs</span>
			<input
				class="input mono"
				aria-label="Required state keys"
				placeholder="customer_id, auth_token"
				disabled={!writable || saving}
				bind:value={form.requires}
			/></label
		>
		<label class="field"
			><span class="label">Before it runs</span>
			<select
				class="input"
				aria-label="Confirmation"
				disabled={!writable || saving}
				bind:value={form.confirmation}
			>
				<option value="by_user">Ask the user to confirm</option>
				<option value="auto">Run automatically</option>
			</select></label
		>
		<label class="field"
			><span class="label">If an input cannot be resolved</span>
			<select
				class="input"
				aria-label="On failure"
				disabled={!writable || saving}
				bind:value={form.onFailure}
			>
				<option value="handover">Hand over to a person</option>
				<option value="refuse">Refuse</option>
			</select></label
		>
	</section>
	{#if err}
		<Callout kind="err" title="Not saved"
			><span data-testid="policy-error" role="alert"
				>{err.message} <span class="mono muted">{err.code}</span></span
			></Callout
		>
	{/if}
	{#if problem}<p class="muted small" data-testid="policy-problem">{problem}</p>{/if}
	<div class="row">
		<button
			class="btn primary"
			type="button"
			{...writeAttrs(session, 'integrations_manager')}
			disabled={!writable || saving || problem !== null}
			onclick={save}>Save to draft</button
		>
		{#if policy}
			<button class="btn" type="button" disabled={!writable || saving} onclick={clear}
				>Remove declaration</button
			>
		{/if}
	</div>
	{#if !writable}<p class="note">
			{writeHint('integrations_manager')} — declared inputs are read-only for you.
		</p>{/if}
</div>
