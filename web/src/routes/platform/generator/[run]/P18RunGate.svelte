<script lang="ts">
	// P18 validation gate (spec 11 §7, rules 1–7): what must be true before anything is tagged. Red blocks Validate and
	// tagging; each red finding carries the fix that clears it.
	import type { GeneratorRun, StageKey } from '$lib/api/types/journey';
	import Card from '$lib/ui/Card.svelte';
	import { RULES, ruleState } from '$lib/journey/P18Run';

	let {
		run: r,
		open,
		onscope,
	}: { run: GeneratorRun; open: boolean; onscope: (stage: StageKey, label: string) => void } = $props();

	const red = $derived(r.findings.filter((f) => f.severity === 'red'));
	const amber = $derived(r.findings.filter((f) => f.severity !== 'red'));
	const MARK = { red: '✕', pass: '✓', later: '…', optional: '○' } as const;
	const TEXT = {
		red: 'red',
		pass: 'pass',
		later: 'at Validate',
		optional: 'optional',
	} as const;
	const stageLabel = (k: StageKey) => r.stages.find((s) => s.key === k)?.label ?? k;
</script>

<div id="p18-rules">
	<Card
		title="Validation gate"
		sub={red.length
			? 'Red items block Validate and tagging.'
			: r.status === 'packaged'
				? 'Passed — the run was packaged.'
				: 'Green so far. Rules 1 and 4 are checked live; the rest at Validate.'}
	>
		{#if red.length}
			<ul class="p18-findings" data-testid="run-findings">
				{#each r.findings as f, fi (`${fi}:${f.message}`)}
					<li class={f.severity}>
						<span class="chip {f.severity === 'red' ? 'err' : 'warn'}"
							>{f.severity === 'red' ? `rule ${f.rule}` : f.severity}</span
						>
						<span class="p18-msg">{f.message}</span>
						{#if f.rule === 4 && f.stage && open}
							{@const st = f.stage}
							<button class="btn sm" type="button" onclick={() => onscope(st, stageLabel(st))}
								>Mark out of scope</button
							>
						{/if}
					</li>
				{/each}
			</ul>
		{:else if amber.length}
			<ul class="p18-findings">
				{#each amber as f, fi (`${fi}:${f.message}`)}<li class={f.severity}>
						<span class="chip warn">{f.severity}</span>
						<span class="p18-msg">{f.message}</span>
					</li>{/each}
			</ul>
		{/if}
		<ol class="p18-rules" aria-label="Validation rules">
			{#each RULES as rule (rule.n)}
				{@const s = ruleState(r, rule.n)}
				<li class="p18-rule {s}">
					<span class="p18-mark" aria-hidden="true">{MARK[s]}</span>
					<span class="p18-rn">{rule.n}</span>
					<span class="p18-rt">{rule.text}</span>
					<span class="p18-rs">{TEXT[s]}</span>
				</li>
			{/each}
		</ol>
	</Card>
</div>

<style>
	.p18-findings {
		list-style: none;
		margin: 0 0 12px;
		padding: 0;
		display: grid;
		gap: 6px;
	}
	.p18-findings li {
		display: flex;
		gap: 6px;
		flex-wrap: wrap;
		align-items: center;
		padding: 6px 8px;
		border-radius: var(--r);
		background: color-mix(in srgb, var(--err) 7%, transparent);
	}
	.p18-findings li.amber,
	.p18-findings li.info {
		background: color-mix(in srgb, var(--warn) 8%, transparent);
	}
	.p18-msg {
		flex: 1 1 160px;
		font-size: 13px;
	}
	.p18-rules {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 4px;
	}
	.p18-rule {
		display: grid;
		grid-template-columns: 16px 14px minmax(0, 1fr) auto;
		gap: 6px;
		align-items: baseline;
		font-size: 12.5px;
	}
	.p18-mark {
		font-weight: 700;
		text-align: center;
	}
	.p18-rn {
		font: 500 11px/1 var(--mono);
		color: var(--faint);
	}
	.p18-rt {
		color: var(--fg);
	}
	.p18-rs {
		font: 500 11px/1 var(--mono);
		color: var(--muted);
	}
	.pass .p18-mark,
	.pass .p18-rs {
		color: var(--ok);
	}
	.red .p18-mark,
	.red .p18-rs {
		color: var(--err);
	}
	.red .p18-rt {
		font-weight: 600;
	}
	.later .p18-rt,
	.optional .p18-rt {
		color: var(--muted);
	}
</style>
