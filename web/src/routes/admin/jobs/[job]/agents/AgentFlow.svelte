<script lang="ts">
	// J23: the message path as a strip of steps (Understand → Plan → Prepare → Act → Check reply). Each step shows the
	// agent role it runs as and the agents in it; a node shows health, instances and what is waiting.
	import type { AgentFleet, BusAgent } from '$lib/api/types/runtime';
	import { ROLES, STEPS, healthOf, plural, stepOf } from '$lib/runtime/J23-agents';
	import MilestoneChip from '$lib/ui/MilestoneChip.svelte';

	let {
		items,
		fleet,
		onpick,
	}: { items: BusAgent[]; fleet: AgentFleet | null; onpick: (a: BusAgent) => void } = $props();
</script>

<div class="j23-flow" data-testid="agent-flow">
	<div class="j23-end">
		<span class="j23-end-dot"></span>Customer message
	</div>
	{#each STEPS as st, i (st.key)}
		{@const list = items.filter((a) => stepOf(a) === st.key)}
		<section class="j23-step" aria-label={st.label}>
			<header>
				<span class="j23-n">{i + 1}</span>
				<b>{st.label}</b>
				<span class="j23-role j23-role-{st.role}">{ROLES[st.role].label}</span>
			</header>
			<p class="j23-does">{st.does}</p>
			<div class="j23-nodes" class:many={list.length > 3}>
				{#each list as a (a.id)}
					{@const h = healthOf(a, fleet)}
					<button
						type="button"
						class="j23-node j23-{h.word}"
						data-testid="agent-{a.id}"
						onclick={() => onpick(a)}
					>
						<span class="j23-node-top">
							<span class="j23-dot" aria-hidden="true"></span>
							<span class="j23-name">{a.name}</span>
						</span>
						<span class="j23-meta">
							<span class="j23-state">{h.label}</span>
							· ×{a.consumers}
							{#if a.consumers}· {a.per_min ?? 0}/min{/if}
						</span>
						{#if h.word === 'warn' || h.word === 'err'}
							<span class="j23-why"
								>{[a.lag ? `${a.lag} waiting` : '', a.pending ? `${a.pending} unfinished` : '']
									.filter(Boolean)
									.join(' · ')}</span
							>
						{/if}
						{#if a.kind === 'runner'}<MilestoneChip milestone="M4" note="operation runner" />{/if}
					</button>
				{:else}
					<span class="muted j23-none">No agent.</span>
				{/each}
			</div>
		</section>
	{/each}
	<div class="j23-end">
		<span class="j23-end-dot"></span>Reply sent
	</div>
</div>
<p class="j23-off muted" data-testid="assistant-off">
	<span class="j23-role j23-role-assistant">{ROLES.assistant.label}</span> role is off in v1: no agent uses
	free LLM tool use. The {plural(items.length, 'agent')} above run as qualifier, procedure or post-gate.
</p>

<style>
	.j23-flow {
		display: grid;
		grid-template-columns: auto repeat(5, minmax(0, 1fr)) auto;
		gap: 10px;
		align-items: stretch;
	}
	.j23-end {
		display: flex;
		flex-direction: column;
		align-items: center;
		justify-content: center;
		gap: 6px;
		font-size: 12px;
		color: var(--muted);
		writing-mode: vertical-rl;
		transform: rotate(180deg);
	}
	.j23-end-dot {
		width: 10px;
		height: 10px;
		border-radius: 50%;
		background: var(--cust);
	}
	.j23-step {
		position: relative;
		display: flex;
		flex-direction: column;
		gap: 6px;
		min-width: 0;
		padding: 10px;
		border: 1px solid var(--line);
		border-radius: 8px;
		background: var(--sunk);
	}
	.j23-step + .j23-step::before,
	.j23-step:first-of-type::before {
		content: '';
		position: absolute;
		left: -10px;
		top: 22px;
		width: 10px;
		border-top: 2px solid var(--line);
	}
	.j23-step header {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 6px;
	}
	.j23-n {
		display: inline-grid;
		place-items: center;
		width: 18px;
		height: 18px;
		border-radius: 50%;
		font-size: 11px;
		background: var(--panel);
		border: 1px solid var(--line);
		color: var(--muted);
	}
	.j23-role {
		font-family: var(--mono);
		font-size: 11px;
		padding: 1px 6px;
		border-radius: 4px;
		border: 1px solid currentColor;
	}
	.j23-role-qualifier {
		color: var(--cust);
	}
	.j23-role-procedure {
		color: var(--accent);
	}
	.j23-role-post_gate {
		color: var(--work);
	}
	.j23-role-assistant {
		color: var(--faint);
		border-style: dashed;
	}
	.j23-does {
		margin: 0;
		font-size: 12px;
		color: var(--muted);
		line-height: 1.35;
	}
	.j23-nodes {
		display: grid;
		gap: 6px;
		align-content: start;
	}
	.j23-node {
		all: unset;
		box-sizing: border-box;
		cursor: pointer;
		display: grid;
		gap: 2px;
		padding: 7px 9px;
		border-radius: 6px;
		border: 1px solid var(--line);
		border-left: 3px solid var(--ok);
		background: var(--panel);
		min-width: 0;
	}
	.j23-node:hover {
		border-color: var(--muted);
	}
	.j23-node:focus-visible {
		outline: 2px solid var(--accent);
		outline-offset: 2px;
	}
	.j23-node.j23-warn {
		border-left-color: var(--warn);
		background: color-mix(in srgb, var(--warn) 9%, var(--panel));
	}
	.j23-node.j23-err {
		border-left-color: var(--err);
		background: color-mix(in srgb, var(--err) 9%, var(--panel));
	}
	.j23-node.j23-idle {
		border-left-color: var(--faint);
	}
	.j23-node-top {
		display: flex;
		align-items: center;
		gap: 6px;
		min-width: 0;
	}
	.j23-dot {
		flex: none;
		width: 8px;
		height: 8px;
		border-radius: 50%;
		background: var(--ok);
	}
	.j23-warn .j23-dot {
		background: var(--warn);
	}
	.j23-err .j23-dot {
		background: var(--err);
	}
	.j23-idle .j23-dot {
		background: var(--faint);
	}
	.j23-name {
		font-weight: 600;
		font-size: 13px;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.j23-meta {
		font-size: 12px;
		color: var(--muted);
	}
	.j23-warn .j23-state {
		color: var(--warn);
		font-weight: 600;
	}
	.j23-err .j23-state {
		color: var(--err);
		font-weight: 600;
	}
	.j23-why {
		font-size: 12px;
		color: var(--warn);
	}
	.j23-none {
		font-size: 12px;
	}
	.j23-off {
		margin: 10px 0 0;
		font-size: 12px;
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		align-items: center;
	}
	@media (max-width: 1100px) {
		.j23-flow {
			grid-template-columns: repeat(3, minmax(0, 1fr));
		}
		.j23-end {
			display: none;
		}
		.j23-step::before {
			display: none;
		}
	}
	@media (max-width: 640px) {
		.j23-flow {
			grid-template-columns: 1fr;
		}
		.j23-nodes.many {
			grid-template-columns: repeat(2, minmax(0, 1fr));
		}
	}
</style>
