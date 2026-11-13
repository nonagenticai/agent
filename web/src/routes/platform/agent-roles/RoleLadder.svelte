<script lang="ts">
	// P15 overview: (1) the order in which a case turn hands off between principals on the bus, and (2) the
	// least-privilege ladder — per role, the side-effect ceiling and how many granted operations sit at each class.
	import type { AgentRoleView, SideEffect } from '$lib/api/types/ontology';
	import { SIDE_EFFECT_LABEL } from '$lib/ontology/shared';
	import { SIDE_EFFECTS, ceilingLadder } from './logic';

	let { roles }: { roles: AgentRoleView[] } = $props();
	const ladder = $derived(ceilingLadder(roles));
	const byKey = $derived(new Map(ladder.map((r) => [r.role.role, r])));
	const flowKeys = ['qualifier', 'procedure', 'post_gate'] as const;
	const assistant = $derived(byKey.get('assistant'));
	const SE_HINT: Record<SideEffect, string> = {
		R: 'reads only',
		W: 'reversible writes',
		X: 'irreversible (cancel, refund, reset)',
		P: 'moves money (hosted link only)',
	};
	const CONNECTOR: Record<string, string> = {
		qualifier: 'pre-gate verdict → op.call',
		procedure: 'op.result → reply draft',
	};
</script>

<div class="rl" data-testid="agent-roles-overview">
	<h3 class="rl-h">How a case turn hands off between principals</h3>
	<ol class="rl-flow">
		<li class="rl-end">Customer message</li>
		{#each flowKeys as k (k)}
			{@const row = byKey.get(k)}
			{#if row}
				<li class="rl-arrow" aria-hidden="true">→</li>
				<li class="rl-node se-{row.role.max_side_effect}" class:off={!row.role.enabled}>
					<a href="#role-{k}" class="rl-node-name">{row.role.label}</a>
					<span class="rl-node-where">{row.role.used_by}</span>
					<span class="rl-node-meta mono">≤ {row.role.max_side_effect} · {row.total} ops</span>
				</li>
				{#if CONNECTOR[k]}<li class="rl-conn" aria-hidden="true">{CONNECTOR[k]}</li>{/if}
			{/if}
		{/each}
		<li class="rl-arrow" aria-hidden="true">→</li>
		<li class="rl-end">Reply</li>
	</ol>
	{#if assistant}
		<p class="rl-assist">
			<span class="rl-node-name">{assistant.role.label}</span>
			({assistant.role.used_by}) — {assistant.role.enabled
				? `enabled, ≤ ${assistant.role.max_side_effect}`
				: 'defined but switched off in v1: no free LLM tool use'}.
		</p>
	{/if}

	<h3 class="rl-h">Least privilege: ceiling and grants per role</h3>
	<div class="rl-grid" role="table" aria-label="Side-effect ceiling per role">
		<div class="rl-row rl-head" role="row">
			<span role="columnheader">Role</span>
			{#each SIDE_EFFECTS as se (se)}
				<span role="columnheader" class="rl-colh">
					<span class="mono rl-long">{SIDE_EFFECT_LABEL[se]}</span><span class="mono rl-short">{se}</span>
					<span class="rl-colhint">{SE_HINT[se]}</span>
				</span>
			{/each}
		</div>
		{#each ladder as row (row.role.role)}
			<div class="rl-row" role="row">
				<a role="rowheader" class="rl-rowh" href="#role-{row.role.role}">{row.role.label}</a>
				{#each SIDE_EFFECTS as se (se)}
					{@const c = row.cells[se]}
					<span
						role="cell"
						class="rl-cell se-{se} {c.kind}"
						class:under={c.kind === 'off' && c.underCeiling}
						title={c.kind === 'allowed' ? c.ops.map((o) => o.label).join(', ') || 'none granted' : undefined}
					>
						{#if c.kind === 'allowed'}
							<span class="rl-count">{c.ops.length}</span>
							<span class="rl-cap">{c.ops.length === 1 ? 'op' : 'ops'}</span>
						{:else if c.kind === 'locked'}
							<span class="rl-cap">above ceiling</span>
						{:else}
							<span class="rl-cap">off</span>
						{/if}
					</span>
				{/each}
			</div>
		{/each}
	</div>
	<p class="rl-foot">
		Shaded = allowed by the role's max side effect; the number is how many granted operations sit at that
		class. A grant above the ceiling is refused, so an X/P call can only come from the procedure role — and
		only in the activity that declares it.
	</p>
</div>

<style>
	.rl {
		--se-R: var(--ok);
		--se-W: var(--cust);
		--se-X: var(--warn);
		--se-P: var(--err);
	}
	.rl-h {
		font-size: 12px;
		text-transform: uppercase;
		letter-spacing: 0.05em;
		color: var(--faint);
		margin: 0 0 8px;
		font-weight: 600;
	}
	.rl-h + .rl-flow {
		margin-top: 0;
	}
	.rl-flow {
		list-style: none;
		padding: 0;
		margin: 0 0 6px;
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: 6px;
	}
	.rl-end {
		font-size: 12.5px;
		color: var(--muted);
		padding: 6px 10px;
		border: 1px dashed var(--line);
		border-radius: 999px;
	}
	.rl-arrow {
		color: var(--faint);
	}
	.rl-conn {
		font-size: 11.5px;
		color: var(--faint);
		font-style: italic;
	}
	.rl-node {
		display: grid;
		gap: 2px;
		padding: 7px 10px;
		border: 1px solid var(--line);
		border-left: 4px solid var(--se-R);
		border-radius: var(--r);
		background: var(--panel);
	}
	.rl-node.se-W {
		border-left-color: var(--se-W);
	}
	.rl-node.se-X {
		border-left-color: var(--se-X);
	}
	.rl-node.se-P {
		border-left-color: var(--se-P);
	}
	.rl-node.off {
		opacity: 0.6;
		border-style: dashed;
	}
	.rl-node-name {
		font-weight: 600;
		font-size: 13px;
	}
	.rl-node-where {
		font-size: 11.5px;
		color: var(--muted);
	}
	.rl-node-meta {
		font-size: 11.5px;
		color: var(--faint);
	}
	.rl-assist {
		font-size: 12.5px;
		color: var(--muted);
		margin: 4px 0 16px;
	}
	.rl-grid {
		display: grid;
		gap: 4px;
	}
	.rl-row {
		display: grid;
		grid-template-columns: minmax(90px, 1.1fr) repeat(4, minmax(0, 1fr));
		gap: 4px;
		align-items: stretch;
	}
	.rl-head {
		font-size: 11.5px;
		color: var(--faint);
	}
	.rl-colh {
		display: grid;
		gap: 2px;
		padding: 0 4px;
	}
	.rl-colhint {
		font-size: 11px;
	}
	.rl-rowh {
		font-weight: 600;
		font-size: 13px;
		align-self: center;
	}
	.rl-cell {
		--c: var(--se-R);
		display: flex;
		align-items: baseline;
		gap: 4px;
		padding: 8px 10px;
		border-radius: var(--r);
		min-height: 36px;
		font-size: 12px;
	}
	.rl-cell.se-W {
		--c: var(--se-W);
	}
	.rl-cell.se-X {
		--c: var(--se-X);
	}
	.rl-cell.se-P {
		--c: var(--se-P);
	}
	.rl-cell.allowed {
		background: color-mix(in srgb, var(--c) 16%, transparent);
		color: var(--c);
	}
	.rl-cell.locked {
		background: repeating-linear-gradient(135deg, var(--sunk) 0 6px, transparent 6px 12px);
		color: var(--faint);
	}
	.rl-cell.off {
		border: 1px dashed var(--line);
		color: var(--faint);
	}
	.rl-cell.off.under {
		background: color-mix(in srgb, var(--c) 5%, transparent);
	}
	.rl-count {
		font: 600 16px/1 var(--mono);
	}
	.rl-cap {
		font-size: 11.5px;
	}
	.rl-short {
		display: none;
	}
	.rl-foot {
		font-size: 12px;
		color: var(--faint);
		margin: 8px 0 0;
	}
	@media (max-width: 560px) {
		.rl-row {
			grid-template-columns: 76px repeat(4, minmax(0, 1fr));
		}
		.rl-long,
		.rl-colhint,
		.rl-cap {
			display: none;
		}
		.rl-short {
			display: inline;
		}
		.rl-cell {
			padding: 8px 6px;
		}
		.rl-conn {
			display: none;
		}
	}
</style>
