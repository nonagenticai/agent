<script lang="ts">
	// J12: the shape of a procedure at a glance: its phases left → right, one lettered tag per step by what it does
	// (knowledge, identify, rule, read, change, case). Parallel steps stack; a gate step carries a diamond.
	import type { ProcedureStep } from '$lib/api/types/ontology';
	import KindTag from './KindTag.svelte';
	import { STEP_KIND, groupPhases, stepKind, stepOp } from './view';

	let { steps, outside = [] }: { steps: ProcedureStep[]; outside?: string[] } = $props();
	const phases = $derived(groupPhases(steps));
</script>

<ol class="shape" aria-label="Steps: {steps.map((s) => STEP_KIND[stepKind(s)].label).join(', ')}">
	{#each phases as ph, i (ph.phase)}
		{#if i}<li class="shape-arrow" aria-hidden="true">›</li>{/if}
		<li class="shape-phase">
			{#each ph.steps as s (s.step_number)}
				<KindTag
					kind={stepKind(s)}
					gate={s.gate}
					bad={outside.includes(stepOp(s))}
					title="{s.step_number}. {s.name} · {stepOp(s)}{s.gate ? ' · gate' : ''}"
				/>
			{/each}
		</li>
	{/each}
</ol>

<style>
	.shape {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 4px;
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.shape-phase {
		display: inline-flex;
		flex-direction: column;
		gap: 3px;
	}
	.shape-arrow {
		color: var(--faint);
		font-size: 12px;
	}
</style>
