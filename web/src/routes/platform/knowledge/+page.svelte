<script lang="ts">
	// PK (T8, T9) Platform knowledge — the super_admin curator on the platform plane (the platform tenant's strata_rag
	// stores). 1. Stores: a tree of groups and leaves. 2. Documents: text with topic keys (tenants replace platform
	// text by topic) and provenance (an https source, a licence strata allows, the retrieval date). 3. Packs: register a
	// draft over a store tree, publish (immutable from then on), retire (no longer listed; pins stay readable).
	// 4. Reference a published pack from a template layer for a stage/kind (or `*`): that writes a `knowledge_pack`
	// row and cuts the layer family's next tag — jobs adopt it like any template change.
	import { onMount } from 'svelte';
	import { api, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { superAdminAttrs } from '$lib/auth/roles';
	import { Resource } from '$lib/load.svelte';
	import { STAGES } from '$lib/ontology/catalogue';
	import {
		PLATFORM_LICENSE_HINTS,
		type CuratorDocument,
		type CuratorDocumentPage,
		type CuratorPack,
		type CuratorPackStores,
		type CuratorStore,
		type PackReferenceResult,
	} from '$lib/api/types/ontology';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Freshness from '$lib/ui/Freshness.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import { when } from '$lib/tenant/ui';
	import { PACK_KEY_RE, checkCuratorDocument, provenanceText, rfc3339, splitTopics } from '$lib/pk/pk';
	import type { Provenance } from '$lib/api/types/grounding';

	const stores = new Resource<CuratorStore[]>(() => api().curatorStores());
	const packs = new Resource<CuratorPack[]>(() => api().curatorPacks());
	/** The layers a pack can apply to: base, every industry and department (P21), retired ones left out. */
	let layerChoices = $state<{ key: string; label: string }[]>([{ key: 'base', label: 'Base (every job)' }]);
	async function loadLayers() {
		try {
			const d = await api().platformIndustries();
			layerChoices = [
				{ key: 'base', label: 'Base (every job)' },
				...d.items
					.filter((i) => !i.retired_at)
					.flatMap((i) => [
						{ key: i.key, label: i.label },
						...i.departments
							.filter((x) => !x.retired_at)
							.map((x) => ({ key: x.key, label: `${i.label} › ${x.label}` })),
					]),
			];
		} catch {
			// the free-text layer field still works without the list
		}
	}
	onMount(() => void Promise.all([stores.refresh(), packs.refresh(), loadLayers()]));
	const sa = $derived(superAdminAttrs(session));

	const all = $derived(stores.value.state === 'ok' ? stores.value.data : []);
	const nameOf = (id: string) => all.find((s) => s.id === id)?.name ?? id.slice(0, 8);
	/** The tree, depth-first, roots by name. */
	const tree = $derived.by(() => {
		const out: { s: CuratorStore; depth: number }[] = [];
		const walk = (parent: string | null, depth: number) => {
			for (const s of all
				.filter((x) => (x.parent_id ?? null) === parent)
				.sort((a, b) => a.name.localeCompare(b.name))) {
				out.push({ s, depth });
				walk(s.id, depth + 1);
			}
		};
		walk(null, 0);
		return out;
	});
	const leaves = $derived(tree.filter((t) => t.s.kind === 'leaf'));

	// ---- stores
	let storeName = $state('');
	let storeParent = $state('');
	async function createStore(e: SubmitEvent) {
		e.preventDefault();
		const name = storeName.trim();
		if (!name) return;
		const ok = await guard(
			() => api().createCuratorStore({ name, parent_id: storeParent || null }),
			`Store ${name} created.`,
		);
		if (ok) storeName = '';
		await stores.refresh();
	}

	// ---- documents of the picked leaf
	let picked = $state('');
	let docs = $state<CuratorDocumentPage | null>(null);
	let docsErr = $state('');
	async function loadDocs() {
		docs = null;
		docsErr = '';
		if (!picked) return;
		try {
			docs = await api().curatorDocuments(picked);
		} catch (e) {
			docsErr = e instanceof Error ? e.message : String(e);
		}
	}
	$effect(() => {
		if (picked) void loadDocs();
	});
	const provOf = (d: CuratorDocument) => (d.metadata?.provenance ?? null) as Provenance | null;
	const topicsOf = (d: CuratorDocument) =>
		Array.isArray(d.metadata?.topic_keys) ? (d.metadata.topic_keys as string[]) : [];

	let title = $state('');
	let text = $state('');
	let topics = $state('');
	let sourceUrl = $state('');
	let license = $state('CC-BY-4.0');
	let retrieved = $state(new Date().toISOString().slice(0, 10));
	let sourceId = $state('');
	let tried = $state(false);
	const docErrors = $derived(
		checkCuratorDocument({
			title,
			text,
			topic_keys: splitTopics(topics),
			source_url: sourceUrl,
			license,
			retrieved_at: rfc3339(retrieved),
		}),
	);
	async function ingest(e: SubmitEvent) {
		e.preventDefault();
		tried = true;
		if (!picked || docErrors.length) return;
		const ok = await guard(
			() =>
				api().ingestCuratorDocument(picked, {
					title: title.trim(),
					text,
					topic_keys: splitTopics(topics),
					provenance: {
						source_url: sourceUrl.trim(),
						license: license.trim(),
						retrieved_at: rfc3339(retrieved),
					},
					...(sourceId.trim() ? { source_id: sourceId.trim() } : {}),
				}),
			`“${title.trim()}” ingested into ${nameOf(picked)}.`,
			'Not ingested',
		);
		if (ok) {
			title = '';
			text = '';
			topics = '';
			sourceId = '';
			tried = false;
		}
		await Promise.all([loadDocs(), stores.refresh()]);
	}
	async function removeDoc(d: CuratorDocument) {
		await guard(() => api().deleteCuratorDocument(d.id), `“${d.title ?? d.source}” deleted.`, 'Not deleted');
		await loadDocs();
	}

	// ---- packs
	const stateOf = (p: CuratorPack) => (p.retired_at ? 'retired' : p.published_at ? 'published' : 'draft');
	const STATE_VARIANT = { draft: 'warn', published: 'ok', retired: 'muted' } as const;
	let packKey = $state('');
	let packVersion = $state(1);
	let packStore = $state('');
	const packKeyBad = $derived(!!packKey.trim() && !PACK_KEY_RE.test(packKey.trim()));
	async function register(e: SubmitEvent) {
		e.preventDefault();
		const key = packKey.trim();
		if (!key || packKeyBad || !packStore) return;
		const version = Math.round(Number(packVersion) || 1);
		const ok = await guard(
			() => api().registerPack({ key, version, store_id: packStore }),
			`${key}@${version} registered as a draft.`,
		);
		if (ok) packKey = '';
		await packs.refresh();
	}
	async function lifecycle(p: CuratorPack, op: 'publish' | 'retire') {
		const pin = `${p.key}@${p.version}`;
		await guard(
			() => (op === 'publish' ? api().publishPack(p.key, p.version) : api().retirePack(p.key, p.version)),
			op === 'publish'
				? `${pin} published: its stores are immutable now; tenants can pin it.`
				: `${pin} retired: no longer listed; existing pins stay readable.`,
		);
		await packs.refresh();
	}

	// ---- a pack's stores (strata's subtree of its root, drafts and retired packs too)
	let storesOf = $state<{ pin: string; data: CuratorPackStores | null; error: string | null } | null>(null);
	async function showStores(p: CuratorPack) {
		const pin = `${p.key}@${p.version}`;
		storesOf = { pin, data: null, error: null };
		try {
			const data = await api().curatorPackStores(p.key, p.version);
			if (storesOf?.pin === pin) storesOf = { pin, data, error: null };
		} catch (e) {
			if (storesOf?.pin === pin)
				storesOf = { pin, data: null, error: e instanceof Error ? e.message : String(e) };
		}
	}

	// ---- references
	let refFor = $state<CuratorPack | null>(null);
	let layer = $state('base');
	let stage = $state('*');
	let kind = $state('*');
	let lastRef = $state<{ pin: string; op: string; result: PackReferenceResult } | null>(null);
	async function reference(op: 'reference' | 'unreference') {
		const p = refFor;
		if (!p || !layer.trim() || !stage.trim() || !kind.trim()) return;
		const input = { layer: layer.trim(), stage: stage.trim(), kind: kind.trim() };
		const box: { r: PackReferenceResult | null } = { r: null };
		const ok = await guard(
			async () => {
				box.r =
					op === 'reference'
						? await api().referencePack(p.key, p.version, input)
						: await api().unreferencePack(p.key, input);
			},
			() =>
				op === 'reference'
					? `${input.layer} now pins ${p.key}@${p.version} for ${input.stage}/${input.kind}; tagged ${box.r?.tag ?? ''}.`
					: `${input.layer} no longer pins ${p.key} for ${input.stage}/${input.kind}; tagged ${box.r?.tag ?? ''}.`,
		);
		if (ok && box.r) lastRef = { pin: `${p.key}@${p.version}`, op, result: box.r };
	}
</script>

<PageHead
	id="P4"
	title="Platform knowledge"
	sub="Curate the knowledge every tenant can search: platform stores, documents with topics and provenance, versioned packs, and which template layers pin them."
>
	{#snippet actions()}
		<Freshness resource={packs} />
		<a class="btn" href="/platform/industries">Industries</a>
		<a class="btn" href="/platform/layers">Layers</a>
	{/snippet}
</PageHead>

<div class="pkc-grid">
	<Card title="Stores" sub="Groups hold stores; leaves hold documents. A published pack freezes its stores.">
		<LoadState resource={stores} empty="No platform stores yet: create the first one below.">
			{#snippet children(_rows)}
				<ul class="list-plain pkc-tree" data-testid="pkc-stores">
					{#each tree as t (t.s.id)}
						<li style="padding-left: {t.depth * 16}px">
							{#if t.s.kind === 'leaf'}
								<button
									type="button"
									class="btn sm ghost"
									class:on={picked === t.s.id}
									aria-pressed={picked === t.s.id}
									onclick={() => (picked = t.s.id)}>{t.s.name}</button
								>
							{:else}
								<b>{t.s.name}</b>
							{/if}
							<span class="muted pkc-small"
								>{t.s.kind}{typeof t.s.chunk_count === 'number' ? ` · ${t.s.chunk_count} chunks` : ''}</span
							>
						</li>
					{/each}
				</ul>
			{/snippet}
		</LoadState>
		<form class="pkc-inline" onsubmit={createStore}>
			<input class="input mono" bind:value={storeName} placeholder="store-name" aria-label="New store name" />
			<select class="input" bind:value={storeParent} aria-label="Parent store">
				<option value="">(root)</option>
				{#each tree as t (t.s.id)}<option value={t.s.id}>{'· '.repeat(t.depth)}{t.s.name}</option>{/each}
			</select>
			<button class="btn sm" type="submit" {...sa} disabled={sa.disabled || !storeName.trim()}
				>Create store</button
			>
		</form>
	</Card>

	<Card
		title={picked ? `Documents · ${nameOf(picked)}` : 'Documents'}
		sub="Pick a leaf store to list and add its documents."
	>
		{#if !picked}
			<div class="empty">Pick a leaf store on the left.</div>
		{:else}
			{#if docsErr}<Callout kind="err" title="Could not list the documents">{docsErr}</Callout>{/if}
			{#if docs}
				{#if docs.items.length}
					<ul class="list-plain pkc-docs" data-testid="pkc-docs">
						{#each docs.items as d (d.id)}
							{@const prov = provOf(d)}
							<li>
								<b>{d.title ?? d.source}</b>
								<span class="pkc-small muted"
									>{topicsOf(d).join(', ') || 'no topics'}{prov ? ` · ${provenanceText(prov)}` : ''}</span
								>
								{#if prov?.source_url}<a
										class="pkc-small mono"
										href={prov.source_url}
										rel="noopener noreferrer"
										target="_blank">{prov.source_url}</a
									>{/if}
								<button class="btn sm ghost" type="button" {...sa} onclick={() => removeDoc(d)}>Delete</button
								>
							</li>
						{/each}
					</ul>
				{:else}
					<div class="empty">No documents in this store yet.</div>
				{/if}
			{:else if !docsErr}
				<p class="muted" role="status">Loading…</p>
			{/if}
			<form class="pkc-form" onsubmit={ingest} data-testid="pkc-doc-form">
				<label class="field"><span class="label">Title</span><input class="input" bind:value={title} /></label
				>
				<label class="field span"
					><span class="label">Text</span><textarea class="input" rows="6" bind:value={text}
					></textarea></label
				>
				<label class="field span"
					><span class="label">Topic keys (comma separated)</span><input
						class="input mono"
						bind:value={topics}
						placeholder="returns.window, eu.withdrawal"
					/><span class="cell-sub">A tenant's own text replaces platform text by these keys (1–32).</span
					></label
				>
				<label class="field span"
					><span class="label">Source URL</span><input
						class="input mono"
						type="url"
						bind:value={sourceUrl}
						placeholder="https://"
					/></label
				>
				<label class="field"
					><span class="label">Licence</span><input
						class="input mono"
						list="pkc-licences"
						bind:value={license}
						aria-label="Licence"
					/><datalist id="pkc-licences"
						>{#each PLATFORM_LICENSE_HINTS as l (l)}<option value={l}></option>{/each}</datalist
					><span class="cell-sub">strata refuses a licence it does not allow.</span></label
				>
				<label class="field"
					><span class="label">Retrieved</span><input
						class="input"
						type="date"
						bind:value={retrieved}
					/></label
				>
				<label class="field"
					><span class="label">Source id (optional)</span><input
						class="input mono"
						bind:value={sourceId}
					/></label
				>
				{#if tried && docErrors.length}
					<ul class="pkc-errs span" role="alert">
						{#each docErrors as m (m)}<li>{m}</li>{/each}
					</ul>
				{/if}
				<div class="row span">
					<button class="btn primary" type="submit" {...sa}>Ingest document</button>
				</div>
			</form>
		{/if}
	</Card>
</div>

<Card
	title="Packs"
	sub="A pack is a versioned, immutable store tree once published. Retired packs stay readable for existing pins."
>
	<LoadState resource={packs} empty="No packs yet: register one over a store tree below.">
		{#snippet children(rows)}
			<div class="tbl-wrap">
				<table class="tbl" data-testid="pkc-packs">
					<thead><tr><th>Pack</th><th>State</th><th>Store tree</th><th>Published</th><th></th></tr></thead>
					<tbody>
						{#each rows as p (`${p.key}@${p.version}`)}
							{@const st = stateOf(p)}
							<tr data-pack={`${p.key}@${p.version}`}>
								<td class="mono">{p.key}@{p.version}</td>
								<td><Chip text={st} variant={STATE_VARIANT[st]} /></td>
								<td class="mono">{nameOf(p.root_id)}</td>
								<td class="muted">{p.published_at ? when(p.published_at) : '—'}</td>
								<td class="right pkc-actions">
									<button class="btn sm ghost" type="button" onclick={() => showStores(p)}>Stores</button>
									{#if st === 'draft'}
										<button class="btn sm" type="button" {...sa} onclick={() => lifecycle(p, 'publish')}
											>Publish</button
										>
									{:else if st === 'published'}
										<button class="btn sm" type="button" {...sa} onclick={() => (refFor = p)}
											>Reference…</button
										>
										<button class="btn sm ghost" type="button" {...sa} onclick={() => lifecycle(p, 'retire')}
											>Retire</button
										>
									{/if}
								</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		{/snippet}
	</LoadState>

	{#if storesOf}
		<div class="pkc-ref" data-testid="pkc-pack-stores">
			<h3>Stores of <span class="mono">{storesOf.pin}</span></h3>
			{#if storesOf.error}
				<p class="err-text">{storesOf.error}</p>
			{:else if !storesOf.data}
				<p class="muted">Loading…</p>
			{:else if storesOf.data.stores.length === 0}
				<p class="muted">This pack's root holds no stores.</p>
			{:else}
				<ul class="pkc-pack-stores">
					{#each storesOf.data.stores as x (x.id)}
						<li>
							<span class="mono">{x.path.join(' / ')}</span>
							<span class="muted"
								>{x.kind} · {x.chunk_count} chunks{x.embed_model ? ` · ${x.embed_model}` : ''}</span
							>
						</li>
					{/each}
				</ul>
			{/if}
			<div class="row">
				<button class="btn sm ghost" type="button" onclick={() => (storesOf = null)}>Close</button>
			</div>
		</div>
	{/if}

	{#if refFor}
		<div class="pkc-ref" data-testid="pkc-ref">
			<h3>Reference <span class="mono">{refFor.key}@{refFor.version}</span> from a template layer</h3>
			<div class="pkc-inline">
				<label class="field"
					><span class="label">Layer</span><input
						class="input mono"
						list="pkc-layers"
						bind:value={layer}
						aria-label="Layer"
					/><datalist id="pkc-layers"
						>{#each layerChoices as c (c.key)}<option value={c.key}>{c.label}</option>{/each}</datalist
					></label
				>
				<label class="field"
					><span class="label">Stage</span><select class="input" bind:value={stage} aria-label="Stage">
						<option value="*">* every stage</option>
						{#each STAGES as [k, l] (k)}<option value={k}>{l}</option>{/each}
					</select></label
				>
				<label class="field"
					><span class="label">Kind</span><input
						class="input mono"
						bind:value={kind}
						aria-label="Kind"
					/></label
				>
			</div>
			<div class="row">
				<button class="btn primary sm" type="button" {...sa} onclick={() => reference('reference')}
					>Reference + tag</button
				>
				<button class="btn sm" type="button" {...sa} onclick={() => reference('unreference')}
					>Remove reference + tag</button
				>
				<button class="btn sm ghost" type="button" onclick={() => (refFor = null)}>Close</button>
			</div>
			<p class="muted pkc-small">
				Each change writes a <span class="mono">knowledge_pack</span> row on the layer and cuts the family's next
				tag; jobs pick it up when they adopt the tag.
			</p>
		</div>
	{/if}
	{#if lastRef}
		<p class="pkc-small" data-testid="pkc-last-ref">
			{lastRef.op === 'reference' ? 'Referenced' : 'Unreferenced'} <span class="mono">{lastRef.pin}</span> →
			tag
			<b class="mono">{lastRef.result.tag}</b>
		</p>
	{/if}

	<form class="pkc-inline pkc-register" onsubmit={register}>
		<input
			class="input mono"
			bind:value={packKey}
			placeholder="industry/retail/returns"
			aria-label="Pack key"
		/>
		<input class="input num" type="number" min="1" bind:value={packVersion} aria-label="Version" />
		<select class="input" bind:value={packStore} aria-label="Store tree">
			<option value="">Store tree…</option>
			{#each tree as t (t.s.id)}<option value={t.s.id}>{'· '.repeat(t.depth)}{t.s.name}</option>{/each}
		</select>
		<button
			class="btn sm"
			type="submit"
			{...sa}
			disabled={sa.disabled || !packKey.trim() || packKeyBad || !packStore}>Register draft</button
		>
		{#if packKeyBad}<span class="err-text pkc-small">1–6 segments of a-z, 0-9, _ or -, separated by /</span
			>{/if}
	</form>
</Card>

<style>
	.pkc-grid {
		display: grid;
		grid-template-columns: minmax(0, 1fr) minmax(0, 2fr);
		gap: 12px;
		margin-bottom: 12px;
	}
	@media (max-width: 860px) {
		.pkc-grid {
			grid-template-columns: minmax(0, 1fr);
		}
	}
	.pkc-tree li,
	.pkc-docs li {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		align-items: center;
		padding: 3px 0;
		overflow-wrap: anywhere;
	}
	.pkc-tree .on {
		font-weight: 700;
		color: var(--accent);
	}
	.pkc-docs li {
		border-bottom: 1px solid var(--line-2);
		padding: 6px 0;
	}
	.pkc-small {
		font-size: 12px;
	}
	.pkc-inline {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
		align-items: end;
		margin-top: 10px;
	}
	.pkc-form {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));
		gap: 8px;
		margin-top: 12px;
		border-top: 1px solid var(--line-2);
		padding-top: 10px;
	}
	.pkc-form .span {
		grid-column: 1 / -1;
	}
	.pkc-errs {
		color: var(--err);
		font-size: 12.5px;
		margin: 0;
		padding-left: 18px;
	}
	.pkc-ref {
		margin-top: 12px;
		border-top: 1px solid var(--line-2);
		padding-top: 10px;
	}
	.pkc-ref h3 {
		font-size: 13.5px;
		margin: 0;
	}
	.pkc-actions {
		white-space: nowrap;
	}
	.right {
		text-align: right;
	}
	.err-text {
		color: var(--err);
	}
</style>
