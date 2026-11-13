<script lang="ts">
	// D97 tenant-driven generator run: the job_designer drives the run from the job's own Discovery page with the
	// tenant's own `generator` provider. Output is refinements only (D8): Package saves them to THIS job's draft, never
	// live (D54). Tokens are the tenant's usage (D56); with a BYO key the tenant's own key funds them. D89 U-08: with
	// no generator model bound the draft call answers 409 `generator_provider_missing`.
	import { onDestroy, onMount } from 'svelte';
	import { api, app, guard } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { canWrite, writeHint } from '$lib/auth/roles';
	import type { GeneratorRun, GeneratorStepKey, StageKey } from '$lib/api/types/journey';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import KeyValues from '$lib/ui/KeyValues.svelte';
	import { STEP_HINT, STEP_LABEL, journeyUi, when } from '$lib/journey/format';
	import {
		DRAFT_STEPS,
		STEP_PRODUCES,
		nodeState,
		outputTargets,
		stepName,
		stepTally,
	} from '$lib/journey/P18Run';
	import { PROVIDERS_LINK, fundingNote, hintArg, hintState, providerMissing } from '$lib/journey/B8Generator';
	import { ApiError } from '$lib/api/errors';
	import P18RunFlow from '../../../../../../platform/generator/[run]/P18RunFlow.svelte';
	import P18RunGate from '../../../../../../platform/generator/[run]/P18RunGate.svelte';
	import P18RunCost from '../../../../../../platform/generator/[run]/P18RunCost.svelte';
	import P18RunDrafts from '../../../../../../platform/generator/[run]/P18RunDrafts.svelte';
	import P18RunProduced from '../../../../../../platform/generator/[run]/P18RunProduced.svelte';
	import P18DraftHint from '../../../../../../platform/generator/[run]/P18DraftHint.svelte';

	let { id }: { id: number } = $props();
	const can = $derived(canWrite(session, 'job_designer'));

	const base = $derived(`/admin/jobs/${app.job}/discovery`);
	let step = $state<GeneratorStepKey>('input');
	let lastCurrent: GeneratorStepKey | null = null;
	const res = new Resource(
		async () => {
			const r = await api().jobGeneratorRun(id);
			if (lastCurrent === null || step === lastCurrent) step = r.current_step;
			lastCurrent = r.current_step;
			return r;
		},
		() => false,
	);
	onMount(() => res.refresh());

	let hint = $state('');
	let hintError = $state('');
	let noProvider = $state(false);
	let drafting = $state(false);
	let timer: ReturnType<typeof setTimeout> | undefined;
	onDestroy(() => clearTimeout(timer));

	function draft() {
		if (hintState(hint).over) return;
		drafting = true;
		hintError = '';
		noProvider = false;
		timer = setTimeout(async () => {
			const ok = await guard(
				async () => {
					try {
						await api().draftJobGeneratorStep(id, hintArg(hint));
					} catch (e) {
						if (providerMissing(e)) noProvider = true;
						else if (e instanceof ApiError && e.code === 'hint_too_long') hintError = e.message;
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
			() => api().setJobGeneratorStageScope(id, stage, out),
			out ? `${label} marked out of scope.` : 'Stage scope saved.',
		);
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
		{@const items = r.items.filter((i) => i.step === step)}
		{@const t = stepTally(r, step)}
		{@const st = nodeState(r, step)}
		{@const outs = outputTargets(r)}
		{@const byo = fundingNote(r)}
		<PageHead
			id="J18"
			title="Generator run #{r.id}"
			sub="Tenant run → refinements only (D8) · {r.industry} / {r.department} / {r.job}"
		>
			{#snippet actions()}
				<a class="btn" href={base}>Back to Discovery</a>
			{/snippet}
		</PageHead>

		{#if noProvider}
			<div data-testid="provider-missing">
				<Callout kind="err" title="No generator model set up">
					The generator needs a model of your own to draft with.
					<a href={PROVIDERS_LINK}>Set up a generator model in Providers</a>, then draft again.
				</Callout>
			</div>
		{/if}
		{#if byo}<p class="muted" data-testid="byo-note">{byo}</p>{/if}

		<Card title="Steps" sub="The LLM drafts each step; you accept, rename, merge or drop every draft.">
			<P18RunFlow run={r} active={step} onselect={(k) => (step = k)} />
		</Card>

		<div class="g-main grid">
			<div id="p18-work" class="stack">
				<Card title={STEP_LABEL[step]} sub={STEP_HINT[step]}>
					{#snippet actions()}
						{#if open && step === r.current_step && DRAFT_STEPS.includes(step)}
							<button
								class="btn"
								type="button"
								title={can ? undefined : writeHint('job_designer')}
								disabled={!can || drafting || hintState(hint).over}
								onclick={draft}
							>
								{#if drafting}<span class="spinner" aria-hidden="true"></span> Drafting…{:else}Draft with LLM{/if}
							</button>
						{/if}
					{/snippet}
					<div class="row">
						<span class="chip {STATE_CHIP[st][1]}">{STATE_CHIP[st][0]}</span>
						<span>produces <b>{STEP_PRODUCES[step]}</b></span>
						{#if t.total}<span class="muted"
								>· {t.total} drafted · {t.kept} kept · {t.dropped} dropped{#if t.pending}
									· <b>{t.pending} to decide</b>{/if}</span
							>{/if}
					</div>
					{#if open && step === r.current_step && DRAFT_STEPS.includes(step)}
						<P18DraftHint bind:value={hint} error={hintError} disabled={drafting} />
					{/if}
					{#if drafting && step === r.current_step}
						<p role="status" data-testid="drafting">
							<span class="spinner" aria-hidden="true"></span> The LLM is drafting {stepName(step)} for {r.job}…
						</p>
					{/if}
					{#if step === 'input'}
						<KeyValues
							pairs={[
								['Scope', 'tenant — writes refinements only'],
								['Industry', r.industry],
								['Department', r.department],
								['Job', r.job],
								['Started', `${when(r.started_at)} by ${r.started_by}`],
							]}
						/>
					{:else if !reached(r, step)}
						<p class="muted">Not reached yet — finish {stepName(r.current_step)} first.</p>
					{:else if step === 'validate'}
						<p>
							No drafts here: the machine checks rules 1–7 over everything kept so far{r.red
								? ` — ${r.red} red item(s) to resolve`
								: ' — green'}.
						</p>
						<P18RunProduced run={r} step="journey" {open} onscope={(s, out) => setScope(s, out)} />
					{:else if step === 'package'}
						<p>
							Saves the kept drafts as refinements of this job. They land in the job's draft and reach
							customers only after you test and publish them (D54).
						</p>
						<ul class="plain">
							{#each outs as o (o.label)}
								<li>
									<span class="chip {r.status === 'packaged' ? 'ok' : 'muted'}"
										>{r.status === 'packaged' ? 'landed' : 'will land'}</span
									>
									<span class="mono">{o.label}</span>
								</li>
							{/each}
						</ul>
					{:else}
						<P18RunProduced run={r} {step} {open} onscope={(s, out) => setScope(s, out)} />
						{#if items.length}
							<P18RunDrafts
								run={r}
								{items}
								{open}
								{act}
								decide={(rid, itemId, d) => api().decideJobGeneratorItem(rid, itemId, d)}
							/>
						{:else}
							<div class="empty">
								No drafts at this step.{open && step === r.current_step ? ' Use “Draft with LLM”.' : ''}
							</div>
						{/if}
					{/if}
					{#if open && step === r.current_step}
						<div class="row">
							{#if step === 'package'}
								<button
									class="btn primary"
									type="button"
									title={can ? undefined : writeHint('job_designer')}
									disabled={!can || r.red > 0}
									onclick={() =>
										act(() => api().packageJobGeneratorRun(r.id), 'Refinements saved to the draft.')}
									>Save refinements to the draft</button
								>
							{:else}
								<button
									class="btn primary"
									type="button"
									title={can ? undefined : writeHint('job_designer')}
									disabled={!can || t.pending > 0 || (r.red > 0 && step === 'validate')}
									onclick={() => act(() => api().advanceJobGeneratorRun(r.id), 'Step done.')}
									>Continue to next step</button
								>
							{/if}
							{#if t.pending && step !== 'package'}<span class="muted"
									>{t.pending} draft(s) still to decide.</span
								>{/if}
						</div>
					{/if}
				</Card>
			</div>
			<div class="stack">
				<P18RunGate run={r} {open} onscope={(s, label) => setScope(s, true, label)} />
				<P18RunCost run={r} />
			</div>
		</div>
	{/snippet}
</LoadState>
