<script lang="ts">
	// The release as a PR-style flow: a steps strip, the changes on the left (children), and on the right the checks
	// with Approve and Go live. Both buttons are ActionButtons: disabled, they say why. Presentational — the page owns
	// the data and the calls.
	import type { Snippet } from 'svelte';
	import ActionButton from '$lib/ui/ActionButton.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import { STEPS, type Check } from './flow';

	let {
		step,
		checks,
		approveReason,
		goLiveReason,
		goLiveLabel,
		onapprove,
		ongolive,
		children,
		below,
		extra,
	}: {
		step: number;
		checks: Check[];
		approveReason: string | null;
		goLiveReason: string | null;
		/** The release Go live would put in front of customers, when one is waiting. */
		goLiveLabel: string | null;
		onapprove: () => void;
		ongolive: () => void;
		children: Snippet;
		below?: Snippet;
		extra?: Snippet;
	} = $props();
	const CHIP: Record<Check['state'], 'ok' | 'err' | 'warn' | 'muted'> = {
		ok: 'ok',
		err: 'err',
		warn: 'warn',
		muted: 'muted',
	};
	const WORD: Record<Check['state'], string> = { ok: 'Pass', err: 'Fix', warn: 'Check', muted: 'Open' };
</script>

<ol class="steps" data-testid="release-steps" aria-label="Release steps">
	{#each STEPS as s, i (s)}
		<li
			class:done={i < step}
			class:on={i === step}
			aria-current={i === step ? 'step' : undefined}
			data-step={i + 1}
			data-state={i < step ? 'done' : i === step ? 'current' : 'todo'}
		>
			<span class="n">{i < step ? '✓' : i + 1}</span>{s}
		</li>
	{/each}
</ol>

<div class="flow">
	<div class="col">{@render children()}</div>
	<div class="col">
		<Card title="Checks" sub="Everything here must pass before this release can go live.">
			<ul class="checks" data-testid="release-checks">
				{#each checks as c (c.key)}
					<li data-check={c.key} data-state={c.state}>
						<Chip text={WORD[c.state]} variant={CHIP[c.state]} />
						<span class="lbl"><b>{c.label}</b> <span class="muted">{c.detail}</span></span>
						{#if c.href}<a href={c.href}>{c.linkLabel ?? 'Open'}</a>{/if}
					</li>
				{/each}
			</ul>
			{#if extra}{@render extra()}{/if}
			<div class="acts">
				<ActionButton disabledReason={approveReason} onclick={onapprove}>Approve</ActionButton>
				<ActionButton primary disabledReason={goLiveReason} onclick={ongolive}
					>Go live{goLiveLabel ? ` with ${goLiveLabel}` : ''}</ActionButton
				>
			</div>
		</Card>
		{#if below}{@render below()}{/if}
	</div>
</div>

<style>
	.steps {
		list-style: none;
		margin: 0 0 14px;
		padding: 0;
		display: flex;
		flex-wrap: wrap;
		gap: 6px 18px;
	}
	.steps li {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		font-size: 13px;
		color: var(--muted);
	}
	.steps li.on {
		color: var(--text);
		font-weight: 600;
	}
	.steps .n {
		display: inline-grid;
		place-items: center;
		width: 22px;
		height: 22px;
		border-radius: 50%;
		border: 1px solid var(--line);
		font-size: 12px;
	}
	.steps li.on .n {
		background: var(--accent);
		border-color: var(--accent);
		color: #fff;
	}
	.steps li.done .n {
		border-color: var(--ok);
		color: var(--ok);
	}
	.flow {
		display: grid;
		grid-template-columns: minmax(0, 1.15fr) minmax(0, 1fr);
		gap: 14px;
		align-items: start;
	}
	.col {
		display: grid;
		gap: 14px;
		min-width: 0;
	}
	@media (max-width: 1100px) {
		.flow {
			grid-template-columns: minmax(0, 1fr);
		}
	}
	.checks {
		list-style: none;
		margin: 0 0 12px;
		padding: 0;
		display: grid;
		gap: 8px;
	}
	.checks li {
		display: flex;
		flex-wrap: wrap;
		align-items: baseline;
		gap: 4px 8px;
		min-width: 0;
	}
	.lbl {
		flex: 1 1 200px;
		min-width: 0;
		overflow-wrap: anywhere;
	}
	.acts {
		display: flex;
		flex-wrap: wrap;
		gap: 12px 16px;
		align-items: flex-start;
	}
</style>
