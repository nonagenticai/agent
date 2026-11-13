<script lang="ts">
	// T1 Jobs: each job this tenant runs, as one card that answers "where is this job in its lifecycle and what do I do
	// next?". A job is built from platform template layers pinned at tags (base → industry → department → job), shaped by
	// the tenant's selection and refinements, and reaches customers only through a release that passes readiness.
	// A tenant_admin creates a job from a tagged template (D123, owner 2026-10-27; supersedes K11), shaped by the company
	// profile (D124); a tenant_admin can also ask for a template upgrade (K5).
	import { onMount } from 'svelte';
	import { api, app, guard, switchTenant } from '$lib/app.svelte';
	import { toast } from '$lib/ui/toaster.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { canTenantAdmin, writeAttrs } from '$lib/auth/roles';
	import { Resource } from '$lib/load.svelte';
	import { LANGUAGE_LABEL } from '$lib/api/types/core';
	import { defaultProfile, profileChoices } from '$lib/tenant/profile-default';
	import { ApiError } from '$lib/api/errors';
	import { jobPath } from '$lib/scope';
	import { fmt } from '$lib/format';
	import { pinTag, type JobAudience, type Paged } from '$lib/api/types/core';
	import type { JobBoardRow, JobTemplateCatalogEntry, TenantProfile } from '$lib/api/types/tenant';
	import { when } from '$lib/tenant/ui';
	import {
		LAYER_LABEL,
		STAGES,
		jobStage,
		layerName,
		nextStep,
		selectionDims,
		templateStack,
	} from '$lib/tenant/jobs-T1';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import Callout from '$lib/ui/Callout.svelte';

	const res = new Resource<Paged<JobBoardRow>>(() => api().jobBoard());
	onMount(() => res.refresh());
	const sa = $derived(canTenantAdmin(session));

	let open = $state(false);
	let templates = $state<JobTemplateCatalogEntry[]>([]);
	let profile = $state<TenantProfile | null>(null);
	let pick = $state('');
	let key = $state('');
	let label = $state('');
	let departments = $state<string[]>([]);
	let busy = $state(false);
	let loadErr = $state('');
	let loaded = $state(false);
	/** D128: a job starts from scratch on the platform's layers, or from a template when the catalog has any. */
	let mode = $state<'scratch' | 'template'>('scratch');
	let industry = $state('');
	let region = $state('');
	let keyTouched = $state(false);
	/** D129: who the new job serves. */
	let audience = $state<JobAudience>('customers');
	/** The server's refusal of the last create (industry_mismatch, job_exists, departments_not_in_template…). */
	let createErr = $state<{ code: string; message: string } | null>(null);
	/** D132.1: the business profile ('' = let the server pick the only fitting one). */
	let profileKey = $state('');
	let profileTouched = $state(false);
	/** D132.4: the languages the job serves (default = the company's). */
	let jobLanguages = $state<string[]>([]);
	const LANGS = Object.keys(LANGUAGE_LABEL);

	const unassigned = $derived(profile?.industry === 'unassigned');
	async function openCreate() {
		open = true;
		loadErr = '';
		createErr = null;
		loaded = false;
		try {
			[templates, profile] = await Promise.all([api().jobTemplateOptions(), api().tenantProfile()]);
			// Recommended templates sort first, so the first one is the best fit.
			if (!templates.some((t) => t.template === pick)) pick = templates[0]?.template ?? '';
			mode = templates.length ? 'template' : 'scratch';
			industry = profile.industry !== 'unassigned' ? profile.industry : '';
			region = profile.region ?? '';
			jobLanguages = [...(profile.languages ?? ['en'])];
			profileTouched = false;
			profileKey = '';
			scratchDepartments = [...profile.departments];
			keyTouched = false;
			audience = 'customers';
			loaded = true;
		} catch (e) {
			loadErr = (e as Error).message;
		}
	}
	const chosen = $derived(mode === 'template' ? templates.find((t) => t.template === pick) : undefined);
	let scratchDepartments = $state<string[]>([]);
	const industries = $derived(profile?.options.industries ?? []);
	/** The industry's departments, offered as the job's departments. */
	const industryDepartments = $derived(
		(profile?.options.departments ?? []).filter((d) => d.industry === industry),
	);
	const regions = $derived(profile?.options.regions ?? []);
	const allProfiles = $derived(profile?.options.profiles ?? []);
	/** The industry the new job works in: the template's, else the chosen one. */
	const jobIndustry = $derived(mode === 'template' ? (chosen?.industry ?? '') : industry);
	const profileOptions = $derived(profileChoices(allProfiles, jobIndustry));
	const profileDefault = $derived(defaultProfile(allProfiles, jobIndustry));
	/** Ambiguous = the user must choose (the server would answer 422 profile_required). */
	const profileNeeded = $derived(allProfiles.length > 0 && !!jobIndustry && profileDefault === null);
	$effect(() => {
		// Pre-select the D132.1 default until the user chooses; keep a choice that still fits.
		if (profileTouched && profileOptions.some((p) => p.key === profileKey)) return;
		profileTouched = false;
		profileKey = profileDefault ?? '';
	});
	function toggleLang(k: string, on: boolean) {
		jobLanguages = on ? [...jobLanguages, k] : jobLanguages.filter((x) => x !== k);
	}
	const nothingPublished = $derived(loaded && industries.length === 0);
	/** A key suggested from the label: lowercase, underscores, starting with a letter. */
	const slug = (v: string) =>
		v
			.toLowerCase()
			.replace(/[^a-z0-9]+/g, '_')
			.replace(/^_+|_+$/g, '')
			.replace(/^[^a-z]+/, '')
			.slice(0, 63);
	function onLabel() {
		if (!keyTouched) key = slug(label);
	}
	const scratchOk = $derived(
		!!industry && !!slug(key) && !!label.trim() && scratchDepartments.length > 0 && !nothingPublished,
	);
	function toggleScratch(k: string, on: boolean) {
		scratchDepartments = on ? [...scratchDepartments, k] : scratchDepartments.filter((x) => x !== k);
	}
	/** D124.3: a new job's departments default to the template's ∩ the company's, else all the template's. */
	function defaultDepartments(t: JobTemplateCatalogEntry | undefined): string[] {
		if (!t) return [];
		const shared = t.departments.filter((d) => profile?.departments.includes(d));
		return shared.length ? shared : [...t.departments];
	}
	$effect(() => {
		departments = defaultDepartments(chosen);
	});
	const deptLabel = (k: string) => profile?.options.departments.find((d) => d.key === k)?.label ?? k;
	function toggleDept(k: string, on: boolean) {
		departments = on ? [...departments, k] : departments.filter((x) => x !== k);
	}
	async function create() {
		if (mode === 'template' && !chosen) return;
		busy = true;
		createErr = null;
		const t = chosen;
		let made = '';
		try {
			const job = t
				? await api().createJobFromTemplate({
						template: t.template,
						key: key.trim() || undefined,
						label: label.trim() || undefined,
						departments: [...departments],
						audience,
						...(profileKey ? { profile: profileKey } : {}),
						...(region ? { region } : {}),
						languages: [...jobLanguages],
					})
				: await api().createJobBlank({
						key: key.trim(),
						label: label.trim(),
						industry,
						departments: industryDepartments.map((d) => d.key).filter((k) => scratchDepartments.includes(k)),
						...(region ? { region } : {}),
						audience,
						...(profileKey ? { profile: profileKey } : {}),
						languages: [...jobLanguages],
					});
			made = job.key;
		} catch (e) {
			createErr =
				e instanceof ApiError ? { code: e.code, message: e.message } : { code: 'error', message: String(e) };
		}
		busy = false;
		if (made) {
			toast(t ? `Job ${made} created from ${t.template}.` : `Job ${made} created.`);
			open = false;
			key = '';
			label = '';
			// The job switcher and job pages read app.jobs: re-read the scope for the active tenant.
			await switchTenant(app.activeTenant);
			await res.refresh();
		}
	}
	async function requestUpgrade(r: JobBoardRow) {
		await guard(
			() => api().requestTemplateUpgrade(r.job.key),
			`Upgrade to ${r.newer.map(pinTag).join(', ')} requested from the platform admin.`,
			'Not requested',
		);
		await res.refresh();
	}

	const stageIndex = (r: JobBoardRow) => STAGES.findIndex((s) => s.key === jobStage(r));
	const STAGE_VARIANT = { draft: 'muted', ready: 'accent', released: 'warn', serving: 'ok' } as const;
	/** D59/D60 names of the selection keys; region is a placeholder (recorded, selects nothing). */
	const DIM_LABEL: Record<string, string> = {
		area: 'areas',
		channel: 'channels',
		region: 'region (placeholder)',
	};
	const dimLabel = (k: string) => DIM_LABEL[k] ?? k.replaceAll('_', ' ');
	const delta = (now: number | null, prev: number | null | undefined) =>
		now == null || prev == null ? null : Math.round((now - prev) * 1000) / 10;
</script>

<PageHead
	id="T1"
	sub="Every agent job this tenant runs: where it is in its lifecycle and what it needs next."
>
	{#snippet actions()}
		{#if sa}
			<button class="btn primary" type="button" onclick={openCreate}>New job</button>
		{/if}
	{/snippet}
</PageHead>

<section class="t1-concept" aria-label="What a job is">
	<p>
		A <b>job</b> is one agent deployment for one area of work, such as customer care. It is built from
		platform
		<b>template layers</b> pinned at a tag, shaped by your <b>selection</b> and <b>refinements</b>, and
		reaches customers only through a <b>release</b> you approve and apply once readiness passes.
	</p>
	{#if !sa}
		<p class="t1-role muted">
			A tenant admin creates jobs from a template. You configure, release and run them.
		</p>
	{/if}
</section>

<LoadState resource={res}>
	{#snippet empty()}
		<div class="empty" data-testid="no-jobs">
			No job yet — ask a tenant admin to create one.
			{#if sa}<button class="btn sm" type="button" onclick={openCreate}>Create the first job</button>{/if}
		</div>
	{/snippet}
	{#snippet children(page)}
		<div class="t1-board" data-testid="job-board">
			{#each page.items as r (r.job.key)}
				{@const stage = jobStage(r)}
				{@const si = stageIndex(r)}
				{@const step = nextStep(r)}
				{@const stack = templateStack(r)}
				{@const dims = selectionDims(r)}
				{@const d = delta(r.containment_7d, r.live?.containment_prev_7d)}
				<article class="t1-job" data-job={r.job.key}>
					<header class="t1-head">
						<div>
							<h2><a href={jobPath(r.job.key)}>{r.job.label}</a></h2>
							<span class="mono t1-key">{r.job.key}</span>
							<span class="t1-ns">North star: <b>{r.job.north_star}</b></span>
							{#if r.job.languages?.length}
								<span class="t1-ns" data-testid="t1-languages"
									>Serves: <b>{r.job.languages.map((l) => LANGUAGE_LABEL[l] ?? l).join(', ')}</b></span
								>
							{/if}
						</div>
						<Chip text={STAGES[si]!.label.toLowerCase()} variant={STAGE_VARIANT[stage]} />
					</header>

					<ol class="t1-track" aria-label={`Lifecycle of ${r.job.label}`}>
						{#each STAGES as s, i (s.key)}
							<li
								class:done={i < si}
								class:now={i === si}
								aria-current={i === si ? 'step' : undefined}
								title={s.hint}
							>
								<span class="dot" aria-hidden="true"></span><b>{s.label}</b>
								<span class="t1-hint">{s.hint}</span>
							</li>
						{/each}
					</ol>

					<div class="t1-next {step.tone}" data-testid="t1-next">
						<span><b>Next:</b> {step.text}</span>
						<a class="btn sm" href={step.href}>{step.cta}</a>
					</div>

					<div class="t1-grid">
						<section aria-label="Built from">
							<h3>Built from</h3>
							<ol class="t1-stack" aria-label="Template layers, most specific first">
								{#each [...stack].reverse() as l (l.pin.layer)}
									<li class="t1-layer {l.kind}" class:behind={!!l.latest}>
										<span class="t1-kind">{LAYER_LABEL[l.kind]}</span>
										<span class="t1-lname mono" title={l.pin.layer}>{layerName(l.pin.layer)}</span>
										<span class="t1-tag mono">v{l.pin.version}</span>
										{#if l.latest}<span class="t1-newer mono" title={`Newer tag ${pinTag(l.latest)}`}
												>→ v{l.latest.version}</span
											>{/if}
									</li>
								{/each}
							</ol>
							<p class="t1-cap">Later layers override earlier ones; pins move only when re-pinned.</p>
							{#if r.newer.length}
								<div class="t1-upgrade">
									<span class="warn-text">Newer template: {r.newer.map(pinTag).join(', ')}</span>
									{#if r.upgrade_requested_at}
										<Chip
											text={`upgrade requested ${when(r.upgrade_requested_at).slice(0, 10)}`}
											variant="accent"
										/>
									{:else}
										<button
											class="btn sm"
											type="button"
											{...writeAttrs(session, 'tenant_admin')}
											onclick={() => requestUpgrade(r)}>Request upgrade</button
										>
									{/if}
								</div>
							{/if}
							<h4>Selection</h4>
							{#if r.selection}
								<p class="t1-sel">
									<span class="mono">{r.selection.profile}</span> · {r.selection.industry} · departments {r.selection.departments.join(
										', ',
									)}
								</p>
								<dl class="t1-dims">
									{#each dims as [k, v] (k)}
										<dt>{dimLabel(k)}</dt>
										<dd>
											{#each v as x (x)}<Chip text={x} />{/each}
										</dd>
									{/each}
								</dl>
							{:else}
								<p class="muted">No selection yet.</p>
							{/if}
							<a class="t1-link" href={jobPath(r.job.key, '/refinements')}
								>{r.job.draft_rev} tenant refinement{r.job.draft_rev === 1 ? '' : 's'}</a
							>
						</section>

						<section aria-label="Release">
							<h3>Release</h3>
							<div class="t1-rel">
								<span class="t1-k">Current</span>
								{#if r.job.current_release}
									<span>
										<a class="mono" href={jobPath(r.job.key, `/release/${r.job.current_release.id}`)}
											>{r.job.current_release.id}</a
										>
										<span class="cell-sub mono">{r.job.current_release.hash} · {when(r.last_release_at)}</span
										>
									</span>
								{:else}
									<span class="muted">No release yet</span>
								{/if}
								{#if r.approved_waiting}
									<span class="t1-k">Approved</span>
									<span>
										<a class="mono" href={jobPath(r.job.key, `/release/${r.approved_waiting.id}`)}
											>{r.approved_waiting.id}</a
										>
										<span class="cell-sub">waiting to be applied</span>
									</span>
								{/if}
							</div>
							<h4>Readiness</h4>
							{#if r.readiness.ready}
								<Chip text="ready" variant="ok" />
								<p class="t1-cap">Every check passes; the next release can be applied.</p>
							{:else}
								<a href={jobPath(r.job.key, '/readiness')}
									><Chip text={`${r.readiness.missing.length} missing`} variant="warn" /></a
								>
								<ul class="t1-gaps">
									{#each r.readiness.missing as m (m)}<li>{m}</li>{/each}
								</ul>
							{/if}
						</section>

						<section aria-label="Live">
							<h3>Live <span class="t1-h-sub">last 7 days</span></h3>
							{#if r.cases_7d === 0 && !r.live}
								<p class="muted">No cases yet. Numbers appear once agents serve a release.</p>
							{:else}
								<dl class="t1-stats">
									<div>
										<dt>Cases</dt>
										<dd><a href={jobPath(r.job.key, '/cases')}>{fmt.n(r.cases_7d)}</a></dd>
									</div>
									<div>
										<dt>Containment</dt>
										<dd>
											{r.containment_7d == null ? '—' : fmt.pct(r.containment_7d, 0)}
											{#if d != null}<small class:up={d >= 0} class:down={d < 0}
													>{d >= 0 ? '▲' : '▼'} {Math.abs(d)} pt</small
												>{/if}
										</dd>
									</div>
									{#if r.live}
										<div>
											<dt>Agents online</dt>
											<dd class:zero={r.live.agents_online === 0}>{r.live.agents_online}</dd>
										</div>
										<div>
											<dt>Open cases</dt>
											<dd>{fmt.n(r.live.open_cases)}</dd>
										</div>
										<div>
											<dt>Handovers</dt>
											<dd>{fmt.n(r.live.handovers_7d)}</dd>
										</div>
										<div>
											<dt>Last case</dt>
											<dd class="mono t1-small">{when(r.live.last_case_at)}</dd>
										</div>
									{/if}
								</dl>
								<a class="t1-link" href={jobPath(r.job.key, '/analytics')}>Analytics</a>
							{/if}
						</section>
					</div>
				</article>
			{/each}
		</div>
		<p class="note">
			A newer template tag reaches a job only when the platform admin re-pins it and you cut and apply a new
			release.
		</p>
	{/snippet}
</LoadState>

<Drawer bind:open title="Create job">
	{#snippet sub()}<span
			>Tenant admin · in {app.tenants.find((t) => t.id === app.activeTenant)?.name ?? 'this tenant'}</span
		>{/snippet}
	{#if loadErr}
		<Callout kind="err" title="Templates unavailable"><span>{loadErr}</span></Callout>
	{:else if !loaded}
		<p class="muted">Loading templates…</p>
	{:else}
		<div class="stack" data-testid="create-job">
			{#if unassigned}
				<Callout kind="info" title="Your company industry is not set">
					<span data-testid="industry-unassigned"
						>Tell us what your company is on <a href="/admin/company">Company</a> to see which templates fit. Until
						then the first job you create sets the industry.</span
					>
				</Callout>
			{/if}
			{#if nothingPublished}
				<Callout kind="info" title="Nothing to build on yet">
					<span data-testid="nothing-published">No industries are published on this platform yet</span>
				</Callout>
			{/if}
			{#if templates.length}
				<div class="seg" role="group" aria-label="How to start">
					<button
						type="button"
						class="tab"
						class:on={mode === 'scratch'}
						aria-pressed={mode === 'scratch'}
						data-testid="mode-scratch"
						onclick={() => (mode = 'scratch')}>From scratch</button
					>
					<button
						type="button"
						class="tab"
						class:on={mode === 'template'}
						aria-pressed={mode === 'template'}
						data-testid="mode-template"
						onclick={() => (mode = 'template')}>From a template</button
					>
				</div>
			{/if}
			{#if mode === 'template'}
				<label class="field"
					><span class="label">Template</span>
					<select class="input" bind:value={pick} data-testid="template-select">
						{#each templates as t (t.template)}
							<option value={t.template}
								>{t.recommended ? '★ ' : ''}{t.label} — {t.template}{t.recommended
									? ' (Recommended for your company)'
									: ''}</option
							>
						{/each}
					</select></label
				>
				{#if chosen}
					<p class="muted" data-testid="template-description">
						{chosen.description}
						{#if chosen.recommended}<Chip text="Recommended for your company" variant="ok" />{/if}
					</p>
					<fieldset class="group" data-testid="job-departments">
						<legend>Departments</legend>
						{#each chosen.departments as d (d)}
							<label class="opt inline"
								><input
									type="checkbox"
									checked={departments.includes(d)}
									onchange={(e) => toggleDept(d, e.currentTarget.checked)}
								/>
								{deptLabel(d)}</label
							>
						{/each}
					</fieldset>
				{/if}
				<label class="field"
					><span class="label">Key (optional)</span><input
						class="input mono"
						bind:value={key}
						placeholder={chosen?.key ?? ''}
					/>
					<span class="muted small"
						>Leave empty to use the template key; a taken key becomes <span class="mono">_2</span>,
						<span class="mono">_3</span>…</span
					></label
				>
				<label class="field"
					><span class="label">Label (optional)</span><input
						class="input"
						bind:value={label}
						placeholder={chosen?.label ?? ''}
					/></label
				>
			{:else}
				<label class="field"
					><span class="label">Label</span><input
						class="input"
						bind:value={label}
						oninput={onLabel}
						data-testid="scratch-label"
					/></label
				>
				<label class="field"
					><span class="label">Key</span><input
						class="input mono"
						bind:value={key}
						oninput={() => (keyTouched = true)}
						data-testid="scratch-key"
					/></label
				>
				{#if industries.length}
					<label class="field"
						><span class="label">Industry</span>
						{#if unassigned}
							<select class="input" bind:value={industry} data-testid="scratch-industry">
								<option value="">Choose an industry…</option>
								{#each industries as i (i.key)}<option value={i.key}>{i.label}</option>{/each}
							</select>
						{:else}
							<input
								class="input"
								value={industries.find((i) => i.key === industry)?.label ?? industry}
								disabled
								data-testid="scratch-industry"
							/>
						{/if}</label
					>
					<fieldset class="group" data-testid="scratch-departments">
						<legend>Departments</legend>
						{#each industryDepartments as d (d.key)}
							<label class="opt inline"
								><input
									type="checkbox"
									checked={scratchDepartments.includes(d.key)}
									onchange={(e) => toggleScratch(d.key, e.currentTarget.checked)}
								/>
								{d.label}</label
							>
						{:else}
							<span class="muted"
								>{industry
									? 'This industry has no departments published.'
									: 'Choose an industry first.'}</span
							>
						{/each}
					</fieldset>
					{#if regions.length}
						<label class="field"
							><span class="label">Region</span>
							<select class="input" bind:value={region} data-testid="scratch-region">
								<option value="">No region</option>
								{#each regions as r (r.key)}<option value={r.key}>{r.label}</option>{/each}
							</select></label
						>
					{/if}
				{/if}
			{/if}
			{#if loaded && allProfiles.length && jobIndustry}
				<label class="field"
					><span class="label">Business profile{profileNeeded ? ' (required)' : ''}</span>
					<select
						class="input"
						data-testid="job-profile"
						bind:value={profileKey}
						onchange={() => (profileTouched = true)}
					>
						{#if profileNeeded}<option value="">Choose a profile…</option>{/if}
						{#each profileOptions as p (p.key)}<option value={p.key}>{p.label}</option>{/each}
					</select></label
				>
			{/if}
			{#if loaded && mode === 'template' && regions.length}
				<label class="field"
					><span class="label">Region</span>
					<select class="input" bind:value={region} data-testid="job-region">
						<option value="">No region</option>
						{#each regions as r (r.key)}<option value={r.key}>{r.label}</option>{/each}
					</select>
					<span class="muted small">Selects the region sub-layers of the knowledge.</span></label
				>
			{/if}
			{#if loaded}
				<fieldset class="group" data-testid="job-languages-pick">
					<legend>Languages served</legend>
					{#each LANGS as l (l)}
						<label class="opt inline"
							><input
								type="checkbox"
								checked={jobLanguages.includes(l)}
								onchange={(e) => toggleLang(l, e.currentTarget.checked)}
							/>
							{LANGUAGE_LABEL[l]}</label
						>
					{/each}
				</fieldset>
			{/if}
			{#if loaded}
				<fieldset class="group" data-testid="job-audience">
					<legend>Who does this job serve?</legend>
					<label class="opt inline"
						><input type="radio" name="audience" value="customers" bind:group={audience} /> Customers</label
					>
					<label class="opt inline"
						><input type="radio" name="audience" value="employees" bind:group={audience} /> Employees</label
					>
				</fieldset>
			{/if}
			{#if createErr}
				<Callout kind="err" title="Job not created">
					<span data-testid="create-error"
						><span class="mono">{createErr.code}</span> — {createErr.message}</span
					>
				</Callout>
			{/if}
			<p class="muted">
				The job pins <span class="mono">base</span>, your industry and departments{mode === 'template'
					? ' and the chosen template tag'
					: ''} at their newest tags. It starts as a draft with no release; select, approve and apply one.
			</p>
			<div class="actions">
				<button class="btn" type="button" onclick={() => (open = false)}>Cancel</button>
				<button
					class="btn primary"
					type="button"
					disabled={busy ||
						(profileNeeded && !profileKey) ||
						jobLanguages.length === 0 ||
						(mode === 'template' ? !chosen || departments.length === 0 : !scratchOk)}
					onclick={create}>{busy ? 'Creating…' : 'Create job'}</button
				>
			</div>
		</div>
	{/if}
</Drawer>

<style>
	.warn-text {
		color: var(--warn);
	}
	.t1-concept {
		background: var(--panel);
		border: 1px solid var(--line);
		border-radius: var(--r);
		padding: 14px 16px;
		margin-bottom: 16px;
	}
	.t1-concept > p {
		margin: 0;
		max-width: 80ch;
		line-height: 1.5;
	}
	.t1-role {
		margin: 10px 0 0;
		font-size: 13px;
	}

	.t1-board {
		display: grid;
		gap: 16px;
	}
	.t1-job {
		background: var(--panel);
		border: 1px solid var(--line);
		border-radius: var(--r);
		padding: 16px;
		min-width: 0;
	}
	.t1-head {
		display: flex;
		justify-content: space-between;
		align-items: flex-start;
		gap: 12px;
	}
	.t1-head h2 {
		margin: 0;
		font-size: 18px;
	}
	.t1-key {
		color: var(--muted);
		font-size: 12px;
		margin-right: 10px;
	}
	.t1-ns {
		font-size: 12px;
		color: var(--muted);
	}

	.t1-track {
		list-style: none;
		padding: 0;
		margin: 14px 0 12px;
		display: grid;
		grid-template-columns: repeat(4, minmax(0, 1fr));
	}
	.t1-track li {
		position: relative;
		font-size: 12px;
		color: var(--faint);
		padding-top: 18px;
	}
	.t1-track li::before {
		content: '';
		position: absolute;
		top: 6px;
		left: 0;
		right: 0;
		height: 2px;
		background: var(--line);
	}
	.t1-track li.done::before {
		background: var(--accent);
	}
	.t1-track li:last-child::before {
		display: none;
	}
	.t1-track b {
		display: block;
		font-weight: inherit;
	}
	.t1-hint {
		display: block;
		font-size: 11px;
		font-weight: 400;
		color: var(--faint);
		padding-right: 10px;
		line-height: 1.35;
	}
	.t1-track .dot {
		position: absolute;
		top: 1px;
		left: 0;
		width: 12px;
		height: 12px;
		border-radius: 50%;
		background: var(--panel);
		border: 2px solid var(--line);
	}
	.t1-track li.done {
		color: var(--muted);
	}
	.t1-track li.done .dot {
		background: var(--accent);
		border-color: var(--accent);
	}
	.t1-track li.now {
		color: var(--fg);
		font-weight: 600;
	}
	.t1-track li.now .dot {
		border-color: var(--accent);
		box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent) 25%, transparent);
	}

	.t1-next {
		display: flex;
		justify-content: space-between;
		align-items: center;
		gap: 12px;
		flex-wrap: wrap;
		padding: 10px 12px;
		border-radius: var(--r);
		border-left: 4px solid var(--accent);
		background: color-mix(in srgb, var(--accent) 8%, transparent);
		font-size: 13.5px;
	}
	.t1-next.warn {
		border-left-color: var(--warn);
		background: color-mix(in srgb, var(--warn) 10%, transparent);
	}
	.t1-next.ok {
		border-left-color: var(--ok);
		background: color-mix(in srgb, var(--ok) 9%, transparent);
	}

	.t1-grid {
		display: grid;
		grid-template-columns: 1.3fr 1fr 1fr;
		gap: 20px;
		margin-top: 16px;
	}
	.t1-grid > section {
		min-width: 0;
	}
	.t1-grid h3 {
		margin: 0 0 8px;
		font: 600 11px/1.2 var(--cond);
		letter-spacing: 0.08em;
		text-transform: uppercase;
		color: var(--muted);
	}
	.t1-h-sub {
		text-transform: none;
		letter-spacing: 0;
		font-weight: 400;
		color: var(--faint);
	}
	.t1-grid h4 {
		margin: 14px 0 6px;
		font-size: 12px;
		color: var(--muted);
		font-weight: 600;
	}

	.t1-stack {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 3px;
	}
	.t1-layer {
		display: grid;
		grid-template-columns: 82px minmax(0, 1fr) auto auto;
		align-items: center;
		gap: 8px;
		padding: 5px 8px;
		border-radius: 4px;
		border: 1px solid var(--line-2);
		background: var(--sunk);
		font-size: 12px;
	}
	.t1-layer.job {
		margin: 0 18px;
		background: color-mix(in srgb, var(--accent) 10%, var(--panel));
	}
	.t1-layer.department {
		margin: 0 12px;
	}
	.t1-layer.industry {
		margin: 0 6px;
	}
	.t1-layer.behind {
		border-color: color-mix(in srgb, var(--warn) 55%, transparent);
	}
	.t1-kind {
		color: var(--muted);
		font-size: 11px;
		text-transform: uppercase;
		letter-spacing: 0.05em;
	}
	.t1-lname {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.t1-tag {
		font-weight: 600;
	}
	.t1-newer {
		color: var(--warn);
		font-weight: 600;
	}
	.t1-cap {
		font-size: 11.5px;
		color: var(--faint);
		margin: 6px 0 0;
	}
	.t1-upgrade {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 8px;
		margin-top: 8px;
		font-size: 12.5px;
	}
	.t1-sel {
		margin: 0 0 6px;
		font-size: 12.5px;
	}
	.t1-dims {
		display: grid;
		grid-template-columns: auto 1fr;
		gap: 4px 10px;
		margin: 0 0 8px;
		font-size: 12px;
	}
	.t1-dims dt {
		color: var(--muted);
		text-transform: capitalize;
		padding-top: 3px;
	}
	.t1-dims dd {
		margin: 0;
		display: flex;
		flex-wrap: wrap;
		gap: 4px;
	}
	.t1-link {
		font-size: 12.5px;
	}

	.t1-rel {
		display: grid;
		grid-template-columns: auto minmax(0, 1fr);
		gap: 6px 12px;
		font-size: 13px;
		align-items: baseline;
	}
	.t1-k {
		color: var(--muted);
		font-size: 12px;
	}
	.t1-gaps {
		margin: 8px 0 0;
		padding-left: 18px;
		font-size: 12.5px;
		display: grid;
		gap: 3px;
	}

	.t1-stats {
		display: grid;
		grid-template-columns: repeat(2, minmax(0, 1fr));
		gap: 10px 14px;
		margin: 0 0 10px;
	}
	.t1-stats dt {
		font-size: 11.5px;
		color: var(--muted);
	}
	.t1-stats dd {
		margin: 2px 0 0;
		font-size: 18px;
		font-weight: 600;
		font-variant-numeric: tabular-nums;
	}
	.t1-stats dd small {
		font-size: 11px;
		font-weight: 500;
		margin-left: 4px;
	}
	.t1-stats dd small.up {
		color: var(--ok);
	}
	.t1-stats dd small.down {
		color: var(--err);
	}
	.t1-stats dd.zero {
		color: var(--err);
	}
	.t1-stats dd.t1-small {
		font-size: 12px;
		font-weight: 500;
	}

	@media (max-width: 1100px) {
		.t1-grid {
			grid-template-columns: 1fr 1fr;
		}
		.t1-grid > section:first-child {
			grid-column: 1 / -1;
		}
	}
	@media (max-width: 640px) {
		.t1-grid {
			grid-template-columns: 1fr;
		}
		.t1-hint {
			display: none;
		}
		.t1-layer {
			grid-template-columns: 70px minmax(0, 1fr) auto auto;
		}
		.t1-layer.job,
		.t1-layer.department,
		.t1-layer.industry {
			margin: 0;
		}
	}
</style>
