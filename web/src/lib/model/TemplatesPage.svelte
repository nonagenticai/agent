<script lang="ts">
	// P6 Templates (D15, D26, 11 §4.1, §6.1 template_tag). The page answers three questions for the super-admin:
	//   1. what does each job actually run? — one immutable tag per layer, stacked base → industry → dept → job;
	//   2. what is waiting to be tagged? — each layer's tag timeline ends in its untagged draft (Package step);
	//   3. who is behind? — a pin on an older tag than the latest; that tenant sees the K5 base-changed badge and
	//      reaches the newer tag only through a new release cut.
	import { superAdminAttrs } from '$lib/auth/roles';
	import { onMount } from 'svelte';
	import { api, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { Resource } from '$lib/load.svelte';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Seg from '$lib/ui/Seg.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import type { RolloutWave, TemplateCatalog, TemplateLayerSummary } from '$lib/api/types/model';
	import { tagHref } from './logic';
	import { SCOPE_LABEL, KIND_LABEL, shortHash, when } from './ui';
	import { jobStacks, layerTimeline, pipelineCounts, rolloutWaves, type WaveStatus } from './templatesP6';

	const WAVE_FALLBACK: Record<RolloutWave, string> = {
		1: 'Customer care',
		2: 'IT service desk',
		3: 'Other job areas',
	};
	const WAVE_CHIP: Record<WaveStatus, [string, 'ok' | 'accent' | 'muted']> = {
		done: ['Done', 'ok'],
		current: ['In progress', 'accent'],
		todo: ['Not started', 'muted'],
	};

	const res = new Resource<TemplateCatalog>(
		() => api().templateCatalog(),
		(d) => d.layers.length === 0,
	);
	onMount(() => res.refresh());

	let jobFilter = $state('all');
	let openKey = $state<string | null>(null);
	let tagKey = $state<string | null>(null);
	let layerOpen = $state(false);
	let tagOpen = $state(false);
	let note = $state('');
	let busy = $state(false);

	const catalog = $derived(res.value.state === 'ok' ? res.value.data : null);
	const openLayer = $derived(catalog?.layers.find((l) => l.layer_key === openKey) ?? null);
	const tagLayer = $derived(catalog?.layers.find((l) => l.layer_key === tagKey) ?? null);
	const nextV = (l: TemplateLayerSummary) => (l.latest?.version ?? 0) + 1;
	const short = (k: string) => k.split('/').slice(1).join('/') || k;

	function openLayerDrawer(key: string) {
		openKey = key;
		layerOpen = true;
	}
	function openTag(key: string) {
		tagKey = key;
		tagOpen = true;
	}

	async function tag() {
		const l = tagLayer;
		if (!l) return;
		busy = true;
		const v = nextV(l);
		const ok = await guard(
			() => api().tagLayerRevisions(l.layer_key, note),
			`Tagged ${l.layer_key}@v${v}. Jobs reach it only through a new release cut.`,
			'Not tagged',
		);
		busy = false;
		if (ok) {
			tagOpen = false;
			note = '';
			await res.refresh();
		}
	}
</script>

<PageHead
	id="P6"
	sub="What every job runs, what is waiting to be tagged, and who is behind. Templates are linked, not copied: a job pins one immutable tag per layer (D15)."
>
	{#snippet actions()}
		<a class="btn" href="/platform/objects">Objects</a>
		<a class="btn" href="/platform/operations">Operations</a>
	{/snippet}
</PageHead>

<LoadState resource={res} empty="No template layers yet. The generator's Package step writes the first tag.">
	{#snippet children(c)}
		{@const n = pipelineCounts(c)}
		{@const stacks = jobStacks(c)}
		{@const waves = rolloutWaves(c, WAVE_FALLBACK)}

		<!-- How a template change travels: the model in one line, with the live counts at each stage. -->
		<ol class="pipe" aria-label="How a template change reaches a job" data-testid="template-pipeline">
			<li>
				<span class="pipe-n">{n.drafts}</span>
				<b>Untagged revisions</b>
				<span class="muted">Overlay edits and generator output, not yet in any tag.</span>
			</li>
			<li>
				<span class="pipe-n">{n.tags}</span>
				<b>Immutable tags <span class="mono">@vN</span></b>
				<span class="muted">The Package step freezes revisions; a tag never changes.</span>
			</li>
			<li>
				<span class="pipe-n">{n.pins}</span>
				<b>Pins across {n.jobs} jobs</b>
				<span class="muted">Each job pins one tag per layer. A new tag moves no one.</span>
			</li>
			<li class:hot={n.behindPins > 0}>
				<span class="pipe-n">{n.behindPins}</span>
				<b>Pins behind latest</b>
				<span class="muted"
					>{n.behindJobs} job{n.behindJobs === 1 ? '' : 's'} show the K5 base-changed badge until the tenant cuts
					a new release.</span
				>
			</li>
		</ol>

		<Card
			title="What each job runs"
			sub="Read bottom-up: base, then industry, department and job overlays; a later layer overrides an earlier one. Amber = a newer tag exists."
		>
			{#snippet actions()}
				<Seg
					label="Jobs"
					bind:active={jobFilter}
					items={[
						['all', `All ${stacks.length}`],
						['behind', `Behind ${n.behindJobs}`],
					]}
				/>
			{/snippet}
			{@const shown = stacks.filter((s) => jobFilter === 'all' || s.behind > 0)}
			{#if shown.length === 0}
				<p class="empty">Every job is on the latest tag of every layer it pins.</p>
			{/if}
			<div class="stacks" data-testid="tenant-lag">
				{#each shown as s (s.tenant_id + s.job)}
					<article
						class="stack-card"
						class:dim={s.tenant_status === 'archived'}
						data-job={`${s.tenant_name} · ${s.job}`}
						aria-label="{s.tenant_name} · {s.job_label}"
					>
						<header>
							<div>
								<a href="/platform/tenants/{s.tenant_id}">{s.tenant_name}</a>
								{#if s.tenant_status !== 'active'}<Chip
										text={s.tenant_status}
										variant={s.tenant_status === 'suspended' ? 'warn' : 'dead'}
									/>{/if}
								<span class="cell-sub">{s.job_label} · <span class="mono">{s.job}</span></span>
							</div>
							{#if s.behind}<Chip text="{s.behind} behind" variant="warn" />{:else}<Chip
									text="latest"
									variant="ok"
								/>{/if}
						</header>
						<ol class="layers">
							{#each [...s.slots].reverse() as slot (slot.scope)}
								<li
									class="slot {slot.scope}"
									class:behind={(slot.pin?.behind ?? 0) > 0}
									class:none={!slot.layer}
								>
									<span class="slot-scope">{SCOPE_LABEL[slot.scope]}</span>
									{#if slot.layer && slot.pin}
										<button
											class="slot-key mono"
											type="button"
											onclick={() => openLayerDrawer(slot.layer!.layer_key)}
											>{short(slot.layer.layer_key)}</button
										>
										<span class="slot-pin mono">
											<a href={tagHref(slot.layer.layer_key, slot.pin.version)}>@v{slot.pin.version}</a>
											{#if slot.pin.behind && slot.layer.latest}
												<span aria-hidden="true">→</span>
												<a
													href={tagHref(slot.layer.layer_key, slot.layer.latest.version)}
													title="Newer tag available">@v{slot.layer.latest.version}</a
												>
											{/if}
										</span>
									{:else}
										<span class="muted slot-none">none</span>
									{/if}
								</li>
							{/each}
						</ol>
						<footer>
							{#if s.behind}
								<span
									>Badge on. A newer tag reaches this job only through a new release cut by
									{s.tenant_name}.</span
								>
							{:else}
								<span class="muted">On the latest tag of every layer it pins.</span>
							{/if}
						</footer>
					</article>
				{/each}
			</div>
		</Card>

		<Card
			title="Tag timelines, in rollout order"
			sub="D26: customer care first, then IT service desk, then the other job areas. Jobs sit on the tag they pin; the dashed node is the next tag."
		>
			<div class="waves" aria-label="Template rollout order" data-testid="template-layers">
				{#each waves as w (w.wave)}
					<section class="wave" aria-labelledby="wave-{w.wave}">
						<header class="wave-head">
							<span class="wave-num" aria-hidden="true">{w.wave ?? '·'}</span>
							<h3 id="wave-{w.wave}">{w.label}</h3>
							<Chip text={WAVE_CHIP[w.status][0]} variant={WAVE_CHIP[w.status][1]} />
							<span class="muted wave-sub"
								>{w.layers.length} layer{w.layers.length === 1 ? '' : 's'}{w.tenants.length
									? ` · ${w.tenants.join(', ')}`
									: ' · no tenant yet'}</span
							>
						</header>
						{#if w.layers.length === 0}
							<p class="empty">No layers in this wave yet.</p>
						{/if}
						{#each w.layers as l (l.layer_key)}
							{@const tl = layerTimeline(l)}
							<div class="tl-row" data-layer={l.layer_key}>
								<div class="tl-meta">
									<button class="table-action mono" type="button" onclick={() => openLayerDrawer(l.layer_key)}
										>{l.layer_key}</button
									>
									<span class="cell-sub">{SCOPE_LABEL[l.scope]} · {l.label}</span>
									{#if l.generator}
										<span class="cell-sub"
											><Chip text="generator {l.generator.run}" variant="accent" /> {l.generator.step}</span
										>
									{/if}
								</div>
								<ol class="track scroll-x" aria-label="{l.layer_key} tags">
									{#each tl.nodes as node (node.tag.version)}
										<li class="node" class:latest={node.latest} class:prov={node.tag.provisional}>
											<span class="dot" aria-hidden="true"></span>
											<a class="mono v" href={tagHref(l.layer_key, node.tag.version)}>@v{node.tag.version}</a>
											{#if node.tag.provisional}<Chip
													text="provisional"
													variant="warn"
												/>{:else if node.latest}<span class="cell-sub">latest</span>{/if}
											<span class="cell-sub">{when(node.tag.tagged_at).replace(/, \d\d:\d\d$/, '')}</span>
											{#each node.pins as p (p.tenant_id + p.job)}
												<span class="pin" class:behind={p.behind > 0} title="{p.job_label} · {p.job}"
													>{p.tenant_name}{p.behind ? ` · ${p.behind} behind` : ''}</span
												>
											{/each}
										</li>
									{/each}
									{#if tl.draft}
										<li class="node draft">
											<span class="dot" aria-hidden="true"></span>
											<span class="mono v">@v{tl.draft.next}</span>
											<span class="cell-sub">{tl.draft.count} untagged</span>
											<button
												class="btn sm"
												type="button"
												{...superAdminAttrs(session)}
												onclick={() => openTag(l.layer_key)}>Tag as @v{tl.draft.next}</button
											>
										</li>
									{:else}
										<li class="node idle">
											<span class="cell-sub">all tagged</span>
										</li>
									{/if}
								</ol>
							</div>
						{/each}
					</section>
				{/each}
			</div>
		</Card>
	{/snippet}
</LoadState>

<Drawer bind:open={layerOpen} title={openLayer?.layer_key ?? ''} wide>
	{#snippet sub()}{openLayer ? `${SCOPE_LABEL[openLayer.scope]} · ${openLayer.label}` : ''}{/snippet}
	{#if openLayer}
		<div class="stack">
			{#if openLayer.generator}
				<Callout
					kind="gap"
					title="Generator run {openLayer.generator.run} in progress — {openLayer.generator.step}"
				>
					<span>Validation red blocks tagging: {openLayer.generator.validation_red.join('; ')}.</span>
					<a href="/platform/generator">Open the generator</a>
				</Callout>
			{/if}
			<h3>Tags</h3>
			<ul class="list-plain">
				{#each openLayer.tags as t (t.version)}
					<li>
						<span>
							<a class="mono" href={tagHref(openLayer.layer_key, t.version)}>@v{t.version}</a>
							{#if t.provisional}<Chip text="provisional" variant="warn" />{:else}<Chip
									text="immutable"
									variant="muted"
								/>{/if}
							<span class="cell-sub">{t.note}</span>
						</span>
						<span class="muted"
							>{when(t.tagged_at)} · {t.tagged_by_name} ·
							<span class="mono">{shortHash(t.content_hash)}</span>
							· {t.item_count} items</span
						>
					</li>
				{/each}
			</ul>
			<h3>Untagged revisions</h3>
			{#if openLayer.draft_revisions.length === 0}
				<p class="muted">None. Every revision of this layer is in a tag.</p>
			{:else}
				<div class="tbl-wrap">
					<table class="tbl dense">
						<thead><tr><th>Rev</th><th>Op</th><th>Kind</th><th>Key</th><th>Detail</th><th>By</th></tr></thead>
						<tbody>
							{#each openLayer.draft_revisions as d (`${d.item.kind}:${d.item.key}:${d.item.region ?? ''}:${d.rev}`)}
								<tr>
									<td class="mono">r{d.rev}</td>
									<td
										><Chip
											text={d.op}
											variant={d.op === 'add' ? 'ok' : d.op === 'tombstone' ? 'dead' : 'accent'}
										/></td
									>
									<td>{KIND_LABEL[d.item.kind]}</td>
									<td class="mono">{d.item.key}</td>
									<td>{d.item.detail}</td>
									<td><span class="cell-sub">{d.by_name} · {when(d.at)}</span></td>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
			{/if}
			<h3>Pinned by</h3>
			{#if openLayer.pins.length === 0}
				<p class="muted">No job pins this layer yet.</p>
			{:else}
				<div class="tbl-wrap">
					<table class="tbl dense">
						<thead><tr><th>Tenant</th><th>Job</th><th>Pin</th><th>Status</th></tr></thead>
						<tbody>
							{#each openLayer.pins as p (p.tenant_id + p.job)}
								<tr>
									<td
										>{p.tenant_name}{#if p.tenant_status !== 'active'}
											<Chip text={p.tenant_status} variant="muted" />{/if}</td
									>
									<td>{p.job_label} <span class="cell-sub mono">{p.job}</span></td>
									<td class="mono"><a href={tagHref(openLayer.layer_key, p.version)}>@v{p.version}</a></td>
									<td
										>{#if p.behind}<Chip text="{p.behind} behind" variant="warn" />{:else}<Chip
												text="latest"
												variant="ok"
											/>{/if}</td
									>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
			{/if}
		</div>
	{/if}
</Drawer>

<Drawer
	bind:open={tagOpen}
	title={tagLayer ? `Tag ${tagLayer.layer_key} as @v${nextV(tagLayer)}` : ''}
	dirty={note.length > 0}
>
	{#snippet sub()}Package step · writes an immutable template_tag row{/snippet}
	{#if tagLayer}
		<div class="stack">
			<p>
				{tagLayer.draft_revisions.length} untagged revision{tagLayer.draft_revisions.length === 1 ? '' : 's'} become
				<b class="mono">{tagLayer.layer_key}@v{nextV(tagLayer)}</b>. The tag cannot change afterwards.
			</p>
			<ul class="list-plain">
				{#each tagLayer.draft_revisions as d (`${d.item.kind}:${d.item.key}:${d.item.region ?? ''}:${d.rev}`)}
					<li>
						<span><Chip text={d.op} variant="accent" /> <span class="mono">{d.item.key}</span></span><span
							class="muted">{d.item.detail}</span
						>
					</li>
				{/each}
			</ul>
			{#if tagLayer.generator?.validation_red.length}
				<Callout kind="err" title="Validation red — the tag will be refused">
					<span>{tagLayer.generator.validation_red.join('; ')}.</span>
					<a href="/platform/generator">Open the generator</a>
				</Callout>
			{/if}
			{#if tagLayer.pinned_jobs}
				<Callout
					kind="info"
					title="{tagLayer.pinned_jobs} job{tagLayer.pinned_jobs === 1 ? '' : 's'} will fall behind"
				>
					<span
						>{tagLayer.pins.map((p) => `${p.tenant_name} (@v${p.version})`).join(', ')} keep their pin and get the
						K5 base-changed badge. Nothing changes for them until they cut and apply a new release.</span
					>
				</Callout>
			{/if}
			<div class="field">
				<label for="tag-note">Tag note</label>
				<input id="tag-note" class="input" bind:value={note} placeholder="What this tag adds" />
			</div>
			<div class="row">
				<button
					class="btn primary"
					type="button"
					{...superAdminAttrs(session)}
					disabled={busy || !session.superAdmin}
					onclick={tag}>Tag as @v{nextV(tagLayer)}</button
				>
				<button class="btn" type="button" onclick={() => ((note = ''), (tagOpen = false))}>Cancel</button>
			</div>
		</div>
	{/if}
</Drawer>

<style>
	/* Pipeline band: Revise → Tag → Pin → Behind */
	.pipe {
		list-style: none;
		margin: 0 0 16px;
		padding: 0;
		display: grid;
		grid-template-columns: repeat(4, minmax(0, 1fr));
		gap: 0;
		border: 1px solid var(--line);
		border-radius: var(--r);
		background: var(--panel);
		overflow: hidden;
	}
	.pipe li {
		position: relative;
		display: grid;
		gap: 3px;
		padding: 12px 16px 12px 18px;
		font-size: 12.5px;
	}
	.pipe li + li {
		border-left: 1px solid var(--line);
	}
	.pipe li + li::before {
		content: '';
		position: absolute;
		left: -6px;
		top: 50%;
		width: 10px;
		height: 10px;
		background: var(--panel);
		border-top: 1px solid var(--line);
		border-right: 1px solid var(--line);
		transform: translateY(-50%) rotate(45deg);
	}
	.pipe-n {
		font: 600 24px/1 var(--cond);
		color: var(--fg);
	}
	.pipe li.hot {
		background: color-mix(in srgb, var(--warn) 8%, var(--panel));
	}
	.pipe li.hot .pipe-n {
		color: var(--warn);
	}
	.pipe li.hot::before {
		background: color-mix(in srgb, var(--warn) 8%, var(--panel));
	}

	/* Job stacks */
	.stacks {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(240px, 1fr));
		gap: 12px;
	}
	.stack-card {
		display: grid;
		grid-template-rows: auto 1fr auto;
		gap: 10px;
		padding: 12px;
		border: 1px solid var(--line);
		border-radius: var(--r);
		background: var(--panel);
		min-width: 0;
	}
	.stack-card.dim {
		opacity: 0.72;
	}
	.stack-card header {
		display: flex;
		justify-content: space-between;
		align-items: flex-start;
		gap: 8px;
	}
	.stack-card header a {
		font-weight: 600;
	}
	.stack-card footer {
		font-size: 12px;
		color: var(--warn);
	}
	.layers {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 3px;
	}
	.slot {
		display: grid;
		grid-template-columns: 74px minmax(0, 1fr) auto;
		align-items: center;
		gap: 6px;
		padding: 6px 8px;
		border-radius: 5px;
		border-left: 3px solid var(--line-2);
		background: var(--sunk);
		font-size: 12px;
	}
	.slot.base {
		border-left-color: var(--muted);
	}
	.slot.industry {
		border-left-color: var(--tool);
	}
	.slot.industry_dept {
		border-left-color: var(--cust);
	}
	.slot.job {
		border-left-color: var(--accent);
	}
	.slot.behind {
		background: color-mix(in srgb, var(--warn) 12%, var(--panel));
		border-left-color: var(--warn);
	}
	.slot.none {
		background: transparent;
		border: 1px dashed var(--line);
		border-left: 3px dashed var(--line);
	}
	.slot-scope {
		font: 600 10.5px/1 var(--cond);
		letter-spacing: 0.05em;
		text-transform: uppercase;
		color: var(--muted);
	}
	.slot-key {
		all: unset;
		cursor: pointer;
		overflow-wrap: anywhere;
		color: var(--fg);
	}
	.slot-key:hover,
	.slot-key:focus-visible {
		text-decoration: underline;
	}
	.slot-pin {
		white-space: nowrap;
	}
	.slot.behind .slot-pin {
		color: var(--warn);
	}
	.slot-none {
		font-size: 11.5px;
	}

	/* Rollout waves + tag timelines */
	.waves {
		display: grid;
		gap: 18px;
	}
	.wave-head {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 8px;
		padding-bottom: 8px;
		border-bottom: 1px solid var(--line);
	}
	.wave-num {
		display: inline-grid;
		place-items: center;
		width: 22px;
		height: 22px;
		border-radius: 50%;
		border: 1px solid var(--line-2);
		font: 600 12px/1 var(--mono);
	}
	.wave-head h3 {
		margin: 0;
		font: 600 14px/1.2 var(--sans);
		text-transform: none;
		letter-spacing: 0;
		color: var(--fg);
	}
	.wave-sub {
		font-size: 12px;
	}
	.tl-row {
		display: grid;
		grid-template-columns: minmax(180px, 260px) minmax(0, 1fr);
		gap: 12px;
		padding: 12px 0;
		border-bottom: 1px dashed var(--line);
	}
	.tl-row:last-child {
		border-bottom: 0;
	}
	.tl-meta {
		display: grid;
		align-content: start;
		gap: 3px;
		min-width: 0;
	}
	.tl-meta .table-action {
		text-align: left;
		overflow-wrap: anywhere;
	}
	.track {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		gap: 0;
		overflow-x: auto;
		position: relative;
	}
	.node {
		position: relative;
		display: grid;
		align-content: start;
		justify-items: start;
		gap: 3px;
		min-width: 130px;
		padding: 0 12px 4px 0;
	}
	/* the rail: a line through the dots, from each node to the next */
	.node::before {
		content: '';
		position: absolute;
		top: 6px;
		left: 0;
		right: 0;
		height: 2px;
		background: var(--line-2);
	}
	.node:last-child::before {
		right: auto;
		width: 14px;
	}
	.dot {
		position: relative;
		width: 14px;
		height: 14px;
		border-radius: 50%;
		background: var(--panel);
		border: 2px solid var(--muted);
		margin-bottom: 4px;
	}
	.node.latest .dot {
		background: var(--accent);
		border-color: var(--accent);
	}
	.node.prov .dot {
		border-style: dashed;
		border-color: var(--warn);
	}
	.node.draft .dot {
		border-style: dashed;
		border-color: var(--accent);
	}
	.node.draft::before {
		background: repeating-linear-gradient(90deg, var(--line-2) 0 4px, transparent 4px 8px);
	}
	.node.idle {
		min-width: 0;
		padding-top: 18px;
	}
	.node.idle::before {
		display: none;
	}
	.v {
		font-weight: 600;
	}
	.pin {
		display: inline-block;
		max-width: 150px;
		font: 500 11px/1.4 var(--mono);
		padding: 1px 6px;
		border-radius: 4px;
		background: color-mix(in srgb, var(--ok) 14%, transparent);
		color: var(--ok);
	}
	.pin.behind {
		background: color-mix(in srgb, var(--warn) 16%, transparent);
		color: var(--warn);
	}

	h3 {
		margin: 6px 0 0;
		font: 600 13px/1.2 var(--cond);
		letter-spacing: 0.04em;
		text-transform: uppercase;
		color: var(--muted);
	}
	.empty {
		color: var(--muted);
		font-size: 12.5px;
	}

	@media (max-width: 760px) {
		.pipe {
			grid-template-columns: repeat(2, minmax(0, 1fr));
		}
		.pipe li:nth-child(3) {
			border-left: 0;
			border-top: 1px solid var(--line);
		}
		.pipe li:nth-child(4) {
			border-top: 1px solid var(--line);
		}
		.pipe li:nth-child(3)::before {
			display: none;
		}
		.tl-row {
			grid-template-columns: minmax(0, 1fr);
		}
	}
</style>
