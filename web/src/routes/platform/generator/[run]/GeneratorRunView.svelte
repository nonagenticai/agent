<script lang="ts">
	// P18 run detail. One discovery-generator run reads IN → steps → OUT: the input keys go in, the LLM drafts each
	// step and a human decides every draft, the validation gate (rules 1–7) must be green, and only Package publishes —
	// a template tag @vN for platform runs, tenant refinements for tenant runs (D8). The banner names the one
	// decision the run is waiting for.
	import { onDestroy, onMount } from 'svelte';
	import { api, guard } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import type { GeneratorRun, GeneratorStepKey, StageKey } from '$lib/api/types/journey';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Card from '$lib/ui/Card.svelte';
	import KeyValues from '$lib/ui/KeyValues.svelte';
	import { STEP_HINT, STEP_LABEL, journeyUi, tokens, when } from '$lib/journey/format';
	import {
		DRAFT_STEPS,
		STEP_PRODUCES,
		nextAction,
		nodeState,
		outputTargets,
		stepName,
		stepTally,
		tally,
	} from '$lib/journey/P18Run';
	import P18RunFlow from './P18RunFlow.svelte';
	import P18RunGate from './P18RunGate.svelte';
	import P18RunCost from './P18RunCost.svelte';
	import P18RunDrafts from './P18RunDrafts.svelte';
	import P18RunProduced from './P18RunProduced.svelte';
	import P18DraftHint from './P18DraftHint.svelte';
	import P18EvidencePanel from './P18EvidencePanel.svelte';
	import { ApiError } from '$lib/api/errors';
	import { hintArg, hintState } from '$lib/journey/B8Generator';

	let { id }: { id: number } = $props();

	let step = $state<GeneratorStepKey>('input');
	// The open step follows the run's current step (first load, and after Continue) unless another step is open.
	let lastCurrent: GeneratorStepKey | null = null;
	const res = new Resource(
		async () => {
			const r = await api().generatorRun(id);
			if (lastCurrent === null || step === lastCurrent) step = r.current_step;
			lastCurrent = r.current_step;
			return r;
		},
		() => false,
	);
	onMount(() => res.refresh());

	let drafting = $state(false);
	let timer: ReturnType<typeof setTimeout> | undefined;
	onDestroy(() => clearTimeout(timer));

	let hint = $state('');
	let hintError = $state('');

	function draft() {
		if (hintState(hint).over) return;
		drafting = true;
		hintError = '';
		timer = setTimeout(async () => {
			const ok = await guard(
				async () => {
					try {
						await api().draftGeneratorStep(id, hintArg(hint));
					} catch (e) {
						if (e instanceof ApiError && e.code === 'hint_too_long') hintError = e.message;
						throw e;
					}
				},
				'Drafts added — decide each one.',
				'Not drafted',
			);
			if (ok) hint = '';
			drafting = false;
			await res.refresh();
		}, journeyUi.draftMs);
	}

	const act = async (fn: () => Promise<unknown>, ok: string) => {
		if (await guard(fn, ok)) await res.refresh();
	};
	const setScope = (stage: StageKey, out: boolean, label: string = stage) =>
		act(
			() => api().setGeneratorStageScope(id, stage, out),
			out ? `${label} marked out of scope.` : 'Stage scope saved.',
		);

	function goTo(target: GeneratorStepKey | 'rules' | null) {
		if (!target) return;
		if (target !== 'rules') step = target;
		const el = document.getElementById(target === 'rules' ? 'p18-rules' : 'p18-work');
		el?.scrollIntoView?.({ behavior: 'smooth', block: 'start' });
	}

	const reached = (r: GeneratorRun, k: GeneratorStepKey) =>
		r.steps.findIndex((s) => s.key === k) <= r.steps.findIndex((s) => s.key === r.current_step);
	const STATE_CHIP = {
		done: ['done', 'ok'],
		current: ['in progress', 'accent'],
		blocked: ['blocked', 'err'],
		todo: ['not reached', 'muted'],
	} as const;
</script>

<LoadState resource={res}>
	{#snippet children(r)}
		{@const open = r.status !== 'packaged' && r.status !== 'abandoned'}
		{@const next = nextAction(r)}
		{@const all = tally(r.items)}
		{@const items = r.items.filter((i) => i.step === step)}
		{@const t = stepTally(r, step)}
		{@const st = nodeState(r, step)}
		{@const outs = outputTargets(r)}
		<PageHead
			id="P18"
			title="Generator run #{r.id}"
			sub="{r.scope === 'platform'
				? 'Platform run → template layers'
				: 'Tenant run → refinements only (D8)'} · {r.industry} / {r.department} / {r.job}"
		>
			{#snippet actions()}
				<a class="btn" href="/platform/generator">All runs</a>
			{/snippet}
		</PageHead>

		<section class="p18-next {next.tone}" aria-label="What this run needs" data-testid="p18-next">
			<div class="p18-next-txt">
				<span class="p18-next-k">{open ? 'Waiting on you' : 'Outcome'}</span>
				<strong>{next.title}</strong>
				<span class="p18-next-d">{next.detail}</span>
			</div>
			{#if next.cta}
				<button
					class="btn {next.tone === 'err' ? 'danger' : 'primary'}"
					type="button"
					onclick={() => goTo(next.target)}>{next.cta}</button
				>
			{/if}
		</section>

		<section class="card p18-io" aria-label="Run input and output">
			<div class="p18-io-cell">
				<span class="p18-io-k">In</span>
				<div class="p18-keys mono">
					<span>{r.industry}</span><span aria-hidden="true">/</span><span>{r.department}</span><span
						aria-hidden="true">/</span
					><span>{r.job}</span>
				</div>
				{#if r.description}<p class="p18-desc">“{r.description}”</p>{/if}
				<span class="p18-by">
					<span class="chip {r.scope === 'tenant' ? 'cust' : 'tool'}">{r.scope}</span>
					{#if r.tenant_name}<span>{r.scope === 'tenant' ? 'for' : 'first tenant'} {r.tenant_name}</span>{/if}
					· started {when(r.started_at)} by {r.started_by}
				</span>
			</div>
			<div class="p18-io-mid" aria-hidden="true">
				<span class="p18-io-arrow"></span>
				<span class="p18-io-stat"
					>{all.total} drafts · {all.kept} kept · {all.dropped} dropped{#if all.pending}
						· {all.pending} open{/if}</span
				>
				<span class="p18-io-stat">{tokens(r.cost.tokens)} tokens</span>
			</div>
			<div class="p18-io-cell">
				<span class="p18-io-k">Out</span>
				<span class="p18-io-state chip {r.status === 'packaged' ? 'ok' : r.red ? 'err' : 'muted'}"
					>{r.status === 'packaged'
						? r.scope === 'platform'
							? 'tagged'
							: 'saved'
						: r.status === 'abandoned'
							? 'abandoned'
							: 'not published yet'}</span
				>
				<ul class="p18-outs">
					{#each outs as o (o.label)}
						<li>
							{#if o.href && r.status === 'packaged'}<a class="mono" href={o.href}>{o.label}</a>{:else}<span
									class="mono">{o.label}</span
								>{/if}
						</li>
					{/each}
				</ul>
				{#if r.tagged && r.scope === 'tenant'}<span class="chip ok">{r.tagged}</span>{/if}
				<span class="p18-by">updated {when(r.updated_at)}</span>
			</div>
		</section>

		<Card title="Steps" sub="The LLM drafts each step; you accept, rename, merge or drop every draft.">
			<P18RunFlow run={r} active={step} onselect={(k) => (step = k)} />
		</Card>

		<div class="g-main grid p18-body">
			<div id="p18-work" class="p18-work">
				<Card title={STEP_LABEL[step]} sub={STEP_HINT[step]}>
					{#snippet actions()}
						{#if open && step === r.current_step && DRAFT_STEPS.includes(step)}
							<button
								class="btn"
								type="button"
								disabled={drafting || hintState(hint).over}
								title={hintState(hint).over ? 'The hint is over the limit' : undefined}
								onclick={draft}
							>
								{#if drafting}<span class="spinner" aria-hidden="true"></span> Drafting…{:else}Draft with LLM{/if}
							</button>
						{/if}
					{/snippet}
					<div class="p18-stepline">
						<span class="chip {STATE_CHIP[st][1]}">{STATE_CHIP[st][0]}</span>
						<span>produces <b>{STEP_PRODUCES[step]}</b></span>
						{#if t.total}<span class="muted"
								>· {t.total} drafted · {t.kept} kept · {t.dropped} dropped{#if t.pending}
									· <b class="p18-warn">{t.pending} to decide</b>{/if}</span
							>{/if}
					</div>
					{#if open && step === r.current_step && DRAFT_STEPS.includes(step)}
						<P18DraftHint bind:value={hint} error={hintError} disabled={drafting} />
					{/if}
					{#if drafting && step === r.current_step}
						<p class="p18-drafting" role="status" data-testid="drafting">
							<span class="spinner" aria-hidden="true"></span> The LLM is drafting {stepName(step)} for {r.industry}
							/ {r.job}…
						</p>
					{/if}
					{#if step === 'input'}
						<KeyValues
							pairs={[
								[
									'Scope',
									r.scope === 'platform'
										? 'platform — writes template layers'
										: 'tenant — writes refinements only',
								],
								['Industry', r.industry],
								['Department', r.department],
								['Job', r.job],
								['Tenant', r.tenant_name ?? '—'],
								['Started', `${when(r.started_at)} by ${r.started_by}`],
							]}
						/>
					{:else if !reached(r, step)}
						<p class="muted">Not reached yet — finish {stepName(r.current_step)} first.</p>
					{:else if step === 'validate'}
						<p class="p18-plain">
							No drafts here: the machine checks rules 1–7 over everything kept so far. The result is the
							<a href="#p18-rules">Validation gate</a>{r.red
								? ` — ${r.red} red item(s) to resolve`
								: ' — green'}.
						</p>
						<div class="p18-gap"></div>
						<P18RunProduced run={r} step="journey" {open} onscope={(s, out) => setScope(s, out)} />
					{:else if step === 'package'}
						<div class="p18-pack">
							<p class="p18-plain">
								{r.scope === 'platform'
									? 'Tagging publishes these template layers (D15). Tenants on older pins see “newer template available”.'
									: `Saves the kept drafts as refinements of ${r.tenant_name} / ${r.job}; they land in that job’s draft and reach customers only after the tenant tests and publishes them (D54).`}
							</p>
							<ul class="p18-outs big">
								{#each outs as o (o.label)}
									<li>
										<span class="chip {r.status === 'packaged' ? 'ok' : 'muted'}"
											>{r.status === 'packaged' ? 'landed' : 'will land'}</span
										>
										{#if o.href && r.status === 'packaged'}<a class="mono" href={o.href}>{o.label}</a
											>{:else}<span class="mono">{o.label}</span>{/if}
									</li>
								{/each}
							</ul>
						</div>
					{:else}
						<P18RunProduced run={r} {step} {open} onscope={(s, out) => setScope(s, out)} />
						{#if items.length}
							<P18RunDrafts run={r} {items} {open} {act} />
						{:else}
							<div class="empty">
								No drafts at this step.{open && step === r.current_step ? ' Use “Draft with LLM”.' : ''}
							</div>
						{/if}
					{/if}
					{#if open && step === r.current_step}
						<div class="row p18-foot">
							{#if step === 'package'}
								<button
									class="btn primary"
									type="button"
									disabled={r.red > 0}
									title={r.red ? 'Red validation items block tagging' : undefined}
									onclick={() =>
										act(
											() => api().tagGeneratorRun(r.id),
											r.scope === 'platform' ? 'Tagged.' : 'Refinements saved.',
										)}>{r.scope === 'platform' ? `Tag ${r.output.join(' + ')}` : 'Save refinements'}</button
								>
							{:else}
								<button
									class="btn primary"
									type="button"
									disabled={t.pending > 0 || (r.red > 0 && step === 'validate')}
									title={t.pending
										? 'Decide every draft of this step first'
										: r.red && step === 'validate'
											? 'Red validation items block Validate'
											: undefined}
									onclick={() => act(() => api().advanceGeneratorRun(r.id), 'Step done.')}
									>Continue to next step</button
								>
							{/if}
							{#if t.pending && step !== 'package'}<span class="muted"
									>{t.pending} draft(s) still to decide.</span
								>{/if}
							{#if r.red}<span class="p18-red"
									>{r.red} red item(s) block {step === 'package' ? 'tagging' : 'Validate'}.</span
								>{/if}
						</div>
					{/if}
				</Card>
			</div>

			<div class="stack">
				{#if open && r.scope === 'platform' && reached(r, 'validate')}
					<P18EvidencePanel run={r} onuploaded={() => res.refresh()} />
				{/if}
				<P18RunGate run={r} {open} onscope={(s, label) => setScope(s, true, label)} />
				<P18RunCost run={r} />
			</div>
		</div>
	{/snippet}
</LoadState>

<style>
	.p18-next {
		display: flex;
		gap: 16px;
		align-items: center;
		justify-content: space-between;
		flex-wrap: wrap;
		padding: 12px 16px;
		margin-bottom: 16px;
		border-radius: var(--r);
		border: 1px solid var(--line);
		border-left: 4px solid var(--muted);
		background: var(--panel);
	}
	.p18-next.ok {
		border-left-color: var(--ok);
	}
	.p18-next.err {
		border-left-color: var(--err);
		background: color-mix(in srgb, var(--err) 5%, var(--panel));
	}
	.p18-next.warn {
		border-left-color: var(--warn);
		background: color-mix(in srgb, var(--warn) 6%, var(--panel));
	}
	.p18-next.accent {
		border-left-color: var(--accent);
	}
	.p18-next-txt {
		display: grid;
		gap: 2px;
		min-width: 0;
		flex: 1 1 260px;
	}
	.p18-next-k {
		font-size: 11px;
		text-transform: uppercase;
		letter-spacing: 0.06em;
		color: var(--muted);
		font-weight: 600;
	}
	.p18-next strong {
		font-size: 16px;
	}
	.p18-next-d {
		font-size: 13px;
		color: var(--muted);
	}

	.p18-io {
		display: grid;
		grid-template-columns: minmax(0, 1fr) 170px minmax(0, 1fr);
		gap: 0;
		padding: 0;
		margin-bottom: 16px;
		overflow: hidden;
	}
	.p18-io-cell {
		padding: 14px 16px;
		display: grid;
		gap: 6px;
		align-content: start;
		min-width: 0;
	}
	.p18-io-k {
		font: 600 11px/1 var(--mono);
		text-transform: uppercase;
		letter-spacing: 0.08em;
		color: var(--faint);
	}
	.p18-keys {
		display: flex;
		flex-wrap: wrap;
		gap: 4px;
		font-size: 14px;
		font-weight: 600;
	}
	.p18-desc {
		margin: 0;
		font-size: 13px;
	}
	.p18-by {
		font-size: 12px;
		color: var(--muted);
		display: flex;
		gap: 6px;
		flex-wrap: wrap;
		align-items: center;
	}
	.p18-io-mid {
		display: flex;
		flex-direction: column;
		align-items: center;
		justify-content: center;
		gap: 4px;
		padding: 10px;
		background: var(--sunk);
		text-align: center;
	}
	.p18-io-arrow {
		width: 100%;
		height: 2px;
		background: var(--line);
		position: relative;
		margin-bottom: 6px;
	}
	.p18-io-arrow::after {
		content: '';
		position: absolute;
		right: -2px;
		top: -4px;
		border: 5px solid transparent;
		border-left: 8px solid var(--line);
		border-right: 0;
	}
	.p18-io-stat {
		font-size: 11.5px;
		color: var(--muted);
	}
	.p18-io-cell > :global(.chip) {
		justify-self: start;
	}
	.p18-gap {
		height: 14px;
	}
	.p18-outs {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 3px;
		font-size: 13px;
		font-weight: 600;
	}
	.p18-outs li {
		overflow-wrap: anywhere;
	}
	.p18-outs.big {
		margin-top: 8px;
		gap: 6px;
	}
	.p18-outs.big li {
		display: flex;
		gap: 8px;
		align-items: center;
	}
	.p18-body {
		margin-top: 16px;
	}
	.p18-work {
		min-width: 0;
		scroll-margin-top: 70px;
	}
	.p18-stepline {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		align-items: center;
		font-size: 12.5px;
		margin-bottom: 12px;
	}
	.p18-warn {
		color: var(--warn);
	}
	.p18-plain {
		margin: 0;
		font-size: 13.5px;
	}
	.p18-foot {
		margin-top: 14px;
		padding-top: 12px;
		border-top: 1px solid var(--line-2);
	}
	.p18-red {
		color: var(--err);
		font-weight: 600;
	}
	.p18-drafting {
		display: flex;
		gap: 8px;
		align-items: center;
		color: var(--accent);
	}
	.spinner {
		display: inline-block;
		width: 14px;
		height: 14px;
		border: 2px solid color-mix(in srgb, var(--accent) 30%, transparent);
		border-top-color: var(--accent);
		border-radius: 50%;
		animation: spin 0.8s linear infinite;
		vertical-align: -2px;
	}
	@keyframes spin {
		to {
			transform: rotate(360deg);
		}
	}
	@media (max-width: 760px) {
		.p18-io {
			grid-template-columns: minmax(0, 1fr);
		}
		.p18-io-mid {
			flex-direction: row;
			flex-wrap: wrap;
			gap: 10px;
		}
		.p18-io-arrow {
			display: none;
		}
	}
</style>
