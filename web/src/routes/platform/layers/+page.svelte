<script lang="ts">
	// P4 Layers — the platform authoring model as an overlay stack: base → business-model profiles → industry →
	// department → job. A job's compiled config is these layers applied in that order; each layer adds objects,
	// overrides fields of earlier ones (patch) or removes them (tombstone). The page answers, per layer: what does
	// it change, who authored it, is it in a tag (pinnable) or still draft, and which templates compose it.
	// Read-only, append-only (05 §4.9): retired revisions stay, `?row=<id>` opens a row (compile errors land here),
	// `?layer=<id or name>` opens a layer (business-model axis rules and the compile stack link here).
	// No `tenant` scope (D8): tenants refine a released job instead.
	import { onMount } from 'svelte';
	import { page } from '$app/state';
	import { api } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import type {
		CompileTarget,
		DepartmentArchetype,
		JobTemplateTag,
		Layer,
		LayerOverride,
		LayerRow,
		LayerScope,
		LayerSummary,
		RowOp,
	} from '$lib/api/types/platform';
	import PageHead from '$lib/ui/PageHead.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import JsonView from '$lib/ui/JsonView.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';
	import { SCOPES, cascadeOf, layerLabel } from '$lib/platform/model';
	import { utc } from '$lib/platform/ui';
	import { tagHref } from '$lib/model/logic';
	import {
		OP_INFO,
		SCOPE_INFO,
		fieldLine,
		findLayer,
		kindCounts,
		shortName,
		tagVersion,
		templatesFor,
	} from '$lib/platform/layers-P4';
	import { compileHref, selectionOfLayer } from '$lib/platform/compileDiff';
	import CompletenessMatrix from '$lib/platform/CompletenessMatrix.svelte';

	const layersRes = new Resource(() => api().layerCatalog());
	let summaries = $state<Record<string, LayerSummary>>({});
	let templates = $state<JobTemplateTag[]>([]);
	/** D60: base department archetypes and the industry departments extending them. */
	let archetypes = $state<DepartmentArchetype[]>([]);
	const archetypeOfLayer = (l: Layer) =>
		l.extends ? (archetypes.find((a) => a.key === l.extends) ?? null) : null;
	/** Compile references: context (industry, department) for a job layer's compile-preview link. */
	let known = $state<CompileTarget[]>([]);
	let selected = $state<Layer | null>(null);
	let rows = $state<LayerRow[]>([]);
	let overrides = $state<Record<string, LayerOverride>>({});
	let rowsState = $state<'idle' | 'loading' | 'error'>('idle');
	let rowsError = $state('');
	let showRetired = $state(false);
	let kind = $state('');
	let search = $state('');
	let scopeFilter = $state<'' | LayerScope>('');
	let highlight = $state<string | null>(null);
	let notice = $state('');
	let allAdds = $state(false);
	const ADDS_PREVIEW = 12;

	const OPS: RowOp[] = ['patch', 'tombstone', 'add'];
	const SECTION: Record<RowOp, { title: string; sub: string }> = {
		patch: {
			title: 'Overrides',
			sub: 'Fields this layer replaces on objects an earlier layer added.',
		},
		tombstone: {
			title: 'Removes',
			sub: 'Objects this layer hides from everything composed on top of it.',
		},
		add: { title: 'Adds', sub: 'New objects this layer introduces.' },
	};

	const current = $derived(rows.filter((r) => r.is_current));
	const matches = $derived(
		rows.filter(
			(r) =>
				(showRetired || r.is_current) &&
				(!kind || r.kind === kind) &&
				r.key.toLowerCase().includes(search.trim().toLowerCase()),
		),
	);
	const byOp = $derived(Object.fromEntries(OPS.map((op) => [op, matches.filter((r) => r.op === op)])));
	const kinds = $derived(kindCounts(rows.filter((r) => showRetired || r.is_current)));
	const summary = $derived(selected ? summaries[selected.id] : undefined);
	const composes = $derived(selected ? templatesFor(selected, templates) : []);
	const authorsLine = $derived(summary?.authors.join(', ') ?? '');

	function untaggedRow(r: LayerRow): boolean {
		if (!selected || !r.is_current) return false;
		return !selected.tag || !selected.tagged_at || r.created_at > selected.tagged_at;
	}

	let ticket = 0;
	async function choose(layer: Layer) {
		const t = ++ticket;
		selected = layer;
		rowsState = 'loading';
		rows = [];
		overrides = {};
		allAdds = false;
		kind = '';
		try {
			const [got, ov] = await Promise.all([
				api().layerRows(layer.id, { include_retired: true }),
				api()
					.layerOverrideTargets(layer.id)
					.catch(() => [] as LayerOverride[]),
			]);
			if (t !== ticket) return;
			rows = got.items;
			overrides = Object.fromEntries(ov.map((o) => [o.row_id, o]));
			rowsState = 'idle';
		} catch (e) {
			if (t !== ticket) return;
			rowsState = 'error';
			rowsError = (e as Error).message;
		}
	}

	function scrollTo(id: string) {
		queueMicrotask(() => document.getElementById(`row-${id}`)?.scrollIntoView?.({ block: 'center' }));
	}

	/** Follow an override to the row it acts on, in its own layer. */
	async function jump(layer: Layer, rowId: string) {
		await choose(layer);
		highlight = rowId;
		allAdds = true;
		scrollTo(rowId);
	}

	function byLabel(all: Layer[], label: string): Layer | undefined {
		return all.find((l) => layerLabel(l) === label);
	}

	// revision drawer
	let historyOpen = $state(false);
	let historyKey = $state('');
	let history = $state<LayerRow[]>([]);
	async function revisions(row: LayerRow) {
		historyKey = `${row.kind} ${row.key}`;
		history = [];
		historyOpen = true;
		try {
			history = await api().layerRowRevisions(row.layer_id, row.kind, row.key);
		} catch (e) {
			notice = (e as Error).message;
		}
	}

	onMount(async () => {
		await layersRes.refresh();
		const v = layersRes.value;
		if (v.state !== 'ok') return;
		// Rollups and templates enrich the page; it still works without them.
		void api()
			.layerSummaries()
			.then((s) => (summaries = Object.fromEntries(s.map((x) => [x.layer_id, x]))))
			.catch(() => {});
		void api()
			.compileReferences()
			.then((r) => (known = r.map((x) => ({ selection: x.selection, job: x.job }))))
			.catch(() => {});
		void api()
			.platformJobTemplates()
			.then((t) => (templates = t))
			.catch(() => {});
		void api()
			.departmentArchetypes()
			.then((a) => (archetypes = a))
			.catch(() => {});
		const target = page.url.searchParams.get('row');
		const layerParam = page.url.searchParams.get('layer');
		if (target) {
			try {
				const found = await api().findLayerRow(target);
				const layer = v.data.items.find((l) => l.id === found.layer.id) ?? found.layer;
				await choose(layer);
				highlight = target;
				allAdds = true;
				if (!found.row.is_current) {
					showRetired = true;
					notice = `Row ${target} is a retired revision (r${found.row.rev}) of ${found.row.key}; the current one is shown above it.`;
				}
				scrollTo(target);
			} catch {
				notice = `Row ${target} does not exist. Pick a layer to inspect its revision history.`;
				await choose(v.data.items[0]!);
			}
		} else if (layerParam) {
			// `?layer=<id or name>` (from Business models' axis rules, Compile preview's stack) opens that layer.
			const want = findLayer(v.data.items, layerParam);
			if (want) await choose(want);
			else {
				notice = `No layer named ${layerParam}. Pick one from the list.`;
				if (v.data.items[0]) await choose(v.data.items[0]);
			}
		} else if (v.data.items[0]) await choose(v.data.items[0]);
	});
</script>

<PageHead
	id="P4"
	sub="Who authored what, and what each layer changes. A job's config is these layers stacked in order — later layers override or remove what earlier ones added. Read-only; every edit is a new revision."
/>

{#if notice}<Callout kind="gap" title="Deep link">{notice}</Callout>{/if}

<LoadState resource={layersRes} loading="Loading layers…" empty="No layers yet.">
	{#snippet children(cat)}
		<Card title="How a job's config is composed" cls="compose-card">
			<ol class="compose" aria-label="Compile order">
				{#each SCOPES as scope, i (scope)}
					{@const group = cat.items.filter((l) => l.scope === scope)}
					{@const tagged = group.filter((l) => l.tag).length}
					<li>
						<button
							type="button"
							class="step"
							class:here={selected?.scope === scope}
							class:filtered={scopeFilter === scope}
							aria-pressed={scopeFilter === scope}
							onclick={() => (scopeFilter = scopeFilter === scope ? '' : scope)}
						>
							<span class="step-n">{i + 1}</span>
							<span class="step-title">{SCOPE_INFO[scope].title}</span>
							<span class="step-blurb">{SCOPE_INFO[scope].blurb}</span>
							<span class="step-foot"
								>{group.length} layer{group.length === 1 ? '' : 's'} · {tagged} tagged</span
							>
						</button>
					</li>
				{/each}
			</ol>
			<p class="legend">
				{#each ['add', 'patch', 'tombstone'] as const as op (op)}
					<span><Chip text={op} variant={OP_INFO[op].variant} /> {OP_INFO[op].hint}</span>
				{/each}
			</p>
			<p class="note">
				Later steps win. There is no tenant layer: tenants refine a released job instead. Try a combination in
				<a
					href={selected ? compileHref(selectionOfLayer(selected, known)) : '/platform/compile'}
					data-testid="layers-compile-link">Compile preview</a
				>{selected && selected.scope !== 'base' && selected.scope !== 'profile'
					? ` (opens with ${shortName(selected)} selected)`
					: ''}.
			</p>
		</Card>

		<div class="layers">
			<Card
				title="Layers"
				sub={scopeFilter
					? `${SCOPE_INFO[scopeFilter].title} only`
					: `${cat.items.length} layers, in compile order`}
			>
				{#snippet actions()}
					{#if scopeFilter}<button class="table-action" onclick={() => (scopeFilter = '')}>Show all</button
						>{/if}
				{/snippet}
				<nav class="tree" aria-label="Layers">
					{#each SCOPES.filter((s) => !scopeFilter || s === scopeFilter) as scope (scope)}
						{@const group = cat.items.filter((l) => l.scope === scope)}
						{#if group.length}
							<div class="label scope">{SCOPE_INFO[scope].title} <span>{group.length}</span></div>
							{#each group as layer (layer.id)}
								{@const s = summaries[layer.id]}
								<button
									type="button"
									class="node"
									class:on={selected?.id === layer.id}
									aria-current={selected?.id === layer.id}
									onclick={() => {
										highlight = null;
										choose(layer);
									}}
								>
									<span class="node-name"
										>{shortName(layer)}{#if layer.extends}<span class="node-ext">extends {layer.extends}</span
											>{/if}</span
									>
									{#if layer.tag}<Chip text={`@${tagVersion(layer.tag)}`} variant="tool" />{:else}<Chip
											text="draft"
											variant="warn"
										/>{/if}
									{#if s}
										<span class="node-ops mono" aria-label="Current rows by op">
											{#if s.adds}<span class="op-add">+{s.adds}</span>{/if}
											{#if s.patches}<span class="op-patch">~{s.patches}</span>{/if}
											{#if s.tombstones}<span class="op-del">−{s.tombstones}</span>{/if}
											{#if layer.tag && s.untagged}<span class="op-draft">{s.untagged} since tag</span>{/if}
										</span>
									{/if}
								</button>
							{/each}
						{/if}
					{/each}
				</nav>
			</Card>

			<div class="detail">
				{#if selected}
					<Card title={layerLabel(selected)} sub={SCOPE_INFO[selected.scope].blurb}>
						{#snippet actions()}
							<span class="chips">
								{#if selected?.tag}<Chip text={selected.tag} variant="tool" />{:else}<Chip
										text="draft — never tagged"
										variant="warn"
									/>{/if}
							</span>
						{/snippet}

						<div class="status" data-testid="tag-status">
							{#if !selected.tag}
								<Callout kind="gap" title="Draft layer">
									Not in any tag yet, so no template can pin it. Only compile previews see its rows until a
									super_admin tags it.
								</Callout>
							{:else if summary && summary.untagged > 0}
								<Callout
									kind="gap"
									title={`${summary.untagged} change${summary.untagged === 1 ? '' : 's'} since ${selected.tag}`}
								>
									Tagged {utc(selected.tagged_at)}; the revisions marked <em>draft</em> below came later. Jobs
									pinned to the tag do not get them until the next tag.
								</Callout>
							{:else}
								<Callout kind="info" title={`Everything here is in ${selected.tag}`}>
									Tagged {utc(selected.tagged_at)}. Templates pin this exact set of revisions.
								</Callout>
							{/if}
						</div>

						<dl class="facts">
							<div>
								<dt>Adds</dt>
								<dd class="op-add">{summary?.adds ?? current.filter((r) => r.op === 'add').length}</dd>
							</div>
							<div>
								<dt>Overrides</dt>
								<dd class="op-patch">{summary?.patches ?? current.filter((r) => r.op === 'patch').length}</dd>
							</div>
							<div>
								<dt>Removes</dt>
								<dd class="op-del">
									{summary?.tombstones ?? current.filter((r) => r.op === 'tombstone').length}
								</dd>
							</div>
							<div>
								<dt>Retired revisions</dt>
								<dd>{summary?.retired ?? rows.length - current.length}</dd>
							</div>
							<div class="wide">
								<dt>Authored by</dt>
								<dd class="small">
									{authorsLine || '—'}{#if summary?.last_change_at}<span class="cell-sub"
											>last change {utc(summary.last_change_at)}</span
										>{/if}
								</dd>
							</div>
						</dl>

						{#if selected.scope === 'industry_dept'}
							{@const arch = archetypeOfLayer(selected)}
							<section class="lineage" aria-label="Specialises" data-testid="dept-lineage">
								<h3 class="label">Specialises a base archetype (D60)</h3>
								<div class="lin-flow">
									<div class="lin-box base-box">
										<span class="lin-tier">base archetype</span>
										<b>{arch?.label ?? selected.extends ?? 'none declared'}</b>
										{#if arch}<span class="small muted"
												>{(arch.activities ?? []).slice(0, 3).join(' · ')}</span
											>{/if}
									</div>
									<span class="lin-arrow" aria-hidden="true">→</span>
									<div class="lin-box">
										<span class="lin-tier">this industry department</span>
										<b class="mono">{layerLabel(selected)}</b>
										{#if arch}
											{@const me = arch.specialised_by.find(
												(d) => d.industry === selected?.industry && d.key === selected?.department,
											)}
											<span class="small muted"
												>{me?.areas.length
													? `areas: ${me.areas.join(', ')}`
													: 'no areas: the whole department applies'}</span
											>
										{/if}
									</div>
								</div>
								<p class="small muted">
									The archetype holds the generic work (activities, request taxonomy, knowledge kinds,
									metrics, approval-rule slots); this layer adds what differs in {selected.industry}. See
									<a href="/platform/dimensions#arch-h">Dimensions → departments</a>.
								</p>
							</section>
						{:else if selected.scope === 'base' && archetypes.length}
							<section class="lineage" aria-label="Department archetypes" data-testid="base-archetypes">
								<h3 class="label">Department archetypes in base (D60)</h3>
								<ul class="arch-ul">
									{#each archetypes as a (a.key)}
										<li>
											<b>{a.label}</b>
											<span class="small muted">→</span>
											{#each a.specialised_by as d (d.industry + d.key)}
												<a
													class="mono small"
													href={d.layer_id ? `/platform/layers?layer=${d.layer_id}` : '/platform/layers'}
													>industry_dept/{d.industry}/{d.key}</a
												>
											{:else}
												<span class="small muted">no industry department yet</span>
											{/each}
										</li>
									{/each}
								</ul>
							</section>
						{/if}

						<section class="feeds" aria-label="Composes into">
							<h3 class="label">Composes into</h3>
							{#if selected.scope === 'base'}
								<p class="small">Every job: base is the first layer of every compile.</p>
							{:else if selected.scope === 'profile'}
								<p class="small">
									Any job whose business model fires the axis rule <code>{selected.rule}</code> — see
									<a href="/platform/business-models">Business models</a>.
								</p>
							{:else if selected.dimension}
								<p class="small">
									<code>job/{selected.job}</code> when
									<code>{selected.dimension[0]} = {selected.dimension[1]}</code>, applied after the job layer
									— see <a href="/platform/dimensions">Dimensions</a>.
								</p>
							{/if}
							{#each composes as c (`${c.template.layer}@${c.template.version}`)}
								<div class="chain" data-testid="compose-chain">
									<a class="chain-name" href={tagHref(c.template.layer, c.template.version)}
										>{c.template.label}
										<span class="mono muted">{c.template.layer}@v{c.template.version}</span></a
									>
									<ol class="pins">
										{#each c.template.pins as p (p.layer)}
											{@const l = byLabel(cat.items, p.layer)}
											{@const mine = p.layer === layerLabel(selected)}
											<li>
												<button
													type="button"
													class="pin"
													class:mine
													disabled={!l || mine}
													onclick={() => l && choose(l)}>{p.layer}@v{p.version}</button
												>
											</li>
										{/each}
									</ol>
									{#if c.template.status === 'draft'}<Chip text="template draft" variant="warn" />{/if}
									{#if selected.tag && `v${c.pinned}` !== tagVersion(selected.tag)}
										<span class="cell-sub">pins v{c.pinned}; the layer is at {tagVersion(selected.tag)}</span>
									{/if}
								</div>
							{:else}
								{#if selected.scope !== 'base' && selected.scope !== 'profile' && !selected.dimension}
									<p class="small muted">
										No template pins this layer yet. Tag it, then build a template on it in
										<a href="/platform/templates">Templates</a>.
									</p>
								{/if}
							{/each}
						</section>
					</Card>

					<Card title="Rows" sub="Grouped by what each row does to the composed config.">
						<div class="toolbar">
							<input
								class="input"
								placeholder="Find a key…"
								aria-label="Filter rows by key"
								bind:value={search}
							/>
							<label class="row"
								><input type="checkbox" bind:checked={showRetired} /> Show retired revisions</label
							>
						</div>
						{#if kinds.length > 1}
							<div class="kinds" role="group" aria-label="Filter by kind">
								<button type="button" class="kind" aria-pressed={!kind} onclick={() => (kind = '')}
									>All</button
								>
								{#each kinds as [k, n] (k)}
									<button
										type="button"
										class="kind"
										aria-pressed={kind === k}
										onclick={() => (kind = kind === k ? '' : k)}
										>{k.replace('_', ' ')} <span>{n}</span></button
									>
								{/each}
							</div>
						{/if}

						{#if rowsState === 'loading'}
							<p class="muted" role="status">Loading rows…</p>
						{:else if rowsState === 'error'}
							<Unavailable reason={rowsError} retry={() => selected && choose(selected)} />
						{:else if matches.length}
							{#each OPS as op (op)}
								{@const list = byOp[op] ?? []}
								{#if list.length}
									{@const capped =
										op === 'add' && !allAdds && !search && !kind && list.length > ADDS_PREVIEW
											? list.slice(0, ADDS_PREVIEW)
											: list}
									<section class="group" data-testid={`group-${op}`}>
										<h3 class="group-head">
											<Chip text={op} variant={OP_INFO[op].variant} />
											{SECTION[op].title} <span class="muted">{list.length}</span>
											<span class="group-sub">{SECTION[op].sub}</span>
										</h3>
										<div class="tbl-wrap">
											<table class="tbl dense">
												<thead
													><tr
														><th>Key</th><th
															>{op === 'add' ? 'Fields' : op === 'patch' ? 'Overrides' : 'Why'}</th
														><th class="r">Rev</th><th>Author</th><th></th></tr
													></thead
												>
												<tbody>
													{#each capped as row (row.id)}
														{@const cascade = cascadeOf(row, current)}
														{@const ov = overrides[row.id]}
														<tr
															id={`row-${row.id}`}
															class:retired={!row.is_current}
															class:hl={row.id === highlight}
															data-testid="layer-row"
														>
															<td>
																<code class="key">{row.key}</code>
																<span class="cell-sub"
																	>{row.kind.replace('_', ' ')} · {row.op} ·
																	<span class="mono">{row.id}</span></span
																>
																{#if !row.is_current}<Chip text="retired" />{/if}
																{#if untaggedRow(row) && selected?.tag}<Chip
																		text="draft"
																		variant="warn"
																	/>{/if}
															</td>
															<td>
																{#if row.op !== 'add' && ov}
																	<span class="small"
																		>{#if row.op === 'patch'}<strong
																				>{Object.keys(row.fields).join(', ')}</strong
																			>
																			from{:else}removes it from{/if}
																		{#if ov.target && ov.target_row_id}<button
																				type="button"
																				class="table-action"
																				onclick={() =>
																					ov.target && ov.target_row_id && jump(ov.target, ov.target_row_id)}
																				>{layerLabel(ov.target)}</button
																			>{:else}<span class="op-del"
																				>{row.op === 'patch'
																					? 'nothing: no earlier layer adds it'
																					: 'this layer (it was added here, then retired)'}</span
																			>{/if}</span
																	>
																{/if}
																<span class="cell-sub">{fieldLine(row)}</span>
																{#if cascade.length}
																	<span class="cell-sub cascade" data-testid="cascade"
																		>cascades to {cascade.length} row{cascade.length === 1 ? '' : 's'}: {cascade
																			.map((c) => c.key)
																			.join(', ')}</span
																	>
																{/if}
																<details>
																	<summary class="muted small"
																		>{Object.keys(row.fields).length} field{Object.keys(row.fields).length ===
																		1
																			? ''
																			: 's'}</summary
																	>
																	<JsonView value={row.fields} />
																</details>
															</td>
															<td class="r mono">r{row.rev}</td>
															<td class="muted"
																>{row.author}<span class="cell-sub mono">{utc(row.created_at)}</span></td
															>
															<td
																><button class="table-action" onclick={() => revisions(row)}>History</button
																></td
															>
														</tr>
													{/each}
												</tbody>
											</table>
										</div>
										{#if capped.length < list.length}
											<button type="button" class="btn more" onclick={() => (allAdds = true)}
												>Show all {list.length} adds</button
											>
										{/if}
									</section>
								{/if}
							{/each}
						{:else}
							<p class="empty">
								{rows.length
									? 'No rows match. Clear the filters, or show retired revisions.'
									: 'This layer has no rows yet.'}
							</p>
						{/if}
					</Card>
				{:else}
					<Card><p class="empty">Choose a layer to see what it changes.</p></Card>
				{/if}
			</div>
		</div>

		<Card
			title="Completeness"
			sub="Every layer × object type against the KC layer contract: what each layer must hold, and what a job layer must not."
			cls="completeness-card"
		>
			<CompletenessMatrix
				layers={cat.items}
				{archetypes}
				load={async (id) => (await api().layerRows(id)).items}
				onopen={(l) => {
					highlight = null;
					choose(l);
				}}
			/>
		</Card>
	{/snippet}
</LoadState>

<Drawer bind:open={historyOpen} title={historyKey} wide>
	{#snippet sub()}<span>Revision history · newest first · append-only</span>{/snippet}
	{#each history as r (r.id)}
		<article class="rev" class:retired={!r.is_current}>
			<div class="split">
				<strong>Revision {r.rev}</strong>
				<span class="chips">
					<Chip text={r.is_current ? 'current' : 'retired'} variant={r.is_current ? 'ok' : 'muted'} />
					<Chip text={r.op} variant={OP_INFO[r.op].variant} />
				</span>
			</div>
			<p class="note">{utc(r.created_at)} · {r.author} · <span class="mono">{r.id}</span></p>
			<JsonView value={r.fields} />
		</article>
	{:else}
		<p class="muted">Loading revisions…</p>
	{/each}
</Drawer>

<style>
	/* compile-order strip */
	.compose {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		grid-template-columns: repeat(5, minmax(0, 1fr));
		gap: 14px;
	}
	.compose li {
		position: relative;
		display: flex;
	}
	.compose li + li::before {
		content: '→';
		position: absolute;
		left: -12px;
		top: 50%;
		transform: translateY(-50%);
		color: var(--muted);
		font-size: 13px;
	}
	.step {
		flex: 1;
		display: grid;
		gap: 3px;
		align-content: start;
		text-align: left;
		padding: 10px 12px;
		border: 1px solid var(--line);
		border-radius: 8px;
		background: var(--panel);
		color: inherit;
		cursor: pointer;
		font: inherit;
	}
	.step:hover {
		background: var(--sunk);
	}
	.step.here {
		border-color: var(--accent);
		box-shadow: inset 0 0 0 1px var(--accent);
	}
	.step.filtered {
		background: color-mix(in srgb, var(--accent) 10%, transparent);
	}
	.step-n {
		font: 600 11px var(--mono);
		color: var(--muted);
	}
	.step-title {
		font-weight: 600;
		font-size: 14px;
	}
	.step-blurb {
		font-size: 12.5px;
		color: var(--muted);
		line-height: 1.35;
	}
	.step-foot {
		margin-top: 4px;
		font-size: 12px;
		font-family: var(--mono);
	}
	.legend {
		display: flex;
		flex-wrap: wrap;
		gap: 6px 18px;
		margin: 12px 0 0;
		font-size: 12.5px;
		color: var(--muted);
	}
	@media (max-width: 860px) {
		.compose {
			grid-template-columns: minmax(0, 1fr);
			gap: 16px;
		}
		.compose li + li::before {
			content: '↓';
			left: 14px;
			top: -15px;
			transform: none;
		}
	}

	.layers {
		display: grid;
		grid-template-columns: 290px minmax(0, 1fr);
		gap: 16px;
		align-items: start;
		margin-top: 16px;
	}
	.detail {
		display: grid;
		gap: 16px;
		min-width: 0;
	}
	@media (max-width: 860px) {
		.layers {
			grid-template-columns: minmax(0, 1fr);
		}
	}
	.tree {
		display: grid;
		gap: 2px;
	}
	.scope {
		display: flex;
		justify-content: space-between;
		padding: 12px 8px 4px;
	}
	.node {
		display: grid;
		grid-template-columns: minmax(0, 1fr) auto;
		align-items: center;
		gap: 2px 8px;
		border: 0;
		background: none;
		color: inherit;
		text-align: left;
		padding: 7px 8px;
		border-radius: 6px;
		cursor: pointer;
		font-size: 13px;
		overflow-wrap: anywhere;
	}
	.node:hover {
		background: var(--sunk);
	}
	.node.on {
		background: color-mix(in srgb, var(--accent) 12%, transparent);
	}
	.node.on .node-name {
		color: var(--accent);
		font-weight: 600;
	}
	.node-ops {
		grid-column: 1 / -1;
		display: flex;
		gap: 8px;
		font-size: 11.5px;
	}
	.op-add {
		color: var(--accent);
	}
	.op-patch {
		color: var(--warn);
	}
	.op-del {
		color: var(--err);
	}
	.op-draft {
		color: var(--warn);
		font-family: var(--sans);
	}

	.status {
		margin-bottom: 12px;
	}
	.facts {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(110px, 1fr));
		gap: 10px;
		margin: 0 0 14px;
	}
	.facts > div {
		border: 1px solid var(--line-2);
		border-radius: 8px;
		padding: 8px 10px;
	}
	.facts .wide {
		grid-column: span 2;
	}
	.facts dt {
		font-size: 11.5px;
		color: var(--muted);
		text-transform: uppercase;
		letter-spacing: 0.04em;
	}
	.facts dd {
		margin: 2px 0 0;
		font: 600 18px var(--mono);
	}
	.facts dd.small {
		font: 500 13px var(--sans);
	}
	@media (max-width: 520px) {
		.facts .wide {
			grid-column: auto;
		}
	}
	.node-ext {
		display: block;
		font-size: 10.5px;
		color: var(--muted);
		font-family: var(--mono);
	}
	.lineage {
		margin: 12px 0;
	}
	.lin-flow {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
		align-items: stretch;
		margin: 6px 0;
	}
	.lin-box {
		display: grid;
		gap: 2px;
		padding: 8px 10px;
		border: 1px solid var(--line);
		border-left: 3px solid var(--accent);
		border-radius: var(--r);
		min-width: 0;
		flex: 1 1 200px;
		overflow-wrap: anywhere;
	}
	.base-box {
		border-left-color: var(--muted);
		background: var(--sunk);
	}
	.lin-tier {
		font: 600 10.5px/1.2 var(--cond);
		letter-spacing: 0.07em;
		text-transform: uppercase;
		color: var(--muted);
	}
	.lin-arrow {
		align-self: center;
		color: var(--accent);
	}
	.arch-ul {
		margin: 6px 0 0;
		padding-left: 18px;
		display: grid;
		grid-template-columns: minmax(0, 1fr);
		gap: 4px;
		font-size: 13px;
	}
	.arch-ul li {
		min-width: 0;
		overflow-wrap: anywhere;
	}
	.arch-ul a {
		margin-right: 6px;
		overflow-wrap: anywhere;
	}
	.feeds h3 {
		margin: 0 0 6px;
	}
	.chain {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 6px 10px;
		padding: 8px 0;
		border-top: 1px solid var(--line-2);
	}
	.chain-name {
		font-weight: 600;
		font-size: 13px;
		display: grid;
	}
	.chain-name .mono {
		font-size: 11.5px;
		font-weight: 400;
	}
	.pins {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-wrap: wrap;
		gap: 4px;
	}
	.pins li + li::before {
		content: '›';
		color: var(--muted);
		margin-right: 4px;
	}
	.pin {
		font: 500 11.5px var(--mono);
		border: 1px solid var(--line);
		border-radius: 999px;
		padding: 2px 8px;
		background: none;
		color: inherit;
		cursor: pointer;
	}
	.pin:disabled {
		cursor: default;
	}
	.pin.mine {
		border-color: var(--accent);
		background: color-mix(in srgb, var(--accent) 14%, transparent);
		color: var(--accent);
		font-weight: 600;
	}

	.toolbar {
		display: flex;
		gap: 10px;
		flex-wrap: wrap;
		align-items: center;
		margin-bottom: 8px;
	}
	.toolbar .input {
		width: auto;
		min-width: 200px;
		flex: 0 1 280px;
	}
	.kinds {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		margin-bottom: 6px;
	}
	.kind {
		border: 1px solid var(--line);
		border-radius: 999px;
		padding: 3px 10px;
		background: none;
		color: inherit;
		font-size: 12.5px;
		cursor: pointer;
	}
	.kind span {
		color: var(--muted);
		font-family: var(--mono);
		margin-left: 2px;
	}
	.kind[aria-pressed='true'] {
		border-color: var(--accent);
		color: var(--accent);
		background: color-mix(in srgb, var(--accent) 10%, transparent);
	}
	.group {
		margin-top: 14px;
	}
	.group-head {
		display: flex;
		flex-wrap: wrap;
		align-items: baseline;
		gap: 4px 8px;
		font-size: 14px;
		margin: 0 0 6px;
	}
	.group-sub {
		flex-basis: 100%;
		font-weight: 400;
		font-size: 12.5px;
		color: var(--muted);
	}
	.more {
		margin-top: 8px;
	}
	.key {
		font: 600 12.5px var(--mono);
		color: var(--accent);
	}
	.small {
		font-size: 12.5px;
	}
	tr.retired td,
	article.retired {
		opacity: 0.55;
	}
	tr.hl td {
		background: color-mix(in srgb, var(--warn) 12%, transparent);
	}
	.cascade {
		color: var(--err);
	}
	.rev {
		display: grid;
		gap: 6px;
		padding-bottom: 14px;
		border-bottom: 1px solid var(--line-2);
	}
</style>
