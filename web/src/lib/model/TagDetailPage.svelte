<script lang="ts">
	// P7 Template tag detail (D15, K48). One immutable `layer@vN`: where it sits on the layer's version line and in
	// the composition stack, what changed against the previous tag (or any other), who is on it / behind it / past it,
	// what it contributes (objects, journey, operations, gates + roles, settings), and what is waiting for the next tag.
	// The decision it supports: is this tag what tenants on older pins should move to on their next cut?
	import { untrack } from 'svelte';
	import { api, guard } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import type { DiffChange, TagDiffRow, TagItem, TemplateTagDetail } from '$lib/api/types/model';
	import { diffItems, pinRelation, tagHref } from './logic';
	import { KIND_LABEL, SCOPE_LABEL, shortHash, when } from './ui';
	import TagTimeline from './TagTimeline.svelte';
	import TagStack from './TagStack.svelte';
	import TagContents from './TagContents.svelte';

	let { layer, version }: { layer: string; version: number } = $props();

	let retireBusy = $state(false);
	async function setRetired(retire: boolean) {
		retireBusy = true;
		await guard(
			() => (retire ? api().retireTemplateTag(layer, version) : api().restoreTemplateTag(layer, version)),
			retire
				? `${layer}@v${version} retired — hidden from new jobs; jobs that pin it keep working.`
				: `${layer}@v${version} restored — tenants can create jobs from it again.`,
			retire ? 'Not retired' : 'Not restored',
		);
		retireBusy = false;
		await res.refresh();
	}

	const res = new Resource<TemplateTagDetail>(() => api().templateTagDetail(layer, version));
	// Re-load when prev/next navigation changes the params on the same page instance.
	$effect(() => {
		void layer;
		void version;
		untrack(() => {
			compareWith = null;
			other = null;
			otherErr = '';
			res.refresh();
		});
	});
	const loaded = $derived(res.value.state === 'ok' ? res.value.data : null);

	/** null = the previous tag (the default diff). */
	let compareWith = $state<number | null>(null);
	let other = $state<TagItem[] | null>(null);
	let otherErr = $state('');

	async function loadCompare(v: number | null) {
		compareWith = v;
		other = null;
		otherErr = '';
		if (v === null) return;
		try {
			other = (await api().templateTagDetail(layer, v)).items;
		} catch (e) {
			otherErr = (e as Error).message;
		}
	}

	const CHANGE: Record<DiffChange, { glyph: string; variant: 'ok' | 'accent' | 'dead' }> = {
		added: { glyph: '+', variant: 'ok' },
		changed: { glyph: '~', variant: 'accent' },
		removed: { glyph: '−', variant: 'dead' },
	};
	const count = (rows: TagDiffRow[], c: DiffChange) => rows.filter((r) => r.change === c).length;
	const changedMap = (rows: TagDiffRow[]) =>
		new Map(rows.map((r) => [`${r.kind}\u0000${r.key}`, r.change] as const));
</script>

{#snippet diffList(rows: TagDiffRow[], empty: string)}
	{#if rows.length === 0}
		<p class="empty">{empty}</p>
	{:else}
		<p class="counts">
			<Chip text="+{count(rows, 'added')} added" variant="ok" />
			<Chip text="~{count(rows, 'changed')} changed" variant="accent" />
			<Chip text="−{count(rows, 'removed')} removed" variant="dead" />
		</p>
		<div class="tbl-wrap">
			<table class="tbl diff" data-testid="tag-diff">
				<thead><tr><th class="sr">Change</th><th>Item</th><th>Before → after</th></tr></thead>
				<tbody>
					{#each rows as r (r.kind + r.key)}
						<tr class="c-{r.change}">
							<td class="g"
								><span class="glyph" title={r.change} aria-label={r.change}>{CHANGE[r.change].glyph}</span
								><span class="sr">{r.change}</span></td
							>
							<td>
								<span class="mono">{r.key}</span>
								<span class="cell-sub">{KIND_LABEL[r.kind]}{r.label !== r.key ? ` · ${r.label}` : ''}</span>
							</td>
							<td>
								{#if r.before !== null}<span class="before">{r.before}</span>{/if}
								{#if r.before !== null && r.after !== null}<span class="arrow" aria-hidden="true">→</span
									>{/if}
								{#if r.after !== null}<span class="after">{r.after}</span>{/if}
							</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	{/if}
{/snippet}

<PageHead
	id="P7"
	title="{layer}@v{version}"
	sub={loaded
		? `${SCOPE_LABEL[loaded.layer.scope]} template · ${loaded.layer.label} — one immutable snapshot of what this layer contributes to a job`
		: 'Template tag'}
>
	{#snippet actions()}
		{#if loaded}
			{@const idx = loaded.versions.indexOf(loaded.tag.version)}
			{#if idx > 0}<a class="btn" href={tagHref(layer, loaded.versions[idx - 1]!)}
					>← @v{loaded.versions[idx - 1]}</a
				>{/if}
			{#if idx < loaded.versions.length - 1}<a class="btn" href={tagHref(layer, loaded.versions[idx + 1]!)}
					>@v{loaded.versions[idx + 1]} →</a
				>{/if}
			{#if loaded.retired_versions?.includes(loaded.tag.version)}
				<button class="btn" type="button" disabled={retireBusy} onclick={() => setRetired(false)}
					>Restore</button
				>
			{:else}
				<button class="btn" type="button" disabled={retireBusy} onclick={() => setRetired(true)}
					>Retire</button
				>
			{/if}
		{/if}
		<a class="btn" href="/platform/templates">All templates</a>
	{/snippet}
</PageHead>

<LoadState resource={res}>
	{#snippet children(d)}
		{@const latest = d.layer.latest?.version ?? d.tag.version}
		{@const behind = d.pins.filter((p) => p.behind > 0).length}
		{@const drafts = d.draft_revisions ?? []}
		{@const isLatest = latest === d.tag.version}

		{#if d.tag.provisional}
			<Callout kind="gap" title="Provisional tag — not immutable yet">
				<span>
					Written by generator run {d.layer.generator?.run ?? ''} ({d.layer.generator?.step ??
						'in progress'}). It cannot be cut into a release. Validation red blocks @v{d.tag.version + 1}: {d.layer.generator?.validation_red.join(
						'; ',
					) || 'none'}.
				</span>
			</Callout>
		{/if}

		<!-- 1. The tag on its version line -->
		<Card cls="hero">
			<div class="hero-head">
				<div class="seal" class:prov={d.tag.provisional}>
					<span class="seal-k">{d.tag.provisional ? 'Provisional' : 'Immutable tag'}</span>
					<span class="mono" title={d.tag.content_hash}>{shortHash(d.tag.content_hash)}</span>
				</div>
				<div class="hero-note">
					<p class="note">{d.tag.note}</p>
					<p class="byline">
						Tagged by {d.tag.tagged_by_name} · {when(d.tag.tagged_at)} · {d.items.length} items ·
						{#if isLatest}<b class="ok-t">latest tag</b>{:else}<b class="warn-t"
								>{latest - d.tag.version} tag{latest - d.tag.version === 1 ? '' : 's'} behind latest</b
							>
							(<a href={tagHref(layer, latest)}>@v{latest}</a>){/if}
						{#if d.layer.rollout_wave != null}· rollout wave {d.layer.rollout_wave} ({d.layer
								.rollout_label}){/if}
					</p>
				</div>
			</div>
			{#if d.tags?.length}
				<TagTimeline tags={d.tags} viewed={d.tag.version} pins={d.pins} drafts={drafts.length} />
			{/if}
			<p class="explain">
				A tag never changes: the content hash covers every item. Edits become new revisions and a new tag; a
				job moves to it only when its pin is re-pointed and a new release is cut (D15).
			</p>
		</Card>

		<div class="grid g2">
			<!-- 2. Who is on it, who would receive it -->
			<Card
				title="Pinned by {d.pins.length} job{d.pins.length === 1 ? '' : 's'} ({behind} behind)"
				sub="{d.pinned_on_this_tag} on this exact tag"
			>
				{#if d.pins.length === 0}
					<p class="empty">
						No job pins this layer yet. A job created from this template pins its latest tag.
					</p>
				{:else}
					<ul class="pins" data-testid="tag-pins">
						{#each d.pins as p (p.tenant_id + p.job)}
							{@const rel = pinRelation(p.version, d.tag.version)}
							{@const cu = d.catch_up?.find((c) => c.from === p.version)}
							<li class="pin r-{rel}">
								<div class="pin-who">
									<a href="/platform/tenants/{p.tenant_id}">{p.tenant_name}</a>
									{#if p.tenant_status !== 'active'}<Chip text={p.tenant_status} variant="muted" />{/if}
									<span class="cell-sub">{p.job_label}</span>
								</div>
								<a class="pin-ver mono" href={tagHref(d.tag.layer_key, p.version)}>@v{p.version}</a>
								<div class="pin-rel">
									{#if rel === 'here'}
										<Chip text="on this tag" variant="accent" />
									{:else if rel === 'behind'}
										<Chip text="before this tag" variant="warn" />
									{:else}
										<Chip text="past this tag" variant="muted" />
									{/if}
									{#if cu}
										<span class="cell-sub"
											>to @v{cu.to}: +{cu.added} ~{cu.changed} −{cu.removed} on next re-point + cut</span
										>
									{:else if p.behind === 0}
										<span class="cell-sub">on the latest tag</span>
									{/if}
								</div>
							</li>
						{/each}
					</ul>
					{#if behind > 0}
						<p class="hint">
							Jobs behind see a “base changed” badge (K5). Their admins pull the newer tag by re-pointing and
							cutting; overrides that collide show up as release conflicts.
						</p>
					{/if}
				{/if}
			</Card>

			<!-- 3. Where it sits in a job's composition -->
			<Card title="Where this layer sits" sub="Compiled bottom-up into every job that pins it">
				{#if d.stack?.length}
					<TagStack stack={d.stack} above={d.composed_on_by ?? []} />
				{/if}
			</Card>
		</div>

		<!-- 4. What changed -->
		<Card
			title={compareWith === null
				? d.previous === null
					? `What @v${d.tag.version} introduced`
					: `What changed in @v${d.tag.version}`
				: `@v${d.tag.version} compared with @v${compareWith}`}
			sub={compareWith === null
				? d.previous === null
					? 'First tag of this layer: everything is new.'
					: `Against the previous tag @v${d.previous}.`
				: compareWith < d.tag.version
					? `Read as @v${compareWith} → @v${d.tag.version}.`
					: `Read as @v${d.tag.version} → @v${compareWith}.`}
		>
			{#snippet actions()}
				{#if d.versions.length > 1}
					<label class="cmp">
						<span>Diff against</span>
						<select
							class="input"
							value={compareWith ?? ''}
							onchange={(e) =>
								loadCompare(e.currentTarget.value === '' ? null : Number(e.currentTarget.value))}
						>
							<option value=""
								>{d.previous === null ? 'nothing (first tag)' : `@v${d.previous} (previous)`}</option
							>
							{#each d.versions.filter((v) => v !== d.tag.version && v !== d.previous) as v (v)}
								<option value={v}>@v{v}</option>
							{/each}
						</select>
					</label>
				{/if}
			{/snippet}
			{#if compareWith === null}
				{@render diffList(
					d.diff_vs_previous,
					d.previous === null ? 'First tag of this layer.' : 'No changes against the previous tag.',
				)}
			{:else if otherErr}
				<p class="err-text">Could not load @v{compareWith}: {otherErr}</p>
			{:else if other}
				{@render diffList(
					compareWith < d.tag.version ? diffItems(other, d.items) : diffItems(d.items, other),
					'The two tags hold the same content.',
				)}
			{:else}
				<p class="muted">Loading @v{compareWith}…</p>
			{/if}
		</Card>

		<!-- 5. What it contributes -->
		<Card
			title="What @v{d.tag.version} contributes"
			sub="Only this layer's own items; lower layers in the stack add the rest."
		>
			<TagContents
				items={d.items}
				changed={changedMap(d.diff_vs_previous)}
				industry={d.layer.industry}
				version={d.tag.version}
			/>
		</Card>

		<!-- 6. Waiting to be packaged -->
		{#if drafts.length}
			<Card
				title="Waiting for @v{latest + 1}: {drafts.length} untagged revision{drafts.length === 1 ? '' : 's'}"
				sub={isLatest
					? 'Edited after this tag. No job sees them until they are packaged as a new tag and pinned.'
					: `Edited after @v${latest}, the latest tag.`}
			>
				{#snippet actions()}
					<a class="btn" href="/platform/templates">Package on Templates →</a>
				{/snippet}
				<ul class="drafts" data-testid="tag-drafts">
					{#each drafts as r (`${r.item.kind}\u0000${r.item.key}\u0000${r.rev}`)}
						<li>
							<Chip text={r.op} variant={r.op === 'tombstone' ? 'dead' : r.op === 'add' ? 'ok' : 'accent'} />
							<span class="mono">{r.item.key}</span>
							<span class="muted">{KIND_LABEL[r.item.kind]}</span>
							<span class="cell-sub">{r.item.detail} — rev {r.rev}, {r.by_name}, {when(r.at)}</span>
						</li>
					{/each}
				</ul>
			</Card>
		{/if}
	{/snippet}
</LoadState>

<style>
	:global(.hero) {
		margin-bottom: 16px;
	}
	.hero-head {
		display: flex;
		gap: 16px;
		align-items: flex-start;
		flex-wrap: wrap;
		margin-bottom: 14px;
	}
	.seal {
		display: flex;
		flex-direction: column;
		gap: 2px;
		padding: 8px 12px;
		border: 2px solid var(--ok);
		border-radius: var(--r);
		font-size: 13px;
	}
	.seal.prov {
		border-color: var(--warn);
		border-style: dashed;
	}
	.seal-k {
		font: 600 11px/1.2 var(--cond);
		letter-spacing: 0.06em;
		text-transform: uppercase;
		color: var(--ok);
	}
	.seal.prov .seal-k {
		color: var(--warn);
	}
	.hero-note {
		flex: 1 1 300px;
		min-width: 0;
	}
	.note {
		margin: 0;
		font-size: 16px;
		font-weight: 500;
	}
	.byline {
		margin: 4px 0 0;
		font-size: 12px;
		color: var(--muted);
	}
	.ok-t {
		color: var(--ok);
		font-weight: 600;
	}
	.warn-t {
		color: var(--warn);
		font-weight: 600;
	}
	.explain,
	.hint {
		margin: 12px 0 0;
		font-size: 12px;
		color: var(--muted);
	}
	.grid {
		margin-bottom: 16px;
	}
	.pins {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.pin {
		display: grid;
		grid-template-columns: minmax(0, 1fr) auto;
		gap: 4px 12px;
		align-items: center;
		padding: 8px 0;
		border-bottom: 1px solid var(--line-2);
		font-size: 13px;
	}
	.pin.r-here {
		background: color-mix(in srgb, var(--accent) 6%, transparent);
	}
	.pin-who {
		text-align: left;
		min-width: 0;
	}
	.pin-ver {
		font-weight: 600;
		justify-self: end;
	}
	.pin-rel {
		grid-column: 1 / -1;
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 4px 8px;
	}
	.pin-rel :global(.cell-sub) {
		display: inline;
	}
	.counts {
		display: flex;
		gap: 6px;
		flex-wrap: wrap;
		margin: 0 0 8px;
	}
	.diff .g {
		width: 28px;
	}
	.glyph {
		display: inline-flex;
		width: 20px;
		height: 20px;
		align-items: center;
		justify-content: center;
		border-radius: 50%;
		font: 700 13px/1 var(--mono);
		color: var(--panel);
	}
	.c-added .glyph {
		background: var(--ok);
	}
	.c-changed .glyph {
		background: var(--accent);
	}
	.c-removed .glyph {
		background: var(--dead);
	}
	.before {
		color: var(--muted);
		text-decoration: line-through;
		text-decoration-color: color-mix(in srgb, var(--muted) 50%, transparent);
	}
	.c-removed .before {
		color: var(--dead);
	}
	.arrow {
		margin: 0 6px;
		color: var(--muted);
	}
	.after {
		color: var(--fg);
	}
	.sr {
		position: absolute;
		width: 1px;
		height: 1px;
		overflow: hidden;
		clip: rect(0 0 0 0);
		white-space: nowrap;
	}
	.cmp {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		font-size: 13px;
		color: var(--muted);
	}
	.cmp .input {
		width: auto;
	}
	.drafts {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.drafts li {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 4px 8px;
		padding: 6px 0;
		border-bottom: 1px solid var(--line-2);
		font-size: 13px;
	}
	.drafts li :global(.cell-sub) {
		flex-basis: 100%;
	}
	.err-text {
		color: var(--err);
	}
</style>
