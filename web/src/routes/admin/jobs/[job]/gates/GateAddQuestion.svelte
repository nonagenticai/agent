<script lang="ts">
	// J7: add a yes/no question of the tenant's own to this job (`POST /api/jobs/{job}/gates`). Saved to the draft
	// (D54) like every refinement; it can be deleted again from its drawer. observer: visible, disabled.
	import { api, guard } from '$lib/app.svelte';
	import { writeHint } from '$lib/auth/roles';
	import { draftToast } from '$lib/ontology/gatesJ7';
	import { RUN_POINT, RUN_POINT_LABEL, parseTools } from '$lib/ontology/gateRules';
	import type { GateAction, GateFail, GatePoint } from '$lib/api/types/ontology';

	let { canWrite, onadded }: { canWrite: boolean; onadded: (key: string) => Promise<void> } = $props();

	let key = $state('');
	let question = $state('');
	let point = $state<GatePoint>('P1');
	let yes = $state('');
	let no = $state('');
	let action = $state<GateAction>('handover');
	let fail = $state<GateFail>('open');
	let tools = $state('');
	let minConf = $state('');
	let busy = $state(false);
	const ready = $derived(!!key.trim() && question.trim().length >= 8 && !!yes.trim() && !!no.trim());

	async function submit(e: SubmitEvent) {
		e.preventDefault();
		const k = key.trim().toUpperCase();
		busy = true;
		const mc = String(minConf ?? '').trim() === '' ? null : Number(minConf);
		const done = await guard(
			() =>
				api().addGateQuestion({
					key: k,
					point,
					question,
					criteria: { true: yes, false: no },
					rule: { fire_when: 'gte', threshold: 0.5, fire_values: null, min_confidence: mc },
					action,
					fail,
					tools: point === 'P2' ? parseTools(tools) : null,
				}),
			draftToast(`${k} added to this job.`),
			'Not added',
		);
		busy = false;
		if (done) {
			key = question = yes = no = tools = minConf = '';
			await onadded(k);
		}
	}
</script>

<form class="aq" onsubmit={submit} aria-label="Add a question">
	<fieldset disabled={!canWrite || busy}>
		<div class="aq-row">
			<label class="aq-f aq-key"
				><span class="label">Key</span><input class="input mono" bind:value={key} placeholder="DR2" /></label
			>
			<label class="aq-f"
				><span class="label">Point</span>
				<select class="input" bind:value={point}>
					{#each ['P1', 'P2', 'R1'] as const as p (p)}<option value={p}
							>{p} · {RUN_POINT_LABEL[RUN_POINT[p]]}</option
						>{/each}
				</select></label
			>
		</div>
		<label class="aq-f"
			><span class="label">Yes/no question</span><input
				class="input"
				bind:value={question}
				placeholder="Does the customer …?"
			/></label
		>
		<label class="aq-f"><span class="label">True when</span><input class="input" bind:value={yes} /></label>
		<label class="aq-f"><span class="label">False when</span><input class="input" bind:value={no} /></label>
		{#if point === 'P2'}
			<label class="aq-f"
				><span class="label">Only before tools (blank: every tool)</span><input
					class="input mono"
					bind:value={tools}
					placeholder="refund.issue, order.cancel"
				/></label
			>
		{/if}
		<div class="aq-row">
			<label class="aq-f"
				><span class="label">When true</span>
				<select class="input" bind:value={action}>
					<option value="handover">Hand over</option>
					<option value="block">Block</option>
					<option value="annotate">Annotate</option>
				</select></label
			>
			<label class="aq-f"
				><span class="label">Fail</span>
				<select class="input" bind:value={fail}>
					<option value="open">open</option>
					<option value="closed">closed</option>
				</select></label
			>
			<label class="aq-f"
				><span class="label">Min confidence</span><input
					class="input mono aq-n"
					type="number"
					step="0.05"
					min="0"
					max="1"
					placeholder="any"
					bind:value={minConf}
				/></label
			>
		</div>
		<button
			type="submit"
			class="btn sm primary"
			disabled={!ready}
			title={canWrite ? undefined : writeHint('job_designer')}>Add to draft</button
		>
	</fieldset>
	{#if !canWrite}<p class="note">{writeHint('job_designer')} to add questions.</p>{/if}
</form>

<style>
	.aq fieldset {
		border: 0;
		padding: 0;
		margin: 0;
		display: grid;
		gap: 6px;
		min-width: 0;
	}
	.aq-row {
		display: flex;
		flex-wrap: wrap;
		gap: 6px 8px;
	}
	.aq-f {
		display: grid;
		gap: 2px;
		font-size: 12.5px;
		min-width: 0;
	}
	.aq-key input {
		width: 80px;
	}
	.aq-n {
		width: 80px;
	}
</style>
