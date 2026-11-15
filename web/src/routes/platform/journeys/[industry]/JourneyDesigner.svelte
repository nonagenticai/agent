<script lang="ts">
	// P8 Journey designer: the owner's grid per industry (Themes → Stages → Activities → Messages per channel →
	// Business objects, D23, 11§4.3.1). Edits stage descriptions/synonyms, theme→stage priors, activities and their
	// object bindings; every empty in-scope cell is validation red (rule 4, 11§7).
	import { onMount } from 'svelte';
	import { page } from '$app/state';
	import { goto } from '$app/navigation';
	import { api, guard } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import type { Actor, IndustryJourney, JourneyStage, StageKey, ThemeKey } from '$lib/api/types/journey';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import MatrixTable from '$lib/ui/MatrixTable.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';
	import JourneyGrid, { type GridActivity } from '$lib/journey/JourneyGrid.svelte';
	import TemplateEditors from './TemplateEditors.svelte';
	import ActivityEditors from './ActivityEditors.svelte';
	import { OBJECT_FIELDS, OPS, STAGE_KEYS, STAGE_LABEL, objectKey, opLabel } from '$lib/journey/catalogue';
	import {
		derivedOutputs,
		priorRowSum,
		ruleChecklist,
		stageLoad,
		type RuleStatus,
	} from '$lib/journey/P8JourneyDesigner';

	let { industry }: { industry: string } = $props();
	const department = $derived(page.url.searchParams.get('department') ?? '');
	const selectedDepartment = () =>
		department || (res.value.state === 'ok' ? res.value.data.department : undefined);
	const journeyHref = (i: { industry: string; department?: string | null }) =>
		`/platform/journeys/${encodeURIComponent(i.industry)}${i.department ? `?department=${encodeURIComponent(i.department)}` : ''}`;

	const res = new Resource(
		async () => {
			let dept = department;
			if (!dept) {
				const grids = await api().journeyIndustries();
				dept = grids.items.find((i) => i.industry === industry && i.department)?.department ?? '';
			}
			return api().industryJourney(industry, dept || undefined);
		},
		() => false,
	);
	const list = new Resource(() => api().journeyIndustries());
	onMount(() => {
		res.refresh();
		list.refresh();
	});

	const STATUS_CHIP: Record<string, string> = {
		tagged: 'ok',
		partial: 'warn',
		draft: 'work',
		not_started: 'muted',
	};

	// ---- stage drawer ----
	let stageOpen = $state(false);
	let stage = $state<JourneyStage | null>(null);
	let sLabel = $state('');
	let sDesc = $state('');
	let sSyn = $state('');
	let sOos = $state(false);
	function openStage(s: JourneyStage) {
		stage = s;
		sLabel = s.label;
		sDesc = s.description;
		sSyn = s.synonyms.join(', ');
		sOos = s.out_of_scope;
		stageOpen = true;
	}
	async function saveStage() {
		if (!stage) return;
		const s = stage;
		const ok = await guard(
			() =>
				api().updateJourneyStage(
					industry,
					s.key,
					{
						label: sLabel,
						description: sDesc,
						synonyms: sSyn.split(','),
						out_of_scope: sOos,
					},
					selectedDepartment(),
				),
			`Stage ${s.label} saved (draft revision).`,
		);
		if (ok) {
			stageOpen = false;
			await res.refresh();
		}
	}

	// ---- activity drawer ----
	let actOpen = $state(false);
	let act = $state<GridActivity | null>(null);
	let moveTo = $state<StageKey>('orientation');
	let bObject = $state('order');
	let bRole = $state<'subject' | 'context'>('context');
	let bFields = $state<string[]>([]);
	let bInputs = $state('');
	// The object's real fields (GET /api/platform/objects/{object}); the bundled catalogue is only the fallback, and
	// a field the API does not know is refused 422 `unknown_field`.
	let liveFields = $state<Record<string, string[]>>({});
	let fieldsFor = $derived(liveFields[objectKey(bObject)] ?? OBJECT_FIELDS[objectKey(bObject)] ?? []);
	$effect(() => {
		const k = objectKey(bObject);
		if (!actOpen || liveFields[k]) return;
		api()
			.objectDetail(k)
			.then((d) => (liveFields[k] = d.fields.map((f) => f.field)))
			.catch(() => {});
	});
	function openActivity(a: GridActivity) {
		act = a;
		moveTo = STAGE_KEYS.find((k) => k !== a.stage) ?? 'orientation';
		bObject = 'order';
		bRole = 'context';
		bFields = [];
		bInputs = '';
		actOpen = true;
	}
	async function afterAct(key: string) {
		await res.refresh();
		const v = res.value;
		if (v.state === 'ok') act = v.data.activities.find((x) => x.key === key) ?? null;
	}
	async function move() {
		if (!act) return;
		const a = act;
		const next = `${moveTo}:${a.key.split(':')[1]}`;
		if (
			await guard(
				() => api().moveJourneyActivity(industry, a.key, moveTo, selectedDepartment()),
				`Moved to ${STAGE_LABEL[moveTo]}.`,
			)
		)
			await afterAct(next);
	}
	async function bind() {
		if (!act) return;
		const a = act;
		const ok = await guard(
			() =>
				api().bindActivityObject(
					industry,
					a.key,
					{
						object: bObject,
						role: bRole,
						fields: bFields,
						required_inputs: bInputs.split(/[\s,]+/).filter(Boolean),
					},
					selectedDepartment(),
				),
			`${bObject} bound to ${a.label}.`,
		);
		if (ok) {
			bFields = [];
			bInputs = '';
			await afterAct(a.key);
		}
	}
	async function unbind(object: string) {
		if (!act) return;
		const a = act;
		if (
			await guard(
				() => api().unbindActivityObject(industry, a.key, object, selectedDepartment()),
				`${object} unbound.`,
			)
		)
			await afterAct(a.key);
	}

	// ---- add activity drawer ----
	let addOpen = $state(false);
	let nStage = $state<StageKey>('orientation');
	let nSlug = $state('');
	let nLabel = $state('');
	let nActor = $state<Actor>('customer');
	let nTrigger = $state('');
	function openAdd(s: JourneyStage) {
		nStage = s.key;
		nSlug = '';
		nLabel = '';
		nActor = 'customer';
		nTrigger = '';
		addOpen = true;
	}
	async function add() {
		const ok = await guard(
			() =>
				api().addJourneyActivity(
					industry,
					{
						stage: nStage,
						slug: nSlug,
						label: nLabel,
						actor: nActor,
						trigger: nTrigger,
					},
					selectedDepartment(),
				),
			`Activity ${nStage}:${nSlug} added.`,
		);
		if (ok) {
			addOpen = false;
			await res.refresh();
		}
	}

	// ---- priors ----
	function priorOf(j: IndustryJourney, t: ThemeKey, s: StageKey) {
		return j.priors.find((p) => p.theme === t && p.stage === s)?.weight ?? 0;
	}
	async function setPrior(t: ThemeKey, s: StageKey, e: Event) {
		const el = e.currentTarget as HTMLInputElement;
		const w = Number(el.value);
		const ok = await guard(
			() => api().setThemeStagePrior(industry, t, s, w, selectedDepartment()),
			w === 0 ? 'Prior removed.' : `Prior ${t} → ${s} = ${w}.`,
		);
		await res.refresh();
		if (!ok) el.focus();
	}
	const RULE_CHIP: Record<RuleStatus, [string, string]> = {
		pass: ['Pass', 'ok'],
		red: ['Red', 'err'],
		amber: ['Advisory', 'warn'],
		package: ['At Package', 'muted'],
		optional: ['Optional', 'muted'],
	};
	const RULE_MARK: Record<RuleStatus, string> = {
		pass: '✓',
		red: '✕',
		amber: '!',
		package: '○',
		optional: '○',
	};
	/** `industry/retail@v2` → `industry/retail@v3`; untagged → `industry/<industry>@v1`. */
	function nextTagOf(tag: string | null, industry: string) {
		const m = tag?.match(/^(.*)@v(\d+)$/);
		return m ? `${m[1]}@v${Number(m[2]) + 1}` : `industry/${industry}@v1`;
	}
</script>

<PageHead
	id="P8"
	sub="An industry's customer journey: Themes → Stages → Activities → Messages per channel → Business objects. The backbone every job of the industry is derived from."
>
	{#snippet actions()}
		{#if list.value.state === 'ok'}
			<label class="field inline">
				<span class="label">Industry / department</span>
				<select
					class="input"
					value={journeyHref({ industry, department: selectedDepartment() })}
					onchange={(e) => goto((e.currentTarget as HTMLSelectElement).value)}
				>
					{#each list.value.data.items as i (`${i.industry}:${i.department ?? ''}`)}
						<option value={journeyHref(i)}
							>{i.label}{i.department
								? ` / ${i.department_label ?? i.department.replaceAll('_', ' ')}`
								: ''}{i.status === 'not_started' ? ' — not started' : ''}{i.red
								? ` — ${i.red} red`
								: ''}</option
						>
					{/each}
				</select>
			</label>
		{/if}
	{/snippet}
</PageHead>

{#if res.value.state === 'unavailable' && res.value.status === 404}
	{@const known =
		list.value.state === 'ok' ? list.value.data.items.find((i) => i.industry === industry) : undefined}
	<Card title={known?.label ?? industry}>
		<div class="empty" data-testid="no-journey">
			<p>No journey grid for <b>{known?.label ?? industry}</b> yet.</p>
			<p class="muted">
				A grid is drafted by a platform generator run (Input → Business model → Journey + activities …) and
				tagged as
				<code>industry/{industry}@v1</code>.
			</p>
			<a class="btn primary" href="/platform/generator">Start a generator run</a>
		</div>
	</Card>
	{#if list.value.state === 'ok'}
		<Card title="Industries">
			<table class="tbl">
				<thead
					><tr
						><th>Industry</th><th>Status</th><th>Tag</th><th class="r">Activities</th><th class="r">Red</th
						></tr
					></thead
				>
				<tbody>
					{#each list.value.data.items as i (`${i.industry}:${i.department ?? ''}`)}
						<tr>
							<td
								><a href={journeyHref(i)}
									>{i.label}{i.department
										? ` / ${i.department_label ?? i.department.replaceAll('_', ' ')}`
										: ''}</a
								></td
							>
							<td><span class="chip {STATUS_CHIP[i.status]}">{i.status.replace('_', ' ')}</span></td>
							<td class="mono">{i.tag ?? '—'}</td>
							<td class="r">{i.activities}</td>
							<td class="r">{i.red || '—'}</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</Card>
	{/if}
{:else if res.value.state === 'unavailable'}
	<Unavailable reason={res.value.reason} retry={res.retry} />
{:else}
	<LoadState resource={res}>
		{#snippet children(j)}
			{@const red = j.findings.filter((f) => f.severity === 'red')}
			{@const amber = j.findings.filter((f) => f.severity !== 'red')}
			{@const outputs = derivedOutputs(j)}
			{@const rules = ruleChecklist(j.findings)}
			{@const load = stageLoad(j)}
			{@const maxOps = Math.max(1, ...load.map((l) => l.operations))}
			{@const nextTag = nextTagOf(j.tag, j.industry)}
			<div class="stack">
				<Card>
					<div class="ident">
						<div class="row">
							<b class="title">{j.label} · {j.department.replaceAll('_', ' ')}</b>
							<span class="chip {STATUS_CHIP[j.status]}">{j.status}</span>
							{#if j.tag}<span class="chip ok mono">{j.tag}</span>{/if}
							{#if j.dept_tag}<span class="chip ok mono">{j.dept_tag}</span>{/if}
							{#if j.draft_rev}<span
									class="chip warn"
									title="Edits since the tag; a new tag needs a generator Package step"
									>draft rev {j.draft_rev}</span
								>{/if}
						</div>
						<p class="lede">
							The customer journey of this industry, designed once. Every job of the industry derives its gate
							options, operation catalogue, knowledge requirements and use cases from these cells at the
							release cut — tenants only refine it (hide, extend, relabel).
						</p>
						<div class="row muted small">
							{j.activities.length} activities · updated {j.updated_at.slice(0, 16).replace('T', ' ')} by {j.updated_by}
							{#if j.source_run}· drafted by <a href="/platform/generator/{j.source_run}"
									>generator run #{j.source_run}</a
								>{/if}
						</div>
					</div>
				</Card>

				<div class="top">
					<Card
						title="What this grid generates"
						sub="Grid rows on the left, what every job of the industry gets on the right."
					>
						<ol class="derive" data-testid="derived">
							{#each outputs as o (o.key)}
								<li class="d-row">
									<span class="d-from">{o.from}</span>
									<span class="d-count"><b>{o.count}</b><small>{o.detail}</small></span>
									<span class="d-arrow" aria-hidden="true">→</span>
									<span class="d-to"
										>{#if o.href}<a href={o.href}>{o.becomes}</a>{:else}{o.becomes}{/if}</span
									>
								</li>
							{/each}
						</ol>
					</Card>

					<Card title="Can this be tagged?" sub="11§7 validation rules. Any red item blocks the tag.">
						<div class="verdict {red.length ? 'bad' : 'good'}" data-testid="verdict">
							{#if red.length}
								<b>Tagging blocked</b>
								<span
									>{red.length} red item{red.length === 1 ? '' : 's'} — fill the cells below, or mark the stage
									out of scope.</span
								>
							{:else}
								<b>Grid complete</b>
								<span
									>{#if j.draft_rev}Draft rev {j.draft_rev} is ready to tag as
										<code>{nextTag}</code>.{:else if j.tag}No edits since <code>{j.tag}</code>; jobs pinned to
										it are current.{:else}Ready for its first tag
										<code>{nextTag}</code>.{/if}</span
								>
							{/if}
						</div>
						<ul class="rules" data-testid="findings">
							{#each rules as r (r.rule)}
								<li class="rule {r.status}">
									<span class="r-mark" aria-hidden="true">{RULE_MARK[r.status]}</span>
									<span class="r-body">
										<span class="r-title"
											><span class="mono muted">{r.rule}</span>
											{r.title}
											<span class="chip {RULE_CHIP[r.status][1]}">{RULE_CHIP[r.status][0]}</span></span
										>
										{#each r.findings.filter((f) => f.severity === 'red') as f, fi (`${fi}:${f.message}`)}
											<span class="r-find">
												{f.message}
												{#if f.stage}
													{@const st = j.stages.find((s) => s.key === f.stage)}
													{#if st}<button type="button" class="btn sm" onclick={() => openStage(st)}
															>Edit stage</button
														>{/if}
												{/if}
											</span>
										{/each}
									</span>
								</li>
							{/each}
						</ul>
						{#if amber.length}
							<p class="muted small">Advisory: {amber.map((f) => f.message).join(' · ')}</p>
						{/if}
						<div class="row">
							{#if red.length}
								<span class="btn" aria-disabled="true">Resolve red items to package</span>
							{:else if j.draft_rev || !j.tag}
								<a class="btn primary" href="/platform/generator">Package as {nextTag}</a>
							{:else}
								<span class="btn" aria-disabled="true" title="No edits since {j.tag}">Nothing to package</span
								>
							{/if}
							<a class="btn" href="/platform/templates">Template tags</a>
						</div>
						<p class="muted small">
							Tags are cut by a generator run's Package step (D15); jobs pin a tag, never the draft.
						</p>
					</Card>
				</div>

				<Card
					title="Journey grid"
					sub="Each stage column is a capability. Read a column top to bottom: who does what, what they say, which objects it touches, what the store must know. Click a stage header to edit it, an activity to bind objects."
				>
					<div class="load" aria-label="Operations per stage">
						{#each load as l, i (l.key)}
							{@const st = j.stages[i]}
							<button
								type="button"
								class="ld"
								class:oos={l.out_of_scope}
								class:red={l.red}
								onclick={() => st && openStage(st)}
								aria-label="Stage {l.label}: {l.activities} activities, {l.operations} operations, {l.required} required"
							>
								<span class="ld-name">{l.label}</span>
								<span class="ld-bar" aria-hidden="true"
									><i style="width:{(l.operations / maxOps) * 100}%"></i><i
										class="req"
										style="width:{(l.required / maxOps) * 100}%"
									></i></span
								>
								<span class="ld-num"
									>{#if l.out_of_scope}out of scope{:else}{l.activities} act · {l.operations} ops ({l.required}
										R){#if l.red}&nbsp;· red{/if}{/if}</span
								>
							</button>
						{/each}
					</div>
					<JourneyGrid
						stages={j.stages}
						themes={j.themes}
						channels={j.channels}
						activities={j.activities}
						messages={j.messages}
						kinds={j.knowledge_kinds}
						findings={j.findings}
						onStage={openStage}
						onActivity={openActivity}
						onAdd={openAdd}
					/>
					<p class="note small">
						<b>Change / Exit labels are owner-vetoable</b> — the slide leaves both lanes unlabelled; D23 names them
						(K48). Stage synonyms feed the GS option text and import mapping.
					</p>
				</Card>

				<TemplateEditors {j} {industry} department={selectedDepartment()} onChanged={() => res.refresh()} />

				<Card
					title="Theme → stage priors"
					sub="Where does a customer in each theme usually stand? Each row is a distribution over stages (sums to 1) and is the GS gate's prior (11§4.3.1). Darker = more likely; 0 removes the row."
				>
					<div class="priors">
						<MatrixTable
							rows={j.themes}
							columns={j.stages}
							rowKey={(t) => t.key}
							colKey={(s) => s.key}
							corner="Theme"
							caption="Theme to stage priors"
						>
							{#snippet rowHeader(t)}
								{@const sum = priorRowSum(j, t.key)}
								<span class="cell-title">{t.label}</span><span class="cell-sub"
									>escalation {t.escalation_weight} ·
									<span
										class={sum === 1 ? '' : 's-warn'}
										title={sum === 1 ? undefined : 'Row should sum to 1'}>Σ {sum}</span
									></span
								>
							{/snippet}
							{#snippet colHeader(s)}{s.label}{/snippet}
							{#snippet cell(t, s)}
								{@const w = priorOf(j, t.key, s.key)}
								<input
									class="input prior"
									type="number"
									min="0"
									max="1"
									step="0.05"
									aria-label="Prior {t.label} → {s.label}"
									value={w}
									style="--w:{w}"
									class:zero={w === 0}
									onchange={(e) => setPrior(t.key, s.key, e)}
								/>
							{/snippet}
						</MatrixTable>
					</div>
				</Card>
			</div>

			<Drawer bind:open={stageOpen} title="Stage · {stage?.label ?? ''}">
				{#if stage}
					<div class="stack">
						<p class="muted">
							Order {stage.order} · themes {stage.themes.join(' → ')}. The description is the GS option text
							the pre-gate classifies against.
						</p>
						<div class="field">
							<label for="st-label">Label</label>
							<input id="st-label" class="input" bind:value={sLabel} />
						</div>
						<div class="field">
							<label for="st-desc">Description</label>
							<textarea id="st-desc" class="input" rows="4" bind:value={sDesc}></textarea>
						</div>
						<div class="field">
							<label for="st-syn">Synonyms (comma separated)</label>
							<input id="st-syn" class="input" bind:value={sSyn} />
						</div>
						<label class="row"
							><input type="checkbox" bind:checked={sOos} /> Out of scope for this industry (rule 4 passes without
							activities)</label
						>
						{#if stage.key === 'change' || stage.key === 'exit'}
							<p class="note">The label “{stage.label}” is owner-vetoable (K48, one word).</p>
						{/if}
						<div class="row">
							<button class="btn primary" type="button" onclick={saveStage}>Save stage</button>
							<button class="btn" type="button" onclick={() => (stageOpen = false)}>Cancel</button>
						</div>
					</div>
				{/if}
			</Drawer>

			<Drawer bind:open={actOpen} title={act?.label ?? 'Activity'} wide>
				{#snippet sub()}<span class="mono">{act?.key}</span>{/snippet}
				{#if act}
					<div class="stack">
						<p>
							<span class="chip {act.actor === 'company' ? 'work' : 'cust'}">{act.actor}</span>
							<span class="muted">Trigger: {act.trigger || '—'}</span>
							{#if act.description}<span data-testid="act-description"> · {act.description}</span>{/if}
							{#if act.also_in_stages.length}<span class="muted">
									· also in {act.also_in_stages.join(', ')}</span
								>{/if}
						</p>
						<h3>Operations</h3>
						{#if act.operations.length}
							<div class="chips">
								{#each act.operations as o (opLabel(o))}
									<span
										class="chip {o.required ? 'accent' : 'muted'}"
										title="{OPS[o.op]?.modality} · {OPS[o.op]?.effect} · level {OPS[o.op]?.id}"
										>{opLabel(o)} {o.required ? 'R' : 'O'}</span
									>
								{/each}
							</div>
						{:else}
							<p class="muted">No runtime operations (company-side content activity).</p>
						{/if}
						<h3>Business objects</h3>
						<table class="tbl dense">
							<thead
								><tr><th>Object</th><th>Role</th><th>Field subset</th><th>Required inputs</th><th></th></tr
								></thead
							>
							<tbody>
								{#each act.objects as o (o.object)}
									<tr>
										<td class="mono">{o.object}</td>
										<td>{o.role}</td>
										<td>{o.fields.join(', ')}</td>
										<td>{o.required_inputs.join(', ') || '—'}</td>
										<td
											><button class="btn sm" type="button" onclick={() => unbind(o.object)}>Unbind</button
											></td
										>
									</tr>
								{:else}
									<tr
										><td colspan="5" class="muted">No objects bound — the stage’s objects cell turns red.</td
										></tr
									>
								{/each}
							</tbody>
						</table>
						<fieldset class="editor-fields">
							<legend>Bind an object</legend>
							<div class="form-grid">
								<div class="field">
									<label for="b-obj">Object</label>
									<select id="b-obj" class="input" bind:value={bObject} onchange={() => (bFields = [])}>
										{#each Object.keys(OBJECT_FIELDS) as k (k)}<option value={k}>{k}</option>{/each}
									</select>
								</div>
								<div class="field">
									<label for="b-role">Role</label>
									<select id="b-role" class="input" bind:value={bRole}>
										<option value="subject">subject</option>
										<option value="context">context</option>
									</select>
								</div>
							</div>
							<div class="field">
								<span class="label">Field subset</span>
								<div class="chips">
									{#each fieldsFor as f (f)}
										<label class="chip muted"
											><input type="checkbox" value={f} bind:group={bFields} /> {f}</label
										>
									{/each}
								</div>
							</div>
							<div class="field">
								<label for="b-in">Required inputs (slots)</label>
								<input
									id="b-in"
									class="input"
									placeholder="verified_identity verified_at"
									bind:value={bInputs}
								/>
							</div>
							<button class="btn" type="button" onclick={bind}>Bind {bObject}</button>
						</fieldset>
						<ActivityEditors
							{act}
							{j}
							{industry}
							department={selectedDepartment()}
							onChanged={afterAct}
							onRetired={async () => {
								actOpen = false;
								act = null;
								await res.refresh();
							}}
						/>
						<fieldset class="editor-fields">
							<legend>Move to another stage</legend>
							<div class="row">
								<select class="input" bind:value={moveTo} aria-label="Target stage">
									{#each STAGE_KEYS.filter((k) => k !== act?.stage) as k (k)}<option value={k}
											>{STAGE_LABEL[k]}</option
										>{/each}
								</select>
								<button class="btn" type="button" onclick={move}>Move</button>
							</div>
						</fieldset>
					</div>
				{/if}
			</Drawer>

			<Drawer bind:open={addOpen} title="Add activity · {STAGE_LABEL[nStage]}">
				<div class="stack">
					<div class="form-grid">
						<div class="field">
							<label for="n-stage">Stage</label>
							<select id="n-stage" class="input" bind:value={nStage}>
								{#each STAGE_KEYS as k (k)}<option value={k}>{STAGE_LABEL[k]}</option>{/each}
							</select>
						</div>
						<div class="field">
							<label for="n-actor">Actor</label>
							<select id="n-actor" class="input" bind:value={nActor}>
								<option value="customer">customer</option>
								<option value="company">company</option>
								<option value="employee">employee</option>
							</select>
						</div>
					</div>
					<div class="field">
						<label for="n-slug">Key</label>
						<div class="row">
							<span class="mono muted">{nStage}:</span><input
								id="n-slug"
								class="input"
								placeholder="change_tariff"
								bind:value={nSlug}
							/>
						</div>
					</div>
					<div class="field">
						<label for="n-label">Label</label>
						<input id="n-label" class="input" bind:value={nLabel} />
					</div>
					<div class="field">
						<label for="n-trig">Trigger</label>
						<input
							id="n-trig"
							class="input"
							placeholder="“switch me to a fixed tariff”"
							bind:value={nTrigger}
						/>
					</div>
					<div class="row">
						<button class="btn primary" type="button" onclick={add}>Add activity</button>
						<button class="btn" type="button" onclick={() => (addOpen = false)}>Cancel</button>
					</div>
				</div>
			</Drawer>
		{/snippet}
	</LoadState>
{/if}

<style>
	.title {
		font: 600 16px var(--cond);
	}
	.small {
		font-size: 12.5px;
	}
	.inline {
		min-width: 220px;
	}
	.ident {
		display: grid;
		gap: 6px;
	}
	.lede {
		margin: 0;
		max-width: 78ch;
		color: var(--muted);
	}
	.top {
		display: grid;
		grid-template-columns: minmax(0, 1.25fr) minmax(0, 1fr);
		gap: 14px;
		align-items: start;
	}
	@media (max-width: 1100px) {
		.top {
			grid-template-columns: minmax(0, 1fr);
		}
	}
	.derive {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
	}
	.d-row {
		display: grid;
		grid-template-columns: 9.5em 7.5em 1.2em minmax(0, 1fr);
		gap: 8px;
		align-items: center;
		padding: 8px 0;
		border-bottom: 1px solid var(--line-2);
	}
	.d-row:last-child {
		border-bottom: 0;
	}
	.d-from {
		font: 600 12px var(--cond);
		text-transform: uppercase;
		letter-spacing: 0.04em;
		color: var(--muted);
	}
	.d-count {
		display: grid;
		line-height: 1.15;
	}
	.d-count b {
		font: 600 20px var(--cond);
		color: var(--fg);
	}
	.d-count small {
		color: var(--faint);
		font-size: 11.5px;
	}
	.d-arrow {
		color: var(--faint);
	}
	.d-to {
		font-size: 13px;
	}
	@media (max-width: 560px) {
		.d-row {
			grid-template-columns: minmax(0, 1fr) auto;
		}
		.d-arrow {
			display: none;
		}
		.d-to {
			grid-column: 1 / -1;
		}
	}
	.verdict {
		display: grid;
		gap: 2px;
		padding: 10px 12px;
		border-radius: var(--r);
		border-left: 4px solid;
		margin-bottom: 10px;
	}
	.verdict.good {
		border-color: var(--ok);
		background: color-mix(in srgb, var(--ok) 9%, var(--panel));
	}
	.verdict.bad {
		border-color: var(--err);
		background: color-mix(in srgb, var(--err) 9%, var(--panel));
	}
	.verdict b {
		font: 600 15px var(--cond);
	}
	.rules {
		list-style: none;
		margin: 0 0 10px;
		padding: 0;
		display: grid;
		gap: 6px;
	}
	.rule {
		display: grid;
		grid-template-columns: 1.4em minmax(0, 1fr);
		gap: 6px;
		align-items: start;
		font-size: 13px;
	}
	.r-mark {
		font-weight: 700;
		text-align: center;
	}
	.rule.pass .r-mark {
		color: var(--ok);
	}
	.rule.red .r-mark {
		color: var(--err);
	}
	.rule.amber .r-mark {
		color: var(--warn);
	}
	.rule.package .r-mark,
	.rule.optional .r-mark {
		color: var(--faint);
	}
	.r-body,
	.r-title {
		display: flex;
		flex-wrap: wrap;
		gap: 4px 6px;
		align-items: center;
	}
	.r-body {
		flex-direction: column;
		align-items: flex-start;
	}
	.r-find {
		display: flex;
		gap: 6px;
		align-items: center;
		flex-wrap: wrap;
		color: var(--err);
	}
	.btn[aria-disabled='true'] {
		opacity: 0.6;
		cursor: not-allowed;
	}
	.load {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(118px, 1fr));
		gap: 6px;
		margin-bottom: 12px;
	}
	.ld {
		display: grid;
		gap: 4px;
		text-align: left;
		padding: 6px 8px;
		border: 1px solid var(--line-2);
		border-radius: var(--r);
		background: var(--sunk);
		cursor: pointer;
	}
	.ld:hover {
		border-color: var(--accent);
	}
	.ld.red {
		border-color: var(--err);
	}
	.ld.oos {
		opacity: 0.6;
	}
	.ld-name {
		font: 600 12.5px var(--cond);
	}
	.ld-bar {
		position: relative;
		height: 6px;
		border-radius: 3px;
		background: var(--line-2);
		overflow: hidden;
	}
	.ld-bar i {
		position: absolute;
		inset: 0 auto 0 0;
		background: color-mix(in srgb, var(--accent) 40%, transparent);
	}
	.ld-bar i.req {
		background: var(--accent);
	}
	.ld-num {
		font: 11.5px var(--mono);
		color: var(--muted);
	}
	.ld.red .ld-num {
		color: var(--err);
	}
	.note {
		margin: 10px 0 0;
		color: var(--muted);
	}
	.priors :global(.matrix tbody th) {
		border-bottom-color: var(--line-2);
	}
	.priors :global(.matrix tbody tr:last-child th) {
		border-bottom: 0;
	}
	.priors :global(.matrix-wrap::-webkit-scrollbar) {
		height: 10px;
	}
	.priors :global(.matrix-wrap::-webkit-scrollbar-track) {
		background: var(--sunk);
		border-radius: 5px;
	}
	.priors :global(.matrix-wrap::-webkit-scrollbar-thumb) {
		background: var(--faint);
		border-radius: 5px;
	}
	.prior {
		width: 76px;
		padding: 3px 6px;
	}
	.prior {
		background: color-mix(in srgb, var(--accent) calc(var(--w, 0) * 38%), var(--panel));
	}
	.prior.zero {
		color: var(--faint);
	}
	.empty p {
		margin: 0 0 8px;
	}
</style>
