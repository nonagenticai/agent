<script lang="ts">
	// J11 concept strip: how this job's ontology is assembled — three inherited template layers (locked, read-only
	// here) plus what this tenant added (editable refinements) = what the agent reasons over. Says where each kind
	// of change is made and how a tenant change reaches customers (draft → test chat → publish, D54). Each entity is the job's name
	// for a platform object; operations and lifecycles live on the object.
	import { app } from '$lib/app.svelte';
	import type { LayerBand, TenantChanges } from '$lib/ontology/J11usage';

	let {
		bands,
		changes,
		total,
		relations,
		objects,
		unused,
		superAdmin,
	}: {
		bands: LayerBand[];
		changes: TenantChanges;
		/** Visible entities in the effective ontology. */
		total: number;
		relations: number;
		/** Platform object keys the entities are views of. */
		objects: string[];
		/** Visible entities no activity uses. */
		unused: number;
		superAdmin: boolean;
	} = $props();

	const plural = (n: number, w: string) => `${n} ${w}${n === 1 ? '' : 's'}`;
	const tenantBits = $derived(
		[
			[changes.entities, 'entity', 'entities'],
			[changes.attributes, 'attribute', 'attributes'],
			[changes.relations, 'relation', 'relations'],
			[changes.hidden, 'hidden', 'hidden'],
		].filter(([n]) => (n as number) > 0) as [number, string, string][],
	);
</script>

<section class="j11-stack card" data-testid="j11-layer-stack" aria-label="How this ontology is assembled">
	<div class="j11-bands">
		{#each bands as b, i (b.lane)}
			{#if i > 0}<span class="j11-op" aria-hidden="true">+</span>{/if}
			<div class="j11-band {b.lane}" class:locked={b.locked}>
				<div class="j11-band-top">
					{#if b.locked}
						<svg class="j11-ic" viewBox="0 0 24 24" width="12" height="12" aria-hidden="true">
							<path
								d="M7 11V8a5 5 0 0 1 10 0v3M5 11h14v10H5z"
								fill="none"
								stroke="currentColor"
								stroke-width="2.2"
								stroke-linejoin="round"
							/>
						</svg>
					{:else}
						<span class="j11-ic plus" aria-hidden="true">+</span>
					{/if}
					<span class="j11-band-title">{b.title}</span>
					<span class="j11-band-state">{b.locked ? 'inherited' : 'yours'}</span>
				</div>
				<div class="j11-band-scope">
					{b.lane === 'generic'
						? 'Every job'
						: b.lane === 'tenant'
							? 'Your refinements to this job'
							: (b.scope ?? '—')}
				</div>
				{#if b.lane === 'tenant'}
					<div class="j11-band-num">
						{#if tenantBits.length}
							{#each tenantBits as [n, one, many], k (one)}{#if k > 0}<span class="sep">·</span>{/if}<span
									><b>{n}</b> {n === 1 ? one : many}</span
								>{/each}
						{:else}
							<span class="muted">Nothing yet</span>
						{/if}
					</div>
				{:else}
					<div class="j11-band-num"><b>{b.entities}</b> {b.entities === 1 ? 'entity' : 'entities'}</div>
				{/if}
				{#if b.layer}<div class="j11-band-layer mono" title={b.layer}>{b.layer}</div>{/if}
			</div>
		{/each}
		<span class="j11-op" aria-hidden="true">=</span>
		<div class="j11-band total">
			<div class="j11-band-top"><span class="j11-band-title">What the agent reasons over</span></div>
			<div class="j11-band-num">
				<b>{total}</b> entities <span class="sep">·</span> <b>{relations}</b> relations
			</div>
			<div class="j11-band-scope">
				views of {plural(objects.length, 'platform object')}{#if unused}<span class="sep">·</span><span
						class="j11-unused">{unused} not used by any activity</span
					>{/if}
			</div>
		</div>
	</div>

	<div class="j11-rules">
		<p>
			<b>Inherited layers are read-only here.</b> They change when the platform publishes a new template
			version{#if superAdmin}{' '}(<a href="/platform/layers">Layers</a>){/if}. On an inherited entity you can
			still add synonyms and relations, or hide it for this job.
		</p>
		<p>
			<b>What you add is saved to draft.</b> Try it in the <a href="/chat">test chat</a>; customers see it
			when you publish:
			<a href="/admin/jobs/{app.job}/refinements">Refinements</a> →
			<a href="/admin/jobs/{app.job}/release">Review &amp; publish</a> (cut → approve → apply).
		</p>
		<p class="j11-objects">
			<b>Each entity is this job's name for a platform object</b> — what the agent can do with it (operations)
			and the states it moves through live on the object:
			{#each objects as o (o)}
				{#if superAdmin}<a class="chip j11-obj" href="/platform/objects/{o}">{o}</a>{:else}<span
						class="chip j11-obj">{o}</span
					>{/if}
			{/each}
		</p>
	</div>
</section>

<style>
	.j11-stack {
		display: grid;
		gap: 12px;
		padding: 14px;
		margin-bottom: 12px;
	}
	.j11-bands {
		display: flex;
		align-items: stretch;
		gap: 6px;
		flex-wrap: wrap;
	}
	.j11-op {
		align-self: center;
		font: 600 16px/1 var(--sans);
		color: var(--faint);
	}
	.j11-band {
		--b: var(--line);
		flex: 1 1 150px;
		min-width: 0;
		display: grid;
		align-content: start;
		gap: 3px;
		padding: 9px 11px;
		border-radius: 8px;
		border: 1px solid color-mix(in srgb, var(--b) 35%, var(--line));
		border-top: 3px solid var(--b);
		background: color-mix(in srgb, var(--b) 5%, var(--panel));
	}
	.j11-band.generic {
		--b: var(--cust);
	}
	.j11-band.industry {
		--b: var(--tool);
	}
	.j11-band.industry_dept {
		--b: var(--work);
	}
	.j11-band.tenant {
		--b: var(--accent);
		border-style: dashed;
		border-top-style: solid;
	}
	.j11-band.total {
		--b: var(--fg);
		flex: 1.3 1 200px;
		background: var(--sunk);
	}
	.j11-band-top {
		display: flex;
		align-items: center;
		gap: 6px;
		color: var(--b);
	}
	.j11-ic.plus {
		font: 700 14px/1 var(--sans);
	}
	.j11-band-title {
		font: 700 13px/1.3 var(--sans);
		color: var(--fg);
	}
	.j11-band-state {
		margin-left: auto;
		font: 600 9.5px/1.4 var(--sans);
		letter-spacing: 0.07em;
		text-transform: uppercase;
		color: var(--b);
	}
	.j11-band-scope {
		font-size: 12px;
		color: var(--muted);
	}
	.j11-band-num {
		font-size: 13px;
		color: var(--muted);
	}
	.j11-band-num b {
		font: 600 18px/1.2 var(--cond);
		color: var(--fg);
		font-variant-numeric: tabular-nums;
	}
	.j11-band-layer {
		font-size: 11px;
		color: var(--faint);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.j11-band.tenant .j11-band-num b {
		font-size: 15px;
	}
	.sep {
		margin: 0 5px;
		color: var(--faint);
	}
	.j11-unused {
		color: var(--warn);
	}
	.j11-rules {
		display: grid;
		gap: 4px;
		font-size: 12.5px;
		color: var(--muted);
	}
	.j11-rules p {
		margin: 0;
	}
	.j11-rules b {
		color: var(--fg);
		font-weight: 600;
	}
	.j11-objects {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 4px;
	}
	.j11-obj {
		font-size: 11px;
		padding: 3px 6px;
		background: var(--sunk);
		color: var(--fg);
		text-decoration: none;
	}
	@media (max-width: 640px) {
		.j11-bands {
			flex-direction: column;
		}
		.j11-op {
			align-self: flex-start;
			margin-left: 12px;
		}
	}
</style>
