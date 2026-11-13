<script lang="ts">
	// P8 activity drawer, template editing (B8 T4a/T4b/T7a): edit or retire the activity (F-43), the operations it
	// expects (F-42) and its use case card (F-44). Each save is one designer call; `onChanged` re-reads the grid.
	import { api, guard } from '$lib/app.svelte';
	import type {
		Actor,
		DialogueTurn,
		IndustryJourney,
		StageKey,
		UseCaseCardPatch,
	} from '$lib/api/types/journey';
	import type { GridActivity } from '$lib/journey/JourneyGrid.svelte';
	import { OPS, objectKey, opLabel } from '$lib/journey/catalogue';

	let {
		act,
		j,
		industry,
		department,
		onChanged,
		onRetired,
	}: {
		act: GridActivity;
		j: IndustryJourney;
		industry: string;
		department: string | undefined;
		onChanged: (key: string) => Promise<void> | void;
		onRetired: () => Promise<void> | void;
	} = $props();

	// ---- edit (F-43) ----
	let label = $state('');
	let trigger = $state('');
	let description = $state('');
	let actor = $state<Actor>('customer');
	let also = $state<StageKey[]>([]);
	let confirmRetire = $state(false);
	$effect(() => {
		label = act.label;
		trigger = act.trigger;
		description = act.description ?? '';
		actor = act.actor;
		also = [...act.also_in_stages];
		confirmRetire = false;
	});
	async function save() {
		const key = act.key;
		if (
			await guard(
				() =>
					api().updateJourneyActivity(
						industry,
						key,
						{ label, trigger, actor, also_in_stages: also, description },
						department,
					),
				`Activity ${key} saved.`,
			)
		)
			await onChanged(key);
	}
	async function retire() {
		const key = act.key;
		if (await guard(() => api().retireJourneyActivity(industry, key, department), `Activity ${key} retired.`))
			await onRetired();
	}

	// ---- operation expectations (F-42) ----
	const bound = $derived(new Set(act.objects.map((o) => objectKey(o.object))));
	const opChoices = $derived(Object.keys(OPS).filter((k) => bound.has(k.split('.')[0]!)));
	let opPick = $state('');
	let opVariant = $state('');
	let opExpect = $state<'R' | 'O'>('R');
	const opName = $derived(opPick || opChoices[0] || '');
	async function expect(op: string, expectation: 'R' | 'O') {
		const key = act.key;
		if (
			await guard(
				() => api().setActivityOperation(industry, key, op, expectation, department),
				`${op} is ${expectation === 'R' ? 'required' : 'optional'} for ${key}.`,
			)
		)
			await onChanged(key);
	}
	async function addOp() {
		const v = opVariant.trim();
		await expect(v ? `${opName}{${v}}` : opName, opExpect);
		opVariant = '';
	}
	async function removeOp(op: string) {
		const key = act.key;
		if (await guard(() => api().removeActivityOperation(industry, key, op, department), `${op} removed.`))
			await onChanged(key);
	}

	// ---- use case card (F-44) ----
	let cSummary = $state('');
	let cExamples = $state('');
	let cMetricLabel = $state('');
	let cMetricTarget = $state('');
	let cDialogue = $state('');
	$effect(() => {
		void act.key;
		cSummary = cExamples = cMetricLabel = cMetricTarget = cDialogue = '';
	});
	/** `customer: text` lines → turns; a line without a speaker is the customer's. */
	function turns(text: string): DialogueTurn[] {
		return text
			.split('\n')
			.map((l) => l.trim())
			.filter(Boolean)
			.map((l) => {
				const m = /^(customer|agent|system):\s*(.*)$/.exec(l);
				return { from: (m?.[1] ?? 'customer') as DialogueTurn['from'], text: m ? m[2]! : l };
			});
	}
	async function saveCard() {
		const patch: UseCaseCardPatch = {};
		if (cSummary.trim()) patch.summary = cSummary.trim();
		const ex = cExamples
			.split('\n')
			.map((x) => x.trim())
			.filter(Boolean);
		if (ex.length) patch.examples = ex;
		if (cMetricLabel.trim())
			patch.metric = {
				key: act.key.split(':')[1] ?? act.key,
				label: cMetricLabel.trim(),
				target: cMetricTarget.trim() || '—',
			};
		const d = turns(cDialogue);
		if (d.length) patch.sample_dialogue = d;
		const key = act.key;
		if (
			await guard(
				() => api().updateUseCaseCard(industry, key, patch, department),
				`Use case card of ${key} saved.`,
			)
		) {
			cSummary = cExamples = cMetricLabel = cMetricTarget = cDialogue = '';
			await onChanged(key);
		}
	}
</script>

<div class="stack" data-testid="activity-editors">
	<fieldset class="editor-fields">
		<legend>Edit activity</legend>
		<div class="form-grid">
			<div class="field">
				<label for="ae-label">Label</label>
				<input id="ae-label" class="input" bind:value={label} />
			</div>
			<div class="field">
				<label for="ae-actor">Actor</label>
				<select id="ae-actor" class="input" bind:value={actor}>
					<option value="customer">customer</option>
					<option value="company">company</option>
					<option value="employee">employee</option>
				</select>
			</div>
		</div>
		<div class="field">
			<label for="ae-trigger">Trigger</label>
			<input id="ae-trigger" class="input" bind:value={trigger} />
		</div>
		<div class="field">
			<label for="ae-description">In plain words</label>
			<input
				id="ae-description"
				class="input"
				bind:value={description}
				placeholder="One line a customer would recognise, e.g. Find out where my order is"
			/>
		</div>
		<div class="field">
			<span class="label">Also shown in</span>
			<div class="chips">
				{#each j.stages.filter((s) => s.key !== act.stage) as s (s.key)}
					<label class="chip muted"><input type="checkbox" value={s.key} bind:group={also} /> {s.label}</label
					>
				{/each}
			</div>
		</div>
		<div class="row">
			<button class="btn" type="button" onclick={save}>Save activity</button>
			{#if confirmRetire}
				<button class="btn primary" type="button" onclick={retire}>Confirm retire</button>
				<button class="btn" type="button" onclick={() => (confirmRetire = false)}>Keep</button>
			{:else}
				<button class="btn" type="button" onclick={() => (confirmRetire = true)}>Retire…</button>
			{/if}
		</div>
		{#if confirmRetire}
			<p class="note">
				Retiring {act.key} also removes its use case card and its “also shown in” references from the template.
				Jobs pinned to an earlier tag keep the activity until they re-pin.
			</p>
		{/if}
	</fieldset>

	<fieldset class="editor-fields">
		<legend>Operations this activity expects</legend>
		<table class="tbl dense" aria-label="Operation expectations">
			<tbody>
				{#each act.operations as o (opLabel(o))}
					<tr>
						<td class="mono">{opLabel(o)}</td>
						<td
							><select
								class="input"
								aria-label="Expectation of {opLabel(o)}"
								value={o.required ? 'R' : 'O'}
								onchange={(e) =>
									expect(opLabel(o), (e.currentTarget as HTMLSelectElement).value as 'R' | 'O')}
							>
								<option value="R">required</option>
								<option value="O">optional</option>
							</select></td
						>
						<td
							><button
								class="btn sm"
								type="button"
								aria-label="Remove {opLabel(o)}"
								onclick={() => removeOp(opLabel(o))}>Remove</button
							></td
						>
					</tr>
				{:else}
					<tr><td class="muted">No operations expected.</td></tr>
				{/each}
			</tbody>
		</table>
		{#if opChoices.length}
			<div class="form-grid">
				<div class="field">
					<label for="ae-op">Operation (of the bound objects)</label>
					<select id="ae-op" class="input" bind:value={opPick}>
						{#each opChoices as k (k)}<option value={k} selected={k === opName}>{k}</option>{/each}
					</select>
				</div>
				<div class="field">
					<label for="ae-variant">Variant (optional)</label>
					<input id="ae-variant" class="input" placeholder="return" bind:value={opVariant} />
				</div>
				<div class="field">
					<label for="ae-exp">Expectation</label>
					<select id="ae-exp" class="input" bind:value={opExpect}>
						<option value="R">required</option>
						<option value="O">optional</option>
					</select>
				</div>
			</div>
			<button class="btn" type="button" onclick={addOp}>Expect operation</button>
		{:else}
			<p class="muted">Bind an object first: only operations of bound objects can be expected.</p>
		{/if}
	</fieldset>

	<fieldset class="editor-fields">
		<legend>Use case card</legend>
		<p class="muted small">Fill only what changes; empty fields keep the current card.</p>
		<div class="field">
			<label for="ac-summary">Summary</label>
			<textarea id="ac-summary" class="input" rows="2" bind:value={cSummary}></textarea>
		</div>
		<div class="field">
			<label for="ac-examples">Customer phrasings (one per line)</label>
			<textarea id="ac-examples" class="input" rows="3" bind:value={cExamples}></textarea>
		</div>
		<div class="form-grid">
			<div class="field">
				<label for="ac-ml">North-star metric</label>
				<input id="ac-ml" class="input" bind:value={cMetricLabel} />
			</div>
			<div class="field">
				<label for="ac-mt">Target</label>
				<input id="ac-mt" class="input" placeholder="≥ 80 %" bind:value={cMetricTarget} />
			</div>
		</div>
		<div class="field">
			<label for="ac-dlg"
				>Sample dialogue (<code>customer:</code>, <code>agent:</code> or <code>system:</code> per line)</label
			>
			<textarea id="ac-dlg" class="input" rows="4" bind:value={cDialogue}></textarea>
		</div>
		<button class="btn" type="button" onclick={saveCard}>Save card</button>
	</fieldset>
</div>
