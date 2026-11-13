<script lang="ts">
	// P8 template editing (B8 T4a, F-45–F-49): stage order and new stages, themes with their escalation weight,
	// per-industry channel status, example messages per stage × channel and stage knowledge kinds. Every control is
	// one API method of the designer; the grid it answers replaces the page's (`onChanged` refreshes it).
	import { api, guard } from '$lib/app.svelte';
	import type { ChannelKey, ChannelStatus, IndustryJourney, StageKey } from '$lib/api/types/journey';
	import Card from '$lib/ui/Card.svelte';

	let {
		j,
		industry,
		department,
		onChanged,
	}: {
		j: IndustryJourney;
		industry: string;
		department: string | undefined;
		onChanged: () => Promise<void> | void;
	} = $props();

	/** Run one write, then refresh the page's grid when it worked. */
	async function write(action: () => Promise<unknown>, ok: string): Promise<boolean> {
		const done = await guard(action, ok);
		if (done) await onChanged();
		return done;
	}

	const stages = $derived([...j.stages].sort((a, b) => a.order - b.order));
	const STATUSES: ChannelStatus[] = ['v1', 'later', 'out_of_scope'];

	// ---- stages: order + add (F-45) ----
	async function moveStage(i: number, by: -1 | 1) {
		const keys = stages.map((s) => s.key);
		const k = i + by;
		if (k < 0 || k >= keys.length) return;
		[keys[i], keys[k]] = [keys[k]!, keys[i]!];
		await write(() => api().reorderJourneyStages(industry, keys, department), 'Stage order saved.');
	}
	let sKey = $state('');
	let sLabel = $state('');
	let sDesc = $state('');
	let sAfter = $state('');
	async function addStage() {
		const input = {
			key: sKey.trim(),
			label: sLabel,
			description: sDesc,
			...(sAfter ? { after: sAfter as StageKey } : {}),
		};
		if (
			await write(() => api().addJourneyStage(industry, input, department), `Stage ${sKey.trim()} added.`)
		) {
			sKey = sLabel = sDesc = sAfter = '';
		}
	}

	// ---- themes (F-46) ----
	async function renameTheme(key: string, e: Event) {
		const el = e.currentTarget as HTMLInputElement;
		const ok = await write(
			() => api().updateJourneyTheme(industry, key as never, { label: el.value }, department),
			`Theme ${key} renamed.`,
		);
		if (!ok) el.focus();
	}
	async function weighTheme(key: string, e: Event) {
		const el = e.currentTarget as HTMLInputElement;
		const ok = await write(
			() =>
				api().updateJourneyTheme(industry, key as never, { escalation_weight: Number(el.value) }, department),
			`Escalation weight of ${key} saved.`,
		);
		if (!ok) el.focus();
	}
	let tKey = $state('');
	let tLabel = $state('');
	let tWeight = $state(10);
	async function addTheme() {
		const input = { key: tKey.trim(), label: tLabel, escalation_weight: Number(tWeight) };
		if (
			await write(() => api().addJourneyTheme(industry, input, department), `Theme ${tKey.trim()} added.`)
		) {
			tKey = tLabel = '';
			tWeight = 10;
		}
	}

	// ---- channels (F-49) ----
	async function setChannel(key: ChannelKey, e: Event) {
		const el = e.currentTarget as HTMLSelectElement;
		const ok = await write(
			() => api().setJourneyChannelStatus(industry, key, el.value as ChannelStatus, department),
			`${key} is ${el.value.replace('_', ' ')} for ${j.label}.`,
		);
		if (!ok) el.value = j.channels.find((c) => c.key === key)?.status ?? 'v1';
	}

	// ---- messages (F-48) ----
	let mStage = $state('');
	let mChannel = $state('');
	const msgStage = $derived(mStage || stages[0]?.key || '');
	const msgChannel = $derived(mChannel || j.channels[0]?.key || '');
	let mText = $state('');
	$effect(() => {
		mText = (j.messages.find((m) => m.stage === msgStage && m.channel === msgChannel)?.utterances ?? []).join(
			'\n',
		);
	});
	async function saveMessages() {
		const lines = mText.split('\n');
		await write(
			() =>
				api().setStageMessages(industry, msgStage as StageKey, msgChannel as ChannelKey, lines, department),
			`Example messages for ${msgStage} · ${msgChannel} saved.`,
		);
	}

	// ---- knowledge kinds (F-47) ----
	let kStage = $state('*');
	let kKind = $state('');
	let kStore = $state('');
	let kRequired = $state(false);
	async function addKind() {
		const kind = kKind.trim();
		if (
			await write(
				() =>
					api().setStageKnowledgeKind(
						industry,
						kStage,
						kind,
						{ store_key: kStore.trim(), required: kRequired },
						department,
					),
				`Knowledge kind ${kind} saved.`,
			)
		) {
			kKind = kStore = '';
			kRequired = false;
		}
	}
	/** Load a declared kind into the form to change it (the store key is not part of the grid). */
	function editKind(stage: string, kind: string, required: boolean) {
		kStage = stage;
		kKind = kind;
		kStore = '';
		kRequired = required;
	}
	async function removeKind(stage: string, kind: string) {
		await write(
			() => api().removeStageKnowledgeKind(industry, stage, kind, undefined, department),
			`Knowledge kind ${kind} removed.`,
		);
	}
</script>

<Card
	title="Edit the template"
	sub="Stages, themes, channels, example messages and knowledge kinds of this journey. Every save is an untagged draft revision; a new tag comes from a generator Package step."
>
	<div class="stack" data-testid="template-editors">
		<fieldset class="editor-fields">
			<legend>Stages</legend>
			<div class="tbl-wrap">
				<table class="tbl dense" aria-label="Stage order">
					<thead><tr><th>#</th><th>Stage</th><th>Key</th><th>Order</th></tr></thead>
					<tbody>
						{#each stages as s, i (s.key)}
							<tr>
								<td>{i + 1}</td>
								<td>{s.label}</td>
								<td class="mono">{s.key}</td>
								<td>
									<button
										type="button"
										class="btn sm"
										disabled={i === 0}
										aria-label="Move {s.label} earlier"
										onclick={() => moveStage(i, -1)}>↑</button
									>
									<button
										type="button"
										class="btn sm"
										disabled={i === stages.length - 1}
										aria-label="Move {s.label} later"
										onclick={() => moveStage(i, 1)}>↓</button
									>
								</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
			<div class="form-grid">
				<div class="field">
					<label for="te-s-key">New stage key</label>
					<input id="te-s-key" class="input" placeholder="loyalty" bind:value={sKey} />
				</div>
				<div class="field">
					<label for="te-s-label">New stage label</label>
					<input id="te-s-label" class="input" bind:value={sLabel} />
				</div>
				<div class="field">
					<label for="te-s-after">After</label>
					<select id="te-s-after" class="input" bind:value={sAfter}>
						<option value="">last</option>
						{#each stages as s (s.key)}<option value={s.key}>{s.label}</option>{/each}
					</select>
				</div>
			</div>
			<div class="field">
				<label for="te-s-desc">Description (the GS option text)</label>
				<input id="te-s-desc" class="input" bind:value={sDesc} />
			</div>
			<button type="button" class="btn" onclick={addStage}>Add stage</button>
		</fieldset>

		<fieldset class="editor-fields">
			<legend>Themes</legend>
			<div class="tbl-wrap">
				<table class="tbl dense" aria-label="Themes">
					<thead><tr><th>Theme</th><th>Key</th><th>Escalation weight (0–100)</th></tr></thead>
					<tbody>
						{#each j.themes as t (t.key)}
							<tr>
								<td
									><input
										class="input"
										aria-label="Label of {t.key}"
										value={t.label}
										onchange={(e) => renameTheme(t.key, e)}
									/></td
								>
								<td class="mono">{t.key}</td>
								<td
									><input
										class="input weight"
										type="number"
										min="0"
										max="100"
										step="1"
										aria-label="Escalation weight of {t.key}"
										value={Math.round(t.escalation_weight * 100)}
										onchange={(e) => weighTheme(t.key, e)}
									/></td
								>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
			<div class="form-grid">
				<div class="field">
					<label for="te-t-key">New theme key</label>
					<input id="te-t-key" class="input" placeholder="waiting" bind:value={tKey} />
				</div>
				<div class="field">
					<label for="te-t-label">New theme label</label>
					<input id="te-t-label" class="input" bind:value={tLabel} />
				</div>
				<div class="field">
					<label for="te-t-w">Escalation weight (0–100)</label>
					<input id="te-t-w" class="input" type="number" min="0" max="100" step="1" bind:value={tWeight} />
				</div>
			</div>
			<button type="button" class="btn" onclick={addTheme}>Add theme</button>
		</fieldset>

		<fieldset class="editor-fields">
			<legend>Channels for {j.label}</legend>
			<div class="tbl-wrap">
				<table class="tbl dense" aria-label="Channel status">
					<thead><tr><th>Channel</th><th>Status</th><th>Note</th></tr></thead>
					<tbody>
						{#each j.channels as c (c.key)}
							<tr>
								<td>{c.label}</td>
								<td
									><select
										class="input"
										aria-label="Status of {c.label}"
										value={c.status}
										onchange={(e) => setChannel(c.key, e)}
									>
										{#each STATUSES as s (s)}<option value={s}>{s.replace('_', ' ')}</option>{/each}
									</select></td
								>
								<td class="muted">{c.note || '—'}</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		</fieldset>

		<fieldset class="editor-fields">
			<legend>Example messages</legend>
			<div class="form-grid">
				<div class="field">
					<label for="te-m-stage">Stage</label>
					<select id="te-m-stage" class="input" bind:value={mStage}>
						{#each stages as s (s.key)}<option value={s.key} selected={s.key === msgStage}>{s.label}</option
							>{/each}
					</select>
				</div>
				<div class="field">
					<label for="te-m-ch">Channel</label>
					<select id="te-m-ch" class="input" bind:value={mChannel}>
						{#each j.channels as c (c.key)}<option value={c.key} selected={c.key === msgChannel}
								>{c.label}</option
							>{/each}
					</select>
				</div>
			</div>
			<div class="field">
				<label for="te-m-text">One customer message per line (at most 50, 500 characters each)</label>
				<textarea id="te-m-text" class="input" rows="5" bind:value={mText}></textarea>
			</div>
			<button type="button" class="btn" onclick={saveMessages}>Save messages</button>
		</fieldset>

		<fieldset class="editor-fields">
			<legend>Knowledge kinds</legend>
			<div class="tbl-wrap">
				<table class="tbl dense" aria-label="Knowledge kinds">
					<thead><tr><th>Stage</th><th>Kind</th><th>Required</th><th></th></tr></thead>
					<tbody>
						{#each j.knowledge_kinds as k (`${k.stage}|${k.kind}`)}
							<tr>
								<td class="mono">{k.stage}</td>
								<td class="mono">{k.kind}</td>
								<td>
									<button
										type="button"
										class="btn sm"
										title="Edit {k.kind}"
										onclick={() => editKind(k.stage, k.kind, k.required)}
										>{k.required ? 'required' : 'optional'}</button
									>
								</td>
								<td
									><button
										type="button"
										class="btn sm"
										aria-label="Remove {k.kind} from {k.stage}"
										onclick={() => removeKind(k.stage, k.kind)}>Remove</button
									></td
								>
							</tr>
						{:else}
							<tr><td colspan="4" class="muted">No knowledge kinds declared.</td></tr>
						{/each}
					</tbody>
				</table>
			</div>
			<div class="form-grid">
				<div class="field">
					<label for="te-k-stage">Stage</label>
					<select id="te-k-stage" class="input" bind:value={kStage}>
						<option value="*">every stage (*)</option>
						{#each stages as s (s.key)}<option value={s.key}>{s.label}</option>{/each}
					</select>
				</div>
				<div class="field">
					<label for="te-k-kind">Kind</label>
					<input id="te-k-kind" class="input" placeholder="policy.returns" bind:value={kKind} />
				</div>
				<div class="field">
					<label for="te-k-store">Store key</label>
					<input id="te-k-store" class="input" placeholder="returns_policy" bind:value={kStore} />
				</div>
			</div>
			<label class="row"
				><input type="checkbox" bind:checked={kRequired} /> Required (a lower layer can make an optional kind required,
				never the reverse)</label
			>
			<button type="button" class="btn" onclick={addKind}>Save knowledge kind</button>
		</fieldset>
	</div>
</Card>

<style>
	.weight {
		width: 90px;
	}
</style>
