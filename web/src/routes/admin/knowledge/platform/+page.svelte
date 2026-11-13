<script lang="ts">
	// PK-10 (T7, T9) Platform knowledge for one tenant: the platform packs this tenant may pin, grouped by key family
	// (base / industry / industry department / job), and its tenant-wide exclusions. An exclusion names a pack key or
	// a key prefix (segment-wise: `industry/retail` covers `industry/retail/hr`, never `industry/retailx`) and a reason;
	// every job of the tenant stops searching those packs AT ONCE — this is live on save, not a draft change (the
	// exclusion lives in strata_rag, not in a job's design). Restore removes it. A single job hides one template pin
	// on its Knowledge kinds page instead.
	import { onMount } from 'svelte';
	import { api, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { canWrite as canWriteFn, writeAttrs } from '$lib/auth/roles';
	import { Resource } from '$lib/load.svelte';
	import type { PlatformExclusion } from '$lib/api/types/tenant';
	import type { PlatformPack } from '$lib/api/types/ontology';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Freshness from '$lib/ui/Freshness.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';
	import { when } from '$lib/tenant/ui';
	import {
		FAMILY_LABEL,
		FAMILY_ORDER,
		MAX_REASON_CHARS,
		checkPrefix,
		checkReason,
		familyOf,
	} from '$lib/pk/pk';

	const res = new Resource<PlatformExclusion[]>(() => api().platformExclusions());
	const packs = new Resource<PlatformPack[]>(() => api().platformPacks());
	const refresh = () => Promise.all([res.refresh(), packs.refresh()]);
	onMount(() => void refresh());
	const canWrite = $derived(canWriteFn(session, 'knowledge_manager'));

	/** The listing grouped by family, newest version first within a key. */
	const families = $derived.by(() => {
		const list = packs.value.state === 'ok' ? packs.value.data : [];
		return FAMILY_ORDER.map((f) => ({
			family: f,
			packs: list
				.filter((p) => familyOf(p.key) === f)
				.sort((a, b) => a.key.localeCompare(b.key) || b.version - a.version),
		})).filter((g) => g.packs.length);
	});

	// One exclusion form, prefilled from a pack or a family row.
	let prefix = $state('');
	let reason = $state('');
	let saving = $state(false);
	let restoring = $state<string | null>(null);
	const problem = $derived(
		prefix.trim() || reason.trim() ? (checkPrefix(prefix) ?? checkReason(reason)) : null,
	);

	function start(p: string) {
		prefix = p;
		reason = '';
		requestAnimationFrame(() => document.getElementById('pk-reason')?.focus());
	}
	async function exclude(e: SubmitEvent) {
		e.preventDefault();
		if (saving || checkPrefix(prefix) || checkReason(reason)) return;
		saving = true;
		const p = prefix.trim();
		const ok = await guard(
			() => api().setPlatformExclusion(p, reason.trim()),
			`${p} excluded: no job searches it any more. Live now.`,
		);
		saving = false;
		if (ok) {
			prefix = '';
			reason = '';
		}
		await refresh();
	}
	async function restore(x: PlatformExclusion) {
		restoring = x.key_prefix;
		await guard(
			() => api().removePlatformExclusion(x.key_prefix),
			`${x.key_prefix} restored: jobs that pin it search it again. Live now.`,
		);
		restoring = null;
		await refresh();
	}
</script>

<PageHead
	id="T7"
	title="Platform knowledge"
	sub="Curated knowledge the platform provides — law, industry practice — that your jobs' knowledge kinds can search next to your own stores. Exclude what does not apply to your business."
>
	{#snippet actions()}
		<Freshness resource={res} />
		<a class="btn" href="/admin/knowledge/stores">Knowledge stores</a>
	{/snippet}
</PageHead>

<Callout kind="info" title="Changes here are live on save">
	An exclusion applies to every job of this tenant at once — it is not a draft change and needs no release. To
	leave one pack out of one job only, hide it on that job's Knowledge kinds page.
</Callout>

<div class="pk-grid">
	<Card title="Excluded" sub="Packs whose key is the prefix or sits below it are never searched.">
		<LoadState resource={res} empty="Nothing is excluded: every pinned platform pack is searched.">
			{#snippet children(rows)}
				<div class="tbl-wrap">
					<table class="tbl" data-testid="pk-exclusions">
						<thead><tr><th>Prefix</th><th>Reason</th><th>Since</th><th></th></tr></thead>
						<tbody>
							{#each rows as x (x.key_prefix)}
								<tr data-prefix={x.key_prefix}>
									<td class="mono">{x.key_prefix}</td>
									<td>{x.reason}</td>
									<td class="muted">{x.created_at ? when(x.created_at) : '—'}</td>
									<td class="right"
										><button
											class="btn sm"
											type="button"
											{...writeAttrs(session, 'knowledge_manager')}
											disabled={!canWrite || restoring === x.key_prefix}
											onclick={() => restore(x)}>Restore</button
										></td
									>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
			{/snippet}
		</LoadState>
	</Card>

	<Card title="Exclude" sub="A pack key (industry/retail/returns) or a whole branch (industry/retail).">
		<form class="pk-form" onsubmit={exclude} data-testid="pk-exclude-form">
			<label class="field"
				><span class="label">Key or prefix</span><input
					class="input mono"
					bind:value={prefix}
					placeholder="industry/retail"
					aria-label="Key or prefix"
				/></label
			>
			<label class="field"
				><span class="label">Reason</span><input
					id="pk-reason"
					class="input"
					bind:value={reason}
					maxlength={MAX_REASON_CHARS}
					placeholder="Why it does not apply to you"
					aria-label="Reason"
				/></label
			>
			{#if problem}<p class="err-text pk-small" role="alert">{problem}</p>{/if}
			<div class="row">
				<button
					class="btn primary"
					type="submit"
					{...writeAttrs(session, 'knowledge_manager')}
					disabled={!canWrite || saving || !prefix.trim() || !reason.trim() || !!problem}
					>{saving ? 'Excluding…' : 'Exclude now'}</button
				>
			</div>
		</form>
	</Card>
</div>

<Card title="Platform packs you can pin" sub="Grouped by the template layer family they belong to.">
	{#if packs.value.state === 'unavailable'}
		<Unavailable reason={packs.value.reason} retry={packs.retry} />
	{:else if packs.value.state === 'loading'}
		<p class="muted" role="status">Loading…</p>
	{:else if !families.length}
		<div class="empty">
			No platform packs are listed for this tenant (platform knowledge is off, or everything is excluded).
		</div>
	{:else}
		<div class="pk-families" data-testid="pk-families">
			{#each families as g (g.family)}
				<section data-family={g.family}>
					<h3>
						{FAMILY_LABEL[g.family]}
						{#if g.family !== 'other'}<button
								class="btn sm ghost"
								type="button"
								disabled={!canWrite}
								title="Prefill an exclusion of the whole {g.family} family"
								onclick={() => start(g.family)}>Exclude all {g.family}…</button
							>{/if}
					</h3>
					<ul class="list-plain">
						{#each g.packs as p (p.pin)}
							<li>
								<Chip text={p.pin} variant="accent" />
								<span class="muted pk-small"
									>{p.published_at ? `published ${when(p.published_at)}` : 'not published'}</span
								>
								<button
									class="btn sm"
									type="button"
									disabled={!canWrite}
									title="Prefill an exclusion of {p.key}"
									onclick={() => start(p.key)}>Exclude…</button
								>
							</li>
						{/each}
					</ul>
				</section>
			{/each}
		</div>
	{/if}
</Card>

<style>
	.pk-grid {
		display: grid;
		grid-template-columns: minmax(0, 2fr) minmax(0, 1fr);
		gap: 12px;
		margin: 12px 0;
	}
	@media (max-width: 860px) {
		.pk-grid {
			grid-template-columns: minmax(0, 1fr);
		}
	}
	.pk-form {
		display: grid;
		gap: 8px;
	}
	.pk-small {
		font-size: 12px;
		margin: 0;
	}
	.err-text {
		color: var(--err);
	}
	.right {
		text-align: right;
	}
	.pk-families {
		display: grid;
		gap: 14px;
	}
	.pk-families h3 {
		display: flex;
		gap: 8px;
		align-items: center;
		font-size: 13.5px;
		margin: 0 0 6px;
	}
	.pk-families li {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
		align-items: center;
		padding: 4px 0;
		overflow-wrap: anywhere;
	}
</style>
