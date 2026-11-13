<script lang="ts">
	// P7 contents: what one tag contributes, grouped into the concept families a job is made of (model, journey,
	// operations, gates + roles, settings). Items new or changed in this tag carry a marker.
	import type { DiffChange, TagItem, TagItemKind } from '$lib/api/types/model';
	import Chip from '$lib/ui/Chip.svelte';
	import { KIND_LABEL } from './ui';

	let {
		items,
		changed,
		industry,
		version,
	}: {
		items: TagItem[];
		/** `kind\0key` → change in this tag vs the previous one. */
		changed: Map<string, DiffChange>;
		industry: string | null;
		version: number;
	} = $props();

	const FAMILIES: { id: string; label: string; what: string; kinds: TagItemKind[] }[] = [
		{
			id: 'model',
			label: 'Objects & business model',
			what: 'Which business objects exist and which business models (profiles, axis rules) switch them on.',
			kinds: ['object', 'profile', 'axis_rule'],
		},
		{
			id: 'journey',
			label: 'Journey',
			what: 'Themes, stages and activities the customer moves through, and the knowledge each stage needs.',
			kinds: [
				'journey_theme',
				'journey_stage',
				'theme_stage_prior',
				'stage_knowledge_kind',
				'activity',
				'activity_object',
				'journey_channel',
				'stage_scope',
				'use_case_card',
			],
		},
		{
			id: 'operations',
			label: 'Operations',
			what: 'Which object.verb operations a tenant must (R) or may (O) connect for this job.',
			kinds: ['operation_expectation'],
		},
		{
			id: 'gates',
			label: 'Gates & agent roles',
			what: 'Checks before and after the agent acts, and the roles that run the conversation.',
			kinds: ['gate_question', 'agent_role'],
		},
		{
			id: 'settings',
			label: 'Job settings',
			what: 'Stance, north-star metric, rule floors and the prompt slots (identity, policy, tone …) the job starts from; tenants refine some of them.',
			kinds: ['job_setting', 'instruction'],
		},
		{
			id: 'content',
			label: 'Ontology & job content',
			what: 'The entities, attributes and relations the job reasons over, and the job’s own content: its definition, context spaces, procedures, connector slots and knowledge stores.',
			kinds: [
				'entity',
				'attribute',
				'relation',
				'job',
				'context_space',
				'procedure',
				'connector_slot',
				'knowledge_store',
			],
		},
		{
			id: 'catalogue',
			label: 'Catalogue',
			what: 'The operations objects offer, the error states they can answer with, and the department archetypes departments extend.',
			kinds: ['operation', 'error_state', 'department_archetype'],
		},
	];

	const id = (i: Pick<TagItem, 'kind' | 'key'>) => `${i.kind}\u0000${i.key}`;
	const families = $derived(
		FAMILIES.map((f) => {
			const list = items.filter((i) => f.kinds.includes(i.kind));
			return { ...f, items: list, fresh: list.filter((i) => changed.has(id(i))).length };
		}),
	);
	const present = $derived(families.filter((f) => f.items.length > 0));

	function href(i: TagItem): string | null {
		if (i.kind === 'object') return `/platform/objects/${i.key}`;
		if (i.kind === 'operation') return `/platform/operations?q=${encodeURIComponent(i.key)}`;
		if (i.kind === 'department_archetype') return '/platform/dimensions';
		if (i.kind === 'operation_expectation') return `/platform/operations?q=${encodeURIComponent(i.key)}`;
		if (i.kind === 'gate_question') return '/platform/gates';
		if (i.kind === 'agent_role') return '/platform/agent-roles';
		if (i.kind === 'profile' || i.kind === 'axis_rule') return '/platform/business-models';
		if (
			industry &&
			[
				'journey_theme',
				'journey_stage',
				'theme_stage_prior',
				'stage_knowledge_kind',
				'activity',
				'journey_channel',
				'stage_scope',
				'use_case_card',
			].includes(i.kind)
		)
			return `/platform/journeys/${industry}`;
		return null;
	}
	const required = (i: TagItem) => i.detail.startsWith('required');
	const extra = (i: TagItem) => i.detail.replace(/^(required|optional)\s*·?\s*/, '');
</script>

{#snippet mark(i: TagItem)}
	{@const c = changed.get(id(i))}
	{#if c === 'added'}<Chip text="new in @v{version}" variant="ok" />{:else if c === 'changed'}<Chip
			text="changed in @v{version}"
			variant="accent"
		/>{/if}
{/snippet}

{#snippet keyOf(i: TagItem)}
	{@const h = href(i)}
	{#if h}<a class="mono" href={h}>{i.key}</a>{:else}<span class="mono">{i.key}</span>{/if}
{/snippet}

<ul class="tiles" aria-label="What this tag contributes">
	{#each families as f (f.id)}
		<li class="tile" class:none={f.items.length === 0}>
			{#if f.items.length}<a href="#fam-{f.id}" class="tile-link">
					<span class="n">{f.items.length}</span>
					<span class="lbl">{f.label}</span>
					{#if f.fresh}<span class="fresh">{f.fresh} new/changed</span>{/if}
				</a>
			{:else}
				<span class="n">0</span>
				<span class="lbl">{f.label}</span>
				<span class="fresh muted">from lower layers</span>
			{/if}
		</li>
	{/each}
</ul>

{#each present as f (f.id)}
	<section class="fam" id="fam-{f.id}">
		<h3>{f.label} <span class="muted">({f.items.length})</span></h3>
		<p class="what">{f.what}</p>
		{#if f.id === 'operations'}
			<div class="ops">
				{#each [true, false] as req (req)}
					{@const list = f.items.filter((i) => required(i) === req)}
					<div>
						<h4>
							<Chip text={req ? 'R' : 'O'} variant={req ? 'accent' : 'muted'} />
							{req ? 'Required — the job cannot go live without them' : 'Optional — used when connected'}
							<span class="muted">({list.length})</span>
						</h4>
						<ul class="items">
							{#each list as i (i.key)}
								<li>
									{@render keyOf(i)}
									{@render mark(i)}
									{#if extra(i)}<span class="cell-sub">{extra(i)}</span>{/if}
								</li>
							{/each}
						</ul>
					</div>
				{/each}
			</div>
		{:else}
			{#each f.kinds.filter((k) => f.items.some((i) => i.kind === k)) as k (k)}
				{#if f.kinds.length > 1}<h4>{KIND_LABEL[k]}</h4>{/if}
				<ul class="items">
					{#each f.items.filter((i) => i.kind === k) as i (i.key)}
						<li>
							{@render keyOf(i)}
							{#if i.label !== i.key}<span class="item-label">{i.label}</span>{/if}
							{#if i.detail.includes('CLOSED')}<Chip text="fail closed" variant="err" />{/if}
							{@render mark(i)}
							<span class="cell-sub">{i.detail}</span>
						</li>
					{/each}
				</ul>
			{/each}
		{/if}
	</section>
{/each}

<style>
	.tiles {
		list-style: none;
		margin: 0 0 8px;
		padding: 0;
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(150px, 1fr));
		gap: 8px;
	}
	.tile {
		border: 1px solid var(--line);
		border-radius: var(--r);
		background: var(--sunk);
	}
	.tile-link,
	.tile.none {
		display: flex;
		flex-direction: column;
		gap: 2px;
		padding: 10px 12px;
		color: inherit;
		text-decoration: none;
	}
	.tile-link:hover {
		background: color-mix(in srgb, var(--accent) 8%, transparent);
	}
	.tile.none {
		opacity: 0.7;
	}
	.n {
		font: 600 22px/1.1 var(--cond);
	}
	.lbl {
		font-size: 13px;
	}
	.fresh {
		font-size: 12px;
		color: var(--accent);
	}
	.fam {
		padding-top: 14px;
		border-top: 1px solid var(--line-2);
		margin-top: 14px;
	}
	h3 {
		margin: 0;
		font: 600 13px/1.2 var(--cond);
		letter-spacing: 0.04em;
		text-transform: uppercase;
	}
	h4 {
		margin: 10px 0 6px;
		font-size: 13px;
		font-weight: 600;
		display: flex;
		align-items: center;
		gap: 6px;
		flex-wrap: wrap;
	}
	.what {
		margin: 4px 0 6px;
		font-size: 12px;
		color: var(--muted);
	}
	.ops {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(260px, 1fr));
		gap: 16px;
	}
	.items {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(240px, 1fr));
		gap: 4px 16px;
	}
	.items li {
		padding: 5px 0;
		border-bottom: 1px solid var(--line-2);
		font-size: 13px;
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 4px 8px;
	}
	.items li :global(.cell-sub) {
		flex-basis: 100%;
	}
	.item-label {
		color: var(--fg);
	}
</style>
