<script lang="ts">
	// P13 (D60): a department is the industry_dept layer, which SPECIALISES a base department archetype. The archetype
	// (base layer content) holds the generic work any department of that kind does; the industry department adds what
	// differs per industry ("marketing for Amazon vs Tesla"). Areas then group the department's context spaces.
	import type { DepartmentArchetype } from '$lib/api/types/platform';
	import Chip from '$lib/ui/Chip.svelte';

	let { archetypes }: { archetypes: DepartmentArchetype[] } = $props();

	const facets = (a: DepartmentArchetype) => [
		{ label: 'Activities', items: a.activities },
		{ label: 'Requests', items: a.request_taxonomy },
		{ label: 'Knowledge kinds', items: a.knowledge_kinds },
		{ label: 'Metrics', items: a.metrics },
		{ label: 'Approval-rule slots', items: a.approval_rule_slots },
	];
</script>

<section class="card arch" aria-labelledby="arch-h" data-testid="dim-departments">
	<header class="card-head">
		<div>
			<h2 id="arch-h">Departments: base archetypes → industry departments</h2>
			<p>
				Any marketing department does marketing work; marketing for a webshop and for a carmaker differs. The
				archetype in <code>base</code> holds the generic part, the <code>industry_dept</code> layer specialises
				it. A job selection picks the industry department.
			</p>
		</div>
		<div class="actions"><Chip text={`${archetypes.length} archetypes`} /></div>
	</header>
	<div class="card-body">
		<ol class="arch-list">
			{#each archetypes as a (a.key)}
				<li class="arch-row" data-testid="archetype" data-key={a.key}>
					<div class="base">
						<div class="tier">base archetype</div>
						<div class="name">{a.label} <span class="mono muted small">{a.key}</span></div>
						<p class="desc">{a.description}</p>
						<details class="facets">
							<summary>Generic content</summary>
							<dl>
								{#each facets(a) as f (f.label)}
									<dt>{f.label}</dt>
									<dd>{(f.items ?? []).join(' · ') || '—'}</dd>
								{/each}
							</dl>
						</details>
					</div>
					<div class="arrow" aria-hidden="true">→</div>
					<div class="spec">
						<div class="tier">industry departments (extends {a.key})</div>
						{#each a.specialised_by as d (d.industry + d.key)}
							<div class="dept" data-testid="archetype-dept">
								<a
									class="mono"
									href={d.layer_id
										? `/platform/layers?layer=${encodeURIComponent(d.layer_id)}`
										: '/platform/layers'}>industry_dept/{d.industry}/{d.key}</a
								>
								{#if d.areas.length}
									<span class="areas">
										areas:
										{#each d.areas as ar (ar)}<span class="area">{ar}</span>{/each}
									</span>
								{:else}
									<span class="muted small">no areas: the whole department applies</span>
								{/if}
							</div>
						{:else}
							<div class="muted small none">
								No industry department yet. A generator run or a template author adds one.
							</div>
						{/each}
					</div>
				</li>
			{/each}
		</ol>
	</div>
</section>

<style>
	.arch-list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 10px;
	}
	.arch-row {
		display: grid;
		grid-template-columns: minmax(0, 1fr) 24px minmax(0, 1.2fr);
		gap: 8px;
		align-items: start;
		padding: 10px;
		border: 1px solid var(--line);
		border-radius: var(--r);
	}
	.tier {
		font: 600 10.5px/1.2 var(--cond);
		letter-spacing: 0.07em;
		text-transform: uppercase;
		color: var(--muted);
		margin-bottom: 4px;
	}
	.name {
		font-weight: 600;
	}
	.desc {
		margin: 4px 0;
		font-size: 12.5px;
		color: var(--muted);
	}
	.arrow {
		align-self: center;
		text-align: center;
		color: var(--accent);
		font-size: 18px;
	}
	.dept {
		display: grid;
		gap: 2px;
		padding: 6px 8px;
		border-left: 3px solid var(--accent);
		background: var(--sunk);
		border-radius: 4px;
		margin-bottom: 6px;
		font-size: 12.5px;
	}
	.dept a {
		overflow-wrap: anywhere;
	}
	.areas {
		display: flex;
		flex-wrap: wrap;
		gap: 4px;
		align-items: center;
		font-size: 12px;
		color: var(--muted);
	}
	.area {
		padding: 0 6px;
		border-radius: 999px;
		border: 1px solid var(--line);
		color: var(--fg);
		font-size: 11.5px;
	}
	.facets summary {
		font-size: 12px;
		cursor: pointer;
		color: var(--accent);
	}
	.facets dl {
		margin: 6px 0 0;
		display: grid;
		grid-template-columns: max-content minmax(0, 1fr);
		gap: 3px 10px;
		font-size: 12px;
	}
	.facets dt {
		color: var(--muted);
	}
	.facets dd {
		margin: 0;
	}
	.small {
		font-size: 11.5px;
	}
	.none {
		padding: 6px 0;
	}
	@media (max-width: 700px) {
		.arch-row {
			grid-template-columns: minmax(0, 1fr);
		}
		.arrow {
			transform: rotate(90deg);
			justify-self: start;
		}
	}
</style>
