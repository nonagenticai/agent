<script lang="ts">
	// J12 detail: one version as a flow, top → bottom. Starts at the context space (what the qualifier asks for),
	// runs phase by phase (steps in a phase run in parallel), and ends in the three outcomes. Each step says what it
	// does (kind), which operation it calls, what it reads from (slot bindings), what it needs (identity level,
	// customer confirm-back) and where that is defined (rule, knowledge kinds, route). Compared with the live
	// version, new and changed steps are marked and removed ones listed.
	import { app } from '$lib/app.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import type { ContextSpace, ProcedureView } from '$lib/api/types/ontology';
	import { SIDE_EFFECT_LABEL, SIDE_EFFECT_VARIANT } from '$lib/ontology/shared';
	import KindTag from '../KindTag.svelte';
	import { STEP_KIND, diffSteps, groupPhases, stepKind, stepOp, stepUses } from '../view';

	let {
		sel,
		eff,
		space,
		onpick,
	}: {
		sel: ProcedureView;
		eff: ProcedureView | null;
		space: ContextSpace | null;
		onpick: (version: number) => void;
	} = $props();

	const j = $derived(`/admin/jobs/${app.job}`);
	const phases = $derived(groupPhases(sel.steps));
	const compare = $derived(eff && eff.version !== sel.version ? eff : null);
	const diff = $derived(diffSteps(sel.steps, compare?.steps ?? null));
	const changes = $derived([...diff.marks.values()].filter((d) => d !== 'same').length + diff.removed.length);
	const DIFF_TEXT = { added: 'new', changed: 'changed' } as const;
</script>

<div class="fl" data-testid="procedure-flow">
	{#if compare}
		<p class="fl-cmp">
			{#if changes}
				Compared with the live <button
					type="button"
					class="linkish mono"
					onclick={() => onpick(compare.version)}>{compare.version}</button
				>:
				<span class="fl-mark added">new</span> and <span class="fl-mark changed">changed</span> steps are
				marked{diff.removed.length ? `, ${diff.removed.length} removed` : ''}.
			{:else}
				Same steps as the live <button
					type="button"
					class="linkish mono"
					onclick={() => onpick(compare.version)}>{compare.version}</button
				>.
			{/if}
		</p>
	{/if}

	<div class="fl-node fl-start">
		<span class="label">Context space</span>
		<b>{space?.label ?? sel.context_space}</b>
		<span class="mono muted fl-key">{sel.context_space}</span>
		<span class="fl-slots">
			{#if space}
				{#if space.required.length}
					<span class="muted">The qualifier asks for</span>
					{#each space.required as slot (slot)}<Chip text={slot} variant="muted" />{/each}
				{:else}<span class="muted">No required slots: the procedure runs on the message itself.</span>{/if}
			{/if}
		</span>
	</div>

	{#each phases as ph, i (ph.phase)}
		<div class="fl-link" aria-hidden="true"></div>
		<section class="fl-phase" aria-label="Phase {i + 1}">
			<span class="label fl-ph">{ph.label}</span>
			<div class="fl-steps">
				{#each ph.steps as s (s.step_number)}
					{@const label = stepOp(s)}
					{@const k = stepKind(s)}
					{@const d = diff.marks.get(s.step_number)}
					{@const uses = stepUses(s)}
					{@const bad = sel.outside_grant.includes(label)}
					<article
						class="fl-step"
						class:stack={ph.steps.length > 1}
						class:bad
						class:added={d === 'added'}
						class:changed={d === 'changed'}
					>
						<div class="fl-what">
							<div class="fl-step-head">
								<span class="fl-n">{s.step_number}</span>
								<KindTag kind={k} wide gate={s.gate} />
								{#if d === 'added' || d === 'changed'}<span class="fl-mark {d}">{DIFF_TEXT[d]}</span>{/if}
							</div>
							<b class="fl-name">{s.name}</b>
							<span class="mono fl-op">{label}</span>
							<a class="fl-uses" href="{j}/{uses.path}" title={STEP_KIND[k].hint}>{uses.text} →</a>
						</div>
						<div class="fl-how">
							<span class="chips">
								<Chip
									text={SIDE_EFFECT_LABEL[s.side_effect]}
									variant={SIDE_EFFECT_VARIANT[s.side_effect]}
									title="Side effect of the operation"
								/>
								<Chip
									text="identity {s.identity_level}"
									title="Identity level the customer must have reached"
								/>
								{#if s.confirm_back}<Chip
										text="customer confirms first"
										variant="warn"
										title="The agent reads the change back and waits for a yes"
									/>{/if}
								{#if bad}<Chip text="outside grant" variant="err" />{/if}
							</span>
							{#if Object.keys(s.args).length}
								<ul class="fl-args" aria-label="Inputs">
									{#each Object.entries(s.args) as [a, v] (a)}<li class="mono">
											{a} <span aria-hidden="true">←</span><span class="sr-only">from</span>
											{v}
										</li>{/each}
								</ul>
							{/if}
						</div>
						{#if s.gate}
							<div class="fl-gate">
								<span aria-hidden="true">◆</span> If “no”: the case ends as <b>not eligible</b> and the agent explains
								why.
							</div>
						{/if}
					</article>
				{/each}
			</div>
		</section>
	{:else}
		<div class="empty">This version has no steps.</div>
	{/each}

	<div class="fl-link" aria-hidden="true"></div>
	<div class="fl-ends">
		<div class="fl-node fl-end ok">
			<b>Resolved</b>
			<span class="muted"
				>The reply is built from what the steps found; the R1 post-gate checks it first.</span
			>
		</div>
		<div class="fl-node fl-end hand">
			<b>Handed over</b>
			<span class="muted"
				>A step fails, times out or is refused: staff pick it up in the <a href="{j}/handover"
					>handover queue</a
				>.</span
			>
		</div>
	</div>

	{#if diff.removed.length && compare}
		<p class="fl-removed">
			Removed compared with {compare.version}:
			{#each diff.removed as r, i (r.step_number)}{i ? ', ' : ''}<span class="mono">{stepOp(r)}</span>
				({r.name}){/each}.
		</p>
	{/if}
</div>

<style>
	.fl {
		display: grid;
		justify-items: stretch;
	}
	.fl-cmp {
		margin: 0 0 10px;
		font-size: 12.5px;
		color: var(--muted);
	}
	.fl-node {
		display: grid;
		gap: 3px;
		padding: 10px 12px;
		border: 1px solid var(--line);
		border-radius: var(--r);
		background: var(--panel);
	}
	.fl-start {
		border-left: 3px solid var(--accent);
	}
	.fl-key {
		font-size: 11.5px;
	}
	.fl-slots {
		display: flex;
		flex-wrap: wrap;
		gap: 4px;
		align-items: center;
		font-size: 12.5px;
		margin-top: 2px;
	}
	.fl-link {
		width: 2px;
		height: 16px;
		margin-left: 22px;
		background: var(--line);
	}
	.fl-phase {
		display: grid;
		gap: 6px;
	}
	.fl-ph {
		margin-left: 2px;
	}
	.fl-steps {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(min(100%, 240px), 1fr));
		gap: 8px;
	}
	.fl-step {
		display: grid;
		grid-template-columns: minmax(0, 1fr) minmax(0, 1.15fr);
		gap: 6px 14px;
		align-content: start;
		padding: 10px 12px;
		border: 1px solid var(--line);
		border-radius: var(--r);
		background: var(--panel);
		min-width: 0;
	}
	.fl-step.added {
		border-color: color-mix(in srgb, var(--ok) 60%, transparent);
		box-shadow: inset 3px 0 0 var(--ok);
	}
	.fl-step.changed {
		border-color: color-mix(in srgb, var(--warn) 60%, transparent);
		box-shadow: inset 3px 0 0 var(--warn);
	}
	.fl-step.bad {
		background: color-mix(in srgb, var(--err) 8%, var(--panel));
		border-color: var(--err);
	}
	.fl-step.stack {
		grid-template-columns: minmax(0, 1fr);
	}
	@media (max-width: 700px) {
		.fl-step {
			grid-template-columns: minmax(0, 1fr);
		}
	}
	.fl-what,
	.fl-how {
		display: grid;
		gap: 4px;
		align-content: start;
		min-width: 0;
	}
	.fl-gate {
		grid-column: 1 / -1;
	}
	.fl-step-head {
		display: flex;
		gap: 6px;
		align-items: center;
	}
	.fl-n {
		font: 600 11px/1 var(--mono);
		color: var(--faint);
		min-width: 14px;
	}
	.fl-name {
		font-size: 13.5px;
	}
	.fl-op {
		font-size: 12px;
		color: var(--muted);
		word-break: break-all;
	}
	.fl-args {
		list-style: none;
		margin: 2px 0 0;
		padding: 6px 8px;
		border-radius: 4px;
		background: var(--sunk);
		font-size: 11.5px;
		display: grid;
		gap: 2px;
	}
	.fl-args li {
		word-break: break-all;
	}
	.fl-uses {
		font-size: 12px;
		justify-self: start;
	}
	.fl-gate {
		font-size: 12px;
		color: var(--warn);
		padding-top: 4px;
		border-top: 1px dashed var(--line);
	}
	.fl-mark {
		font: 600 10px/1.5 var(--mono);
		text-transform: uppercase;
		padding: 0 5px;
		border-radius: 3px;
	}
	.fl-mark.added {
		color: var(--ok);
		background: color-mix(in srgb, var(--ok) 14%, transparent);
	}
	.fl-mark.changed {
		color: var(--warn);
		background: color-mix(in srgb, var(--warn) 14%, transparent);
	}
	.fl-ends {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(min(100%, 220px), 1fr));
		gap: 8px;
	}
	.fl-end {
		font-size: 12.5px;
	}
	.fl-end.ok {
		border-left: 3px solid var(--ok);
	}
	.fl-end.hand {
		border-left: 3px solid var(--faint);
	}
	.fl-removed {
		margin: 10px 0 0;
		font-size: 12.5px;
		color: var(--err);
	}
	.linkish {
		background: none;
		border: 0;
		padding: 0;
		color: var(--accent);
		text-decoration: underline;
		cursor: pointer;
		font-size: inherit;
	}
	.sr-only {
		position: absolute;
		width: 1px;
		height: 1px;
		overflow: hidden;
		clip: rect(0 0 0 0);
	}
</style>
