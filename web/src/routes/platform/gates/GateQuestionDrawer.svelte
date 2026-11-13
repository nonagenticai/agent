<script lang="ts">
	// P14 edit drawer (super_admin): question text, threshold, generated-at-cut switch (choice only), and authored
	// options. Generated options are read-only; safety gates keep fail closed (lock) and have no threshold. The
	// template rule (gte / lt / in + min confidence) is edited here for every job that asks the question. Library
	// questions are never deleted per job; only a question a tenant added to its own job can be (J7).
	import { untrack } from 'svelte';
	import { api, guard } from '$lib/app.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Lock from '$lib/ontology/Lock.svelte';
	import type { GateQuestion, GateRule, GateUsage } from '$lib/api/types/ontology';
	import { when } from '$lib/ontology/shared';
	import { session } from '$lib/auth/session.svelte';
	import { ruleText } from './gates';
	import GateRuleEditor from '$lib/ontology/GateRuleEditor.svelte';
	import {
		ACTION_TEXT,
		RUN_POINT,
		RUN_POINT_LABEL,
		deletePermission,
		questionRuleText,
	} from '$lib/ontology/gateRules';

	let {
		open = $bindable(),
		gate,
		usage = null,
		onchanged,
	}: {
		open: boolean;
		gate: GateQuestion | null;
		usage?: GateUsage | null;
		onchanged: () => Promise<void>;
	} = $props();
	const activeTenant = $derived(session.me?.tenant?.id ?? null);

	let question = $state('');
	let threshold = $state('');
	let optLabel = $state('');
	let optDesc = $state('');
	let busy = $state(false);
	const dirty = $derived(
		open &&
			!!gate &&
			(question !== gate.question ||
				(gate.threshold_value !== null && threshold !== String(gate.threshold_value)) ||
				optLabel !== ''),
	);
	$effect(() => {
		void gate;
		untrack(() => {
			question = gate?.question ?? '';
			threshold = gate?.threshold_value == null ? '' : String(gate.threshold_value);
			optLabel = '';
			optDesc = '';
		});
	});
	const tunable = $derived(!!gate && !gate.safety && gate.threshold_value !== null);
	const canAddOption = $derived(!!gate && gate.type === 'choice' && !gate.generated_from);

	async function run(fn: () => Promise<unknown>, ok: string) {
		busy = true;
		const done = await guard(fn, ok);
		busy = false;
		if (done) await onchanged();
		return done;
	}
	function save() {
		if (!gate) return;
		const edit: { question?: string; threshold_value?: number } = {};
		if (question !== gate.question) edit.question = question;
		if (tunable && threshold !== String(gate.threshold_value)) edit.threshold_value = Number(threshold);
		run(() => api().updateGateQuestion(gate.key, edit), `Saved ${gate.key}.`);
	}
	async function addOption(e: SubmitEvent) {
		e.preventDefault();
		if (!gate) return;
		const label = optLabel;
		if (
			await run(
				() => api().addGateOption(gate.key, { label, description: optDesc }),
				`Added “${label}” to ${gate.key}.`,
			)
		) {
			optLabel = '';
			optDesc = '';
		}
	}
	const del = $derived(gate ? deletePermission(gate, true, '') : { ok: false, reason: null });
	const saveRule = (r: GateRule) =>
		run(() => api().updateGateQuestion(gate!.key, { rule: r }), `Saved the ${gate!.key} rule.`);
	const setGenerated = (v: boolean) =>
		run(
			() => api().updateGateQuestion(gate!.key, { generated: v }),
			v
				? `${gate!.key} options are now generated at the cut.`
				: `${gate!.key} options are now authored here.`,
		);
</script>

<Drawer bind:open title={gate ? `${gate.key} · gate question` : 'Gate question'} wide {dirty}>
	{#snippet sub()}
		{#if gate}
			<Chip text="{gate.point} · {RUN_POINT_LABEL[RUN_POINT[gate.point]]}" />
			<Chip text={gate.type} variant="tool" />
			<Chip text="fail {gate.fail}" variant={gate.fail === 'closed' ? 'err' : 'muted'} />
			{#if gate.safety}<Lock />{/if}
			<span class="mono muted"
				>{gate.layer} · updated {when(gate.updated_at)} by {gate.updated_by ?? '—'}</span
			>
		{/if}
	{/snippet}
	{#if gate}
		<div class="stack">
			<div class="field">
				<label for="gq-q">Question</label>
				<textarea id="gq-q" class="input" rows="2" bind:value={question}></textarea>
			</div>
			<div class="form-grid">
				<div class="field">
					<label for="gq-th">Threshold</label>
					{#if tunable}
						<input
							id="gq-th"
							class="input mono"
							type="number"
							step={gate.type === 'score' ? 1 : 0.05}
							bind:value={threshold}
						/>
					{:else}
						<input id="gq-th" class="input mono" value={gate.threshold} disabled />
					{/if}
				</div>
				<div class="field">
					<span class="label">Fail policy</span>
					<p class="fail">
						{#if gate.fail === 'closed'}<Lock /> Fails closed to a person. Fixed for safety gates (D29).{:else}Fails
							open: the conversation continues without the verdict.{/if}
					</p>
				</div>
			</div>
			<p class="gq-rule">
				<span class="mono">{ruleText(gate)}</span> → {gate.effect}
				<br /><span class="mono">{questionRuleText(gate)}</span>
			</p>
			<div class="row">
				<button type="button" class="btn primary" disabled={busy || !dirty || optLabel !== ''} onclick={save}
					>Save question</button
				>
			</div>

			<h3>Rule</h3>
			<p class="note">
				Asked at {gate.point} ({RUN_POINT_LABEL[RUN_POINT[gate.point]].toLowerCase()}); when it fires it will {ACTION_TEXT[
					gate.action
				]}; fail {gate.fail}{gate.fail === 'closed' ? ' — an unsure answer also goes to a person' : ''}.
			</p>
			<GateRuleEditor
				id={gate.key}
				type={gate.type}
				options={gate.options}
				rule={gate.rule}
				disabled={gate.safety}
				disabledReason="A safety gate keeps its rule: fail closed on any true (D29)."
				{busy}
				saveLabel="Save template rule"
				onsave={saveRule}
			/>

			<h3>Asked by</h3>
			{#if usage && usage.jobs.length}
				<ul class="gq-use">
					{#each usage.jobs as j (j.tenant_id + j.job)}
						<li>
							<span>
								{#if j.tenant_id === activeTenant}<a href="/admin/jobs/{encodeURIComponent(j.job)}/gates"
										>{j.job_label}</a
									>{:else}{j.job_label}{/if}
								<span class="muted">· {j.tenant_name}</span>
							</span>
							<span class="muted gq-use-n">
								{j.refinements
									? `${j.refinements} refinement${j.refinements === 1 ? '' : 's'}`
									: 'no refinements'}{j.verdicts_7d !== null ? ` · ${j.verdicts_7d} verdicts / 7d` : ''}
							</span>
						</li>
					{/each}
				</ul>
				<p class="note">
					Editing the question changes the template for every job above; each job picks it up when its tenant
					publishes its next release (template upgrades ride the publish). Tenant refinements stay on top.
				</p>
			{:else}
				<p class="note">No job asks this question yet. A template layer adds it to a job's gate set.</p>
			{/if}

			<h3>Options</h3>
			{#if gate.type === 'choice'}
				<label class="check">
					<input
						type="checkbox"
						checked={!!gate.generated_from}
						disabled={busy}
						onchange={(e) => setGenerated(e.currentTarget.checked)}
					/>
					Generated at the release cut
				</label>
				{#if gate.generated_from}<p class="note">
						From {gate.generated_from}. Generated options are read-only here.
					</p>{/if}
			{:else}
				<p class="note">
					{gate.type === 'noul'
						? 'noul answers true/false only, with no confidence.'
						: 'Score levels are fixed (2–10 ordered levels).'}
				</p>
			{/if}
			<div class="tbl-wrap">
				<table class="tbl dense">
					<thead><tr><th>Key</th><th>Label</th><th>Description</th><th>Source</th></tr></thead>
					<tbody>
						{#each gate.options as o (o.key)}
							<tr>
								<td class="mono">{o.key}</td>
								<td>{o.label}</td>
								<td class="muted">{o.description || '—'}</td>
								<td
									>{#if o.generated}<Chip text="generated · read-only" variant="tool" />{:else}<Chip
											text="authored"
										/>{/if}</td
								>
							</tr>
						{:else}
							<tr
								><td colspan="4" class="muted">Built per case from {gate.generated_from ?? 'the case'}.</td
								></tr
							>
						{/each}
					</tbody>
				</table>
			</div>
			<form class="row add" onsubmit={addOption}>
				<input
					class="input"
					aria-label="New option label"
					placeholder="New option label"
					bind:value={optLabel}
					disabled={!canAddOption}
				/>
				<input
					class="input"
					aria-label="New option description"
					placeholder="Description (optional)"
					bind:value={optDesc}
					disabled={!canAddOption}
				/>
				<button
					type="submit"
					class="btn sm"
					disabled={busy || !canAddOption || !optLabel.trim()}
					title={canAddOption
						? undefined
						: gate.generated_from
							? 'Options are generated at the cut'
							: 'Only choice questions take options'}>Add option</button
				>
			</form>
			<h3>Delete</h3>
			<div class="row">
				<button type="button" class="btn sm danger" disabled title={del.reason ?? undefined}
					>Delete question</button
				>
				<span class="muted gq-why">{del.reason}</span>
			</div>
		</div>
	{/if}
</Drawer>

<style>
	h3 {
		margin: 8px 0 0;
		font: 600 13px/1.3 var(--cond);
		letter-spacing: 0.06em;
		text-transform: uppercase;
		color: var(--muted);
	}
	.row {
		display: flex;
		gap: 8px;
		align-items: center;
		flex-wrap: wrap;
	}
	.add .input {
		flex: 1;
		min-width: 160px;
	}
	.fail {
		margin: 6px 0 0;
		font-size: 13px;
	}
	.gq-rule {
		margin: 0;
		font-size: 13px;
		padding: 8px 10px;
		border-radius: 6px;
		background: var(--sunk);
	}
	.gq-use {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 4px;
		font-size: 13px;
	}
	.gq-use li {
		display: flex;
		flex-wrap: wrap;
		justify-content: space-between;
		gap: 2px 12px;
	}
	.gq-why {
		font-size: 12.5px;
	}
	.gq-use-n {
		font-size: 12px;
	}
	.check {
		display: inline-flex;
		gap: 6px;
		align-items: center;
		font-size: 13px;
	}
</style>
