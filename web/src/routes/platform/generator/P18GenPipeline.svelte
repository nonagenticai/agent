<script lang="ts">
	// P18: the generator pipeline as a diagram — four phases, the LLM/person split per step, the Validate gate, and
	// which open runs are parked where.
	import type { GeneratorRunSummary } from '$lib/api/types/journey';
	import { P18_PHASES, P18_STEPS, P18_TENANT_STEPS, runsAtStep } from '$lib/journey/P18GenPipeline';

	let { runs }: { runs: GeneratorRunSummary[] } = $props();
	const parked = $derived(runsAtStep(runs));
</script>

<div class="p18-pipe" data-testid="p18-pipeline">
	{#each P18_PHASES as ph, pi (ph.key)}
		<section class="p18-phase" class:ship={ph.key === 'ship'}>
			<header>
				<span class="p18-phase-n">{pi + 1}</span>
				<div>
					<b>{ph.label}</b>
					<small>{ph.hint}</small>
				</div>
			</header>
			<ol>
				{#each P18_STEPS.filter((s) => s.phase === ph.key) as s (s.key)}
					<li
						class="p18-node"
						class:gate={s.key === 'validate'}
						class:publish={s.key === 'package'}
						data-step={s.key}
					>
						<div class="p18-node-top">
							<span class="p18-code mono">{s.code}</span>
							<span class="p18-label">{s.label}</span>
							{#if s.drafts}<span class="p18-tag llm" title="The LLM drafts here (costs tokens)">LLM</span
								>{/if}
							{#if !P18_TENANT_STEPS.includes(s.key)}<span
									class="p18-tag plat"
									title="Tenant runs skip this step">platform only</span
								>{/if}
						</div>
						<p class="p18-what">
							{#if s.drafts}<span class="muted">drafts</span> {s.drafts} ·
							{/if}<span class="muted">you</span>
							{s.decides}
						</p>
						{#if s.key === 'validate'}
							<p class="p18-gate-note">Red items (rules 1–7) block Package</p>
						{:else if s.writes !== '—'}
							<p class="p18-writes mono">→ {s.writes}</p>
						{/if}
						{#if parked[s.key]?.length}
							<div class="p18-parked">
								{#each parked[s.key] ?? [] as r (r.id)}
									<a
										class="p18-pill"
										class:red={r.red > 0}
										href="/platform/generator/{r.id}"
										title="{r.industry} / {r.job} — {r.red ? `${r.red} red` : r.status}"
										>#{r.id} here{#if r.red}&nbsp;· {r.red} red{/if}</a
									>
								{/each}
							</div>
						{/if}
					</li>
				{/each}
			</ol>
		</section>
	{/each}
</div>

<style>
	.p18-pipe {
		display: grid;
		grid-template-columns: 1fr 1.35fr 1.1fr 1fr;
		gap: 10px;
	}
	.p18-phase {
		background: var(--sunk);
		border-radius: var(--r);
		padding: 10px;
		position: relative;
		min-width: 0;
	}
	.p18-phase:not(:last-child)::after {
		content: '→';
		position: absolute;
		right: -10px;
		top: 12px;
		width: 10px;
		text-align: center;
		color: var(--faint);
		font-size: 13px;
	}
	.p18-phase.ship {
		background: color-mix(in srgb, var(--ok) 8%, var(--sunk));
	}
	header {
		display: flex;
		gap: 8px;
		align-items: flex-start;
		margin-bottom: 8px;
	}
	header b {
		display: block;
		font-size: 13px;
	}
	header small {
		color: var(--muted);
		font-size: 11.5px;
		line-height: 1.3;
		display: block;
	}
	.p18-phase-n {
		flex: none;
		width: 20px;
		height: 20px;
		border-radius: 50%;
		background: var(--panel);
		border: 1px solid var(--line);
		font-size: 11px;
		display: grid;
		place-items: center;
		color: var(--muted);
	}
	ol {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 6px;
	}
	.p18-node {
		background: var(--panel);
		border: 1px solid var(--line);
		border-radius: var(--r);
		padding: 7px 9px;
		min-width: 0;
	}
	.p18-node.gate {
		border: 1px dashed var(--err);
	}
	.p18-node.publish {
		border-color: var(--ok);
	}
	.p18-node-top {
		display: flex;
		gap: 6px;
		align-items: baseline;
		flex-wrap: wrap;
	}
	.p18-code {
		font-size: 11px;
		color: var(--accent);
		font-weight: 600;
	}
	.p18-label {
		font-weight: 600;
		font-size: 13px;
	}
	.p18-tag {
		font-size: 10px;
		border-radius: 3px;
		padding: 0 4px;
		line-height: 15px;
		text-transform: uppercase;
		letter-spacing: 0.03em;
	}
	.p18-tag.llm {
		background: color-mix(in srgb, var(--tool) 14%, transparent);
		color: var(--tool);
	}
	.p18-tag.plat {
		border: 1px solid var(--line);
		color: var(--faint);
		text-transform: none;
		letter-spacing: 0;
	}
	.p18-what {
		margin: 3px 0 0;
		font-size: 11.5px;
		line-height: 1.35;
	}
	.p18-writes {
		margin: 3px 0 0;
		font-size: 10.5px;
		color: var(--faint);
		overflow-wrap: anywhere;
	}
	.p18-gate-note {
		margin: 3px 0 0;
		font-size: 11px;
		color: var(--err);
		font-weight: 600;
	}
	.p18-parked {
		display: flex;
		gap: 4px;
		flex-wrap: wrap;
		margin-top: 5px;
	}
	.p18-pill {
		font-size: 11px;
		font-weight: 600;
		border-radius: 10px;
		padding: 1px 7px;
		background: color-mix(in srgb, var(--accent) 14%, transparent);
		color: var(--accent);
		text-decoration: none;
	}
	.p18-pill.red {
		background: color-mix(in srgb, var(--err) 14%, transparent);
		color: var(--err);
	}
	@media (max-width: 1100px) {
		.p18-pipe {
			grid-template-columns: 1fr 1fr;
		}
		.p18-phase::after {
			display: none;
		}
	}
	@media (max-width: 640px) {
		.p18-pipe {
			grid-template-columns: 1fr;
		}
	}
</style>
