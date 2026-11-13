<script lang="ts">
	// P5 Compile preview — "what would a job built from this selection actually run, and why?". The page leads with
	// the compiled layer stack (base → business-model rules → industry → department → job), then the places where a
	// later layer overrode an earlier one, then the difference against a reference (a running job's release
	// selection), then the full effective model with per-item provenance. Read-only (compile never writes).
	// Hide/Unhide is deleted (07 §4.3, D8: hiding is a tenant refinement, not a platform preview action). A failed
	// compile lists every error (422 compile_failed, never first-fail), each linking to its row in /platform/layers.
	import { onMount } from 'svelte';
	import { page } from '$app/state';
	import { api } from '$lib/app.svelte';
	import { CompileFailed } from '$lib/api/areas/platform';
	import type {
		CompileError,
		CompileOptions,
		CompileReference,
		CompileTarget,
		EffectiveConfig,
	} from '$lib/api/types/platform';
	import { departmentKey, emptySelection, selectionValues, withSelectionValues } from '$lib/api/types/core';
	import DepartmentPicker from '$lib/ui/DepartmentPicker.svelte';
	import PageHead from '$lib/ui/PageHead.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Tabs from '$lib/ui/Tabs.svelte';
	import JsonView from '$lib/ui/JsonView.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';
	import Provenance from '$lib/platform/Provenance.svelte';
	import {
		COMPILE_KINDS,
		countsOf,
		diffConfigs,
		fieldsLine,
		itemsOf,
		kindLabel,
		overridesOf,
		selectionFromQuery,
		stackOf,
		type CompileKind,
	} from '$lib/platform/compileDiff';
	import { layerLabel } from '$lib/platform/model';
	import LayerStack from './LayerStack.svelte';

	/** What the page opens on: Demo Retail's approved selection, compared with what it runs today. */
	const DEFAULT_SELECTION = 'demo-retail-approved';
	const DEFAULT_REFERENCE = 'demo-retail-live';

	let options = $state<CompileOptions | null>(null);
	let references = $state<CompileReference[]>([]);
	let optionsError = $state('');
	let industry = $state('');
	let depts = $state<string[]>([]);
	let areas = $state<string[]>([]);
	let job = $state('');
	let profile = $state('');
	let dims = $state<Record<string, string[]>>({});
	let refKey = $state('');
	/** The selection came from a link (another page preselected it), not the default. */
	let deepLinked = $state(false);

	let result = $state<EffectiveConfig | null>(null);
	let compiledSel = $state('');
	let errors = $state<CompileError[]>([]);
	let error = $state('');
	let compiling = $state(false);
	let seq = 0;

	let reference = $state<EffectiveConfig | null>(null);
	let referenceError = $state('');
	let refSeq = 0;

	let tab = $state<string>('entity');
	let search = $state('');
	let onlyTouched = $state(false);

	/** The form as a compile target (D115 selection: one industry, ordered departments, primary first; several
	 *  channels; one region). */
	function current(): CompileTarget {
		let selection = {
			...emptySelection(),
			profile: profile || null,
			industry,
			departments: [...depts],
			areas: [...areas],
		};
		for (const [k, v] of Object.entries(dims))
			if (v.length) selection = withSelectionValues(selection, k, [...v]);
		return { selection, job: job || null };
	}
	const selectionKey = $derived(JSON.stringify(current()));
	const stale = $derived(!!result && compiledSel !== selectionKey);

	/** The chosen industry's departments, keyed by their layer key (`industry_dept/<industry>/<dept>`). */
	const departments = $derived(
		(options?.departments ?? [])
			.filter((d) => d.industry === industry)
			.map((d) => ({ ...d, key: departmentKey(d.industry, d.key) })),
	);
	/** In declared order (primary first). */
	const chosenDepts = $derived(
		depts.map((k) => departments.find((d) => d.key === k)).filter((d) => d !== undefined),
	);
	const ref = $derived(references.find((r) => r.key === refKey) ?? null);

	const stack = $derived(result ? stackOf(result) : []);
	const overrides = $derived(result ? overridesOf(result) : []);
	const counts = $derived(result ? countsOf(result) : null);
	const total = $derived(counts ? Object.values(counts).reduce((a, b) => a + b, 0) : 0);
	const diff = $derived(result && reference ? diffConfigs(reference, result) : null);
	const touched = $derived(new Set(overrides.map((o) => `${o.kind}:${o.key}`)));

	const tabItems = $derived<[string, string, number?][]>(
		COMPILE_KINDS.map(([k, l]) => [k, l, counts ? counts[k] : undefined]),
	);
	const entries = $derived(
		result
			? Object.entries(itemsOf(result, tab as CompileKind)).filter(
					([k]) =>
						k.toLowerCase().includes(search.trim().toLowerCase()) &&
						(!onlyTouched || (result!.provenance[`${tab}:${k}`]?.length ?? 0) > 1),
				)
			: [],
	);

	onMount(async () => {
		try {
			[options, references] = await Promise.all([api().compileOptions(), api().compileReferences()]);
		} catch (e) {
			optionsError = (e as Error).message;
			return;
		}
		// A deep link (`?profile=…&industry=…&job=…&channel=…`, from Business models or Layers) preselects; without
		// one the page opens on Demo Retail's approved selection. `?ref=<reference key>` picks the comparison.
		const q = page.url.searchParams;
		const linked = selectionFromQuery(
			q,
			options.dimensions.map((d) => d.key),
		);
		const start = references.find((r) => r.key === DEFAULT_SELECTION);
		if (linked) {
			load(linked);
			deepLinked = true;
		} else if (start) load(start);
		const wantRef = q.get('ref') ?? DEFAULT_REFERENCE;
		if (references.some((r) => r.key === wantRef)) {
			refKey = wantRef;
			void compileReference();
		}
		await compile();
	});

	function load(t: CompileTarget) {
		const s = t.selection;
		industry = s.industry;
		depts = [...s.departments];
		areas = [...s.areas];
		job = t.job ?? '';
		profile = s.profile ?? '';
		// Every dimension gets a list ([] = Any).
		dims = Object.fromEntries(
			(options?.dimensions ?? []).map((d) => [d.key, [...selectionValues(s, d.key)]]),
		);
	}

	function resetToDefault() {
		const start = references.find((r) => r.key === DEFAULT_SELECTION);
		if (start) load(start);
		deepLinked = false;
		void compile();
	}

	function onIndustry() {
		depts = [];
		areas = [];
	}
	function toggle(list: string[], v: string, on: boolean): string[] {
		return on ? [...list, v] : list.filter((x) => x !== v);
	}
	function onDepts(next: string[]) {
		depts = next;
		const keep = new Set(
			departments.filter((d) => depts.includes(d.key)).flatMap((d) => d.areas.map((a) => a.key)),
		);
		areas = areas.filter((a) => keep.has(a));
	}

	async function compile(e?: SubmitEvent) {
		e?.preventDefault();
		const mine = ++seq;
		const sel = current();
		compiling = true;
		error = '';
		errors = [];
		try {
			const out = await api().compilePreview(sel);
			if (mine !== seq) return;
			result = out;
			compiledSel = JSON.stringify(sel);
		} catch (err) {
			if (mine !== seq) return;
			result = null;
			error = (err as Error).message;
			errors = err instanceof CompileFailed ? err.errors : [];
		} finally {
			if (mine === seq) compiling = false;
		}
	}

	async function compileReference() {
		const mine = ++refSeq;
		reference = null;
		referenceError = '';
		const r = references.find((x) => x.key === refKey);
		if (!r) return;
		try {
			const out = await api().compilePreview({ selection: r.selection, job: r.job });
			if (mine === refSeq) reference = out;
		} catch (err) {
			if (mine === refSeq) referenceError = (err as Error).message;
		}
	}

	function golden() {
		industry = 'retail';
		depts = ['industry_dept/retail/customer_service'];
		areas = ['faq', 'logistics', 'order-support', 'payments'];
		job = 'customer_care_agent';
		profile = 'retail_ecommerce';
		dims = { channel: ['chat'], region: ['eu'] };
	}
	// The customer care job on utilities billing: its faq-policies patch targets the retail customer service
	// department's context space, which utilities does not have (patch_target_missing). (Before seed v3 this was the
	// IT desk on retail; KH T2 moved internal IT and Asset to base, so that combination compiles now.)
	function invalid() {
		industry = 'utilities';
		depts = ['industry_dept/utilities/billing'];
		areas = [];
		job = 'customer_care_agent';
		profile = 'electricity_residential';
		dims = {};
	}

	function detailOf(v: Record<string, unknown>): string {
		const parts = [v.label, v.data_type, v.cardinality, v.area && `area ${v.area}`]
			.filter(Boolean)
			.map(String);
		if (Array.isArray(v.uses_slots)) parts.push(`slots: ${v.uses_slots.join(', ')}`);
		if (Array.isArray(v.implements)) parts.push(`${v.implements.length} operations`);
		if (Array.isArray(v.kinds)) parts.push(`${v.kinds.length} knowledge kinds`);
		return parts.join(' · ');
	}

	const OUTCOME = {
		patched: ['Patched', 'tool'],
		removed: ['Removed', 'err'],
		cascade: ['Removed with parent', 'warn'],
	} as const;
	const CHANGE = { added: ['+', 'ok'], removed: ['−', 'err'], changed: ['~', 'tool'] } as const;
</script>

<PageHead
	id="P5"
	sub="What would a job built from this selection actually run — and why? The compile stacks layers in order; each can add, patch or remove what the layers before it set. Every item keeps the trail of rows that built it. Previewing never changes anything."
>
	{#snippet actions()}<Chip text="Read-only preview" />{/snippet}
</PageHead>

<div class="compile">
	<div class="side-col">
		<Card title="Selection" sub="What a job picks. Standard values only (D8).">
			{#if deepLinked && !optionsError}<p class="hint linked" data-testid="compile-linked">
					Preselected by the link you followed.
					<button class="table-action" type="button" onclick={resetToDefault}>Start from the default</button>
				</p>{/if}
			{#if optionsError}
				<Unavailable reason={optionsError} />
			{:else}
				<form class="stack" onsubmit={compile}>
					<div class="field">
						<label for="industry">Industry</label>
						<select
							id="industry"
							class="input"
							bind:value={industry}
							onchange={onIndustry}
							disabled={!options}
						>
							<option value="">Base model only (preview)</option>
							{#each options?.industries ?? [] as v (v.key)}<option value={v.key}>{v.label}</option>{/each}
						</select>
					</div>
					<fieldset class="areas" data-testid="compile-departments" disabled={!industry}>
						<legend class="label">Departments (in order, primary first)</legend>
						{#if industry}
							<DepartmentPicker
								options={departments}
								value={depts}
								testid="compile-department-picker"
								onchange={onDepts}
							/>
						{:else}
							<span class="note">Pick an industry.</span>
						{/if}
						<span class="note"
							>A job needs its industry and at least one department (D106). Without both, only the base-only
							preview compiles (no job, no profile).</span
						>
					</fieldset>
					{#each chosenDepts as d (d.key)}
						<fieldset class="areas" data-testid={`compile-areas-${d.key}`}>
							<legend class="label">Areas in {d.label} (groupings of context spaces)</legend>
							{#each d.areas as a (a.key)}
								<label class="row"><input type="checkbox" bind:group={areas} value={a.key} />{a.label}</label>
							{/each}
							<span class="note">None ticked = every area.</span>
						</fieldset>
					{/each}
					<div class="field">
						<label for="job">Job</label>
						<select id="job" class="input" bind:value={job} disabled={!options}>
							<option value="">Ontology only · no job slice</option>
							{#each options?.jobs ?? [] as j (j.key)}<option value={j.key}>{j.label}</option>{/each}
						</select>
					</div>
					<div class="field">
						<label for="profile">Business model (profile)</label>
						<select id="profile" class="input" bind:value={profile} disabled={!options}>
							<option value="">No profile</option>
							{#each options?.profiles ?? [] as p (p.key)}<option value={p.key}
									>{p.label}{p.industry ? ` · ${p.industry}` : ''}</option
								>{/each}
						</select>
						<span class="hint"
							>Its axis values decide which <a href="/platform/business-models">axis rules</a> fire.</span
						>
					</div>
					<div class="dims">
						{#each options?.dimensions ?? [] as d (d.key)}
							<fieldset class="areas" data-testid={`compile-dim-${d.key}`} disabled={!job}>
								<legend class="label">{d.label}</legend>
								{#each d.values as v (v.key)}
									<label class="row"
										><input
											type="checkbox"
											checked={(dims[d.key] ?? []).includes(v.key)}
											onchange={(e) =>
												(dims[d.key] = toggle(dims[d.key] ?? [], v.key, e.currentTarget.checked))}
										/>{v.label}</label
									>
								{/each}
								<span class="note">None ticked = any.</span>
							</fieldset>
						{/each}
					</div>
					{#if !job}<span class="hint"
							>Dimension values qualify job layers, so they need a job. <a href="/platform/dimensions"
								>Dimensions</a
							></span
						>{/if}
					<button class="btn primary" type="submit" disabled={!options || compiling}
						>{compiling ? 'Compiling…' : 'Compile'}</button
					>
					{#if stale}<span class="hint stale" role="status">Selection changed since the last compile.</span
						>{/if}
					<div class="row presets">
						<button class="btn sm" type="button" onclick={golden} disabled={!options}
							>Golden: retail customer care</button
						>
						<button class="table-action" type="button" onclick={invalid} disabled={!options}
							>Try an invalid combination</button
						>
					</div>
				</form>
			{/if}
		</Card>

		<Card title="Compare with" sub="A reference compile: what a running job uses today.">
			<div class="stack">
				<select
					class="input"
					aria-label="Compare with"
					bind:value={refKey}
					onchange={(e) => {
						refKey = e.currentTarget.value;
						void compileReference();
					}}
					disabled={!references.length}
				>
					<option value="">Nothing</option>
					{#each references as r (r.key)}<option value={r.key}>{r.label}</option>{/each}
				</select>
				{#if ref}
					<p class="muted small">{ref.note}</p>
					<div class="row">
						{#if ref.release}<Chip
								text={`${ref.release.id} · ${ref.release.status}`}
								variant={ref.release.status === 'current' ? 'ok' : 'accent'}
							/>{/if}
						<button class="table-action" type="button" onclick={() => load(ref!)}>Load into selection</button>
					</div>
				{/if}
				<p class="note">Compiled on today's layer rows, not on a release's template tag.</p>
			</div>
		</Card>
	</div>

	<div class="main-col">
		{#if error}
			<div class="callout err" role="alert">
				<strong class="callout-title">This selection does not compile</strong>
				<span
					>{error} A job cannot be cut from it. Every error is listed — fix the row it points at, or change the
					selection.</span
				>
				{#if errors.length}
					<ul class="errs" data-testid="compile-errors">
						{#each errors as e, i (i)}
							<li>
								<Chip text={e.code} variant="err" />
								<code>{e.key}</code> — {e.message}
								{#if e.row_id}<a href={`/platform/layers?row=${encodeURIComponent(e.row_id)}`}
										>Open row {e.row_id}</a
									>{/if}
							</li>
						{/each}
					</ul>
				{/if}
			</div>
		{/if}

		{#if result && counts}
			{#if result.warnings.some((w) => w.code === 'base_only_preview')}
				<p class="hint linked" data-testid="compile-base-only">
					<b>Base model only — a preview, not a job.</b> A job needs an industry and at least one of its departments.
				</p>
			{/if}
			<div class="verdict" data-testid="compile-verdict" class:is-stale={stale}>
				<div class="verdict-main">
					<span class="label">Effective model</span>
					<strong>{total} items from {result.layers.length} layers</strong>
					<span class="muted small"
						>{counts.entity} entities · {counts.attribute} attributes · {counts.relation} relations{#if result.job}
							· {counts.context_space} context spaces · {counts.procedure} procedures · {counts.connector_slot}
							connector slots{/if}</span
					>
				</div>
				<div class="verdict-facts">
					<span><b>{overrides.length}</b> overrides</span>
					<span><b>{result.rules_fired.length}</b> rules fired</span>
					<span class:warn={result.warnings.length}><b>{result.warnings.length}</b> warnings</span>
					<span class="mono hash" title="Content hash of the effective model">#{result.hash}</span>
				</div>
			</div>

			<Card
				title="Layer stack"
				sub="Which layers this selection pulls in, why, and what each one did to the model."
			>
				<LayerStack {stack} />
			</Card>

			<Card
				cls="flush"
				title="Where layers override each other"
				sub="The decisions inside this compile: a later layer patched or removed what an earlier one set."
			>
				{#if overrides.length}
					<div class="tbl-wrap">
						<table class="tbl dense" data-testid="overrides">
							<thead>
								<tr><th>Item</th><th>Outcome</th><th>Trail (earliest → winning)</th><th>Last write</th></tr>
							</thead>
							<tbody>
								{#each overrides as o (`${o.outcome}:${o.kind}:${o.key}`)}
									{@const last = o.origins[o.origins.length - 1]}
									<tr data-testid="override-row">
										<td>
											<code class="key">{o.key}</code>
											<span class="cell-sub">{kindLabel(o.kind)}</span>
										</td>
										<td>
											<Chip text={OUTCOME[o.outcome][0]} variant={OUTCOME[o.outcome][1]} />
											{#if o.by}<span class="cell-sub">by <code>{o.by}</code></span>{/if}
										</td>
										<td><Provenance origins={o.origins} /></td>
										<td class="small">
											{#if o.outcome === 'cascade'}
												<span class="muted">{o.by} was tombstoned</span>
											{:else if last}
												<span class="muted">{layerLabel(last.layer)}:</span>
												{fieldsLine(last.fields) || '—'}
											{/if}
										</td>
									</tr>
								{/each}
							</tbody>
						</table>
					</div>
				{:else}
					<p class="empty pad">
						No layer overrides another in this selection: every item comes from a single row.
					</p>
				{/if}
			</Card>

			{#if ref}
				<Card
					title={`Difference from ${ref.label}`}
					sub="What a job on this selection would get that the reference does not, and the reverse."
				>
					{#if referenceError}
						<Unavailable reason={`The reference does not compile: ${referenceError}`} />
					{:else if !diff}
						<p class="muted">Compiling the reference…</p>
					{:else if diff.same}
						<div class="callout info" data-testid="diff-same">
							<strong class="callout-title">Identical effective model</strong>
							<span
								>Same hash as the reference (#{reference?.hash}). A release cut from this selection would
								change nothing in the model.</span
							>
						</div>
					{:else}
						<div class="diff" data-testid="compile-diff">
							<div class="diff-sum">
								<span class="d-add"><b>+{diff.added}</b> added</span>
								<span class="d-rem"><b>−{diff.removed}</b> removed</span>
								<span class="d-chg"><b>~{diff.changed}</b> changed</span>
								<span class="mono muted">#{reference?.hash} → #{result.hash}</span>
							</div>
							{#if diff.layers_added.length || diff.layers_removed.length}
								<p class="small">
									{#each diff.layers_added as l (l)}<span class="layer-delta add"
											>+ layer <code>{l}</code></span
										>{/each}
									{#each diff.layers_removed as l (l)}<span class="layer-delta rem"
											>− layer <code>{l}</code></span
										>{/each}
								</p>
							{/if}
							<ul class="diff-list">
								{#each diff.entries as d (`${d.kind}:${d.key}`)}
									<li data-testid="diff-row">
										<Chip text={CHANGE[d.change][0]} variant={CHANGE[d.change][1]} />
										<span class="muted small">{kindLabel(d.kind)}</span>
										<code class="key">{d.key}</code>
										{#if d.fields?.length}<span class="muted small">fields: {d.fields.join(', ')}</span>{/if}
										{#if d.change !== 'removed'}<Provenance
												origins={result.provenance[`${d.kind}:${d.key}`]}
											/>{/if}
									</li>
								{/each}
							</ul>
						</div>
					{/if}
					{#if ref.release}
						<p class="note">
							{ref.release.tenant} runs {ref.release.id} ({ref.release.status}). Tenants change their own job
							through refinements and a new cut, not here —
							<a href={`/platform/tenants/${ref.release.tenant_id}`}>open the tenant</a>.
						</p>
					{/if}
				</Card>
			{/if}

			<Card cls="flush" title="Effective model" sub="Every item, with the rows that built it.">
				<Tabs items={tabItems} bind:active={tab} label="Object kind">
					<div class="tools">
						<input class="input" placeholder="Find an item…" aria-label="Find an item" bind:value={search} />
						<label class="row small"
							><input type="checkbox" bind:checked={onlyTouched} />Only items more than one layer touched</label
						>
					</div>
					{#if entries.length}
						<div class="tbl-wrap">
							<table class="tbl dense">
								<thead><tr><th>Key</th><th>Details</th><th>Provenance</th></tr></thead>
								<tbody>
									{#each entries as [key, value] (key)}
										<tr data-testid="effective-row" class:hl={touched.has(`${tab}:${key}`)}>
											<td><code class="key">{key}</code></td>
											<td>
												{detailOf(value)}
												{#if value.pii}<Chip text="PII" variant="warn" />{/if}
											</td>
											<td><Provenance origins={result.provenance[`${tab}:${key}`]} /></td>
										</tr>
									{/each}
								</tbody>
							</table>
						</div>
					{:else}
						<p class="empty pad">
							No {kindLabel(tab, true).toLowerCase()} in this selection{tab !== 'entity' &&
							tab !== 'attribute' &&
							tab !== 'relation' &&
							!result.job
								? ' — job content needs a job'
								: ''}.
						</p>
					{/if}
				</Tabs>
			</Card>

			<div class="grid g2 lower">
				{#if result.warnings.length}
					<Card title="Warnings" sub="Compiles, but worth a look">
						<ul class="list-plain">
							{#each result.warnings as w, i (i)}<li>
									<span><Chip text={w.code} variant="warn" /> {w.message}</span>
								</li>{/each}
						</ul>
						{#if result.rules_fired.length}
							<p class="note">
								Axis rules fired for the business model: {result.rules_fired.join(', ')}.
							</p>
						{/if}
					</Card>
				{/if}
				{#if result.job}
					<Card title={`Job definition · ${String(result.job.label ?? result.job.key)}`}>
						<JsonView value={result.job} />
					</Card>
				{/if}
			</div>
			<p class="note">
				Hiding an inherited item is a tenant refinement (job → Refinements), not part of this platform
				preview.
			</p>
		{:else if !error}
			<Card>
				<div class="placeholder">
					<h2>{compiling ? 'Compiling…' : 'Pick a selection and compile'}</h2>
					<p class="muted">
						The compile stacks base → business-model rules → industry → department → job and shows where every
						item came from.
					</p>
				</div>
			</Card>
		{/if}
	</div>
</div>

<style>
	.compile {
		display: grid;
		grid-template-columns: 300px minmax(0, 1fr);
		gap: 16px;
		align-items: start;
	}
	.side-col,
	.main-col {
		display: grid;
		gap: 16px;
		min-width: 0;
	}
	@media (max-width: 960px) {
		.compile {
			grid-template-columns: minmax(0, 1fr);
		}
	}
	.areas {
		border: 1px solid var(--line-2);
		border-radius: 6px;
		padding: 8px 10px;
		display: grid;
		gap: 4px;
		font-size: 13px;
	}
	.dims {
		display: grid;
		grid-template-columns: 1fr 1fr;
		gap: 10px;
	}
	.hint {
		font-size: 12px;
		color: var(--faint);
	}
	.hint.stale {
		color: var(--warn);
	}
	.small {
		font-size: 12.5px;
	}
	p.small,
	p.muted.small {
		margin: 0;
	}
	.presets {
		justify-content: space-between;
		flex-wrap: wrap;
	}
	.verdict {
		display: flex;
		justify-content: space-between;
		align-items: center;
		gap: 12px 20px;
		flex-wrap: wrap;
		background: var(--panel);
		border: 1px solid var(--line);
		border-left: 4px solid var(--ok);
		border-radius: 8px;
		padding: 12px 16px;
	}
	.verdict.is-stale {
		border-left-color: var(--warn);
	}
	.verdict-main {
		display: grid;
		gap: 2px;
	}
	.verdict-main strong {
		font: 600 19px/1.2 var(--cond);
	}
	.verdict-facts {
		min-width: 0;
		display: flex;
		gap: 14px;
		flex-wrap: wrap;
		font-size: 13px;
		color: var(--muted);
		align-items: baseline;
	}
	.verdict-facts b {
		color: var(--fg);
		font-variant-numeric: tabular-nums;
	}
	.verdict-facts .warn b {
		color: var(--warn);
	}
	.hash {
		color: var(--faint);
		/* a sha256 is one unbroken word: on a phone it must wrap, not push the page sideways */
		min-width: 0;
		overflow-wrap: anywhere;
	}
	.tools {
		padding: 10px 16px;
		display: flex;
		gap: 14px;
		align-items: center;
		flex-wrap: wrap;
	}
	.tools .input {
		max-width: 260px;
	}
	.key {
		font: 600 12.5px var(--mono);
		color: var(--accent);
		overflow-wrap: anywhere;
	}
	[data-testid='overrides'] .key {
		white-space: nowrap;
		overflow-wrap: normal;
	}
	tr.hl td:first-child {
		box-shadow: inset 3px 0 0 var(--tool);
	}
	.pad {
		padding-left: 16px;
		padding-right: 16px;
	}
	.errs {
		margin: 6px 0 0;
		padding-left: 18px;
		display: grid;
		gap: 6px;
	}
	.diff {
		display: grid;
		gap: 10px;
	}
	.diff-sum {
		display: flex;
		gap: 16px;
		flex-wrap: wrap;
		align-items: baseline;
		font-size: 13px;
	}
	.d-add b {
		color: var(--ok);
	}
	.d-rem b {
		color: var(--err);
	}
	.d-chg b {
		color: var(--tool);
	}
	.layer-delta {
		margin-right: 12px;
	}
	.layer-delta.add {
		color: var(--ok);
	}
	.layer-delta.rem {
		color: var(--err);
	}
	.diff-list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 6px;
	}
	.diff-list li {
		display: flex;
		flex-wrap: wrap;
		gap: 6px 10px;
		align-items: center;
		padding: 6px 8px;
		border: 1px solid var(--line-2);
		border-radius: 6px;
	}
	.lower:empty {
		display: none;
	}
	@container (max-width: 720px) {
		.lower {
			grid-template-columns: minmax(0, 1fr);
		}
	}
	.placeholder {
		min-height: 260px;
		display: grid;
		place-content: center;
		text-align: center;
		gap: 8px;
	}
	.placeholder h2 {
		margin: 0;
		font: 600 20px var(--cond);
	}
</style>
