<script lang="ts">
	// Edit `jobs.selection` (K6): profile, industry, departments, areas, channels, region — platform options only
	// (D8). D106/D115: one industry and one or more of its departments, ordered, primary first (DepartmentPicker);
	// the save refuses anything else 422 `hierarchy_incomplete` / `department_industry_mismatch` /
	// `duplicate_department`. D60: a department is the industry_dept layer (extends a base archetype); areas group
	// its context spaces. D59: channel is the real dimension; D111: one optional region (recorded, selects nothing).
	// Residency (tenant) and languages (job setting, J13) are NOT part of the selection.
	// `PUT /api/jobs/{job}/target`, audited `job.selection_update`; 409 selection_changed when someone saved first.
	import { untrack } from 'svelte';
	import { api } from '$lib/app.svelte';
	import { ApiError } from '$lib/api/errors';
	import type { JobTarget } from '$lib/api/types/release';
	import { emptySelection, type Selection } from '$lib/api/types/core';
	import { defaultProfile, profileChoices } from '$lib/tenant/profile-default';
	import { jobPath } from '$lib/scope';
	import DepartmentPicker from '$lib/ui/DepartmentPicker.svelte';
	import { toast } from '$lib/ui/toaster.svelte';

	let {
		target,
		dirty = $bindable(false),
		onsaved,
		oncancel,
	}: { target: JobTarget; dirty?: boolean; onsaved: () => void; oncancel: () => void } = $props();

	const o = $derived(target.options);
	const empty: Selection = emptySelection();
	let draft = $state<Selection>(
		untrack(() => structuredClone($state.snapshot(target.selection ?? empty)) as Selection),
	);
	// D132.3: a selection with no profile yet starts on the D132.1 default (the only profile of its industry,
	// else the only industry-less one); the editor then counts as changed so the pre-selection can be saved.
	const original = untrack(() => JSON.stringify(target.selection ?? empty));
	const profileOptions = $derived(profileChoices(o.profiles, draft.industry));
	$effect(() => {
		if (draft.profile) return;
		const d = defaultProfile(o.profiles, draft.industry);
		if (d) draft.profile = d;
	});
	$effect(() => {
		dirty = JSON.stringify(draft) !== original;
	});

	function toggle(list: string[], v: string, on: boolean): string[] {
		return on ? [...list, v] : list.filter((x) => x !== v);
	}
	// D65: only the chosen industry's departments, only the chosen departments' areas.
	const depts = $derived(o.departments.filter((d) => d.industry === draft.industry));
	const areas = $derived(o.areas.filter((a) => draft.departments.includes(a.industry_dept ?? '')));
	/** A new industry keeps only what still belongs to it. */
	function onIndustry() {
		draft.departments = draft.departments.filter((k) =>
			o.departments.some((d) => d.key === k && d.industry === draft.industry),
		);
		pruneAreas();
	}
	function pruneAreas() {
		draft.areas = draft.areas.filter((k) =>
			o.areas.some((a) => a.key === k && draft.departments.includes(a.industry_dept ?? '')),
		);
	}
	let saving = $state(false);
	let error = $state<{ code: string; message: string } | null>(null);
	async function save() {
		if (saving || !dirty) return;
		saving = true;
		error = null;
		try {
			await api().saveReleaseTarget($state.snapshot(draft) as Selection, target.selection_updated_at);
			toast('Selection saved (audited). Approve a release to use it.');
			dirty = false;
			onsaved();
		} catch (e) {
			error =
				e instanceof ApiError ? { code: e.code, message: e.message } : { code: 'error', message: String(e) };
		} finally {
			saving = false;
		}
	}
</script>

<form
	class="stack"
	novalidate
	data-testid="selection-editor"
	onsubmit={(e) => {
		e.preventDefault();
		save();
	}}
>
	<p class="muted">
		The selection names platform templates only. Saving does not change what runs: the next approved release
		compiles it.
	</p>
	<fieldset class="group">
		<legend>Business-model profile</legend>
		{#each profileOptions.length ? profileOptions : o.profiles as p (p.key)}
			<label class="opt"
				><input type="radio" name="profile" value={p.key} bind:group={draft.profile} />
				<span
					><b>{p.label}</b> <span class="mono muted">{p.key}</span>{#if p.note}<br /><span class="muted small"
							>{p.note}</span
						>{/if}</span
				></label
			>
		{/each}
	</fieldset>
	<div class="field">
		<label for="sel-industry">Industry</label>
		<select id="sel-industry" class="input" bind:value={draft.industry} onchange={onIndustry}>
			{#each o.industries as i (i.key)}<option value={i.key}>{i.label}</option>{/each}
		</select>
	</div>
	<fieldset class="group" data-testid="sel-departments">
		<legend>Departments</legend>
		<p class="muted small hint">
			The departments of this industry the job works in — at least one, in order; the first is the primary.
			Each one specialises a base archetype (customer service, IT service desk, billing…): generic work from
			the archetype, industry specifics on top.
		</p>
		<DepartmentPicker
			options={depts}
			value={draft.departments}
			testid="sel-department-picker"
			onchange={(next) => {
				draft.departments = next;
				pruneAreas();
			}}
		/>
	</fieldset>
	<fieldset class="group" data-testid="sel-dim-area" data-kind="grouping">
		<legend>Areas <span class="mono muted small">area</span></legend>
		<p class="muted small hint">
			Groupings of context spaces inside the chosen departments. Only the chosen areas’ context spaces are
			compiled.
		</p>
		<div class="chips-row">
			{#each areas as a (a.key)}
				<label class="opt inline"
					><input
						type="checkbox"
						checked={draft.areas.includes(a.key)}
						onchange={(e) => (draft.areas = toggle(draft.areas, a.key, e.currentTarget.checked))}
					/>
					{a.label}</label
				>
			{:else}
				<span class="muted small">Pick a department to see its areas.</span>
			{/each}
		</div>
	</fieldset>
	{#each [o.dimensions.channel] as dim (dim.key)}
		<fieldset
			class="group"
			class:placeholder={dim.kind === 'placeholder'}
			data-testid={`sel-dim-${dim.key}`}
			data-kind={dim.kind ?? 'dimension'}
		>
			<legend
				>{dim.label} <span class="mono muted small">{dim.key}</span>{#if dim.kind === 'placeholder'}
					<span class="ph-tag">recorded only</span>{/if}</legend
			>
			{#if dim.note}<p class="muted small hint">{dim.note}</p>{/if}
			<div class="chips-row">
				{#each dim.values as v (v.key)}
					<label class="opt inline" title={v.note}
						><input
							type="checkbox"
							checked={draft.dimensions.channel.includes(v.key)}
							onchange={(e) =>
								(draft.dimensions.channel = toggle(draft.dimensions.channel, v.key, e.currentTarget.checked))}
						/>
						{v.label}{#if v.note}<span class="muted small"> — {v.note}</span>{/if}</label
					>
				{/each}
			</div>
		</fieldset>
	{/each}
	{#each [o.dimensions.region] as dim (dim.key)}
		<fieldset class="group placeholder" data-testid="sel-dim-region" data-kind={dim.kind ?? 'placeholder'}>
			<legend
				>{dim.label} <span class="mono muted small">region</span>
				<span class="ph-tag">one value</span></legend
			>
			<p class="muted small hint">Selects the region sub-layers (D111).</p>
			<div class="chips-row">
				<label class="opt inline"
					><input
						type="radio"
						name="region"
						checked={draft.region === null}
						onchange={() => (draft.region = null)}
					/>
					None</label
				>
				{#each dim.values as v (v.key)}
					<label class="opt inline" title={v.note}
						><input
							type="radio"
							name="region"
							checked={draft.region === v.key}
							onchange={() => (draft.region = v.key)}
						/>
						{v.label}{#if v.note}<span class="muted small"> — {v.note}</span>{/if}</label
					>
				{/each}
			</div>
		</fieldset>
	{/each}
	<div class="outside" data-testid="sel-outside">
		<strong>Not part of the selection</strong>
		<span
			>Data residency — a tenant setting fixed at provisioning (<a href="/admin">Tenant overview</a>).</span
		>
		<span
			>Languages served — a job setting (<a
				href={`${jobPath(target.job)}#job-settings`}
				data-testid="sel-languages-link">edit the job's languages</a
			>).</span
		>
	</div>
	{#if error}
		<div class="callout err" role="alert">
			<strong>Not saved ({error.code})</strong><span>{error.message}</span>
		</div>
	{/if}
	<div class="row">
		<button type="submit" class="btn primary" disabled={saving || !dirty}
			>{saving ? 'Saving…' : 'Save selection'}</button
		>
		<button type="button" class="btn" onclick={oncancel}>Cancel</button>
	</div>
</form>

<style>
	.group {
		border: 1px solid var(--line);
		border-radius: 6px;
		padding: 8px 12px 10px;
		margin: 0;
		display: grid;
		gap: 6px;
	}
	legend {
		font-weight: 600;
		padding: 0 4px;
	}
	.group.placeholder {
		border-style: dashed;
		background: var(--sunk);
	}
	.ph-tag {
		font: 600 10px/1.2 var(--cond);
		letter-spacing: 0.07em;
		text-transform: uppercase;
		color: var(--muted);
		border: 1px dashed var(--faint);
		border-radius: 4px;
		padding: 1px 5px;
		margin-left: 4px;
	}
	.hint {
		margin: 0;
	}
	.outside {
		display: grid;
		gap: 2px;
		font-size: 12.5px;
		padding: 8px 12px;
		border-radius: 6px;
		background: var(--sunk);
	}
	.opt {
		display: flex;
		gap: 8px;
		align-items: flex-start;
	}
	.opt.inline {
		display: inline-flex;
		align-items: center;
		gap: 6px;
	}
	.chips-row {
		display: flex;
		flex-wrap: wrap;
		gap: 6px 16px;
	}
	.small {
		font-size: 12px;
	}
	.row {
		display: flex;
		gap: 8px;
	}
</style>
