<script lang="ts">
	// J7 gate detail: the question as this job runs it (inherited from a template layer), what it did in the last
	// 7 days, and the tenant refinements: relabel / describe / hide an option, tune the threshold — with a preview of
	// what the new threshold would have done to the same week. Refinements are saved to the job's draft (D54): tested in
	// the test chat, they reach customers when a release that contains them is published (cut → approve → apply).
	// Safety gates: lock, no hide, no threshold, fixed rule. The rule (gte / lt / in + min confidence) is a draft
	// refinement too; a question the tenant added to this job can be deleted (draft), a template one cannot.
	import { untrack } from 'svelte';
	import { api, guard } from '$lib/app.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import BarsH from '$lib/ui/BarsH.svelte';
	import { writeHint } from '$lib/auth/roles';
	import Lock from '$lib/ontology/Lock.svelte';
	import { fmt } from '$lib/format';
	import {
		actedText,
		belowMeans,
		changeText,
		SHIPS_TEXT,
		draftToast,
		ruleText,
		thresholdPreview,
	} from '$lib/ontology/gatesJ7';
	import type { EffGateQuestion, GateChange, GateRule } from '$lib/api/types/ontology';
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
		changes = [],
		live = null,
		canWrite,
		onchanged,
	}: {
		open: boolean;
		gate: EffGateQuestion | null;
		changes?: GateChange[];
		/** The applied release id, or null. */
		live?: string | null;
		canWrite: boolean;
		onchanged: () => Promise<void>;
	} = $props();

	let drafts = $state<Record<string, { label: string; description: string }>>({});
	let threshold = $state('');
	let busy = $state(false);
	const preview = $derived(
		gate && threshold != null && String(threshold) !== '' && Number(threshold) !== gate.threshold_value
			? thresholdPreview(gate, Number(threshold))
			: null,
	);
	const gateKey = $derived(gate?.key ?? '');
	// Fresh edit state per gate, and after every save (the gate prop is re-read then).
	$effect(() => {
		void gateKey;
		void gate;
		untrack(() => {
			drafts = Object.fromEntries(
				(gate?.options ?? []).map((o) => [o.key, { label: o.label, description: o.description }]),
			);
			threshold = gate?.threshold_value === null || !gate ? '' : String(gate.threshold_value);
		});
	});

	const total = $derived(gate ? gate.options.reduce((a, o) => a + o.verdicts, 0) : 0);
	const tunable = $derived(!!gate && !gate.safety && gate.base_threshold_value !== null);

	async function run(fn: () => Promise<unknown>, ok: string) {
		busy = true;
		const done = await guard(fn, ok);
		busy = false;
		if (done) await onchanged();
	}
	const saveOption = (key: string) => {
		const d = drafts[key]!;
		return run(
			() => api().refineGateOption(gate!.key, key, { label: d.label, description: d.description }),
			draftToast(`Saved ${gate!.key} option “${d.label || key}”.`),
		);
	};
	const toggleHidden = (key: string, hidden: boolean) =>
		run(
			() => api().refineGateOption(gate!.key, key, { hidden }),
			draftToast(
				hidden
					? `${gate!.key}: “${key}” is hidden for this job.`
					: `${gate!.key}: “${key}” is offered again.`,
			),
		);
	const del = $derived(
		gate ? deletePermission(gate, canWrite, writeHint('job_designer')) : { ok: false, reason: null },
	);
	const saveRule = (r: GateRule | null) =>
		run(
			() => api().setGateRule(gate!.key, r),
			draftToast(r === null ? `${gate!.key} uses the template rule again.` : `${gate!.key} rule saved.`),
		);
	async function deleteQuestion() {
		const key = gate!.key;
		busy = true;
		const done = await guard(
			() => api().deleteGateQuestion(key),
			draftToast(`${key} is deleted from this job's draft.`),
			'Not deleted',
		);
		busy = false;
		if (done) {
			open = false;
			await onchanged();
		}
	}
	// F-16 (B5 T8a): the text the customer reads when this question hands over or blocks.
	let reply = $state('');
	$effect(() => {
		void gateKey;
		void gate;
		untrack(() => {
			reply = gate?.customer_reply ?? '';
		});
	});
	const saveReply = (text: string | null) =>
		run(
			() => api().setGateCustomerReply(gate!.key, text),
			draftToast(
				text === null
					? `${gate!.key} uses the default customer reply again.`
					: `${gate!.key} customer reply saved.`,
			),
		);
	const saveThreshold = (v: number | null) =>
		run(
			() => api().setGateThreshold(gate!.key, v),
			draftToast(
				v === null ? `${gate!.key} uses the base threshold again.` : `${gate!.key} threshold is now ${v}.`,
			),
		);
</script>

<Drawer bind:open title={gate ? `${gate.key} · ${gate.question}` : 'Gate'} wide>
	{#snippet sub()}
		{#if gate}
			<Chip text="{gate.point} · {RUN_POINT_LABEL[RUN_POINT[gate.point]]}" />
			<Chip text={gate.type} variant="tool" />
			{#if gate.origin === 'tenant'}<Chip text="added by your team" variant="accent" />{/if}
			<Chip text="fail {gate.fail}" variant={gate.fail === 'closed' ? 'err' : 'muted'} />
			{#if gate.safety}<Lock /> <span class="muted">safety gate</span>{/if}
			<span class="mono muted">{gate.layer}</span>
		{/if}
	{/snippet}
	{#if gate}
		<dl class="kvs">
			<dt>Acts</dt>
			<dd><span class="mono">{ruleText(gate)}</span> → {gate.effect}</dd>
			<dt>Rule</dt>
			<dd>
				<span class="mono">{questionRuleText(gate)}</span>
				{#if gate.rule_refined}<Chip text="refined" variant="accent" />{/if}
			</dd>
			<dt>Point</dt>
			<dd>
				{gate.point} · {RUN_POINT_LABEL[RUN_POINT[gate.point]]}{#if gate.tools?.length}
					· asked only before <span class="mono">{gate.tools.join(', ')}</span>{/if}
			</dd>
			<dt>Comes from</dt>
			<dd>
				The <span class="mono">{gate.layer}</span> template layer. The question{gate.generated_from
					? ' and its option list'
					: ''} are read-only here; you refine labels, descriptions, hidden options and the threshold below.
			</dd>
			{#if gate.type !== 'noul'}
				<dt>Below the line</dt>
				<dd>{belowMeans(gate)}.</dd>
			{/if}
			<dt>When laya is down</dt>
			<dd>
				{gate.fail === 'closed'
					? `Fails closed: the message goes to a person — also when laya is unsure (below the minimum confidence).${gate.safety ? ' This cannot be changed (D29).' : ''}`
					: 'Fails open: the conversation continues without this verdict — also when laya is unsure.'}
			</dd>
			<dt>Last 7 days</dt>
			<dd>
				{actedText(gate)} · {fmt.n(gate.stats.verdicts_7d)} verdicts · {fmt.n(gate.stats.below_threshold)} below
				threshold · failed open
				{gate.stats.fail_open}, closed {gate.stats.fail_closed} · laya p50 {fmt.ms(
					gate.stats.latency_p50_ms,
				)}, p95
				{fmt.ms(gate.stats.latency_p95_ms)} · {gate.stats.timeouts} timeouts
			</dd>
		</dl>

		<section>
			<h3>Customer reply</h3>
			<p class="note">
				What the customer reads when {gate.key} hands the conversation to a person or blocks the text.
				{gate.customer_reply_default
					? `Default: “${gate.customer_reply_default}”.`
					: 'Default: the standard handover / block message.'}
			</p>
			<textarea
				rows="3"
				maxlength="500"
				aria-label="Customer reply for {gate.key}"
				bind:value={reply}
				disabled={!canWrite || busy}
				placeholder={gate.customer_reply_default ?? 'The standard handover / block message'}></textarea>
			<div class="row">
				<button
					type="button"
					class="btn"
					disabled={!canWrite || busy || !reply.trim() || reply.trim() === (gate.customer_reply ?? '')}
					onclick={() => saveReply(reply)}>Save reply</button
				>
				<button
					type="button"
					class="btn"
					disabled={!canWrite || busy || gate.customer_reply === gate.customer_reply_default}
					onclick={() => saveReply(null)}>Use default</button
				>
			</div>
		</section>

		{#if changes.length}
			<section>
				<h3>Your refinements of {gate.key}</h3>
				<ul class="chg">
					{#each changes as c, i (i)}
						<li>
							<span>{changeText(c)}</span>
							<Chip
								text={c.release_id ? `${SHIPS_TEXT[c.ships]} · ${c.release_id}` : SHIPS_TEXT[c.ships]}
								variant={c.ships === 'live' ? 'ok' : c.ships === 'approved' ? 'accent' : 'warn'}
							/>
							{#if c.ships !== 'live'}<small class="muted">live still: {c.from || '—'}</small>{/if}
							<small class="muted">{c.by_name} · {c.at.slice(0, 16).replace('T', ' ')}</small>
						</li>
					{/each}
				</ul>
				<p class="note">
					Saved changes go to the draft: try them in the <a href="/chat">test chat</a>. Customers keep the
					live value{live ? ` (release ${live})` : ''} until a release that contains the change is published.
				</p>
			</section>
		{/if}

		<section>
			<h3>Rule</h3>
			<GateRuleEditor
				id={gate.key}
				type={gate.type}
				options={gate.options.filter((o) => !o.hidden)}
				rule={gate.rule}
				disabled={!canWrite || gate.safety}
				disabledReason={gate.safety
					? 'A safety gate keeps its template rule (D29).'
					: writeHint('job_designer')}
				{busy}
				saveLabel="Save rule to draft"
				onsave={(r) => saveRule(r)}
			/>
			{#if gate.rule_refined}
				<p class="row gd-base">
					<span class="muted"
						>Template: <span class="mono">{questionRuleText({ ...gate, rule: gate.base_rule })}</span></span
					>
					<button
						type="button"
						class="btn sm"
						disabled={!canWrite || busy}
						title={canWrite ? undefined : writeHint('job_designer')}
						onclick={() => saveRule(null)}>Revert rule</button
					>
				</p>
			{/if}
			<p class="note">Fired → {ACTION_TEXT[gate.action]}. Saving the rule replaces a tuned threshold.</p>
		</section>

		<section>
			<h3>Threshold</h3>
			{#if tunable}
				<div class="row">
					<input
						class="input mono th"
						type="number"
						step={gate.type === 'score' ? 1 : 0.05}
						min={gate.type === 'score' ? 1 : 0.05}
						max={gate.type === 'score' ? gate.options.length : 1}
						aria-label="{gate.key} threshold"
						bind:value={threshold}
						disabled={!canWrite}
					/>
					<span class="muted">base {gate.base_threshold_value} ({gate.threshold})</span>
					<button
						type="button"
						class="btn sm primary"
						disabled={!canWrite || busy}
						title={canWrite ? undefined : writeHint('job_designer')}
						onclick={() => saveThreshold(Number(threshold))}>Save threshold</button
					>
					{#if gate.threshold_refined}
						<button
							type="button"
							class="btn sm"
							disabled={!canWrite || busy}
							title={canWrite ? undefined : writeHint('job_designer')}
							onclick={() => saveThreshold(null)}>Revert to base</button
						>
					{/if}
				</div>
				{#if preview}<p class="preview" role="status" data-testid="threshold-preview">{preview}</p>{/if}
			{:else}
				<p class="muted">
					{gate.threshold} — {gate.safety
						? 'a safety gate acts on any true; not tunable.'
						: 'acts on any true; nothing to tune.'}
				</p>
			{/if}
		</section>

		{#if total > 0}
			<section>
				<h3>Verdict distribution (7 d)</h3>
				<BarsH
					ariaLabel="{gate.key} verdicts"
					items={gate.options.map((o) => ({
						label: o.hidden ? `${o.label} (hidden)` : o.label,
						parts: [{ v: o.verdicts, cls: o.hidden ? 's-dead' : 's-cust', label: 'verdicts' }],
						right: `${fmt.n(o.verdicts)} · ${Math.round((o.verdicts / total) * 100)}%`,
					}))}
				/>
			</section>
		{/if}

		<section>
			<h3>Options ({gate.options.length})</h3>
			{#if gate.generated_from}
				<p class="note">
					Generated at the release cut from {gate.generated_from}. The list itself is read-only; you may
					relabel, re-describe or hide an option for this job.
				</p>
			{/if}
			{#if gate.options.length === 0}
				<p class="muted">
					Options are built per case from {gate.generated_from ?? 'the case'}; nothing to refine here.
				</p>
			{:else}
				<div class="tbl-wrap">
					<table class="tbl dense">
						<thead
							><tr><th>Option</th><th>Label</th><th>Description</th><th class="r">Verdicts</th><th></th></tr
							></thead
						>
						<tbody>
							{#each gate.options as o (o.key)}
								{@const d = drafts[o.key]}
								<tr class:hid={o.hidden}>
									<td class="mono">
										{o.key}
										{#if o.generated}<Chip text="generated" variant="tool" />{/if}
										{#if o.refined}<Chip
												text="refined"
												variant="accent"
												title="Base label: {o.base_label}"
											/>{/if}
									</td>
									<td>
										{#if d}<input
												class="input"
												aria-label="{o.key} label"
												bind:value={d.label}
												disabled={!canWrite}
											/>{/if}
									</td>
									<td>
										{#if d}<input
												class="input"
												aria-label="{o.key} description"
												bind:value={d.description}
												disabled={!canWrite}
											/>{/if}
									</td>
									<td class="r mono">{fmt.n(o.verdicts)}</td>
									<td class="acts">
										<button
											type="button"
											class="btn sm"
											disabled={!canWrite ||
												busy ||
												!d ||
												(d.label === o.label && d.description === o.description)}
											title={canWrite ? undefined : writeHint('job_designer')}
											onclick={() => saveOption(o.key)}>Save</button
										>
										{#if gate.safety}
											<button type="button" class="btn sm" disabled title="Safety gates cannot be hidden"
												><Lock /> Hide</button
											>
										{:else}
											<button
												type="button"
												class="btn sm"
												disabled={!canWrite || busy}
												title={canWrite ? undefined : writeHint('job_designer')}
												onclick={() => toggleHidden(o.key, !o.hidden)}>{o.hidden ? 'Unhide' : 'Hide'}</button
											>
										{/if}
									</td>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
				<p class="note">
					Clear a label and save to return to the base label ({gate.options.find((o) => o.refined)
						?.base_label ?? 'as authored'}).
				</p>
			{/if}
		</section>

		<section>
			<h3>Delete</h3>
			<div class="row">
				<button
					type="button"
					class="btn sm danger"
					disabled={!del.ok || busy}
					title={del.reason ?? undefined}
					onclick={deleteQuestion}>Delete {gate.key} from this job</button
				>
				{#if del.reason}<span class="muted gd-why">{del.reason}</span>{:else}<span class="muted gd-why"
						>Saved to draft: customers keep it until you publish.</span
					>{/if}
			</div>
		</section>
	{/if}
</Drawer>

<style>
	section {
		margin-top: 14px;
	}
	h3 {
		margin: 0 0 8px;
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
	.th {
		width: 90px;
	}
	.gd-base {
		margin: 8px 0 0;
		font-size: 12.5px;
	}
	.gd-why {
		font-size: 12.5px;
	}

	.preview {
		margin: 8px 0 0;
		padding: 7px 10px;
		border-left: 3px solid var(--accent);
		background: color-mix(in srgb, var(--accent) 7%, var(--panel));
		font-size: 12.5px;
	}
	.chg {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 6px;
	}
	.chg li {
		display: flex;
		flex-wrap: wrap;
		gap: 4px 8px;
		align-items: baseline;
	}
	tr.hid td {
		opacity: 0.6;
	}
	.acts {
		white-space: nowrap;
		display: flex;
		gap: 4px;
	}
	td :global(.chip) {
		margin-left: 4px;
	}
	.tbl .input {
		padding: 4px 7px;
	}
</style>
