<script lang="ts">
	// F-23: propose a NEW operation from an Operations-step draft. The super_admin fills the operation as the platform's
	// operation editor takes it (F-22); the API checks it with the operation validator (422 `operation_invalid`,
	// 409 `operation_exists`). The proposal is then approved or rejected like a new object.
	import { api, guard } from '$lib/app.svelte';
	import type { GeneratorDraftItem, GeneratorRun } from '$lib/api/types/journey';
	import Drawer from '$lib/ui/Drawer.svelte';
	import { ApiError } from '$lib/api/errors';
	import {
		IDEMPOTENCY,
		IDENTITY_LEVELS,
		MODALITIES,
		SIDE_EFFECTS,
		formFromItem,
		operationErrors,
		operationRefusal,
		toOperationDraft,
		type OperationForm,
	} from '$lib/journey/B8Generator';

	let {
		run: r,
		item,
		open = $bindable(false),
		onproposed,
	}: {
		run: GeneratorRun;
		item: GeneratorDraftItem | null;
		open: boolean;
		onproposed: () => void | Promise<void>;
	} = $props();

	let f = $state<OperationForm>(formFromItem('', ''));
	let serverError = $state('');
	let showErrors = $state(false);
	let seeded = '';
	$effect(() => {
		if (open && item && seeded !== item.id) {
			seeded = item.id;
			f = formFromItem(item.title, item.detail);
			serverError = '';
			showErrors = false;
		}
		if (!open) seeded = '';
	});

	const errs = $derived(operationErrors(f));

	async function submit() {
		showErrors = true;
		serverError = '';
		if (!item || Object.keys(errs).length) return;
		const it = item;
		try {
			const ok = await guard(
				async () => {
					try {
						await api().proposeGeneratorOperation(r.id, it.id, toOperationDraft(f));
					} catch (e) {
						serverError = operationRefusal(e);
						if (e instanceof ApiError && e.status === 403) throw e;
						throw new Error(serverError);
					}
				},
				`Proposed ${f.object}.${f.verb} to the operation catalogue.`,
				'Not proposed',
			);
			if (ok) {
				open = false;
				await onproposed();
			}
		} catch {
			/* guard already toasted */
		}
	}
</script>

<Drawer bind:open title="Propose a new operation">
	<form
		class="stack"
		data-testid="op-form"
		onsubmit={(e) => {
			e.preventDefault();
			void submit();
		}}
	>
		<p class="muted">
			From the draft <span class="mono">{item?.title}</span>. The operation validator checks it; approving the
			proposal adds it to the catalogue.
		</p>
		<div class="row">
			<div class="field">
				<label for="op-object">Object</label>
				<input id="op-object" class="input mono" bind:value={f.object} />
				{#if showErrors && errs.object}<span class="op-err" role="alert">{errs.object}</span>{/if}
			</div>
			<div class="field">
				<label for="op-verb">Verb</label>
				<input id="op-verb" class="input mono" bind:value={f.verb} />
				{#if showErrors && errs.verb}<span class="op-err" role="alert">{errs.verb}</span>{/if}
			</div>
			<div class="field">
				<label for="op-variant">Variant</label>
				<input id="op-variant" class="input mono" bind:value={f.variant} placeholder="optional" />
				{#if showErrors && errs.variant}<span class="op-err" role="alert">{errs.variant}</span>{/if}
			</div>
		</div>
		<div class="row">
			<div class="field">
				<label for="op-modality">Modality</label>
				<select id="op-modality" class="input" bind:value={f.modality}>
					{#each MODALITIES as m (m)}<option value={m}>{m}</option>{/each}
				</select>
			</div>
			<div class="field">
				<label for="op-side">Side effect</label>
				<select id="op-side" class="input" bind:value={f.side_effect}>
					{#each SIDE_EFFECTS as s (s)}<option value={s}>{s}</option>{/each}
				</select>
			</div>
			<div class="field">
				<label for="op-id">Identity level</label>
				<select id="op-id" class="input" bind:value={f.identity_level}>
					{#each IDENTITY_LEVELS as l (l)}<option value={l}>{l}</option>{/each}
				</select>
			</div>
			<div class="field">
				<label for="op-idem">Idempotency</label>
				<select id="op-idem" class="input" bind:value={f.idempotency}>
					{#each IDEMPOTENCY as i (i)}<option value={i}>{i}</option>{/each}
				</select>
			</div>
		</div>
		<label class="row"
			><input type="checkbox" bind:checked={f.confirm_back} /> Confirm back to the customer before running</label
		>
		{#if showErrors && errs.confirm_back}<span class="op-err" role="alert">{errs.confirm_back}</span>{/if}
		<div class="field">
			<label for="op-verify">Verify read</label>
			<input id="op-verify" class="input mono" bind:value={f.verify_read} placeholder="order.read" />
		</div>
		<div class="row">
			<div class="field">
				<label for="op-in">Input fields</label>
				<input id="op-in" class="input mono" bind:value={f.input_fields} placeholder="username, reason" />
			</div>
			<div class="field">
				<label for="op-out">Output fields</label>
				<input id="op-out" class="input mono" bind:value={f.output_fields} placeholder="case_id, state" />
			</div>
		</div>
		<div class="field">
			<label for="op-desc">Description</label>
			<textarea id="op-desc" class="input" rows="2" bind:value={f.description}></textarea>
		</div>
		{#if serverError}<p class="op-err" role="alert" data-testid="op-server-error">{serverError}</p>{/if}
		<div class="row">
			<button class="btn primary" type="submit">Propose operation</button>
			<button class="btn" type="button" onclick={() => (open = false)}>Cancel</button>
		</div>
	</form>
</Drawer>

<style>
	.op-err {
		color: var(--err);
		font-size: 12px;
	}
</style>
