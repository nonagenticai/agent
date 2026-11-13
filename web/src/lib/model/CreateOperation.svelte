<script lang="ts">
	import { api, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { superAdminAttrs } from '$lib/auth/roles';
	import type { IdentityLevel, NewOperationInput, ObjectSummary, OperationRow } from '$lib/api/types/model';
	import Drawer from '$lib/ui/Drawer.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import AuthoringLayer from './AuthoringLayer.svelte';
	import { EFFECT_LABEL, LEVEL_LABEL } from './ui';

	let {
		objects,
		operations,
		oncreated,
	}: { objects: ObjectSummary[]; operations: OperationRow[]; oncreated: () => void | Promise<void> } =
		$props();
	const blank = () => ({
		object: '',
		verb: '',
		variant: '',
		layer_key: '',
		modality: 'observe' as NewOperationInput['modality'],
		op_kind: 'standard' as NewOperationInput['op_kind'],
		identity_level: '' as IdentityLevel | '',
		side_effect: 'R' as NewOperationInput['side_effect'],
		confirm_back: false,
		verify_read: '',
		idempotency: 'nat' as NewOperationInput['idempotency'],
		input: '',
		output: '',
		description: '',
		discriminator_field: '',
		discriminator_value: '',
	});
	let open = $state(false);
	let busy = $state(false);
	let form = $state(blank());
	let created = $state<OperationRow | null>(null);
	const names = (s: string) => [
		...new Set(
			s
				.split(',')
				.map((x) => x.trim())
				.filter(Boolean),
		),
	];
	function start() {
		form = blank();
		created = null;
		open = true;
	}
	async function save() {
		if (busy || !form.identity_level) return;
		busy = true;
		const input: NewOperationInput = {
			object: form.object,
			verb: form.verb.trim(),
			variant: form.variant.trim() || undefined,
			layer_key: form.layer_key,
			modality: form.modality,
			op_kind: form.op_kind,
			identity_level: form.identity_level,
			side_effect: form.side_effect,
			confirm_back: form.confirm_back,
			verify_read: form.verify_read || null,
			idempotency: form.idempotency,
			input_fields: names(form.input),
			output_fields: names(form.output),
			description: form.description.trim(),
			extra_errors: [],
			...(form.variant.trim()
				? {
						discriminator: { field: form.discriminator_field.trim(), value: form.discriminator_value.trim() },
					}
				: {}),
		};
		if (
			await guard(async () => {
				created = await api().createOperation(input);
			}, 'Operation draft created.')
		)
			await oncreated();
		busy = false;
	}
</script>

<button class="btn primary" type="button" {...superAdminAttrs(session)} onclick={start}
	>Create operation</button
>
<Drawer bind:open title="Create operation" dirty={!created && !!(form.object || form.verb)}>
	{#if created}
		<Callout kind="info" title="Operation created">
			<p>{created.key} is an untagged change in {form.layer_key}.</p>
			<p>
				Inputs: {created.input_fields.join(', ') || 'none'}. Outputs: {created.output_fields.join(', ') ||
					'none'}.
			</p>
			<p>
				<a href="/platform/operations?op={encodeURIComponent(created.key)}">Open operation</a>, then
				<a href="/platform/templates">tag the template layer</a>.
			</p>
		</Callout>
	{:else if objects.length === 0}
		<p>Create an <a href="/platform/objects">object</a> before adding its operations.</p>
	{:else}
		<form
			class="stack"
			onsubmit={(e) => {
				e.preventDefault();
				void save();
			}}
		>
			<AuthoringLayer bind:value={form.layer_key} disabled={busy} />
			<div class="field">
				<label for="new-op-object">Object</label><select
					id="new-op-object"
					class="input"
					bind:value={form.object}
					required
					disabled={busy}
					><option value="" disabled>Choose an object</option>{#each objects as o (o.object)}<option
							value={o.object}>{o.label} · {o.object}</option
						>{/each}</select
				>
			</div>
			<div class="field">
				<label for="new-op-verb">Verb</label><input
					id="new-op-verb"
					class="input"
					bind:value={form.verb}
					required
					pattern="[a-z][a-z0-9_]*"
					placeholder="read"
					disabled={busy}
				/>
			</div>
			<div class="field">
				<label for="new-op-variant">Variant (optional)</label><input
					id="new-op-variant"
					class="input"
					bind:value={form.variant}
					pattern="[A-Za-z0-9_\-]+"
					disabled={busy}
				/><span class="muted"
					>Create the unqualified operation first. A variant inherits its unchanged attributes.</span
				>
			</div>
			{#if form.variant.trim()}
				<div class="field">
					<label for="new-op-discriminator-field">Discriminator field</label><input
						id="new-op-discriminator-field"
						class="input"
						bind:value={form.discriminator_field}
						required
						pattern="[a-z][a-z0-9_]*"
						disabled={busy}
					/>
				</div>
				<div class="field">
					<label for="new-op-discriminator-value">Discriminator value</label><input
						id="new-op-discriminator-value"
						class="input"
						bind:value={form.discriminator_value}
						required
						disabled={busy}
					/>
				</div>
			{/if}
			<div class="field">
				<label for="new-op-modality">Modality</label><select
					id="new-op-modality"
					class="input"
					bind:value={form.modality}
					disabled={busy}
					><option value="observe">Observe — read state</option><option value="do"
						>Do — change the record</option
					><option value="request">Request — ask the provider</option></select
				>
			</div>
			<div class="field">
				<label for="new-op-kind">Operation kind</label><select
					id="new-op-kind"
					class="input"
					bind:value={form.op_kind}
					disabled={busy}
					><option value="standard">Standard</option><option value="hosted_payment_link"
						>Hosted payment link</option
					></select
				>
			</div>
			<div class="field">
				<label for="new-op-identity">Minimum identity level</label><select
					id="new-op-identity"
					class="input"
					bind:value={form.identity_level}
					required
					disabled={busy}
					><option value="" disabled>Choose the required level</option
					>{#each ['0', '1', '2', '3', 'S'] as l}<option value={l}>{LEVEL_LABEL[l]}</option>{/each}</select
				><span class="muted">The server enforces the contract's minimum level.</span>
			</div>
			<div class="field">
				<label for="new-op-side-effect">Side effect</label><select
					id="new-op-side-effect"
					class="input"
					bind:value={form.side_effect}
					disabled={busy}
					>{#each ['R', 'W', 'X', 'P'] as effect}<option value={effect}
							>{effect} — {EFFECT_LABEL[effect as NewOperationInput['side_effect']]}</option
						>{/each}</select
				>
			</div>
			<label
				><input type="checkbox" bind:checked={form.confirm_back} disabled={busy} /> Require customer confirmation</label
			>
			<div class="field">
				<label for="new-op-idempotency">Idempotency</label><select
					id="new-op-idempotency"
					class="input"
					bind:value={form.idempotency}
					disabled={busy}
					><option value="nat">Natural</option><option value="key">Idempotency key</option><option
						value="key+dup">Key and duplicate search</option
					></select
				>
			</div>
			<div class="field">
				<label for="new-op-verify">Verification read</label><select
					id="new-op-verify"
					class="input"
					bind:value={form.verify_read}
					disabled={busy}
					><option value="">None</option
					>{#each operations.filter((o) => o.side_effect === 'R') as o (o.key)}<option value={o.key}
							>{o.key}</option
						>{/each}</select
				>
			</div>
			<div class="field">
				<label for="new-op-input">Input fields (comma separated)</label><input
					id="new-op-input"
					class="input"
					bind:value={form.input}
					disabled={busy}
				/>
			</div>
			<div class="field">
				<label for="new-op-output">Output fields (comma separated)</label><input
					id="new-op-output"
					class="input"
					bind:value={form.output}
					disabled={busy}
				/><span class="muted"
					>Leave I/O blank to use the operation contract's fields. Custom operations use the object's live
					fields.</span
				>
			</div>
			<div class="field">
				<label for="new-op-description">Description</label><textarea
					id="new-op-description"
					class="input"
					bind:value={form.description}
					disabled={busy}
				></textarea>
			</div>
			<button class="btn primary" type="submit" disabled={busy || !form.layer_key || !form.identity_level}
				>{busy ? 'Creating…' : 'Create operation draft'}</button
			>
		</form>
	{/if}
</Drawer>

<style>
	.stack {
		display: grid;
		gap: 14px;
	}
</style>
