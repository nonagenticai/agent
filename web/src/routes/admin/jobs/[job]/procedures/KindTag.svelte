<script lang="ts">
	// J12: one step kind as a small lettered tag (letter + colour, so the kind is never colour alone). `wide` adds
	// the kind's name; `gate` adds the diamond of a step that can end the case as not eligible.
	import type { StepKind } from './view';
	import { STEP_KIND } from './view';

	let {
		kind,
		gate = false,
		bad = false,
		wide = false,
		title,
	}: { kind: StepKind; gate?: boolean; bad?: boolean; wide?: boolean; title?: string } = $props();
	const LETTER: Record<StepKind, string> = {
		knowledge: 'K',
		identity: 'I',
		rule: 'R',
		read: 'D',
		change: 'C',
		case: 'S',
	};
</script>

<span class="kt k-{kind}" class:bad class:wide title={title ?? STEP_KIND[kind].hint}
	><b>{LETTER[kind]}</b>{#if wide}<span class="kt-name">{STEP_KIND[kind].label}</span>{/if}{#if gate}<span
			class="kt-gate"
			aria-label="gate">◆</span
		>{/if}</span
>

<style>
	.kt {
		position: relative;
		display: inline-flex;
		align-items: center;
		gap: 5px;
		min-width: 22px;
		height: 18px;
		padding: 0 5px;
		justify-content: center;
		border-radius: 4px;
		font: 600 10.5px/1 var(--mono);
		color: var(--kc);
		background: color-mix(in srgb, var(--kc) 14%, transparent);
		border: 1px solid color-mix(in srgb, var(--kc) 40%, transparent);
		white-space: nowrap;
	}
	.kt.wide {
		height: 20px;
		padding: 0 7px;
	}
	.kt-name {
		font: 500 11.5px/1 var(--sans);
	}
	.kt.bad {
		outline: 2px solid var(--err);
		outline-offset: 1px;
	}
	.kt-gate {
		position: absolute;
		top: -6px;
		right: -5px;
		font-size: 9px;
		color: var(--work);
	}
	.k-knowledge {
		--kc: var(--tool);
	}
	.k-identity {
		--kc: var(--cust);
	}
	.k-rule {
		--kc: var(--work);
	}
	.k-read {
		--kc: var(--ok);
	}
	.k-change {
		--kc: var(--err);
	}
	.k-case {
		--kc: var(--accent);
	}
</style>
