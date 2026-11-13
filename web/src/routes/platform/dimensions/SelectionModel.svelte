<script lang="ts">
	// P13 concept model (D59/D60): what a job selection is made of, in compile order, and what each part does to the
	// layer stack. Department = the industry_dept layer (extends a base archetype); areas group its context spaces;
	// channel switches channel layers on; region is a placeholder. Residency and languages are NOT in the selection.
	import type { DepartmentArchetype, Dimension } from '$lib/api/types/platform';

	let { dims, archetypes }: { dims: Dimension[]; archetypes: DepartmentArchetype[] | null } = $props();

	const count = (k: string) => dims.find((d) => d.key === k)?.values.length ?? 0;
	const depts = $derived(archetypes?.reduce((n, a) => n + a.specialised_by.length, 0) ?? null);

	type Part = {
		key: string;
		label: string;
		kind: 'pick' | 'dimension' | 'grouping' | 'placeholder';
		does: string;
		values: string;
		href?: string;
	};
	const parts = $derived<Part[]>([
		{
			key: 'industry',
			label: 'Business model + industry',
			kind: 'pick',
			does: 'Fires axis rules, adds the industry layer.',
			values: 'profile · industry',
			href: '/platform/business-models',
		},
		{
			key: 'department',
			label: 'Department',
			kind: 'pick',
			does: 'Adds the industry_dept layer, which specialises a base archetype.',
			values: archetypes
				? `${archetypes.length} archetypes → ${depts} industry departments`
				: 'archetype → industry department',
			href: '#arch-h',
		},
		{
			key: 'area',
			label: 'Areas',
			kind: 'grouping',
			does: 'Keeps only the chosen areas’ context spaces inside the department.',
			values: `${count('area')} areas`,
			href: '#dim-h-area',
		},
		{
			key: 'channel',
			label: 'Channels',
			kind: 'dimension',
			does: 'Switches channel-qualified job layers on (job × channel=email).',
			values: `${count('channel')} channels`,
			href: '#dim-h-channel',
		},
		{
			key: 'region',
			label: 'Region',
			kind: 'placeholder',
			does: 'Recorded in the release. Selects nothing.',
			values: `${count('region')} values · placeholder`,
			href: '#region-h',
		},
	]);
	const KIND: Record<Part['kind'], string> = {
		pick: 'picks a layer',
		dimension: 'dimension',
		grouping: 'grouping',
		placeholder: 'placeholder',
	};
</script>

<ol class="model" data-testid="selection-model" aria-label="What a job selection is made of">
	{#each parts as p (p.key)}
		<li class="part {p.kind}" data-kind={p.kind}>
			<div class="head">
				<a class="name" href={p.href}>{p.label}</a>
				<span class="kind">{KIND[p.kind]}</span>
			</div>
			<p>{p.does}</p>
			<span class="vals mono">{p.values}</span>
		</li>
	{/each}
</ol>
<div class="outside" data-testid="not-in-selection">
	<span class="o-label">Not in the selection</span>
	<span><b>Data residency</b> — a tenant setting at provisioning (EU/US)</span>
	<span><b>Languages served</b> — a job setting (gate B6, reply language)</span>
	<span><b>Jurisdiction</b> — future packs (industry × country)</span>
</div>

<style>
	.model {
		list-style: none;
		margin: 0 0 10px;
		padding: 0;
		display: grid;
		grid-template-columns: repeat(5, minmax(0, 1fr));
		gap: 6px;
		counter-reset: part;
	}
	.part {
		position: relative;
		border: 1px solid var(--line);
		border-top: 3px solid var(--accent);
		border-radius: var(--r);
		padding: 8px;
		display: grid;
		gap: 4px;
		align-content: start;
		background: var(--panel);
	}
	.part.grouping {
		border-top-color: var(--work);
	}
	.part.dimension {
		border-top-color: var(--ok);
	}
	.part.placeholder {
		border-style: dashed;
		border-top: 3px dashed var(--faint);
		background: var(--sunk);
	}
	.head {
		display: grid;
		gap: 2px;
	}
	.name {
		font-weight: 600;
		font-size: 13px;
	}
	.kind {
		font: 600 10px/1.2 var(--cond);
		letter-spacing: 0.07em;
		text-transform: uppercase;
		color: var(--muted);
	}
	p {
		margin: 0;
		font-size: 12px;
		color: var(--muted);
	}
	.vals {
		font-size: 11px;
	}
	.outside {
		display: flex;
		flex-wrap: wrap;
		gap: 4px 14px;
		font-size: 12px;
		padding: 8px 10px;
		border-radius: var(--r);
		background: var(--sunk);
		color: var(--muted);
	}
	.o-label {
		font: 600 10.5px/1.6 var(--cond);
		letter-spacing: 0.07em;
		text-transform: uppercase;
	}
	@media (max-width: 1100px) {
		.model {
			grid-template-columns: repeat(auto-fit, minmax(150px, 1fr));
		}
	}
</style>
