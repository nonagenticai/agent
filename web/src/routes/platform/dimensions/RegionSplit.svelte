<script lang="ts">
	// P13 (D59): "region" mixed four concerns. Each now lives where it belongs; the region dimension itself stays a
	// placeholder (recorded in the release, selects nothing).
	import type { RegionConcern } from '$lib/api/types/platform';
	import Chip from '$lib/ui/Chip.svelte';

	let { concerns }: { concerns: RegionConcern[] } = $props();

	const OWNER: Record<RegionConcern['owner'], string> = {
		tenant: 'Tenant setting',
		job: 'Job setting',
		jurisdiction_pack: 'Jurisdiction pack',
		tenant_wiring: 'Tenant wiring',
	};
	const STATUS: Record<RegionConcern['status'], { text: string; variant: 'ok' | 'muted' | 'accent' }> = {
		live: { text: 'modelled now', variant: 'ok' },
		future: { text: 'future · no behaviour', variant: 'accent' },
		note: { text: 'note only', variant: 'muted' },
	};
</script>

<div class="split" data-testid="region-split">
	{#each concerns as c (c.key)}
		<article class="concern {c.status}" data-testid="region-concern" data-key={c.key}>
			<div class="top">
				<span class="owner">{OWNER[c.owner]}</span>
				<Chip text={STATUS[c.status].text} variant={STATUS[c.status].variant} />
			</div>
			<h3>{c.label}</h3>
			<p>{c.text}</p>
			<div class="ex">
				{#each c.examples as e (e)}<span class="mono">{e}</span>{/each}
			</div>
			{#if c.where}<a class="where" href={c.where.href}>Set in {c.where.label} →</a>{/if}
		</article>
	{/each}
</div>

<style>
	.split {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(200px, 1fr));
		gap: 10px;
		margin-bottom: 12px;
	}
	.concern {
		border: 1px solid var(--line);
		border-radius: var(--r);
		padding: 10px;
		display: grid;
		gap: 4px;
		align-content: start;
	}
	.concern.live {
		border-top: 3px solid var(--ok);
	}
	.concern.future {
		border-top: 3px dashed var(--accent);
	}
	.concern.note {
		border-top: 3px dotted var(--faint);
	}
	.top {
		display: flex;
		justify-content: space-between;
		gap: 6px;
		align-items: center;
	}
	.owner {
		font: 600 10.5px/1.2 var(--cond);
		letter-spacing: 0.07em;
		text-transform: uppercase;
		color: var(--muted);
	}
	h3 {
		margin: 0;
		font-size: 14px;
	}
	p {
		margin: 0;
		font-size: 12.5px;
		color: var(--muted);
	}
	.ex {
		display: flex;
		flex-wrap: wrap;
		gap: 4px;
	}
	.ex span {
		font-size: 11.5px;
		padding: 0 6px;
		border-radius: 4px;
		background: var(--sunk);
	}
	.where {
		font-size: 12px;
	}
</style>
