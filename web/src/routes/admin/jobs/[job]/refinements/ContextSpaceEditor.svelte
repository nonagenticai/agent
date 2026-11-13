<script lang="ts">
	// The tenant's version of a context space (from cb refinements/ContextSpaceEditor.svelte@45ee128).
	// `PUT /api/jobs/{job}/context-spaces/{key}`: label, required fields and entities; routing fields stay the release's.
	import { untrack } from 'svelte';
	import { api } from '$lib/app.svelte';
	import { ApiError } from '$lib/api/errors';
	import type { OverlayItem } from '$lib/api/types/release';
	import { toast } from '$lib/ui/toaster.svelte';
	import {
		SPACE_FIELD_LABEL,
		SPACE_ROUTING,
		fmtValue,
		spaceDraft,
		spaceFormErrors,
		spacePatch,
		spaceValues,
		type SpaceDraft,
	} from '$lib/release/refinements';

	let {
		base,
		tenant,
		entities,
		dirty = $bindable(false),
		onsaved,
		oncancel,
	}: {
		/** The release's row (null for a space of the tenant's own). */
		base: OverlayItem | null;
		/** This tenant's row (null when the space uses the base). */
		tenant: OverlayItem | null;
		/** Entity names the space may reference. */
		entities: string[];
		dirty?: boolean;
		onsaved: () => void;
		oncancel: () => void;
	} = $props();

	const current = $derived((tenant ?? base)!);
	let draft = $state<SpaceDraft>(untrack(() => spaceDraft(spaceValues((tenant ?? base)!))));
	const patch = $derived(spacePatch(spaceValues(current), draft));
	const errors = $derived(spaceFormErrors(draft));
	$effect(() => {
		dirty = Object.keys(patch).length > 0;
	});
	const choices = $derived([...new Set([...entities, ...draft.entities])].sort());

	let saving = $state(false);
	let apiError = $state('');
	async function save() {
		if (saving || errors.length || !dirty) return;
		saving = true;
		apiError = '';
		try {
			await api().saveContextSpaceOverride(current.key, patch);
			toast(`Saved your version of ${current.key} to draft — live when a release publishes it.`);
			dirty = false;
			onsaved();
		} catch (e) {
			apiError = e instanceof ApiError ? e.message : 'The change could not be saved.';
		} finally {
			saving = false;
		}
	}
	function toggle(name: string, on: boolean) {
		draft.entities = on ? [...draft.entities, name] : draft.entities.filter((x) => x !== name);
	}
</script>

<form
	class="stack"
	novalidate
	data-testid="context-space-editor"
	onsubmit={(e) => {
		e.preventDefault();
		save();
	}}
>
	<p class="muted">
		Your version replaces the release's for this tenant only once a release publishes it (saved to draft
		first). Revert on the table brings the release's back; a new release that changes this space is flagged as
		a conflict.
	</p>
	<div class="tbl-wrap">
		<table class="tbl dense">
			<thead><tr><th>Field</th><th>Base (release)</th><th>Yours</th></tr></thead>
			<tbody>
				{#each SPACE_ROUTING as f}
					<tr>
						<td>{SPACE_FIELD_LABEL[f]}</td>
						<td class="mono">{fmtValue(base?.[f])}</td>
						<td class="muted" title="Routes the space; changes only with a release"
							><span class="mono">{fmtValue(current[f])}</span> · from the release</td
						>
					</tr>
				{/each}
				<tr>
					<td><label for="cs-label">{SPACE_FIELD_LABEL.label}</label></td>
					<td>{fmtValue(base?.label)}</td>
					<td><input id="cs-label" class="input" bind:value={draft.label} autocomplete="off" /></td>
				</tr>
				<tr>
					<td><label for="cs-required">{SPACE_FIELD_LABEL.required}</label></td>
					<td class="mono">{fmtValue(base?.required)}</td>
					<td>
						<input
							id="cs-required"
							class="input mono"
							bind:value={draft.required}
							placeholder="order_id, reason"
							autocomplete="off"
						/>
						<span class="muted small">Comma separated; what the agent asks before it acts.</span>
					</td>
				</tr>
				<tr>
					<td>{SPACE_FIELD_LABEL.entities}</td>
					<td class="mono">{fmtValue(base?.entities)}</td>
					<td>
						<fieldset class="choices" aria-label={SPACE_FIELD_LABEL.entities}>
							{#each choices as name (name)}
								<label class="choice"
									><input
										type="checkbox"
										checked={draft.entities.includes(name)}
										onchange={(e) => toggle(name, e.currentTarget.checked)}
									/>
									<span class="mono">{name}</span></label
								>
							{:else}<span class="muted">No entities</span>{/each}
						</fieldset>
					</td>
				</tr>
			</tbody>
		</table>
	</div>
	{#if errors.length}
		<div class="callout err" role="alert">
			<strong>Fix these first</strong>
			<span>{errors.join(' ')}</span>
		</div>
	{/if}
	{#if apiError}
		<div class="callout err" role="alert"><strong>Not saved</strong><span>{apiError}</span></div>
	{/if}
	<div class="row">
		<button type="submit" class="btn primary" disabled={saving || !dirty || errors.length > 0}
			>{saving ? 'Saving…' : 'Save my version'}</button
		>
		<button type="button" class="btn" onclick={oncancel}>Cancel</button>
	</div>
</form>

<style>
	.choices {
		border: 0;
		margin: 0;
		padding: 0;
		display: flex;
		flex-wrap: wrap;
		gap: 4px 12px;
	}
	.choice {
		display: inline-flex;
		align-items: center;
		gap: 4px;
	}
	.small {
		display: block;
		font-size: 12px;
	}
	.row {
		display: flex;
		gap: 8px;
	}
</style>
