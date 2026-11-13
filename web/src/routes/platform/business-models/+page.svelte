<script lang="ts">
	// P12 Business models. A business model (configurator "profile") is a selection over the classifier axes; each
	// axis classifies one canonical object (Party, Offering, Order, Renewal, Delivery, Payment, Contract — 11 §1.2).
	// The selection decides the object model: base objects, plus the objects every axis rule whose condition holds
	// adds as a profile layer (11 §4.1, D19). The page leads with one business model in the picker and what it
	// derives; the vocabulary (rules, axes) sits below. Axes are insert-only (05 §2.2); a profile is updated in place
	// and inlined into each release's provenance; freemium is `Offering.price_tier = free`, not an axis value (D24).
	import { superAdminAttrs } from '$lib/auth/roles';
	import { onMount } from 'svelte';
	import { api, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { Resource } from '$lib/load.svelte';
	import type {
		Axis,
		AxisCondition,
		AxisRule,
		BusinessProfile,
		RenewalPreset,
	} from '$lib/api/types/platform';
	import PageHead from '$lib/ui/PageHead.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import Tabs from '$lib/ui/Tabs.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import MatrixTable from '$lib/ui/MatrixTable.svelte';
	import MilestoneChip from '$lib/ui/MilestoneChip.svelte';
	import { describe } from '$lib/platform/model';
	import { compileHref } from '$lib/platform/compileDiff';
	import { emptySelection } from '$lib/api/types/core';
	import { layerHref } from '$lib/platform/layers-P4';
	import { utc } from '$lib/platform/ui';
	import { derive, diffSelection, groupAxes, openAxes, type Selection } from '$lib/platform/BusinessModelP12';

	type Data = {
		axes: Axis[];
		rules: AxisRule[];
		profiles: BusinessProfile[];
		presets: RenewalPreset[];
		industries: { key: string; label: string }[];
	};
	const res = new Resource<Data>(
		async () => {
			const a = api();
			const [axes, rules, profiles, presets, opts] = await Promise.all([
				a.businessAxes(),
				a.axisRules(),
				a.businessProfiles(),
				a.renewalPresets(),
				a.compileOptions(),
			]);
			return { axes, rules, profiles, presets, industries: opts.industries };
		},
		(d) => d.axes.length === 0,
	);
	// The base layer's objects: always in the model, whatever the business model selects.
	const base = new Resource<{ tag: string | null; objects: string[] }>(async () => {
		const a = api();
		const layer = (await a.layerCatalog()).items.find((l) => l.scope === 'base');
		if (!layer) return { tag: null, objects: [] };
		const rows = (await a.layerRows(layer.id)).items;
		return {
			tag: layer.tag,
			objects: rows.filter((r) => r.kind === 'entity' && r.op === 'add').map((r) => r.key),
		};
	});
	onMount(() => {
		void res.refresh();
		void base.refresh();
	});

	// ---- the picker: one business model at a time ----
	let sel = $state<Selection>({});
	let editingKey = $state<string | null>(null);
	let preset = $state('');
	let started = false;

	const data = $derived(res.value.state === 'ok' ? res.value.data : null);
	const editing = $derived(data?.profiles.find((p) => p.key === editingKey) ?? null);

	$effect(() => {
		// Open the first business model once the data is in: the page leads with a real example, not a blank form.
		if (!started && data?.profiles[0]) {
			started = true;
			load(data.profiles[0]);
		}
	});

	function toggle(a: Axis, key: string) {
		preset = '';
		const cur = sel[a.key] ?? [];
		sel[a.key] = cur.includes(key) ? cur.filter((v) => v !== key) : [...cur, key];
	}
	function applyPreset(p: RenewalPreset) {
		preset = p.key;
		sel['order_type'] = [p.order_type];
		sel['renewal.initiator'] = [p.initiator];
		sel['renewal.mode'] = p.mode ? [p.mode] : [];
	}
	function load(p: BusinessProfile) {
		editingKey = p.key;
		preset = '';
		sel = structuredClone($state.snapshot(p.selection));
	}
	function startNew() {
		sel = {};
		editingKey = null;
		preset = '';
	}
	function revert() {
		if (editing) load(editing);
		else startNew();
	}
	const depth = (key: string) => key.split('.').length - 1;
	const objCount = (r: AxisRule) => r.adds.length;
	const valueLabel = (axes: Axis[], axis: string, v: string) =>
		axes.find((a) => a.key === axis)?.values.find((x) => x.key === v)?.label ?? v;

	// ---- save business model ----
	let saveOpen = $state(false);
	let saveAsNew = $state(false);
	let pKey = $state('');
	let pLabel = $state('');
	let pIndustry = $state('');
	let pDescription = $state('');
	function openSave(asNew: boolean) {
		saveAsNew = asNew || !editing;
		pKey = saveAsNew ? '' : (editing?.key ?? '');
		pLabel = saveAsNew ? '' : (editing?.label ?? '');
		pIndustry = editing?.industry ?? '';
		pDescription = saveAsNew ? '' : (editing?.description ?? '');
		saveOpen = true;
	}
	async function saveProfile(e: SubmitEvent) {
		e.preventDefault();
		const key = pKey.trim();
		const inPlace = !!editing && editing.key === key;
		const ok = await guard(
			() =>
				api().saveBusinessProfile({
					key,
					label: pLabel,
					industry: pIndustry || null,
					description: pDescription,
					selection: $state.snapshot(sel),
				}),
			inPlace ? `${pLabel} updated in place.` : `Business model ${key} created.`,
		);
		if (ok) {
			saveOpen = false;
			await res.refresh();
			editingKey = key;
		}
	}

	// ---- add axis value ----
	let valueOpen = $state(false);
	let valueAxis = $state<Axis | null>(null);
	let vKey = $state('');
	let vLabel = $state('');
	let vParent = $state('');
	function openValue(a: Axis) {
		valueAxis = a;
		vKey = '';
		vLabel = '';
		vParent = '';
		valueOpen = true;
	}
	async function addValue(e: SubmitEvent) {
		e.preventDefault();
		if (!valueAxis) return;
		const ax = valueAxis;
		const ok = await guard(
			() => api().addAxisValue(ax.key, { key: vKey, label: vLabel, parent: vParent || null }),
			`${ax.label}: value ${vParent ? `${vParent}.` : ''}${vKey} added (insert-only).`,
		);
		if (ok) {
			valueOpen = false;
			await res.refresh();
		}
	}

	// ---- add rule ----
	let ruleOpen = $state(false);
	let rKey = $state('');
	let rPriority = $state(300);
	let clauses = $state<{ axis: string; value: string }[]>([{ axis: 'order_type', value: '' }]);
	let rEntity = $state('');
	let rEntityLabel = $state('');
	function openRule() {
		rKey = '';
		rPriority = 300;
		clauses = [{ axis: 'order_type', value: '' }];
		rEntity = '';
		rEntityLabel = '';
		ruleOpen = true;
	}
	async function addRule(e: SubmitEvent) {
		e.preventDefault();
		const parts: AxisCondition[] = clauses.filter((c) => c.value).map((c) => ({ eq: [c.axis, c.value] }));
		const condition: AxisCondition = parts.length === 1 ? parts[0]! : { and: parts };
		const ok = await guard(
			() =>
				api().createAxisRule({
					key: rKey,
					priority: Number(rPriority),
					condition,
					adds: { key: rEntity, label: rEntityLabel },
				}),
			`Rule ${rKey} added: ${describe(condition)} → ${rEntity}.`,
		);
		if (ok) {
			ruleOpen = false;
			await res.refresh();
		}
	}

	let tab = $state('compare');
</script>

<PageHead
	id="P12"
	sub="How a kind of business trades, as a selection over the classifier axes. The selection decides the agent's object model: the base objects, plus every object an axis rule adds when its condition holds."
>
	{#snippet actions()}
		<button class="btn" onclick={startNew}>New business model</button>
		<button class="btn" {...superAdminAttrs(session)} onclick={openRule}>Add rule</button>
	{/snippet}
</PageHead>

<LoadState resource={res} loading="Loading business models…" empty="No axes defined.">
	{#snippet children(d)}
		{@const groups = groupAxes(d.axes)}
		{@const out = derive(d.rules, sel)}
		{@const diff = diffSelection(editing?.selection ?? null, sel)}
		{@const dirty = diff.added.length + diff.removed.length > 0}
		{@const open = openAxes(d.axes, sel)}
		{@const added = [...new Set(out.fired.flatMap((r) => r.adds))]}

		<!-- How the concept works, with the live numbers of the business model in the picker. -->
		<ol class="flow" aria-label="From axes to the object model" data-testid="bm-flow">
			<li>
				<span class="step">1</span>
				<span><strong>{d.axes.length} axes</strong> classify {groups.length} objects</span>
			</li>
			<li>
				<span class="step">2</span>
				<span><strong>A business model</strong> picks values on {d.axes.length - open.length} of them</span>
			</li>
			<li>
				<span class="step">3</span>
				<span><strong>{out.fired.length} of {d.rules.length} axis rules</strong> fire</span>
			</li>
			<li>
				<span class="step">4</span>
				<span
					><strong
						>{base.value.state === 'ok' ? `${base.value.data.objects.length} base` : 'Base'} + {added.length} added</strong
					> objects</span
				>
			</li>
			<li>
				<span class="step">5</span>
				<span
					>Compile layers them: base → <em>profile</em> → industry → dept → job ·
					<a
						data-testid="bm-compile-link"
						href={editing
							? compileHref({
									selection: {
										...emptySelection(),
										profile: editing.key,
										industry: editing.industry ?? '',
									},
									job: null,
								})
							: '/platform/compile'}
						title={editing ? `Compile preview with ${editing.label} selected` : undefined}
						>Compile preview{editing ? ` of ${editing.label}` : ''}</a
					></span
				>
			</li>
		</ol>

		<!-- Which business model is in the picker. -->
		<div class="models" role="group" aria-label="Business models">
			{#each d.profiles as p (p.key)}
				{@const n = derive(d.rules, p.selection).fired.length}
				<button
					type="button"
					class="model"
					class:on={editingKey === p.key}
					aria-pressed={editingKey === p.key}
					data-testid="profile-row"
					onclick={() => load(p)}
				>
					<span class="cell-title">{p.label}</span>
					<span class="cell-sub mono">{p.industry ?? 'any industry'} · {n} rule{n === 1 ? '' : 's'} fire</span
					>
					<span class="cell-sub">{p.description}</span>
				</button>
			{/each}
			<button
				type="button"
				class="model new"
				class:on={editingKey === null}
				aria-pressed={editingKey === null}
				onclick={startNew}
			>
				<span class="cell-title">+ New business model</span>
				<span class="cell-sub">Start from an empty selection.</span>
			</button>
		</div>

		<div class="workspace" id="picker">
			<section class="pick" aria-label="Axis picker">
				<header class="pick-head">
					<div>
						<h2>{editing ? editing.label : 'New business model'}</h2>
						<p class="muted">
							{#if editing}
								<span class="mono">{editing.key}</span> · updated {utc(editing.updated_at)} by {editing.updated_by}
							{:else}
								Pick the values this kind of business allows, then save it as a business model.
							{/if}
						</p>
						{#if editing?.notes.length}
							<ul class="notes">
								{#each editing.notes as n (n)}<li>{n}</li>{/each}
							</ul>
						{/if}
					</div>
				</header>

				<div class="presets-wrap">
					<div class="label">Order &amp; renewal pattern</div>
					<p class="note tight">
						The owner's six worked examples — each sets order type, renewal initiator and mode.
					</p>
					<div class="presets" role="group" aria-label="Renewal presets">
						{#each d.presets as p (p.key)}
							<button
								type="button"
								class="preset"
								class:on={preset === p.key}
								aria-pressed={preset === p.key}
								onclick={() => applyPreset(p)}
							>
								<strong>{p.example}</strong>
								<span class="muted">{p.label}</span>
							</button>
						{/each}
					</div>
				</div>

				<div class="groups">
					{#each groups as g (g.object)}
						<div class="group" data-testid="axis-group">
							<div class="group-head">
								<span class="obj mono">{g.object}</span>
								<span class="muted">{g.question}</span>
							</div>
							{#each g.axes as a (a.key)}
								<div class="axis-row">
									<div class="axis-name">
										<span>{a.label}</span>
										<span class="cell-sub mono"
											>{a.key} · {a.cardinality === 'one' ? 'one per offering' : 'many'}</span
										>
										{#if a.milestone === 'M4'}<MilestoneChip milestone="M4" note="axis split (N5, N7)" />{/if}
									</div>
									<div class="chips" role="group" aria-label={a.label}>
										{#each a.values as v (v.key)}
											<button
												type="button"
												class="chip val"
												class:child={depth(v.key) > 0}
												class:accent={sel[a.key]?.includes(v.key)}
												aria-pressed={!!sel[a.key]?.includes(v.key)}
												title={v.parent ? `${valueLabel(d.axes, a.key, v.parent)} › ${v.label}` : undefined}
												onclick={() => toggle(a, v.key)}>{v.label}</button
											>
										{/each}
									</div>
								</div>
							{/each}
							{#if g.object === 'Offering'}
								<p class="note tight">
									<strong>Freemium is not an axis value</strong> — it is an Offering with
									<code>price_tier = free</code> and <code>upgrade_to[]</code> naming the paid tiers (D24).
								</p>
							{/if}
						</div>
					{/each}
				</div>
			</section>

			<aside class="derived" data-testid="selection-adds" aria-label="What this selection derives">
				<div class="label">The object model this selection derives</div>

				<div class="block">
					<div class="block-head">
						<strong>Added by axis rules</strong>
						<span class="muted">{added.length} object{added.length === 1 ? '' : 's'}</span>
					</div>
					{#if out.fired.length}
						<ul class="rules">
							{#each out.fired as r (r.key)}
								<li class="fired">
									<div class="chips">
										{#each r.adds as o (o)}<Chip text={o} variant="cust" />{:else}<span class="muted"
												>extends existing objects ({r.rows} row{r.rows === 1 ? '' : 's'})</span
											>{/each}
									</div>
									<span class="why"
										><a href={layerHref(r.layer_id)} title="Open this rule's profile layer"
											><strong>{r.key}</strong></a
										>
										· because {describe(r.condition)}</span
									>
								</li>
							{/each}
						</ul>
					{:else}
						<p class="muted">Base objects only — no axis rule holds for this selection.</p>
					{/if}
				</div>

				<div class="block">
					<div class="block-head">
						<strong>Always there</strong>
						<span class="muted"
							>{base.value.state === 'ok' && base.value.data.tag ? base.value.data.tag : 'base layer'}</span
						>
					</div>
					{#if base.value.state === 'ok'}
						<div class="chips">
							{#each base.value.data.objects as o (o)}<Chip text={o} />{/each}
						</div>
					{:else if base.value.state === 'unavailable'}
						<p class="muted">
							Base objects unavailable ({base.value.reason}).
							<button class="table-action" onclick={() => base.refresh()}>Retry</button>
						</p>
					{:else}
						<p class="muted">Loading base objects…</p>
					{/if}
				</div>

				{#if out.idle.length}
					<div class="block">
						<div class="block-head">
							<strong>Not firing</strong>
							<span class="muted">what each would need</span>
						</div>
						<ul class="rules">
							{#each out.idle as r (r.key)}
								<li class="idle">
									<span
										><strong>{r.key}</strong> → {r.adds.join(', ') ||
											`${r.rows} row${r.rows === 1 ? '' : 's'}`}</span
									>
									<span class="why">needs {describe(r.condition)}</span>
								</li>
							{/each}
						</ul>
					</div>
				{/if}

				<div class="block save" data-testid="bm-save">
					{#if editing && dirty}
						<p class="change">
							<strong>Unsaved changes</strong> to {editing.label}:
							{#each diff.added as p (p)}<Chip text={`+ ${p}`} variant="ok" />{/each}
							{#each diff.removed as p (p)}<Chip text={`− ${p}`} variant="err" />{/each}
						</p>
					{:else if editing}
						<p class="muted">Matches the saved {editing.label}.</p>
					{/if}
					{#if open.length && !editing}
						<p class="muted">{open.length} axes still open.</p>
					{/if}
					<div class="row">
						{#if editing}
							<button
								class="btn primary"
								{...superAdminAttrs(session)}
								disabled={!session.superAdmin || !dirty}
								onclick={() => openSave(false)}>Save in place…</button
							>
							<button class="btn" {...superAdminAttrs(session)} onclick={() => openSave(true)}
								>Save as new…</button
							>
							<button class="btn" disabled={!dirty} onclick={revert}>Revert</button>
						{:else}
							<button class="btn primary" {...superAdminAttrs(session)} onclick={() => openSave(true)}
								>Save as business model…</button
							>
						{/if}
					</div>
					<p class="note tight">
						Saving updates the business model in place; the next release inlines the new selection in its
						provenance — releases already cut keep the old one.
					</p>
				</div>
			</aside>
		</div>

		<!-- Reference: the vocabulary the picker draws on. -->
		<section class="vocab" aria-label="Vocabulary">
			<Tabs
				items={[
					['compare', 'Compare business models', d.profiles.length],
					['rules', 'Axis rules', d.rules.length],
					['axes', 'Axes & values', d.axes.length],
				]}
				bind:active={tab}
				label="Business-model vocabulary"
			/>
			{#if tab === 'compare'}
				<Card
					cls="flush"
					title="Side by side"
					sub="Where the business models differ. An empty cell leaves the axis open."
				>
					<MatrixTable
						rows={d.axes}
						columns={d.profiles}
						rowKey={(a) => a.key}
						colKey={(p) => p.key}
						caption="Business models by axis"
						corner="Axis"
					>
						{#snippet rowHeader(a)}<span class="cell-title">{a.label}</span><span class="cell-sub mono"
								>{a.object}</span
							>{/snippet}
						{#snippet colHeader(p)}<button class="table-action" onclick={() => load(p)}>{p.label}</button
							>{/snippet}
						{#snippet cell(a, p)}
							<div class="chips">
								{#each p.selection[a.key] ?? [] as v (v)}<Chip
										text={valueLabel(d.axes, a.key, v)}
										variant="accent"
									/>{:else}<span class="faint">—</span>{/each}
							</div>
						{/snippet}
					</MatrixTable>
				</Card>
			{:else if tab === 'rules'}
				<Card
					title="Axis rules"
					sub="When a condition holds, the rule's profile layer joins the compile. Lower priority applies first."
				>
					{#snippet actions()}
						<button class="btn sm" {...superAdminAttrs(session)} onclick={openRule}>Add rule</button>
					{/snippet}
					<ul class="list-plain">
						{#each d.rules as r (r.key)}
							{@const users = d.profiles.filter((p) => derive([r], p.selection).fired.length)}
							<li data-testid="rule-row">
								<span>
									<span class="mono muted">{r.priority}</span>
									<a
										href={layerHref(r.layer_id)}
										data-testid="rule-layer-link"
										title="Open this rule's profile layer"><strong>{r.key}</strong></a
									>
									<span class="cell-sub">when {describe(r.condition)}</span>
									<span class="cell-sub"
										>fires for {users.map((u) => u.label).join(', ') || 'no saved business model'}</span
									>
								</span>
								<span class="chips">
									{#each r.adds as o (o)}<Chip text={`+ ${o}`} variant="cust" />{/each}
									<Chip text={`${r.rows} rows · ${objCount(r)} object${objCount(r) === 1 ? '' : 's'}`} />
								</span>
							</li>
						{/each}
					</ul>
					<p class="note">
						Each rule's name opens its profile layer in <a href="/platform/layers">Layers</a>, where its rows
						are edited.
					</p>
				</Card>
			{:else}
				<Card
					title="Axes & values"
					sub="Insert-only (05 §2.2): values are appended, never renamed or removed, so every saved selection stays valid. A child value also satisfies its parent."
				>
					<div class="axis-cards">
						{#each d.axes as a (a.key)}
							<div class="axis" data-testid="axis">
								<div class="axis-head">
									<span>
										<strong>{a.label}</strong>
										<span class="cell-sub mono">{a.key} → {a.object}</span>
									</span>
									<button class="btn sm" {...superAdminAttrs(session)} onclick={() => openValue(a)}
										>Add value</button
									>
								</div>
								<p class="note tight">{a.description}</p>
								<ul class="tree">
									{#each a.values as v (v.key)}
										<li style:padding-left="{depth(v.key) * 16}px">
											{depth(v.key) ? '└ ' : ''}{v.label} <span class="mono faint">{v.key}</span>
										</li>
									{/each}
								</ul>
							</div>
						{/each}
					</div>
				</Card>
			{/if}
		</section>

		<Drawer bind:open={ruleOpen} title="Add axis rule">
			{#snippet sub()}<span
					>The rule adds one object (a new profile layer); edit its rows later in Layers.</span
				>{/snippet}
			<form class="stack" onsubmit={addRule}>
				<div class="form-grid">
					<div class="field">
						<label for="r-key">Rule key</label>
						<input id="r-key" class="input mono" bind:value={rKey} required placeholder="pickup_points" />
					</div>
					<div class="field">
						<label for="r-prio">Priority</label>
						<input id="r-prio" class="input" type="number" bind:value={rPriority} min="1" max="999" />
					</div>
				</div>
				<fieldset class="clauses">
					<legend class="label">When (all hold)</legend>
					{#each clauses as c, i (i)}
						<div class="form-grid">
							<select
								class="input"
								aria-label={`Axis ${i + 1}`}
								bind:value={c.axis}
								onchange={() => (c.value = '')}
							>
								{#each d.axes as a (a.key)}<option value={a.key}>{a.label}</option>{/each}
							</select>
							<select class="input" aria-label={`Value ${i + 1}`} bind:value={c.value} required>
								<option value="">=…</option>
								{#each d.axes.find((a) => a.key === c.axis)?.values ?? [] as v (v.key)}<option value={v.key}
										>{v.key}</option
									>{/each}
							</select>
						</div>
					{/each}
					<button
						type="button"
						class="table-action"
						onclick={() => clauses.push({ axis: 'delivery_type', value: '' })}>+ and another condition</button
					>
				</fieldset>
				<div class="form-grid">
					<div class="field">
						<label for="r-entity">Adds object</label>
						<input id="r-entity" class="input mono" bind:value={rEntity} required placeholder="PickupPoint" />
					</div>
					<div class="field">
						<label for="r-label">Label</label>
						<input id="r-label" class="input" bind:value={rEntityLabel} placeholder="Pickup point" />
					</div>
				</div>
				<div class="actions">
					<button class="btn primary" type="submit" {...superAdminAttrs(session)}>Add rule</button>
				</div>
			</form>
		</Drawer>

		<Drawer
			bind:open={saveOpen}
			title={saveAsNew ? 'Save as a new business model' : `Save ${editing?.label ?? ''} in place`}
		>
			{#snippet sub()}<span
					>{saveAsNew
						? "Saves the picker's selection under a new key."
						: 'Updates the business model in place; the next release inlines it in its provenance.'}</span
				>{/snippet}
			<form class="stack" onsubmit={saveProfile}>
				<div class="form-grid">
					<div class="field">
						<label for="p-key">Key</label>
						<input
							id="p-key"
							class="input mono"
							bind:value={pKey}
							required
							readonly={!saveAsNew}
							placeholder="bike_retail"
						/>
					</div>
					<div class="field">
						<label for="p-label">Name</label>
						<input id="p-label" class="input" bind:value={pLabel} required />
					</div>
				</div>
				<div class="field">
					<label for="p-industry">Industry</label>
					<select id="p-industry" class="input" bind:value={pIndustry}>
						<option value="">Any industry</option>
						{#each d.industries as i (i.key)}<option value={i.key}>{i.label}</option>{/each}
					</select>
				</div>
				<div class="field">
					<label for="p-desc">Description</label>
					<textarea id="p-desc" class="input" rows="3" bind:value={pDescription}></textarea>
				</div>
				<p class="note">
					{Object.values(sel).flat().length} axis values · {out.fired.length} axis rules fire ({added.join(
						', ',
					) || 'base objects only'}).
				</p>
				<div class="actions">
					<button class="btn primary" type="submit" {...superAdminAttrs(session)}>Save</button>
				</div>
			</form>
		</Drawer>
	{/snippet}
</LoadState>

<Drawer bind:open={valueOpen} title={valueAxis ? `Add a value to ${valueAxis.label}` : 'Add value'}>
	{#snippet sub()}<span>Insert-only: a value can never be renamed or removed afterwards.</span>{/snippet}
	{#if valueAxis}
		<form class="stack" onsubmit={addValue}>
			<div class="field">
				<label for="v-parent">Parent (optional)</label>
				<select id="v-parent" class="input" bind:value={vParent}>
					<option value="">Top level</option>
					{#each valueAxis.values.filter((v) => !v.parent) as v (v.key)}<option value={v.key}
							>{v.label}</option
						>{/each}
				</select>
			</div>
			<div class="form-grid">
				<div class="field">
					<label for="v-key">Key</label>
					<input id="v-key" class="input mono" bind:value={vKey} required />
				</div>
				<div class="field">
					<label for="v-label">Label</label>
					<input id="v-label" class="input" bind:value={vLabel} required />
				</div>
			</div>
			<div class="actions">
				<button class="btn primary" type="submit" {...superAdminAttrs(session)}>Add value</button>
			</div>
		</form>
	{/if}
</Drawer>

<style>
	/* ---- concept flow ---- */
	.flow {
		list-style: none;
		margin: 0 0 14px;
		padding: 0;
		display: grid;
		grid-template-columns: repeat(5, minmax(0, 1fr));
		gap: 6px;
		font-size: 12.5px;
	}
	.flow li {
		display: flex;
		gap: 8px;
		align-items: flex-start;
		padding: 8px 10px;
		background: var(--sunk);
		border-radius: var(--r);
		position: relative;
	}
	.flow li:not(:last-child)::after {
		content: '→';
		position: absolute;
		right: -6px;
		top: 50%;
		transform: translate(50%, -50%);
		color: var(--faint);
		z-index: 1;
	}
	.step {
		flex: none;
		width: 18px;
		height: 18px;
		border-radius: 50%;
		display: grid;
		place-items: center;
		font: 600 11px/1 var(--mono);
		background: var(--accent);
		color: var(--panel);
	}
	@media (max-width: 900px) {
		.flow {
			grid-template-columns: minmax(0, 1fr);
		}
		.flow li:not(:last-child)::after {
			content: '';
		}
	}

	/* ---- business model switcher ---- */
	.models {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(220px, 1fr));
		gap: 8px;
		margin-bottom: 14px;
	}
	.model {
		display: grid;
		gap: 3px;
		align-content: start;
		text-align: left;
		border: 1px solid var(--line);
		background: var(--panel);
		border-radius: var(--r);
		padding: 10px 12px;
		cursor: pointer;
		color: inherit;
		font: inherit;
	}
	.model.on {
		border-color: var(--accent);
		box-shadow: inset 3px 0 0 var(--accent);
		background: color-mix(in srgb, var(--accent) 6%, var(--panel));
	}
	.model.new {
		border-style: dashed;
	}

	/* ---- workspace: picker + derived model ---- */
	.workspace {
		display: grid;
		grid-template-columns: minmax(0, 1fr) 320px;
		gap: 16px;
		align-items: start;
		margin-bottom: 18px;
	}
	@media (max-width: 1100px) {
		.workspace {
			grid-template-columns: minmax(0, 1fr);
		}
	}
	.pick,
	.derived {
		border: 1px solid var(--line);
		background: var(--panel);
		border-radius: var(--r);
		padding: 14px 16px;
		min-width: 0;
	}
	.derived {
		position: sticky;
		top: 12px;
		display: grid;
		gap: 12px;
	}
	@media (max-width: 1100px) {
		.derived {
			position: static;
		}
	}
	.pick-head h2 {
		margin: 0;
		font-size: 17px;
	}
	.pick-head p {
		margin: 2px 0 0;
		font-size: 12.5px;
	}
	.notes {
		margin: 6px 0 0;
		padding-left: 18px;
		font-size: 12.5px;
		color: var(--muted);
	}
	.presets-wrap {
		margin: 14px 0;
	}
	.presets {
		display: grid;
		/* The owner's six presets: 3 × 2, never a lone sixth tile on its own row. */
		grid-template-columns: repeat(3, minmax(0, 1fr));
		gap: 6px;
		margin-top: 6px;
	}
	@media (max-width: 700px) {
		.presets {
			grid-template-columns: repeat(2, minmax(0, 1fr));
		}
	}
	.preset {
		display: grid;
		gap: 2px;
		text-align: left;
		border: 1px solid var(--line);
		background: var(--panel);
		color: inherit;
		border-radius: var(--r);
		padding: 6px 9px;
		cursor: pointer;
		font-size: 12px;
	}
	.preset.on {
		border-color: var(--accent);
		background: color-mix(in srgb, var(--accent) 8%, var(--panel));
	}
	.note.tight {
		margin: 2px 0 0;
	}
	.groups {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(min(100%, 340px), 1fr));
		gap: 12px;
		align-items: start;
	}
	.group {
		border: 1px solid var(--line-2);
		border-radius: var(--r);
		padding: 10px 12px;
		display: grid;
		gap: 8px;
		align-content: start;
	}
	.group-head {
		display: flex;
		gap: 8px;
		align-items: baseline;
		font-size: 12.5px;
		padding-bottom: 6px;
		border-bottom: 1px solid var(--line-2);
	}
	.obj {
		font-weight: 600;
		color: var(--cust);
	}
	.axis-row {
		display: grid;
		gap: 5px;
	}
	.axis-name {
		display: flex;
		flex-wrap: wrap;
		gap: 2px 8px;
		align-items: baseline;
		font-size: 13px;
		font-weight: 500;
	}
	.axis-name .cell-sub {
		display: inline;
	}
	.val {
		border: 1px solid var(--line);
		cursor: pointer;
	}
	.val.accent {
		border-color: var(--accent);
	}
	.val.child {
		border-style: dashed;
	}

	.label {
		margin-bottom: 0;
	}
	.block {
		display: grid;
		gap: 6px;
		padding-top: 10px;
		border-top: 1px solid var(--line-2);
	}
	.block-head {
		display: flex;
		justify-content: space-between;
		gap: 8px;
		font-size: 13px;
	}
	.rules {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 8px;
	}
	.rules li {
		display: grid;
		gap: 3px;
		font-size: 12.5px;
	}
	.rules .why {
		color: var(--muted);
		font-size: 12px;
	}
	.rules .idle {
		color: var(--faint);
	}
	.change {
		margin: 0;
		display: flex;
		flex-wrap: wrap;
		gap: 4px;
		align-items: center;
		font-size: 12.5px;
	}
	.save .row {
		gap: 6px;
	}

	/* ---- vocabulary ---- */
	.vocab {
		display: grid;
		gap: 10px;
	}
	.axis-cards {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(min(100%, 260px), 1fr));
		gap: 10px;
	}
	.axis {
		border: 1px solid var(--line-2);
		border-radius: var(--r);
		padding: 8px 10px;
	}
	.axis-head {
		display: flex;
		justify-content: space-between;
		align-items: flex-start;
		gap: 8px;
	}
	.axis-head > .btn {
		flex: none;
	}
	.tree {
		list-style: none;
		margin: 6px 0 0;
		padding: 0;
		font-size: 12.5px;
		display: grid;
		gap: 2px;
	}
	.faint {
		color: var(--faint);
	}
	.clauses {
		border: 1px solid var(--line-2);
		border-radius: 6px;
		padding: 8px 10px;
		display: grid;
		gap: 8px;
	}
</style>
