<script lang="ts">
	// T15 Identity procedures (A3): named, reusable chains of tools that produce state such as a sign-in token or a
	// customer id for your company's own systems. A procedure is a list of steps; each step is a tool with the same
	// "inputs & state" declarations as a job's tool page. A job attaches the ones it uses (Tools page). Any member
	// reads; a tenant admin creates, edits and deletes. A procedure a job still uses cannot be deleted.
	import { onMount } from 'svelte';
	import { api } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { canTenantAdmin, writeHint } from '$lib/auth/roles';
	import { Resource } from '$lib/load.svelte';
	import { opKey, type OpDefLite } from '$lib/connectors/ops';
	import type { IamProcedure, IdentityStrength } from '$lib/api/types/iam';
	import { toast } from '$lib/ui/toaster.svelte';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import DeclEditor from '$lib/iam/DeclEditor.svelte';
	import { explainCode, formProblem, toDeclForm, toStep, type DeclForm } from '$lib/iam/policy';

	interface Data {
		procedures: IamProcedure[];
		defs: OpDefLite[];
	}
	const res = new Resource<Data>(async () => {
		const [procedures, defs] = await Promise.all([
			api().iamProcedures(),
			api()
				.connectorOpDefs()
				.catch(() => [] as OpDefLite[]),
		]);
		return { procedures, defs };
	});
	onMount(() => void res.refresh());
	const mayEdit = $derived(canTenantAdmin(session));

	interface StepDraft {
		id: number;
		op: string;
		form: DeclForm;
	}
	let open = $state(false);
	let editing = $state<IamProcedure | null>(null);
	let name = $state('');
	let description = $state('');
	let strength = $state<IdentityStrength>('identified');
	let steps = $state<StepDraft[]>([]);
	let saving = $state(false);
	let err = $state<{ code: string; message: string } | null>(null);
	let seq = 0;

	const defOf = (defs: OpDefLite[], op: string) => defs.find((d) => opKey(d) === op);
	const choices = (defs: OpDefLite[]) => defs.map((d) => ({ key: opKey(d), output_fields: d.output_fields }));
	const formFor = (defs: OpDefLite[], op: string, p?: IamProcedure['steps'][number]): DeclForm => {
		const d = defOf(defs, op);
		return toDeclForm(d?.input_fields ?? [], d?.output_fields ?? [], p?.inputs, p?.outputs);
	};
	const problem = $derived(
		!name.trim()
			? 'Give the procedure a name.'
			: steps.some((s) => !s.op)
				? 'Choose a tool for every step.'
				: (steps.map((s) => formProblem(s.form)).find((x) => x !== null) ?? null),
	);

	function start(defs: OpDefLite[], p: IamProcedure | null) {
		editing = p;
		name = p?.name ?? '';
		description = p?.description ?? '';
		strength = p?.strength ?? 'identified';
		steps = (p?.steps ?? []).map((s) => ({ id: seq++, op: s.op, form: formFor(defs, s.op, s) }));
		err = null;
		open = true;
	}
	function addStep() {
		steps = [...steps, { id: seq++, op: '', form: { inputs: [], outputs: [] } }];
	}
	function pickOp(defs: OpDefLite[], i: number, op: string) {
		steps[i]!.op = op;
		steps[i]!.form = formFor(defs, op);
	}

	async function save() {
		if (saving || problem) return;
		saving = true;
		err = null;
		const input = {
			name: name.trim(),
			description: description.trim(),
			strength,
			steps: steps.map((s) => toStep(s.op, s.form)),
		};
		try {
			if (editing) await api().updateIamProcedure(editing.id, input);
			else await api().createIamProcedure(input);
			toast(`${input.name} saved. Attach it to a job on that job's Tools page.`);
			open = false;
			await res.refresh();
		} catch (e) {
			const x = e as { code?: string; message: string };
			err = { code: x.code ?? 'error', message: explainCode(x.code, x.message) };
		} finally {
			saving = false;
		}
	}

	let deleteErr = $state<{ id: string; message: string } | null>(null);
	async function remove(p: IamProcedure) {
		deleteErr = null;
		try {
			await api().deleteIamProcedure(p.id);
			toast(`${p.name} deleted.`);
			await res.refresh();
		} catch (e) {
			const x = e as { code?: string; message: string };
			deleteErr = { id: p.id, message: explainCode(x.code, x.message) };
		}
	}
</script>

<PageHead
	id="T15"
	sub="Reusable chains of tools that sign a customer in or look them up in your systems. Attach them to the jobs that need them."
>
	{#snippet actions()}
		{#if res.value.state === 'ok' || res.value.state === 'empty'}
			{@const defs = res.value.data.defs}
			<button
				class="btn primary"
				type="button"
				data-testid="new-procedure"
				disabled={!mayEdit}
				title={mayEdit ? undefined : writeHint('tenant_admin')}
				onclick={() => start(defs, null)}>New procedure</button
			>
		{/if}
	{/snippet}
</PageHead>

<LoadState resource={res} empty="No identity procedures yet. Create one to reuse a sign-in across jobs.">
	{#snippet children(d)}
		{#if !mayEdit}
			<Callout kind="info" title="Read only"
				><span>{writeHint('tenant_admin')} to change identity procedures.</span></Callout
			>
		{/if}
		<ul class="stack" data-testid="procedures">
			{#each d.procedures as p (p.id)}
				<li>
					<Card title={p.name} sub={p.description}>
						<div class="stack">
							<ol class="small">
								{#each p.steps as s, i (i)}
									<li>
										<span class="mono">{s.op}</span>
										{#if Object.keys(s.outputs).length}
											→ writes
											<span class="mono"
												>{Object.values(s.outputs)
													.map((o) => o.key)
													.join(', ')}</span
											>
										{/if}
									</li>
								{:else}
									<li class="muted">No steps.</li>
								{/each}
							</ol>
							<p class="muted small">
								{p.strength === 'verified'
									? 'Verified: the procedure proves who the person is.'
									: 'Identified: the procedure establishes who the person is.'}
								{p.jobs
									? ` Used by ${p.jobs} job${p.jobs === 1 ? '' : 's'}.`
									: ' Not attached to any job yet.'}
							</p>
							<div class="row">
								<button class="btn sm" type="button" disabled={!mayEdit} onclick={() => start(d.defs, p)}
									>{mayEdit ? 'Edit' : 'View'}</button
								>
								<button
									class="btn sm"
									type="button"
									disabled={!mayEdit}
									title={p.jobs ? 'A job still uses it: detach it first' : undefined}
									onclick={() => remove(p)}>Delete</button
								>
							</div>
							{#if deleteErr?.id === p.id}
								<p class="s-err small" role="alert" data-testid="delete-error">{deleteErr.message}</p>
							{/if}
						</div>
					</Card>
				</li>
			{/each}
		</ul>

		<Drawer bind:open title={editing ? editing.name : 'New identity procedure'} wide>
			<form
				class="stack"
				data-testid="procedure-form"
				onsubmit={(e) => {
					e.preventDefault();
					void save();
				}}
			>
				<fieldset class="stack" disabled={!mayEdit || saving}>
					<label class="field"
						><span class="label">Name</span>
						<input
							class="input"
							aria-label="Procedure name"
							placeholder="Acme sign-in"
							bind:value={name}
						/></label
					>
					<label class="field"
						><span class="label">Description</span>
						<textarea class="input" rows="2" aria-label="Procedure description" bind:value={description}
						></textarea></label
					>
					<label class="field"
						><span class="label">What this procedure establishes</span>
						<select class="input" aria-label="Identity strength" bind:value={strength}>
							<option value="identified">Identified — we know who they are</option>
							<option value="verified">Verified — the procedure proved it (e.g. a one-time code)</option>
						</select>
						<span class="muted small"
							>Identified: the conversation knows the person. Verified: the platform also treats them as a
							verified customer (their orders and other customer data can be read).</span
						></label
					>
					{#each steps as s, i (s.id)}
						<section class="stack" data-testid="step-{i}">
							<div class="row">
								<b>Step {i + 1}</b>
								<select
									class="input"
									aria-label="Tool of step {i + 1}"
									value={s.op}
									onchange={(e) => pickOp(d.defs, i, e.currentTarget.value)}
								>
									<option value="" disabled>Choose a tool…</option>
									{#each choices(d.defs) as c (c.key)}<option value={c.key}>{c.key}</option>{/each}
								</select>
								<button
									class="btn sm"
									type="button"
									onclick={() => (steps = steps.filter((x) => x.id !== s.id))}>Remove step</button
								>
							</div>
							{#if s.op}
								<DeclEditor
									bind:form={steps[i]!.form}
									tools={choices(d.defs).filter((c) => c.key !== s.op)}
									idPrefix="step{i}"
									identity
								/>
							{/if}
						</section>
					{/each}
					<button class="btn" type="button" data-testid="add-step" onclick={addStep}>Add a step</button>
				</fieldset>
				{#if err}
					<Callout kind="err" title="Not saved"
						><span role="alert" data-testid="procedure-error"
							>{err.message} <span class="mono muted">{err.code}</span></span
						></Callout
					>
				{/if}
				{#if problem}<p class="muted small">{problem}</p>{/if}
				<div class="row">
					<button class="btn primary" type="submit" disabled={!mayEdit || saving || problem !== null}
						>Save procedure</button
					>
					<button class="btn" type="button" onclick={() => (open = false)}>Cancel</button>
				</div>
			</form>
		</Drawer>
	{/snippet}
</LoadState>
