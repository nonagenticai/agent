<script lang="ts">
	// J22 failure patterns: op × system over 30 days, worst failure rate first. Each row says how often the operation
	// fails, what the failures are (user outcomes the agent answers vs system errors it escalates vs unknown
	// outcomes), the codes behind them, and where to fix it (the system, the route, or the procedure that offers it).
	import { app } from '$lib/app.svelte';
	import { setQuery } from '$lib/query';
	import { jobPath } from '$lib/scope';
	import { fmt } from '$lib/format';
	import type { Pattern } from '$lib/runtime/J22-calls';
	import { ENDING, SIDE_EFFECT, readingOf, statKey, systemHref } from '$lib/runtime/J22-calls';
	import Chip from '$lib/ui/Chip.svelte';

	let { patterns }: { patterns: Pattern[] } = $props();

	const KIND_LABEL: Record<Pattern['kind'], { text: string; variant: 'warn' | 'err' | 'accent' }> = {
		user: { text: 'answered to the customer', variant: 'accent' },
		system: { text: 'system errors, escalated', variant: 'warn' },
		mixed: { text: 'answered + escalated', variant: 'warn' },
		unknown: { text: 'includes unknown outcomes', variant: 'err' },
	};
	const FAILS = ['replied', 'escalated', 'unknown'] as const;
</script>

{#if !patterns.length}
	<p class="j22p-none">No operation fails on 2 % or more of its calls. Nothing to fix here.</p>
{:else}
	<ol class="j22p" data-testid="failure-patterns">
		{#each patterns as p, i (statKey(p.stat))}
			{@const failed = p.mix.replied + p.mix.escalated + p.mix.unknown}
			{@const href = systemHref(p.stat.instance)}
			<li class="j22p-row" class:hot={p.rate >= 0.1} data-testid="pattern-{p.stat.op}">
				<div class="j22p-rank" aria-hidden="true">{i + 1}</div>
				<div class="j22p-main">
					<div class="j22p-title">
						<span class="mono strong">{p.stat.op}</span>
						{#if p.stat.side_effect}<Chip
								text={p.stat.side_effect}
								variant={p.stat.side_effect === 'R' ? 'muted' : 'tool'}
								title={SIDE_EFFECT[p.stat.side_effect].hint}
							/>{/if}
						<span class="muted">on</span>
						{#if href}<a class="mono" {href}>{p.stat.instance}</a>{:else}<span class="mono"
								>{p.stat.instance ?? p.stat.route}</span
							>{/if}
						{#if p.stat.template}<span class="mono faint">{p.stat.template}</span>{/if}
					</div>
					<div
						class="j22p-bar"
						role="img"
						aria-label="{failed} failed calls: {p.mix.replied} answered, {p.mix.escalated} escalated, {p.mix
							.unknown} unknown"
					>
						{#each FAILS as k (k)}
							{#if p.mix[k]}
								<span
									class="seg {ENDING[k].cls}"
									style="flex-grow:{p.mix[k]}"
									title="{ENDING[k].label}: {p.mix[k]}">{p.mix[k]}</span
								>
							{/if}
						{/each}
					</div>
					<div class="j22p-codes">
						{#each p.codes as c (c.code)}
							<span
								class="code {c.code === 'unknown_outcome'
									? 'e-unknown'
									: c.handling === 'reply'
										? 'e-replied'
										: 'e-escalated'}"
								><b>{c.count}×</b> <span class="mono">{c.code}</span>
								<span class="arrow"
									>→ {c.code === 'unknown_outcome'
										? 'a person checks'
										: c.handling === 'reply'
											? 'answered'
											: 'escalated'}</span
								></span
							>
						{:else}
							<span class="muted">No code breakdown recorded.</span>
						{/each}
					</div>
					<p class="j22p-read">{readingOf(p)}</p>
				</div>
				<div class="j22p-rate">
					<div class="pct" class:bad={p.rate >= 0.1}>{fmt.pct(p.rate)}</div>
					<div class="of">failing · {fmt.n(p.stat.errors)} of {fmt.n(p.stat.calls)}</div>
					<Chip text={KIND_LABEL[p.kind].text} variant={KIND_LABEL[p.kind].variant} />
					<div class="j22p-links">
						<button class="btn sm" type="button" onclick={() => setQuery({ op: p.stat.op, outcome: 'error' })}
							>See failed calls</button
						>
						<a class="btn sm" href={jobPath(app.job, '/operations')}>Route</a>
					</div>
				</div>
			</li>
		{/each}
	</ol>
{/if}

<style>
	.j22p {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.j22p-none {
		margin: 0;
		color: var(--muted);
	}
	.j22p-row {
		display: grid;
		grid-template-columns: 28px minmax(0, 1fr) 190px;
		gap: 12px;
		padding: 12px 0 14px;
		border-top: 1px solid var(--line-2);
	}
	.j22p-row:first-child {
		border-top: 0;
	}
	.j22p-rank {
		font-family: var(--cond);
		font-size: 18px;
		color: var(--faint);
		text-align: right;
	}
	.hot .j22p-rank {
		color: var(--err);
	}
	.j22p-title {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 6px;
	}
	.strong {
		font-weight: 600;
		font-size: 14px;
	}
	.faint {
		color: var(--faint);
		font-size: 12px;
	}
	.j22p-bar {
		display: flex;
		gap: 2px;
		height: 18px;
		margin: 8px 0 6px;
		max-width: 520px;
	}
	.seg {
		min-width: 22px;
		border-radius: 3px;
		font-size: 11px;
		line-height: 18px;
		text-align: center;
		font-weight: 600;
		color: var(--panel);
	}
	.seg.e-replied {
		background: var(--accent);
	}
	.seg.e-escalated {
		background: var(--warn);
	}
	.seg.e-unknown {
		background: var(--err);
	}
	.j22p-codes {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
	}
	.code {
		font-size: 12px;
		border: 1px solid var(--line);
		border-left-width: 3px;
		border-radius: 4px;
		padding: 1px 6px;
		background: var(--panel);
	}
	.code.e-replied {
		border-left-color: var(--accent);
	}
	.code.e-escalated {
		border-left-color: var(--warn);
	}
	.code.e-unknown {
		border-left-color: var(--err);
	}
	.arrow {
		color: var(--muted);
	}
	.j22p-read {
		margin: 6px 0 0;
		font-size: 13px;
		color: var(--muted);
	}
	.j22p-rate {
		display: flex;
		flex-direction: column;
		align-items: flex-end;
		gap: 4px;
		text-align: right;
	}
	.pct {
		font-family: var(--cond);
		font-size: 24px;
		font-weight: 600;
		line-height: 1;
	}
	.pct.bad {
		color: var(--err);
	}
	.of {
		font-size: 12px;
		color: var(--muted);
	}
	.j22p-links {
		display: flex;
		gap: 6px;
		margin-top: 4px;
	}
	@media (max-width: 760px) {
		.j22p-row {
			grid-template-columns: 22px minmax(0, 1fr);
		}
		.j22p-rate {
			grid-column: 2;
			align-items: flex-start;
			text-align: left;
		}
	}
</style>
