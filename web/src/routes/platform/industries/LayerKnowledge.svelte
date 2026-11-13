<script lang="ts">
	// The knowledge one template layer (base, an industry, a department) gives every job built on it: the packs it
	// references (a `knowledge_pack` row `<stage>|<kind>|<pack>`), attach a published pack, remove one. Each change
	// writes the row and cuts the layer family's next tag (`referencePack` / `unreferencePack`), so tenants pick it
	// up like any template change.
	import { api, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { superAdminAttrs } from '$lib/auth/roles';
	import { STAGES } from '$lib/ontology/catalogue';
	import type { CuratorPack } from '$lib/api/types/ontology';
	import type { LayerPackRef } from '$lib/api/types/platform';

	let {
		layer,
		refs,
		packs,
		disabled = false,
		onchange,
	}: {
		/** The layer key: `base`, `industry/<i>`, `industry_dept/<i>/<d>`. */
		layer: string;
		refs: LayerPackRef[];
		/** The curator's packs; only published, not retired ones can be attached. */
		packs: CuratorPack[];
		disabled?: boolean;
		onchange: () => Promise<void>;
	} = $props();
	const sa = $derived(superAdminAttrs(session));
	const usable = $derived(packs.filter((p) => p.published_at && !p.retired_at));
	const stageLabel = (k: string) => (k === '*' ? 'every stage' : (STAGES.find((s) => s[0] === k)?.[1] ?? k));
	const kindLabel = (k: string) => (k === '*' ? 'every kind' : k);

	let pin = $state('');
	let stage = $state('*');
	let kind = $state('*');
	let busy = $state(false);

	async function attach(e: SubmitEvent) {
		e.preventDefault();
		const p = usable.find((x) => `${x.key}@${x.version}` === pin);
		if (!p || !kind.trim()) return;
		busy = true;
		const input = { layer, stage, kind: kind.trim() };
		const ok = await guard(
			() => api().referencePack(p.key, p.version, input),
			`${p.key} v${p.version} now applies to ${layer}.`,
		);
		busy = false;
		if (ok) {
			pin = '';
			await onchange();
		}
	}

	async function remove(r: LayerPackRef) {
		busy = true;
		const ok = await guard(
			() => api().unreferencePack(r.pack, { layer, stage: r.stage, kind: r.kind }),
			`${r.pack} no longer applies to ${layer}.`,
			'Not removed',
		);
		busy = false;
		if (ok) await onchange();
	}
</script>

<div class="lk" data-testid="layer-knowledge" data-layer={layer}>
	{#if refs.length}
		<ul class="list-plain lk-list">
			{#each refs as r (`${r.stage}|${r.kind}|${r.pack}`)}
				<li>
					<span class="mono">{r.pack}</span>{#if r.version !== null}<span class="muted">
							v{r.version}</span
						>{/if}
					<span class="muted">· {stageLabel(r.stage)} · {kindLabel(r.kind)}</span>
					<button
						class="btn sm ghost"
						type="button"
						{...sa}
						disabled={busy || disabled}
						onclick={() => remove(r)}
						aria-label="Remove {r.pack} from {layer}">Remove</button
					>
				</li>
			{/each}
		</ul>
	{:else}
		<p class="muted">No knowledge on this layer yet.</p>
	{/if}
	<form class="lk-add" onsubmit={attach}>
		<label class="field"
			><span class="label">Pack</span><select class="input" bind:value={pin} aria-label="Pack for {layer}">
				<option value="">Choose a published pack…</option>
				{#each usable as p (`${p.key}@${p.version}`)}<option value="{p.key}@{p.version}"
						>{p.key} v{p.version}</option
					>{/each}
			</select></label
		>
		<label class="field"
			><span class="label">Stage</span><select
				class="input"
				bind:value={stage}
				aria-label="Stage for {layer}"
			>
				<option value="*">Every stage</option>
				{#each STAGES as [k, l] (k)}<option value={k}>{l}</option>{/each}
			</select></label
		>
		<label class="field"
			><span class="label">Kind</span><input
				class="input mono"
				bind:value={kind}
				aria-label="Knowledge kind for {layer}"
			/></label
		>
		<button class="btn sm primary" type="submit" {...sa} disabled={busy || disabled || !pin}>Attach</button>
	</form>
	{#if !usable.length}
		<p class="muted lk-hint">
			No published pack yet: create and publish one in <a href="/platform/knowledge">Platform knowledge</a>.
		</p>
	{/if}
</div>

<style>
	.lk-list li {
		display: flex;
		gap: 6px;
		align-items: center;
		flex-wrap: wrap;
		padding: 2px 0;
	}
	.lk-add {
		display: flex;
		gap: 8px;
		align-items: flex-end;
		flex-wrap: wrap;
		margin-top: 8px;
	}
	.lk-hint {
		margin-top: 4px;
	}
</style>
