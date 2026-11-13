<script lang="ts">
	// T9 feed map: one row per store this job uses or fills — what feeds it (sources, hand uploads) → the store →
	// the knowledge kinds (and journey stages) the agent answers from it. Reads left to right as the chain.
	import { fmt } from '$lib/format';
	import type { MappingTarget, Source } from '$lib/api/types/tenant';
	import { COVERAGE, ago } from '$lib/tenant/knowledge-T7';
	import {
		FEED,
		HEALTH,
		mappedEntities,
		scheduleText,
		sourceHealth,
		type StoreFeed,
	} from '$lib/tenant/sources-T9';
	import { SOURCE_TYPES, structured } from '$lib/tenant/sources';
	import { STAGES } from '$lib/ontology/catalogue';
	import Chip from '$lib/ui/Chip.svelte';

	let {
		feeds,
		targets,
		now,
		onopen,
	}: {
		feeds: StoreFeed[];
		targets: MappingTarget[];
		now: number;
		onopen: (s: Source) => void;
	} = $props();

	const typeLabel = (t: string) => SOURCE_TYPES.find(([k]) => k === t)?.[1] ?? t;
	const stageLabel = (k: string) => STAGES.find(([s]) => s === k)?.[1] ?? k;
	/** Kinds of one store, grouped: `catalog` → Orientation, Investigation. */
	function kindsOf(f: StoreFeed) {
		const m = new Map<string, { kind: string; stages: string[]; required: boolean }>();
		for (const c of f.kinds) {
			const e = m.get(c.row.kind) ?? { kind: c.row.kind, stages: [], required: false };
			e.stages.push(stageLabel(c.row.stage));
			e.required ||= c.row.required;
			m.set(c.row.kind, e);
		}
		return [...m.values()];
	}
</script>

<ol class="t9-map" data-testid="feed-map">
	{#each feeds as f (f.store.id)}
		{@const st = f.kinds[0]?.state}
		<li class="t9-row" data-store={f.store.key} data-feed={f.feed}>
			<div class="t9-col t9-feeds">
				<span class="t9-col-h">Fed by</span>
				{#each f.sources as s (s.id)}
					{@const h = sourceHealth(s, targets, now)}
					<button class="t9-feed" type="button" onclick={() => onopen(s)} title={HEALTH[h].hint}>
						<span class="t9-feed-name">{s.name}</span>
						<span class="t9-feed-meta"
							>{typeLabel(s.type)} · {scheduleText(s.schedule)}{#if structured(s.type) && s.mapping.length}
								· {s.mapping.length} fields → {mappedEntities(s).join(', ')}{/if}</span
						>
						<Chip text={HEALTH[h].text} variant={HEALTH[h].variant} />
					</button>
				{/each}
				{#if f.uploads.jobs}
					<div class="t9-feed t9-upload">
						<span class="t9-feed-name">Uploads · {f.uploads.jobs} file{f.uploads.jobs === 1 ? '' : 's'}</span>
						{#if f.uploads.last}<span class="t9-feed-meta"
								>last <a href={`/admin/knowledge/jobs?id=${encodeURIComponent(f.uploads.last.id)}`}
									>{f.uploads.last.source_label}</a
								>, {ago(f.uploads.last.created_at, now)}</span
							>{/if}
					</div>
				{:else if !f.sources.length}
					<div class="t9-feed t9-upload">
						<span class="t9-feed-name">{f.store.documents ? 'Added by hand' : 'Nothing yet'}</span>
						<span class="t9-feed-meta">no source, no recent upload</span>
					</div>
				{/if}
			</div>
			<span class="t9-arrow" aria-hidden="true">→</span>
			<div class="t9-col t9-store">
				<span class="t9-col-h">Store</span>
				<a class="mono t9-store-key" href="/admin/knowledge/stores?store={encodeURIComponent(f.store.id)}"
					>{f.store.key}</a
				>
				<span class="t9-feed-meta"
					>{f.store.label} · {fmt.n(f.store.documents)} docs · updated {ago(f.store.updated_at, now)}</span
				>
				<span class="t9-chips">
					<Chip text={FEED[f.feed].text} variant={FEED[f.feed].variant} title={FEED[f.feed].hint} />
					{#if st && st !== 'ok' && f.feed !== 'gap'}<Chip
							text={COVERAGE[st].text}
							variant={COVERAGE[st].variant}
							title={COVERAGE[st].hint}
						/>{:else if st && f.feed === 'gap'}<Chip
							text={COVERAGE[st].text}
							variant="err"
							title={COVERAGE[st].hint}
						/>{/if}
				</span>
			</div>
			<span class="t9-arrow" aria-hidden="true">→</span>
			<div class="t9-col t9-kinds">
				<span class="t9-col-h">Agent uses it for</span>
				{#each kindsOf(f) as k (k.kind)}
					<span class="t9-kind" class:req={k.required}>
						<span class="mono">{k.kind}</span>
						<span class="t9-feed-meta">{k.stages.join(', ')}{k.required ? ' · required' : ''}</span>
					</span>
				{:else}
					<span class="t9-feed-meta">No knowledge kind of this job points here.</span>
				{/each}
			</div>
		</li>
	{/each}
</ol>

<style>
	.t9-map {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
	}
	.t9-row {
		display: grid;
		grid-template-columns: minmax(0, 1.25fr) 20px minmax(0, 1fr) 20px minmax(0, 1fr);
		gap: 10px;
		align-items: start;
		padding: 12px 16px;
		border-top: 1px solid var(--line-2);
	}
	.t9-row:first-child {
		border-top: 0;
	}
	.t9-row[data-feed='gap'],
	.t9-row[data-feed='broken'] {
		background: color-mix(in srgb, var(--err) 4%, transparent);
	}
	.t9-col {
		display: flex;
		flex-direction: column;
		gap: 6px;
		min-width: 0;
	}
	.t9-col-h {
		font: 600 10.5px/1.2 var(--cond);
		letter-spacing: 0.06em;
		text-transform: uppercase;
		color: var(--faint);
	}
	.t9-arrow {
		color: var(--faint);
		font-size: 18px;
		padding-top: 18px;
		text-align: center;
	}
	.t9-feed {
		display: flex;
		flex-direction: column;
		align-items: flex-start;
		gap: 3px;
		text-align: left;
		border: 1px solid var(--line);
		border-left: 3px solid var(--accent);
		border-radius: var(--r);
		background: var(--panel);
		padding: 6px 9px;
		font: inherit;
		color: inherit;
		cursor: pointer;
		min-width: 0;
		max-width: 100%;
	}
	button.t9-feed:hover {
		border-color: var(--muted);
		border-left-color: var(--accent);
	}
	.t9-upload {
		border-left-color: var(--work);
		border-style: dashed;
		border-left-style: solid;
		cursor: default;
		background: transparent;
	}
	.t9-feed-name {
		font-weight: 600;
		font-size: 13px;
	}
	.t9-feed-meta {
		color: var(--muted);
		font-size: 12px;
		overflow-wrap: anywhere;
	}
	.t9-store-key {
		font-weight: 600;
	}
	.t9-chips {
		display: flex;
		gap: 4px;
		flex-wrap: wrap;
	}
	.t9-kind {
		display: flex;
		flex-direction: column;
		padding-left: 8px;
		border-left: 2px solid var(--line);
	}
	.t9-kind.req {
		border-left-color: var(--cust);
	}
	@media (max-width: 760px) {
		.t9-row {
			grid-template-columns: minmax(0, 1fr);
			gap: 6px;
		}
		.t9-arrow {
			padding: 0;
			transform: rotate(90deg);
			width: 20px;
			line-height: 1;
		}
	}
</style>
