<script lang="ts">
	// R10 (B3 T14): the platform packs one knowledge kind also searches, pinned `key@version`. A pin never moves by
	// itself: a newer listed version shows a badge and re-pinning is an explicit save (a draft change, D54). A pin the
	// tenant's listing no longer has stays pinned and readable (flagged); one strata could not read is flagged too and
	// searches run without it until it reads again.
	// PK (T9): each pin shows why a search runs without it — hidden for this job, excluded tenant-wide (with the
	// prefix), unreadable, or its state (draft / retired / missing). A pin that comes from the job template can be
	// hidden for this job only ("Hide for this job" = a `knowledge_pack` tombstone refinement, saved to the draft) and
	// shown again ("Restore"); tenant-wide exclusions are managed on Platform knowledge.
	import Chip from '$lib/ui/Chip.svelte';
	import { api, guard } from '$lib/app.svelte';
	import {
		packRefinementKey,
		parsePackPin,
		type KnowledgeKindRow,
		type KnowledgePackPin,
		type PlatformPack,
	} from '$lib/api/types/ontology';
	import { pinBlocked } from '$lib/pk/pk';

	let {
		row,
		listing,
		disabled = false,
		onpin,
		onchanged,
	}: {
		row: KnowledgeKindRow;
		/** The tenant's listing (`platformPacks`); null = not loaded (the picker is hidden, the pins still show). */
		listing: PlatformPack[] | null;
		disabled?: boolean;
		onpin: (pins: string[], done: string) => void;
		/** After a hide / restore (the kinds must be re-read). */
		onchanged?: () => void;
	} = $props();

	const pins = $derived((row.packs ?? []).map((p) => p.pin));
	const keyOf = (pin: string) => parsePackPin(pin)?.key ?? pin;
	/** Listed packs whose key is not pinned yet: the newest version of each key. */
	const addable = $derived.by(() => {
		const taken = new Set(pins.map(keyOf));
		const newest = new Map<string, PlatformPack>();
		for (const p of listing ?? [])
			if (!taken.has(p.key) && (newest.get(p.key)?.version ?? 0) < p.version) newest.set(p.key, p);
		return [...newest.values()].sort((a, b) => a.key.localeCompare(b.key));
	});
	let pick = $state('');
	let busy = $state<string | null>(null);
	/** Template pins can be hidden per job; a pin the job bound itself is removed with ×. */
	const fromTemplate = $derived(row.packs_source === 'template');

	function add() {
		if (!pick) return;
		onpin([...pins, pick], `${row.kind}: pinned ${pick}. Saved to draft.`);
		pick = '';
	}
	const remove = (pin: string) =>
		onpin(
			pins.filter((p) => p !== pin),
			`${row.kind}: unpinned ${pin}. Saved to draft.`,
		);
	const repin = (pin: string, newer: string) =>
		onpin(
			pins.map((p) => (p === pin ? newer : p)),
			`${row.kind}: re-pinned ${pin} → ${newer}. Saved to draft.`,
		);
	/** Hide (tombstone) or restore the template's pin on this kind for this job: a draft change (D54). */
	async function toggleHidden(p: KnowledgePackPin) {
		const key = keyOf(p.pin);
		busy = p.pin;
		await guard(
			() =>
				api().refine(
					p.tombstoned ? 'unhide' : 'hide',
					'knowledge_pack',
					packRefinementKey(row.stage, row.kind, key),
				),
			p.tombstoned
				? `${row.kind}: ${key} restored for this job. Saved to draft.`
				: `${row.kind}: ${key} hidden for this job. Saved to draft.`,
		);
		busy = null;
		onchanged?.();
	}
</script>

{#if pins.length || (listing && listing.length)}
	<div class="packs" data-testid="pack-pins-{row.stage}-{row.kind}">
		{#each row.packs ?? [] as p (p.pin)}
			{@const why = pinBlocked(p)}
			<span class="pin" class:off={!!why && why.chip !== 'retired'} data-testid="pack-pin" data-pin={p.pin}>
				<Chip
					text={p.pin}
					variant={why ? (why.variant === 'muted' ? 'warn' : why.variant) : 'accent'}
					title={why?.title ?? 'Platform pack: searched with this kind'}
				/>
				{#if why}<span
						class="flag"
						class:err-text={why.variant === 'err'}
						class:warn-text={why.variant === 'warn'}
						title={why.title}
						data-testid="pack-pin-why">{why.chip}</span
					>{/if}
				{#if p.excluded_by}<a class="flag" href="/admin/knowledge/platform">manage</a>{/if}
				{#if typeof p.leaves === 'number' && !why}<span class="flag" title="Leaf stores a search names"
						>{p.leaves} store{p.leaves === 1 ? '' : 's'}</span
					>{/if}
				{#if p.newer && !p.tombstoned}
					<button
						type="button"
						class="btn sm"
						{disabled}
						title="Re-pin to the newer version (saved to draft)"
						onclick={() => repin(p.pin, p.newer!)}>newer: {p.newer}</button
					>
				{/if}
				{#if fromTemplate || p.tombstoned}
					<button
						type="button"
						class="btn sm"
						disabled={disabled || busy === p.pin}
						title={p.tombstoned
							? 'Search this pack again for this kind (saved to draft)'
							: 'Hide this template pin for this job only (saved to draft); other jobs keep it'}
						onclick={() => toggleHidden(p)}>{p.tombstoned ? 'Restore' : 'Hide for this job'}</button
					>
				{/if}
				{#if !p.tombstoned}
					<button
						type="button"
						class="btn sm ghost"
						aria-label="Unpin {p.pin} from {row.kind}"
						{disabled}
						onclick={() => remove(p.pin)}>×</button
					>
				{/if}
			</span>
		{/each}
		{#if addable.length}
			<span class="add">
				<select class="input sm mono" aria-label="Platform pack for {row.kind}" bind:value={pick} {disabled}>
					<option value="">+ platform pack</option>
					{#each addable as a (a.pin)}<option value={a.pin}>{a.pin}</option>{/each}
				</select>
				{#if pick}<button type="button" class="btn sm" {disabled} onclick={add}>Pin</button>{/if}
			</span>
		{/if}
	</div>
{/if}

<style>
	.packs {
		display: flex;
		flex-wrap: wrap;
		gap: 4px 8px;
		align-items: center;
		width: 100%;
		font-size: 12px;
	}
	.pin,
	.add {
		display: inline-flex;
		flex-wrap: wrap;
		gap: 4px;
		align-items: center;
		max-width: 100%;
		min-width: 0;
	}
	.pin :global(.chip) {
		max-width: 100%;
		white-space: normal;
		overflow-wrap: anywhere;
	}
	.flag {
		font-size: 11px;
		color: var(--muted);
	}
	.err-text {
		color: var(--err);
	}
	.warn-text {
		color: var(--warn);
	}
	.pin.off :global(.chip) {
		text-decoration: line-through;
	}
	.input.sm {
		padding: 3px 6px;
		font-size: 12px;
	}
</style>
