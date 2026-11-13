<script lang="ts">
	// P18 pipeline: the nine generator steps as one left-to-right flow. Each node says what the step produces and how
	// the human decided the LLM's drafts there (kept / dropped / waiting). Clicking a node opens that step below.
	import type { GeneratorRun, GeneratorStepKey } from '$lib/api/types/journey';
	import { STEP_PRODUCES, nodeState, stepCode, stepName, stepTally } from '$lib/journey/P18Run';

	let {
		run: r,
		active,
		onselect,
	}: { run: GeneratorRun; active: GeneratorStepKey; onselect: (k: GeneratorStepKey) => void } = $props();

	const GLYPH = { done: '✓', current: '●', blocked: '!', todo: '' } as const;
	const STATE_TEXT = {
		done: 'done',
		current: 'in progress',
		blocked: 'blocked',
		todo: 'not reached',
	} as const;
</script>

<ol class="p18-flow" aria-label="Generator steps">
	{#each r.steps as s, i (s.key)}
		{@const st = nodeState(r, s.key)}
		{@const t = stepTally(r, s.key)}
		<li class="p18-node {st}" class:sel={active === s.key}>
			<button
				type="button"
				class="p18-node-btn"
				aria-pressed={active === s.key}
				aria-current={st === 'current' || st === 'blocked' ? 'step' : undefined}
				onclick={() => onselect(s.key)}
			>
				<span class="p18-dot" aria-hidden="true">{GLYPH[st] || i + 1}</span>
				<span class="p18-code">{stepCode(s.key) || ' '}</span>
				<span class="p18-name">{stepName(s.key)}</span>
				<span class="p18-state">{STATE_TEXT[st]}</span>
				{#if s.key === 'validate' && r.red}
					<span class="p18-tally"><span class="p18-red">{r.red} red</span></span>
				{:else if t.total}
					<span class="p18-bar" aria-hidden="true">
						{#if t.kept}<span class="k" style:flex={t.kept}></span>{/if}
						{#if t.dropped}<span class="d" style:flex={t.dropped}></span>{/if}
						{#if t.pending}<span class="p" style:flex={t.pending}></span>{/if}
					</span>
					<span class="p18-tally"
						>{t.kept} kept{#if t.dropped}&nbsp;· {t.dropped} dropped{/if}{#if t.pending}&nbsp;·
							<b>{t.pending} to decide</b>{/if}</span
					>
				{:else}
					<span class="p18-tally faint">→ {STEP_PRODUCES[s.key]}</span>
				{/if}
			</button>
		</li>
	{/each}
</ol>

<style>
	.p18-flow {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		grid-template-columns: repeat(9, minmax(0, 1fr));
		gap: 6px;
		position: relative;
	}
	.p18-node {
		position: relative;
		min-width: 0;
	}
	/* connector to the next node */
	.p18-node:not(:last-child)::after {
		content: '';
		position: absolute;
		top: 21px;
		left: calc(50% + 14px);
		right: calc(-50% + 8px);
		height: 2px;
		background: var(--line);
	}
	.p18-node.done:not(:last-child)::after {
		background: var(--ok);
	}
	.p18-node-btn {
		all: unset;
		box-sizing: border-box;
		cursor: pointer;
		width: 100%;
		height: 100%;
		display: flex;
		flex-direction: column;
		align-items: center;
		text-align: center;
		gap: 3px;
		padding: 8px 4px 10px;
		border-radius: var(--r);
		border: 1px solid transparent;
	}
	.p18-node-btn:hover {
		background: var(--sunk);
	}
	.p18-node-btn:focus-visible {
		outline: 2px solid var(--accent);
		outline-offset: 1px;
	}
	.sel .p18-node-btn {
		background: color-mix(in srgb, var(--accent) 8%, transparent);
		border-color: color-mix(in srgb, var(--accent) 40%, transparent);
	}
	.p18-dot {
		position: relative;
		z-index: 1;
		width: 26px;
		height: 26px;
		border-radius: 50%;
		display: grid;
		place-items: center;
		font: 600 12px/1 var(--mono);
		border: 2px solid var(--line);
		background: var(--panel);
		color: var(--muted);
	}
	.done .p18-dot {
		border-color: var(--ok);
		background: var(--ok);
		color: var(--panel);
	}
	.current .p18-dot {
		border-color: var(--accent);
		color: var(--accent);
		box-shadow: 0 0 0 4px color-mix(in srgb, var(--accent) 18%, transparent);
	}
	.blocked .p18-dot {
		border-color: var(--err);
		background: var(--err);
		color: var(--panel);
	}
	.p18-code {
		font: 500 10.5px/1.2 var(--mono);
		color: var(--faint);
		margin-top: 2px;
	}
	.p18-name {
		font-weight: 600;
		font-size: 12.5px;
		line-height: 1.25;
	}
	.todo .p18-name {
		color: var(--muted);
	}
	.p18-state {
		font-size: 11px;
		color: var(--muted);
	}
	.current .p18-state {
		color: var(--accent);
	}
	.blocked .p18-state {
		color: var(--err);
		font-weight: 600;
	}
	.p18-bar {
		display: flex;
		width: 80%;
		height: 5px;
		border-radius: 3px;
		overflow: hidden;
		gap: 1px;
		margin-top: 3px;
		background: var(--line-2);
	}
	.p18-bar .k {
		background: var(--ok);
	}
	.p18-bar .d {
		background: var(--dead);
	}
	.p18-bar .p {
		background: var(--warn);
	}
	.p18-tally {
		font-size: 11px;
		color: var(--muted);
		line-height: 1.3;
	}
	.p18-tally b {
		color: var(--warn);
	}
	.p18-tally.faint {
		color: var(--faint);
	}
	.p18-red {
		color: var(--err);
		font-weight: 600;
	}

	@media (max-width: 900px) {
		.p18-flow {
			grid-template-columns: minmax(0, 1fr);
			gap: 2px;
		}
		.p18-node:not(:last-child)::after {
			top: 34px;
			bottom: -10px;
			left: 21px;
			right: auto;
			width: 2px;
			height: auto;
		}
		.p18-node-btn {
			display: grid;
			grid-template-columns: 26px minmax(0, 1fr) auto;
			grid-template-areas:
				'dot name state'
				'dot code code'
				'dot bar bar'
				'dot tally tally';
			text-align: left;
			align-items: center;
			column-gap: 10px;
			row-gap: 2px;
			padding: 8px;
		}
		.p18-dot {
			grid-area: dot;
			align-self: start;
		}
		.p18-name {
			grid-area: name;
		}
		.p18-code {
			grid-area: code;
			margin: 0;
		}
		.p18-state {
			grid-area: state;
		}
		.p18-bar {
			grid-area: bar;
			width: 100%;
		}
		.p18-tally {
			grid-area: tally;
		}
	}
</style>
