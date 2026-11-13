<script lang="ts">
	// P10 Object detail (11 §1.2–§1.3, §3.1–§3.2, §6.1–§6.2). The question this page answers for the super_admin:
	// "what can the agent do to this object, in which status — and what does it refuse?" The lifecycle (state machine)
	// is the centrepiece: agent operations vs backend events, and per verb the `invalid_state:<s>` errors derived from
	// it. Around it: where the object is anchored (template layers, connector templates + status mapping, journey
	// activities) and its schema. Edits reach tenants only through a new cut + apply.
	import { superAdminAttrs } from '$lib/auth/roles';
	import { untrack } from 'svelte';
	import { api, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { Resource } from '$lib/load.svelte';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Tabs from '$lib/ui/Tabs.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import ObjectDetailLifecycle from './ObjectDetailLifecycle.svelte';
	import ObjectDetailContext from './ObjectDetailContext.svelte';
	import type {
		FieldInput,
		ObjectContext,
		FieldType,
		ObjectDetail,
		ObjectField,
		ObjectState,
		ObjectTransition,
		StateInput,
		TransitionInput,
	} from '$lib/api/types/model';
	import { isBackendEvent } from './logic';
	import { EFFECT_LABEL, EFFECT_VARIANT, LEVEL_LABEL, TIER_VARIANT, when } from './ui';
	import { shortVerb, stateFacts, verbMatrix } from './objectDetailP10';

	let { object }: { object: string } = $props();

	const res = new Resource<ObjectDetail>(() => api().objectDetail(object));
	const ctxRes = new Resource<ObjectContext>(
		() => api().objectContext(object),
		() => false,
	);
	$effect(() => {
		void object;
		untrack(() => {
			selState = null;
			selVerb = null;
			res.refresh();
			ctxRes.refresh();
		});
	});
	const ctx = $derived(ctxRes.value.state === 'ok' ? ctxRes.value.data : null);

	let tab = $state('fields');
	let showEvents = $state(true);
	let selState = $state<string | null>(null);
	let selVerb = $state<string | null>(null);

	/** The status the inspector explains: the picked one, else where the object starts. */
	const focusOf = (d: ObjectDetail) =>
		selState ?? d.states.find((s) => s.initial)?.status ?? d.states[0]?.status;
	const labelOf = (d: ObjectDetail, s: string) => d.states.find((x) => x.status === s)?.label ?? s;
	/** System statuses (per connector template) that a connector maps onto one canonical status. */
	const systemStatuses = (c: ObjectContext | null, status: string) =>
		(c?.systems ?? []).flatMap((s) =>
			s.status_map.filter((m) => m.canonical === status).map((m) => ({ system: s.label, raw: m.system })),
		);
	const activitiesFor = (c: ObjectContext | null, verb: string) =>
		(c?.activities ?? []).filter((a) => a.operations.some((o) => o.op.replace(/\{.*\}$/, '') === verb));
	const plural = (n: number, w: string) => `${n} ${w}${n === 1 ? '' : 's'}`;

	const TYPES: FieldType[] = [
		'id',
		'string',
		'text',
		'enum',
		'money',
		'int',
		'decimal',
		'bool',
		'date',
		'datetime',
		'address',
		'ref',
		'list',
		'json',
		'url',
	];

	// --- field editor
	let fieldOpen = $state(false);
	let fieldNew = $state(true);
	let fieldForm = $state<FieldInput & { enum_text: string }>(blankField());
	function blankField() {
		return {
			field: '',
			data_type: 'string' as FieldType,
			pii: false,
			enum_values: [],
			ref: null,
			required: false,
			description: '',
			enum_text: '',
		};
	}
	function editField(f: ObjectField | null) {
		fieldNew = !f;
		fieldForm = f
			? { ...f, enum_values: [...f.enum_values], enum_text: f.enum_values.join(', ') }
			: blankField();
		fieldOpen = true;
	}
	async function saveField() {
		const { enum_text, ...rest } = fieldForm;
		const input: FieldInput = { ...rest, enum_values: enum_text.split(','), ref: rest.ref?.trim() || null };
		if (
			await guard(
				() => api().saveObjectField(object, input),
				`Field ${input.field} saved. It reaches tenants with the next cut.`,
			)
		) {
			fieldOpen = false;
			await res.refresh();
		}
	}

	// --- state editor
	let stateOpen = $state(false);
	let stateNew = $state(true);
	let stateForm = $state<StateInput>({ status: '', label: '', terminal: false, canonical: true });
	function editState(s: ObjectState | null) {
		stateNew = !s;
		stateForm = s
			? { status: s.status, label: s.label, terminal: s.terminal, canonical: s.canonical }
			: { status: '', label: '', terminal: false, canonical: true };
		stateOpen = true;
	}
	async function saveState() {
		const input = { ...stateForm };
		if (await guard(() => api().saveObjectState(object, input), `State ${input.status} saved.`)) {
			stateOpen = false;
			selState = input.status;
			await res.refresh();
		}
	}

	// --- transition editor
	let trOpen = $state(false);
	let trNew = $state(true);
	let trForm = $state<TransitionInput>({ verb: '', from_status: '', to_status: '', guard_rule: null });
	function editTransition(t: ObjectTransition | null, d: ObjectDetail) {
		trNew = !t;
		trForm = t
			? { ...t }
			: {
					verb: `${object}.`,
					from_status: d.states[0]?.status ?? '',
					to_status: d.states[0]?.status ?? '',
					guard_rule: null,
				};
		trOpen = true;
	}
	async function saveTransition() {
		const input = { ...trForm, guard_rule: trForm.guard_rule || null };
		if (
			await guard(
				() => api().saveObjectTransition(object, input),
				`${input.verb}: ${input.from_status} → ${input.to_status} saved. Derived errors updated.`,
			)
		) {
			trOpen = false;
			await res.refresh();
		}
	}
	async function removeTransition(t: ObjectTransition) {
		if (
			await guard(
				() => api().deleteObjectTransition(object, t.verb, t.from_status),
				`Removed ${t.verb} from ${t.from_status}; ${isBackendEvent(t.verb) ? 'backend event' : `invalid_state:${t.from_status} now applies`}.`,
			)
		)
			await res.refresh();
	}
</script>

<PageHead
	id="P10"
	title={`Object · ${object}`}
	sub="A canonical business record the agent reasons about. Its lifecycle decides which operations may run in which status; everything else is refused."
>
	{#snippet actions()}
		<a class="btn" href="/platform/operations?object={object}">Operations on {object}</a>
		<a class="btn" href="/platform/objects">All objects</a>
	{/snippet}
</PageHead>

<LoadState resource={res}>
	{#snippet children(d)}
		{@const ops = d.verbs.length}
		{@const events = new Set(d.transitions.filter((t) => isBackendEvent(t.verb)).map((t) => t.verb)).size}
		<section class="hero" aria-label="{d.label} at a glance">
			<div class="ident">
				<div class="row">
					<h2>{d.label}</h2>
					<Chip text={d.tier} variant={TIER_VARIANT[d.tier]} />
					{#if d.in_retail_job}<Chip text="retail customer-care job" variant="cust" />{/if}
					{#if d.storage_shape_of}<Chip text="stored as {d.storage_shape_of} (D20)" variant="muted" />{/if}
				</div>
				<p class="desc">{d.description}</p>
				<p class="origin muted">Origin: {d.origin}</p>
			</div>
			<ul class="facts" aria-label="Counts">
				<li><b>{d.fields.length}</b> fields<span>{d.fields.filter((f) => f.pii).length} PII</span></li>
				<li>
					<b>{d.states.length}</b> statuses<span>{d.states.filter((s) => s.terminal).length} terminal</span>
				</li>
				<li>
					<b>{ops}</b> agent {ops === 1 ? 'verb' : 'verbs'}<span>{plural(events, 'backend event')}</span>
				</li>
				<li>
					<b>{ctx?.systems.length ?? '—'}</b> systems<span>hold it</span>
				</li>
				<li>
					<b>{ctx?.activities.length ?? '—'}</b> activities<span>touch it</span>
				</li>
			</ul>
		</section>

		{#if d.pending_edits.length}
			<Callout kind="gap" title="Edits reach tenants only via a new cut + apply">
				<span>
					<b>{plural(d.pending_edits.length, 'edit')} since the last tag</b>: {d.pending_edits
						.slice(0, 3)
						.map((e) => `${e.what} (${e.by_name}, ${when(e.at)})`)
						.join('; ')}{d.pending_edits.length > 3 ? '; …' : ''}. Tenants keep the resolved rows of their
					current release until a new cut is applied; the runtime never reads these platform rows.
				</span>
			</Callout>
		{:else}
			<p class="note-line muted">
				Edits reach tenants only via a new cut + apply. No edits since the last tag.
			</p>
		{/if}

		<Card
			title="Lifecycle"
			sub="Solid arrows are operations the agent may run; dashed arrows are what the system of record does. Pick a status or a verb."
		>
			{#snippet actions()}
				<button class="btn sm" type="button" {...superAdminAttrs(session)} onclick={() => editState(null)}
					>Add state</button
				>
				{#if d.states.length}
					<button
						class="btn sm"
						type="button"
						{...superAdminAttrs(session)}
						onclick={() => editTransition(null, d)}>Add transition</button
					>
				{/if}
			{/snippet}
			{#if d.states.length === 0}
				<div class="no-life" data-testid="no-lifecycle">
					<p>
						<b>{d.label} has no lifecycle.</b> It is read or updated in place, so no operation on it can fail
						with
						<span class="mono">invalid_state</span>. Add a status only when an operation must be refused in
						some situation.
					</p>
				</div>
			{:else}
				<div class="toolbar">
					<div class="verbs" role="group" aria-label="Highlight an agent verb">
						<button
							type="button"
							class="vbtn"
							class:on={!selVerb}
							aria-pressed={!selVerb}
							onclick={() => (selVerb = null)}>All</button
						>
						{#each d.verbs as v (v.verb)}
							<button
								type="button"
								class="vbtn mono"
								class:on={selVerb === v.verb}
								aria-pressed={selVerb === v.verb}
								onclick={() => {
									selVerb = selVerb === v.verb ? null : v.verb;
									selState = null;
								}}>{v.verb}</button
							>
						{/each}
					</div>
					<label class="check"><input type="checkbox" bind:checked={showEvents} /> Backend events</label>
				</div>

				<ObjectDetailLifecycle
					def={d}
					{showEvents}
					bind:selectedState={selState}
					bind:selectedVerb={selVerb}
				/>

				<div class="below">
					<div class="inspector" data-testid="lifecycle-inspector" aria-live="polite">
						{#if selVerb}
							{@const v = d.verbs.find((x) => x.verb === selVerb)}
							{@const op = d.operations.find((o) => o.key === selVerb)}
							{@const acts = activitiesFor(ctx, selVerb)}
							{#if v}
								<p class="eyebrow">Agent verb</p>
								<h3 class="mono">{v.verb}</h3>
								{#if op}
									<p class="muted small">{op.description}</p>
									<div class="chips">
										<Chip text={EFFECT_LABEL[op.side_effect]} variant={EFFECT_VARIANT[op.side_effect]} />
										<Chip text="identity {LEVEL_LABEL[op.identity_level]}" variant="muted" />
									</div>
								{/if}
								<dl class="facts-dl">
									<dt>Runs from</dt>
									<dd>
										{#each v.allowed_from as s (s)}
											{@const to = d.transitions.find((t) => t.verb === v.verb && t.from_status === s)}
											<div>
												<b>{labelOf(d, s)}</b> → {to && to.to_status !== s
													? labelOf(d, to.to_status)
													: 'stays'}
												{#if to?.guard_rule}<Chip text="ineligible:{to.guard_rule}" variant="warn" />{/if}
											</div>
										{/each}
									</dd>
									<dt>Refused in</dt>
									<dd>
										<p class="muted small">
											The agent tells the customer why, from the error — it never tries the call.
										</p>
										<div class="chips">
											{#each v.invalid_states as s (s)}<Chip
													text="invalid_state:{s}"
													variant="err"
												/>{:else}<span class="muted">no status — it can always run</span>{/each}
										</div>
									</dd>
									{#if acts.length}
										<dt>Used by</dt>
										<dd>
											{acts.map((a) => `${a.label} (${a.industry_label} · ${a.stage})`).join('; ')}
										</dd>
									{/if}
								</dl>
								<a class="small" href="/platform/operations?q={encodeURIComponent(v.verb)}"
									>Open {v.verb} in the operations catalogue</a
								>
							{/if}
						{:else}
							{@const focus = focusOf(d)}
							{#if focus}
								{@const st = d.states.find((s) => s.status === focus)!}
								{@const f = stateFacts(d, focus)}
								{@const sys = systemStatuses(ctx, focus)}
								<p class="eyebrow">{selState ? 'Status' : 'Starts in'}</p>
								<div class="split">
									<h3>{st.label} <span class="mono muted">{st.status}</span></h3>
									<button
										class="btn sm"
										type="button"
										{...superAdminAttrs(session)}
										onclick={() => editState(st)}>Edit state</button
									>
								</div>
								<div class="chips">
									{#if st.initial}<Chip text="initial" variant="accent" />{/if}
									{#if st.terminal}<Chip text="terminal — no way out" variant="dead" />{/if}
									{#if !st.canonical}<Chip text="connector-only" variant="muted" />{/if}
								</div>
								<dl class="facts-dl">
									<dt>Agent may</dt>
									<dd>
										{#each f.allowed as a (a.verb)}
											<div>
												<span class="mono">{a.verb}</span>
												→ {a.to === focus ? 'stays' : labelOf(d, a.to)}
												{#if a.guard}<Chip text="guard {a.guard}" variant="warn" />{/if}
											</div>
										{:else}
											<span class="muted">nothing — no agent operation changes it from here</span>
										{/each}
									</dd>
									<dt>Refused here</dt>
									<dd class="chips">
										{#each f.refused as r (r)}<Chip
												text="{shortVerb(r)} → invalid_state:{focus}"
												variant="err"
											/>{:else}<span class="muted">no verb is refused</span>{/each}
									</dd>
									<dt>System moves it</dt>
									<dd>
										{#each f.events as e (e.verb)}
											<div>{shortVerb(e.verb)} → {labelOf(d, e.to)}</div>
										{:else}
											<span class="muted"
												>{st.terminal ? 'never — terminal' : 'no backend event from here'}</span
											>
										{/each}
									</dd>
									{#if sys.length}
										<dt>Seen in systems as</dt>
										<dd>
											{#each sys as m (m.system + m.raw)}
												<div class="small"><b>{m.system}</b> <span class="mono">{m.raw}</span></div>
											{/each}
										</dd>
									{/if}
								</dl>
							{/if}
						{/if}
					</div>

					<div class="matrix-wrap">
						<p class="eyebrow">Which verb runs in which status</p>
						{#if d.verbs.length === 0}
							<p class="empty">
								No agent operation changes {d.label}'s status — only backend events move it, so no operation
								on it returns <span class="mono">invalid_state</span>.
							</p>
						{:else}
							<div class="tbl-wrap">
								<table class="matrix" data-testid="verb-errors">
									<caption class="sr-only"
										>Every status without a transition row for the verb yields invalid_state:&lt;status&gt; —
										complete by construction.</caption
									>
									<thead>
										<tr>
											<th scope="col">Verb</th>
											{#each d.states as s (s.status)}
												<th scope="col" class:sel={s.status === focusOf(d) && !selVerb}
													><button
														type="button"
														class="colbtn"
														onclick={() => ((selState = s.status), (selVerb = null))}>{s.label}</button
													></th
												>
											{/each}
										</tr>
									</thead>
									<tbody>
										{#each verbMatrix(d) as row (row.verb)}
											<tr class:sel={row.verb === selVerb}>
												<th scope="row">
													<button
														type="button"
														class="colbtn mono"
														onclick={() => ((selVerb = row.verb), (selState = null))}>{row.verb}</button
													>
													<span class="sr-only">runs from {row.allowed_from.join(', ')}</span>
												</th>
												{#each row.cells as c (c.status)}
													{#if c.to}
														<td class="yes" title="{row.verb} from {c.status} → {c.to}"
															>{c.to === c.status ? '↺' : '→'}<span class="to"
																>{c.to === c.status ? 'stays' : labelOf(d, c.to)}</span
															></td
														>
													{:else}
														<td class="no" title="invalid_state:{c.status}"
															>✕<span class="sr-only">invalid_state:{c.status}</span></td
														>
													{/if}
												{/each}
											</tr>
										{/each}
									</tbody>
								</table>
							</div>
							<p class="muted small">
								✕ = the call is refused with <span class="mono">invalid_state:&lt;status&gt;</span>, derived
								from the missing transition row (complete by construction). Add a transition to allow it.
							</p>
						{/if}
					</div>
				</div>
			{/if}
		</Card>

		<Card
			title="Where {d.label} lives"
			sub="Anchored in templates, held in systems of record, used by journeys."
		>
			<LoadState resource={ctxRes}>
				{#snippet children(c)}
					<ObjectDetailContext ctx={c} label={d.label} />
				{/snippet}
			</LoadState>
		</Card>

		<Card title="Schema and rows">
			<Tabs
				bind:active={tab}
				items={[
					['fields', 'Fields', d.fields.length],
					['relations', 'Relations', d.relations.length],
					['states', 'States & transitions', d.transitions.length],
					['operations', 'Operations', d.operations.length],
				]}
			>
				<div class="pad">
					{#if tab === 'fields'}
						<div class="split">
							<p class="muted">
								PII fields are masked in logs and transcripts and never quoted back by the post-gate.
							</p>
							<button
								class="btn sm"
								type="button"
								{...superAdminAttrs(session)}
								onclick={() => editField(null)}>Add field</button
							>
						</div>
						<div class="tbl-wrap">
							<table class="tbl" data-testid="object-fields">
								<thead
									><tr><th>Field</th><th>Type</th><th>Values / target</th><th>Description</th><th></th></tr
									></thead
								>
								<tbody>
									{#each d.fields as f (f.field)}
										<tr>
											<td>
												<span class="mono">{f.field}</span>
												<span class="flags">
													{#if f.required}<Chip text="required" variant="accent" />{/if}
													{#if f.pii}<Chip text="PII" variant="warn" title="Personal data" />{/if}
												</span>
											</td>
											<td class="mono">{f.data_type}</td>
											<td>
												{#if f.enum_values.length}<span class="mono enum">{f.enum_values.join(' · ')}</span>
												{:else if f.ref}<a class="mono" href="/platform/objects/{f.ref}">→ {f.ref}</a>{/if}
											</td>
											<td class="muted">{f.description}</td>
											<td
												><button
													class="btn sm"
													type="button"
													{...superAdminAttrs(session)}
													onclick={() => editField(f)}>Edit</button
												></td
											>
										</tr>
									{/each}
								</tbody>
							</table>
						</div>
					{:else if tab === 'relations'}
						{#if d.relations.length === 0}
							<p class="empty">No relations declared from this object.</p>
						{:else}
							<ul class="rels" data-testid="object-relations">
								{#each d.relations as r (r.subject + r.predicate + r.object)}
									<li>
										<a class="mono" class:self={r.subject === object} href="/platform/objects/{r.subject}"
											>{r.subject}</a
										>
										<span class="pred">{r.predicate} <span class="mono muted">{r.cardinality}</span></span>
										<a class="mono" class:self={r.object === object} href="/platform/objects/{r.object}"
											>{r.object}</a
										>
									</li>
								{/each}
							</ul>
						{/if}
					{:else if tab === 'states'}
						{#if d.states.length === 0}
							<p class="empty">No states: this object is read or updated in place.</p>
						{:else}
							<div class="grid g2">
								<div>
									<h3>States</h3>
									<table class="tbl dense" data-testid="object-states">
										<thead><tr><th>Status</th><th>Label</th><th></th><th></th></tr></thead>
										<tbody>
											{#each d.states as s (s.status)}
												<tr>
													<td class="mono">{s.status}</td>
													<td>{s.label}</td>
													<td>
														{#if s.initial}<Chip text="initial" variant="accent" />{/if}
														{#if s.terminal}<Chip text="terminal" variant="dead" />{/if}
														{#if !s.canonical}<Chip text="non-canonical" variant="muted" />{/if}
													</td>
													<td
														><button
															class="btn sm"
															type="button"
															{...superAdminAttrs(session)}
															onclick={() => editState(s)}>Edit</button
														></td
													>
												</tr>
											{/each}
										</tbody>
									</table>
								</div>
								<div>
									<h3>Transitions</h3>
									<div class="tbl-wrap">
										<table class="tbl dense" data-testid="object-transitions">
											<thead><tr><th>Verb</th><th>From → to</th><th>Guard</th><th></th></tr></thead>
											<tbody>
												{#each d.transitions as t (t.verb + t.from_status)}
													<tr>
														<td>
															<span class="mono">{t.verb}</span>
															{#if isBackendEvent(t.verb)}<Chip text="backend" variant="muted" />{/if}
														</td>
														<td class="mono">{t.from_status} → {t.to_status}</td>
														<td class="mono">{t.guard_rule ?? ''}</td>
														<td class="nowrap">
															<button
																class="btn sm"
																type="button"
																{...superAdminAttrs(session)}
																onclick={() => editTransition(t, d)}>Edit</button
															>
															<button
																class="btn sm danger"
																type="button"
																{...superAdminAttrs(session)}
																onclick={() => removeTransition(t)}>Remove</button
															>
														</td>
													</tr>
												{/each}
											</tbody>
										</table>
									</div>
								</div>
							</div>
						{/if}
					{:else if d.operations.length === 0}
						<p class="empty">No operations on this object yet.</p>
					{:else}
						<div class="tbl-wrap">
							<table class="tbl" data-testid="object-operations">
								<thead
									><tr
										><th>Operation</th><th>Modality</th><th>Effect</th><th>Id</th><th>Retail job</th><th
											>Errors</th
										></tr
									></thead
								>
								<tbody>
									{#each d.operations as o (o.key)}
										<tr>
											<td
												><a class="mono" href="/platform/operations?q={encodeURIComponent(o.key)}">{o.key}</a
												><span class="cell-sub">{o.description}</span></td
											>
											<td>{o.modality}{o.is_derive ? ' · derive' : ''}</td>
											<td><Chip text={o.side_effect} variant={EFFECT_VARIANT[o.side_effect]} /></td>
											<td class="mono">{o.identity_level}</td>
											<td>{o.retail ?? '—'}</td>
											<td class="muted"
												>{o.error_states
													.filter((e) => e.startsWith('invalid_state') || e.startsWith('ineligible'))
													.join(', ') || '—'}</td
											>
										</tr>
									{/each}
								</tbody>
							</table>
						</div>
					{/if}
				</div>
			</Tabs>
		</Card>

		<Drawer
			bind:open={fieldOpen}
			title={fieldNew ? `Add a field to ${object}` : `Edit ${object}.${fieldForm.field}`}
		>
			<form
				class="stack"
				onsubmit={(e) => {
					e.preventDefault();
					saveField();
				}}
			>
				<div class="form-grid">
					<div class="field">
						<label for="f-name">Field</label>
						<input
							id="f-name"
							class="input mono"
							bind:value={fieldForm.field}
							readonly={!fieldNew}
							required
						/>
					</div>
					<div class="field">
						<label for="f-type">Type</label>
						<select id="f-type" class="input" bind:value={fieldForm.data_type}>
							{#each TYPES as t (t)}<option value={t}>{t}</option>{/each}
						</select>
					</div>
				</div>
				{#if fieldForm.data_type === 'enum'}
					<div class="field">
						<label for="f-enum">Values (comma-separated)</label>
						<input id="f-enum" class="input mono" bind:value={fieldForm.enum_text} />
					</div>
				{/if}
				{#if fieldForm.data_type === 'ref' || fieldForm.data_type === 'list'}
					<div class="field">
						<label for="f-ref">Target object</label>
						<input
							id="f-ref"
							class="input mono"
							value={fieldForm.ref ?? ''}
							oninput={(e) => (fieldForm.ref = e.currentTarget.value)}
						/>
					</div>
				{/if}
				<label class="check"><input type="checkbox" bind:checked={fieldForm.pii} /> Personal data (PII)</label
				>
				<label class="check"><input type="checkbox" bind:checked={fieldForm.required} /> Required</label>
				<div class="field">
					<label for="f-desc">Description</label>
					<input id="f-desc" class="input" bind:value={fieldForm.description} />
				</div>
				<div class="row">
					<button class="btn primary" type="submit" {...superAdminAttrs(session)}>Save field</button>
					<button class="btn" type="button" onclick={() => (fieldOpen = false)}>Cancel</button>
				</div>
			</form>
		</Drawer>

		<Drawer
			bind:open={stateOpen}
			title={stateNew ? `Add a state to ${object}` : `Edit state ${stateForm.status}`}
		>
			<form
				class="stack"
				onsubmit={(e) => {
					e.preventDefault();
					saveState();
				}}
			>
				<div class="form-grid">
					<div class="field">
						<label for="s-status">Status</label>
						<input
							id="s-status"
							class="input mono"
							bind:value={stateForm.status}
							readonly={!stateNew}
							required
						/>
					</div>
					<div class="field">
						<label for="s-label">Label</label>
						<input id="s-label" class="input" bind:value={stateForm.label} />
					</div>
				</div>
				<label class="check"
					><input type="checkbox" bind:checked={stateForm.terminal} /> Terminal (no way out)</label
				>
				<label class="check"
					><input type="checkbox" bind:checked={stateForm.canonical} /> Canonical (connectors map onto it)</label
				>
				{#if stateNew}
					<p class="muted">
						A new state is an <span class="mono">invalid_state</span> error for every verb until you add a transition
						from it.
					</p>
				{/if}
				<div class="row">
					<button class="btn primary" type="submit" {...superAdminAttrs(session)}>Save state</button>
					<button class="btn" type="button" onclick={() => (stateOpen = false)}>Cancel</button>
				</div>
			</form>
		</Drawer>

		<Drawer
			bind:open={trOpen}
			title={trNew ? `Add a transition to ${object}` : `Edit ${trForm.verb} from ${trForm.from_status}`}
		>
			<form
				class="stack"
				onsubmit={(e) => {
					e.preventDefault();
					saveTransition();
				}}
			>
				<div class="field">
					<label for="t-verb">Verb</label>
					<input
						id="t-verb"
						class="input mono"
						bind:value={trForm.verb}
						readonly={!trNew}
						list="t-verbs"
						required
					/>
					<datalist id="t-verbs">
						{#each [...new Set(d.transitions.map((t) => t.verb))] as v (v)}<option value={v}></option>{/each}
					</datalist>
					<span class="muted hint"
						>“{object}.&lt;verb&gt;” for an operation, “backend:&lt;event&gt;” for a system event.</span
					>
				</div>
				<div class="form-grid">
					<div class="field">
						<label for="t-from">From</label>
						<select id="t-from" class="input" bind:value={trForm.from_status} disabled={!trNew}>
							{#each d.states as s (s.status)}<option value={s.status}>{s.status}</option>{/each}
						</select>
					</div>
					<div class="field">
						<label for="t-to">To</label>
						<select id="t-to" class="input" bind:value={trForm.to_status}>
							{#each d.states as s (s.status)}<option value={s.status}>{s.status}</option>{/each}
						</select>
					</div>
				</div>
				<div class="field">
					<label for="t-guard">Guard rule (optional)</label>
					<input
						id="t-guard"
						class="input mono"
						value={trForm.guard_rule ?? ''}
						oninput={(e) => (trForm.guard_rule = e.currentTarget.value)}
						placeholder="PAY-04"
					/>
				</div>
				<div class="row">
					<button class="btn primary" type="submit" {...superAdminAttrs(session)}>Save transition</button>
					<button class="btn" type="button" onclick={() => (trOpen = false)}>Cancel</button>
				</div>
			</form>
		</Drawer>
	{/snippet}
</LoadState>

<style>
	.hero {
		display: grid;
		grid-template-columns: minmax(0, 1.4fr) minmax(0, 1fr);
		gap: 20px;
		align-items: center;
		padding: 16px 18px;
		border: 1px solid var(--line);
		border-radius: var(--r);
		background: var(--panel);
	}
	.ident h2 {
		margin: 0;
		font: 600 20px/1.2 var(--cond);
	}
	.desc {
		margin: 6px 0 2px;
		max-width: 70ch;
	}
	.origin {
		margin: 0;
		font-size: 12px;
	}
	.facts {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(92px, 1fr));
		gap: 8px;
	}
	.facts li {
		display: grid;
		padding: 8px 10px;
		border-radius: 6px;
		background: var(--sunk);
		font-size: 12.5px;
		color: var(--muted);
	}
	.facts b {
		font: 600 20px/1.1 var(--cond);
		color: var(--fg);
	}
	.facts span {
		font-size: 11.5px;
		color: var(--faint);
	}
	.note-line {
		margin: 0;
		font-size: 12.5px;
	}
	.toolbar {
		display: flex;
		justify-content: space-between;
		gap: 10px;
		align-items: center;
		flex-wrap: wrap;
		margin-bottom: 10px;
	}
	.verbs {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
	}
	.vbtn {
		border: 1px solid var(--line);
		background: var(--panel);
		border-radius: 999px;
		padding: 3px 10px;
		font-size: 12px;
		cursor: pointer;
	}
	.vbtn.on {
		border-color: var(--accent);
		background: color-mix(in srgb, var(--accent) 13%, var(--panel));
		color: var(--accent);
	}
	.below {
		display: grid;
		grid-template-columns: minmax(260px, 1fr) minmax(0, 2fr);
		gap: 20px;
		margin-top: 14px;
	}
	.inspector {
		display: grid;
		gap: 8px;
		align-content: start;
		padding: 12px 14px;
		border-radius: 8px;
		border: 1px solid var(--line-2);
		border-left: 3px solid var(--accent);
	}
	.inspector h3 {
		margin: 0;
		font: 600 17px/1.2 var(--cond);
		letter-spacing: 0;
		text-transform: none;
		color: var(--fg);
	}
	.eyebrow {
		margin: 0;
		font: 600 11px/1.2 var(--cond);
		letter-spacing: 0.06em;
		text-transform: uppercase;
		color: var(--faint);
	}
	.facts-dl {
		margin: 0;
		display: grid;
		grid-template-columns: max-content minmax(0, 1fr);
		gap: 8px 12px;
		font-size: 13px;
	}
	.facts-dl dt {
		color: var(--muted);
		font-size: 12px;
		padding-top: 2px;
	}
	.facts-dl dd {
		margin: 0;
		display: grid;
		gap: 4px;
	}
	.facts-dl dd.chips {
		display: flex;
	}
	.small {
		font-size: 12px;
		margin: 0;
	}
	.matrix-wrap {
		display: grid;
		gap: 8px;
		align-content: start;
	}
	.matrix {
		border-collapse: separate;
		border-spacing: 3px;
		font-size: 12px;
		width: 100%;
	}
	.matrix th {
		font-weight: 500;
		color: var(--muted);
		text-align: center;
		padding: 2px 4px;
		white-space: nowrap;
	}
	.matrix th[scope='row'] {
		text-align: left;
	}
	.matrix thead th.sel,
	.matrix tr.sel th {
		color: var(--accent);
	}
	.colbtn {
		background: none;
		border: 0;
		padding: 0;
		cursor: pointer;
		color: inherit;
		font: inherit;
	}
	.colbtn:hover {
		color: var(--accent);
	}
	.matrix td {
		text-align: center;
		padding: 6px 4px;
		border-radius: 4px;
		min-width: 64px;
	}
	.matrix td.yes {
		background: color-mix(in srgb, var(--ok) 14%, transparent);
		color: var(--ok);
		font-weight: 600;
	}
	.matrix td.no {
		background: color-mix(in srgb, var(--err) 7%, transparent);
		color: color-mix(in srgb, var(--err) 70%, var(--muted));
	}
	.matrix tr.sel td {
		outline: 1px solid color-mix(in srgb, var(--accent) 40%, transparent);
	}
	.to {
		display: block;
		font-weight: 400;
		font-size: 11px;
	}
	.no-life {
		padding: 14px;
		border: 1px dashed var(--line);
		border-radius: 8px;
		background: var(--sunk);
	}
	.no-life p {
		margin: 0;
	}
	.pad {
		padding-top: 12px;
		display: grid;
		gap: 12px;
	}
	.flags {
		display: inline-flex;
		gap: 4px;
		margin-left: 6px;
		vertical-align: middle;
	}
	.rels {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 6px;
	}
	.rels li {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 8px;
	}
	.rels a.self {
		font-weight: 700;
		color: var(--fg);
	}
	.pred {
		padding: 2px 10px;
		border-radius: 999px;
		background: var(--sunk);
		font-size: 12.5px;
	}
	.check {
		display: inline-flex;
		gap: 6px;
		align-items: center;
		font-size: 13px;
	}
	.enum {
		font-size: 11.5px;
	}
	.nowrap {
		white-space: nowrap;
	}
	.hint {
		font-size: 12px;
	}
	h3 {
		margin: 0 0 6px;
		font: 600 13px/1.2 var(--cond);
		letter-spacing: 0.04em;
		text-transform: uppercase;
		color: var(--muted);
	}
	.sr-only {
		position: absolute;
		width: 1px;
		height: 1px;
		overflow: hidden;
		clip-path: inset(50%);
		white-space: nowrap;
	}
	@media (max-width: 1000px) {
		.hero,
		.below {
			grid-template-columns: minmax(0, 1fr);
		}
	}
</style>
