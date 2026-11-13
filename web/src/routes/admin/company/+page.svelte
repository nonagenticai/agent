<script lang="ts">
	// T14 Company (D124): what the company is, so a tenant picks the platform knowledge that applies. Industry, the
	// departments of that industry, an optional region and a description. It decides which job templates the create-job
	// drawer recommends and the default departments/region of a new job; existing jobs are never rewritten (a job's
	// selection stays the release-managed per-job setting). Any member reads; a tenant admin saves.
	// `PUT /api/tenant/profile` with `expected_updated_at`: 409 `profile_changed` (someone saved first), 409
	// `industry_in_use` (a job works in another industry), 422 `invalid_profile`.
	import { onMount } from 'svelte';
	import { api } from '$lib/app.svelte';
	import { ApiError } from '$lib/api/errors';
	import { session } from '$lib/auth/session.svelte';
	import { canTenantAdmin, writeHint } from '$lib/auth/roles';
	import { Resource } from '$lib/load.svelte';
	import { LANGUAGE_LABEL } from '$lib/api/types/core';
	import type { TenantProfile } from '$lib/api/types/tenant';
	import { toast } from '$lib/ui/toaster.svelte';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Callout from '$lib/ui/Callout.svelte';

	const res = new Resource<TenantProfile>(() => api().tenantProfile());
	onMount(() => res.refresh());
	const mayEdit = $derived(canTenantAdmin(session));

	let industry = $state('unassigned');
	let departments = $state<string[]>([]);
	let region = $state('');
	let description = $state('');
	/** D132.4: the languages a new job serves by default. */
	let languages = $state<string[]>(['en']);
	const LANGS = Object.keys(LANGUAGE_LABEL);
	/** The token the draft was loaded at (sent back as `expected_updated_at`). */
	let loadedAt = $state<string | null>(null);
	let original = $state('');
	let saving = $state(false);
	let error = $state<{ code: string; message: string } | null>(null);

	const snapshot = () =>
		JSON.stringify([industry, [...departments].sort(), region, description.trim(), [...languages].sort()]);
	const dirty = $derived(original !== '' && snapshot() !== original);

	function load(p: TenantProfile) {
		industry = p.industry;
		departments = [...p.departments];
		region = p.region ?? '';
		description = p.description;
		languages = [...(p.languages ?? ['en'])];
		loadedAt = p.updated_at;
		original = snapshot();
	}
	// Load the draft once per fetched profile (a refetch after a save or a 409 reload resets it).
	let loaded: TenantProfile | null = null;
	$effect(() => {
		if (res.value.state === 'ok' && res.value.data !== loaded) {
			loaded = res.value.data;
			load(res.value.data);
		}
	});

	function onIndustry(p: TenantProfile) {
		// A new industry keeps only the departments that still belong to it.
		departments = departments.filter((k) =>
			p.options.departments.some((d) => d.key === k && d.industry === industry),
		);
	}
	function toggle(k: string, on: boolean) {
		departments = on ? [...departments, k] : departments.filter((x) => x !== k);
	}

	async function save(e: SubmitEvent) {
		e.preventDefault();
		if (saving || !mayEdit) return;
		saving = true;
		error = null;
		try {
			const saved = await api().saveTenantProfile({
				industry,
				departments: [...departments],
				region: region || null,
				description: description.trim(),
				languages: [...languages],
				expected_updated_at: loadedAt,
			});
			toast('Company profile saved (audited).');
			loaded = saved;
			load(saved);
		} catch (err) {
			error =
				err instanceof ApiError
					? { code: err.code, message: err.message }
					: { code: 'error', message: String(err) };
		} finally {
			saving = false;
		}
	}
	async function reload() {
		error = null;
		loaded = null;
		await res.refresh();
	}
</script>

<PageHead
	id="T14"
	sub="What your company is. It decides which platform job templates are recommended and what a new job starts with."
/>

<LoadState resource={res}>
	{#snippet children(p)}
		{@const depts = p.options.departments.filter((d) => d.industry === industry)}
		<form class="stack company" data-testid="company-form" novalidate onsubmit={save}>
			{#if !mayEdit}
				<Callout kind="info" title="Read only"
					><span data-testid="company-readonly"
						>{writeHint('tenant_admin')} to change the company profile.</span
					></Callout
				>
			{/if}
			<Card title="Company profile" sub="Used when you create jobs; existing jobs keep their own selection.">
				<fieldset class="stack" disabled={!mayEdit || saving}>
					<label class="field"
						><span class="label">Industry</span>
						<select
							class="input"
							data-testid="company-industry"
							disabled={!mayEdit}
							bind:value={industry}
							onchange={() => onIndustry(p)}
						>
							<option value="unassigned">Not set yet</option>
							{#each p.options.industries as i (i.key)}<option value={i.key}>{i.label}</option>{/each}
						</select></label
					>
					<fieldset class="group" data-testid="company-departments">
						<legend>Departments</legend>
						{#if industry === 'unassigned'}
							<p class="muted small">Choose an industry to see its departments.</p>
						{:else}
							{#each depts as d (d.key)}
								<label class="opt inline"
									><input
										type="checkbox"
										checked={departments.includes(d.key)}
										onchange={(e) => toggle(d.key, e.currentTarget.checked)}
									/>
									{d.label}</label
								>
							{:else}
								<p class="muted small">The platform has no departments for this industry yet.</p>
							{/each}
						{/if}
					</fieldset>
					<label class="field"
						><span class="label">Region</span>
						<select class="input" data-testid="company-region" bind:value={region}>
							<option value="">No region</option>
							{#each p.options.regions as r (r.key)}<option value={r.key}>{r.label}</option>{/each}
						</select></label
					>
					<fieldset class="group" data-testid="company-languages">
						<legend>Languages</legend>
						{#each LANGS as l (l)}
							<label class="opt inline"
								><input
									type="checkbox"
									checked={languages.includes(l)}
									onchange={(e) =>
										(languages = e.currentTarget.checked
											? [...languages, l]
											: languages.filter((x) => x !== l))}
								/>
								{LANGUAGE_LABEL[l]}</label
							>
						{/each}
						<p class="muted small">A new job serves these languages unless you choose others.</p>
					</fieldset>
					<label class="field"
						><span class="label">Description</span>
						<textarea
							class="input"
							rows="3"
							data-testid="company-description"
							bind:value={description}
							placeholder="What the company does, in a sentence or two"></textarea></label
					>
				</fieldset>
			</Card>
			{#if error}
				<Callout kind="err" title="Not saved">
					<span data-testid="company-error"
						><span class="mono">{error.code}</span> — {error.message}
						{#if error.code === 'profile_changed'}
							<button class="btn sm" type="button" onclick={reload}>Reload</button>
						{/if}
					</span>
				</Callout>
			{/if}
			<div class="actions">
				<button
					class="btn primary"
					type="submit"
					disabled={!mayEdit || saving || !dirty || languages.length === 0}
					>{saving ? 'Saving…' : 'Save profile'}</button
				>
			</div>
		</form>
	{/snippet}
</LoadState>

<style>
	.company {
		max-width: 720px;
	}
	fieldset {
		border: 0;
		padding: 0;
		margin: 0;
		min-width: 0;
	}
</style>
