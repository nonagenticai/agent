<script lang="ts">
	// P14 concept band: where the three gate points sit in one conversation turn (spec 03 §3.1/§3.2, 10 §6).
	// Each gate point is a filter button carrying its question count and how many of those fail closed.
	import Lock from '$lib/ontology/Lock.svelte';
	import type { GatePoint, GateQuestion } from '$lib/api/types/ontology';

	let { items, point = $bindable() }: { items: GateQuestion[]; point: GatePoint | 'all' } = $props();

	type Step =
		| { kind: 'stage'; label: string; sub: string }
		| { kind: 'gate'; point: GatePoint; label: string; sub: string };
	const STEPS: Step[] = [
		{ kind: 'stage', label: 'Customer message', sub: 'arrives at the edge' },
		{
			kind: 'gate',
			point: 'P1',
			label: 'Message gate',
			sub: 'Safety, tone, theme, stage, activity — before the qualifier reads it',
		},
		{ kind: 'stage', label: 'Qualifier', sub: 'builds the request' },
		{
			kind: 'gate',
			point: 'P2',
			label: 'Request gate',
			sub: 'Right record, right request — before a case opens',
		},
		{ kind: 'stage', label: 'Procedure', sub: 'runs operations, drafts a reply' },
		{
			kind: 'gate',
			point: 'R1',
			label: 'Resolution gate',
			sub: 'Fact-check the draft against its facts — before it leaves',
		},
		{ kind: 'stage', label: 'Reply sent', sub: 'or a person takes over' },
	];
	const at = (p: GatePoint) => items.filter((q) => q.point === p);
	const toggle = (p: GatePoint) => (point = point === p ? 'all' : p);
</script>

<ol class="gf" aria-label="Where gates run in one conversation turn">
	{#each STEPS as s, i (i)}
		<li class="gf-step {s.kind}">
			{#if s.kind === 'gate'}
				{@const qs = at(s.point)}
				{@const closed = qs.filter((q) => q.fail === 'closed').length}
				<button
					type="button"
					class="gf-gate"
					class:on={point === s.point}
					aria-pressed={point === s.point}
					onclick={() => toggle(s.point)}
				>
					<span class="gf-pt mono">{s.point}</span>
					<span class="gf-label">{s.label}</span>
					<span class="gf-sub">{s.sub}</span>
					<span class="gf-count">
						<b>{qs.length}</b>
						{qs.length === 1 ? 'question' : 'questions'}{#if closed}<span class="gf-closed"
								><Lock /> {closed} fail closed</span
							>{/if}
					</span>
				</button>
			{:else}
				<div class="gf-stage">
					<span class="gf-label">{s.label}</span>
					<span class="gf-sub">{s.sub}</span>
				</div>
			{/if}
		</li>
	{/each}
</ol>

<style>
	.gf {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		align-items: stretch;
		gap: 0;
	}
	.gf-step {
		display: flex;
		align-items: stretch;
		position: relative;
		min-width: 0;
	}
	.gf-step.stage {
		flex: 0.8 1 0;
	}
	.gf-step.gate {
		flex: 1.3 1 0;
	}
	/* connector arrow between steps */
	.gf-step + .gf-step {
		padding-left: 18px;
	}
	.gf-step + .gf-step::before {
		content: '';
		position: absolute;
		left: 3px;
		top: 50%;
		width: 10px;
		border-top: 2px solid var(--line);
	}
	.gf-step + .gf-step::after {
		content: '';
		position: absolute;
		left: 10px;
		top: calc(50% - 4px);
		border: 5px solid transparent;
		border-left-color: var(--line);
		border-right: 0;
	}
	.gf-stage,
	.gf-gate {
		display: flex;
		flex-direction: column;
		gap: 3px;
		width: 100%;
		padding: 10px 12px;
		border-radius: 8px;
		text-align: left;
		min-width: 0;
	}
	.gf-stage {
		border: 1px dashed var(--line);
		justify-content: center;
		color: var(--muted);
	}
	.gf-gate {
		border: 1px solid color-mix(in srgb, var(--tool) 45%, var(--line));
		background: color-mix(in srgb, var(--tool) 7%, var(--panel));
		color: var(--fg);
		cursor: pointer;
		font: inherit;
	}
	.gf-gate:hover {
		border-color: var(--tool);
	}
	.gf-gate.on {
		border-color: var(--tool);
		box-shadow: 0 0 0 2px color-mix(in srgb, var(--tool) 35%, transparent);
	}
	.gf-pt {
		font-weight: 600;
		color: var(--tool);
	}
	.gf-label {
		font: 600 13.5px/1.25 var(--cond);
	}
	.gf-stage .gf-label {
		color: var(--fg);
		font-weight: 500;
	}
	.gf-sub {
		font-size: 12px;
		line-height: 1.35;
		color: var(--muted);
	}
	.gf-count {
		margin-top: auto;
		padding-top: 4px;
		font-size: 12px;
		display: flex;
		flex-wrap: wrap;
		gap: 4px 10px;
	}
	.gf-closed {
		color: var(--err);
	}
	@media (max-width: 900px) {
		.gf {
			flex-direction: column;
		}
		.gf-step + .gf-step {
			padding-left: 0;
			padding-top: 16px;
		}
		.gf-step + .gf-step::before {
			left: 22px;
			top: 2px;
			width: 0;
			height: 8px;
			border-top: 0;
			border-left: 2px solid var(--line);
		}
		.gf-step + .gf-step::after {
			left: 18px;
			top: 9px;
			border: 5px solid transparent;
			border-top-color: var(--line);
			border-bottom: 0;
		}
		.gf-stage {
			flex-direction: row;
			align-items: baseline;
			gap: 8px;
			padding: 6px 12px;
		}
	}
</style>
