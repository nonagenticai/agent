<script lang="ts">
	// J4 Journey: the job's customer journey = the industry grid the platform authored (P8, inherited, read-only)
	// as this tenant refines it (D8: hide, extend, relabel). The page answers two questions at a glance —
	// "what did we inherit vs change?" and "is every in-scope activity ready to serve customers?" (routes J6,
	// knowledge J10) — and shows how a change reaches customers (D54): it is saved to the job's draft, tried in the
	// test chat, and published by a release cut (J14), where conflicts with newer template tags are flagged (J16).
	import { onMount } from 'svelte';
	import { api, app, guard } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { writeHint, canWrite, isSuperAdmin, writeAttrs } from '$lib/auth/roles';
	import type { Actor, JobJourneyActivity, JourneyStage, StageKey } from '$lib/api/types/journey';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import Seg from '$lib/ui/Seg.svelte';
	import Meter from '$lib/ui/Meter.svelte';
	import StatusChip from '$lib/ui/StatusChip.svelte';
	import { releaseLabel } from '$lib/ui/labels';
	import JourneyGrid, { type GridActivity } from '$lib/journey/JourneyGrid.svelte';
	import { STAGE_KEYS, STAGE_LABEL, opLabel } from '$lib/journey/catalogue';
	import { when } from '$lib/journey/format';
	import DraftBar from '$lib/release/DraftBar.svelte';
	import {
		activityGaps,
		containment,
		stageSummaries,
		toneOf,
		totals,
		type J4Lens,
	} from '$lib/journey/J4Readiness';

	let lens = $state<J4Lens>('ready');
	const res = new Resource(
		() => api().jobJourney(),
		() => false,
	);
	/** The live release and state come from the one agent status, the same source as every other page. */
	const stat = new Resource(
		() => api().agentStatus(),
		() => false,
	);
	const st = $derived(stat.value.state === 'ok' ? stat.value.data : null);
	onMount(() => {
		void stat.refresh();
		// `?lens=refined|live` deep-links a lens (e.g. from the release page).
		const q = new URLSearchParams(window.location.search).get('lens');
		if (q === 'refined' || q === 'live' || q === 'ready') lens = q;
		res.refresh();
	});
	const base = $derived(`/admin/jobs/${app.job}`);
	const can = $derived(canWrite(session, 'job_designer'));
	const sa = $derived(isSuperAdmin(session));
	const jd = $derived(res.value.state === 'ok' || res.value.state === 'empty' ? res.value.data : null);

	const LENSES: [string, string][] = [
		['ready', 'Ready vs gap'],
		['refined', 'Inherited vs refined'],
		['live', 'Live outcomes'],
	];
	const pct = (n: number) => `${Math.round(n * 100)}%`;

	let open = $state(false);
	let act = $state<JobJourneyActivity | null>(null);
	let relabel = $state('');
	function openActivity(a: GridActivity) {
		act = a as JobJourneyActivity;
		relabel = a.label;
		open = true;
	}
	async function write(fn: () => Promise<unknown>, ok: string) {
		if (await guard(fn, `${ok} Saved to draft.`)) {
			open = false;
			await res.refresh();
		}
	}

	let addOpen = $state(false);
	let nStage = $state<StageKey>('support');
	let nSlug = $state('');
	let nLabel = $state('');
	let nActor = $state<Actor>('customer');
	let nTrigger = $state('');
	function openAdd(s?: JourneyStage) {
		nStage = s?.key ?? 'support';
		nSlug = '';
		nLabel = '';
		nActor = 'customer';
		nTrigger = '';
		addOpen = true;
	}
	async function add() {
		if (
			await guard(
				() =>
					api().addJobExtensionActivity({
						stage: nStage,
						slug: nSlug,
						label: nLabel,
						actor: nActor,
						trigger: nTrigger,
					}),
				`Extension ${nStage}:${nSlug} added. Saved to draft — try it in the test chat; customers get it when you publish.`,
			)
		) {
			addOpen = false;
			await res.refresh();
		}
	}
</script>

<PageHead
	id="J4"
	sub="Your customers’ journey for this job: the stages and activities the platform authored for your industry, as you refine them. Decide what is in scope, close the gaps, try it in the test chat and publish it."
>
	{#snippet actions()}
		<button
			class="btn primary"
			type="button"
			{...writeAttrs(session, 'job_designer')}
			onclick={() => openAdd()}>Add extension activity</button
		>
	{/snippet}
</PageHead>

<DraftBar page="J4" tick={res.value} />

<LoadState resource={res}>
	{#snippet children(j)}
		{@const t = totals(j)}
		{@const stages = stageSummaries(j)}
		{@const red = j.findings.filter((f) => f.severity === 'red')}
		{@const outcomes = new Map((j.outcomes?.rows ?? []).map((r) => [r.activity, r]))}
		{@const gapActs = j.activities
			.map((a) => ({ a, gaps: activityGaps(a, j.knowledge_kinds) }))
			.filter((x) => x.gaps.length)}
		{@const gsOptions = j.stages.filter((s) => !s.out_of_scope).length}
		<div class="stack">
			<!-- 1. Where the journey comes from and how a change reaches customers. -->
			<section class="j4-lineage" data-testid="journey-lineage" aria-label="How this journey is assembled">
				<div class="j4-node inherited">
					<span class="j4-step">1 · Inherited</span>
					<b>{j.industry_label} template</b>
					<span class="j4-pins">
						{#each j.pins as p (p)}<span class="chip muted mono">{p}</span>{/each}
					</span>
					<span class="j4-desc"
						>{t.inherited} activities in {j.stages.length} stages, messages, knowledge kinds and theme → stage
						priors — authored by the platform, read-only here.</span
					>
					{#if sa}
						<a class="j4-link" href="/platform/journeys/{encodeURIComponent(j.industry)}"
							>Edit the template in the Journey designer →</a
						>
					{/if}
				</div>
				<span class="j4-arrow" aria-hidden="true">+</span>
				<div class="j4-node refined">
					<span class="j4-step">2 · Your refinements</span>
					<b
						>{t.hidden + t.extensions + t.relabelled} change{t.hidden + t.extensions + t.relabelled === 1
							? ''
							: 's'} to the template</b
					>
					<span class="chips">
						<span class="chip dead">{t.hidden} hidden</span>
						<span class="chip cust">{t.extensions} extension{t.extensions === 1 ? '' : 's'}</span>
						<span class="chip work">{t.relabelled} relabelled</span>
					</span>
					<span class="j4-desc">
						{#if t.unreleased}<b class="warn-text">{t.unreleased} unpublished (draft)</b> — in the
							<a href="/chat">test chat</a> now; customers get them when you publish.
						{:else if !(t.hidden + t.extensions + t.relabelled)}None yet: this job runs the template as
							authored.
						{:else if j.release}All of them are in the current release.
						{:else}They ship with the first release.{/if}
					</span>
					<span class="j4-links">
						<a class="j4-link" href="{base}/refinements">All refinements →</a>
						<a class="j4-link" href="{base}/discovery">Find gaps from your tickets →</a>
					</span>
				</div>
				<span class="j4-arrow" aria-hidden="true">=</span>
				<div class="j4-node effective">
					<span class="j4-step">3 · What the agent follows</span>
					<b>{t.enabled} activities in {t.stagesInScope} stages</b>
					<span class="j4-desc"
						>At the next cut this compiles into the gate options: {j.themes.length} themes (GT), {gsOptions}
						stages (GS) and {t.enabled} activities (GA).</span
					>
					<a class="j4-link" href="{base}/gates">Gates →</a>
				</div>
				<span class="j4-arrow" aria-hidden="true">→</span>
				<div class="j4-node live">
					<span class="j4-step">4 · Customers</span>
					{#if st?.live_release}
						<b
							>Live in <span data-testid="live-release">{releaseLabel(st.live_release.id)}</span>
							<StatusChip status={st} /></b
						>
						<span class="j4-desc"
							>{t.unreleased
								? 'Customers still see the published journey; publish a release to ship your draft.'
								: 'Customers see exactly this journey.'}</span
						>
					{:else}
						<b>Not live yet</b>
						<span class="j4-desc">No release has been applied to this job.</span>
					{/if}
					<a class="j4-link" href="{base}/release">Review &amp; publish →</a>
				</div>
			</section>

			<!-- 2. Ready vs gap: can every enabled activity serve a customer? -->
			<Card title="Can every activity serve a customer?">
				<div class="j4-ready" data-testid="journey-readiness">
					<div class="j4-score">
						<span class="j4-big">{t.ready}<span class="muted">/{t.enabled}</span></span>
						<span class="muted">enabled activities ready</span>
						<Meter value={t.enabled ? t.ready / t.enabled : 0} cls={t.gaps ? 's-warn' : 's-ok'} />
						<span class="note"
							>Ready = every required operation is routed in test and prod (J6) and the knowledge it cites is
							in your store (J10). Release apply refuses while a required operation is unrouted (K41).</span
						>
					</div>
					<div class="j4-gaps">
						{#if gapActs.length}
							<ul class="j4-gaplist">
								{#each gapActs as g (g.a.key)}
									<li>
										<button type="button" class="linkish" onclick={() => openActivity(g.a)}
											>{g.a.label}</button
										>
										<span class="muted small">{STAGE_LABEL[g.a.stage]}</span>
										<span class="chips">
											{#each g.gaps as x (x.what)}
												<a
													class="chip warn"
													href={x.kind === 'route' ? `${base}/operations` : `${base}/knowledge-kinds`}
													title={x.kind === 'route'
														? 'Route this operation on Operations & routes'
														: 'Add documents of this kind to your knowledge store'}
													>{x.kind === 'route' ? 'no route' : 'missing knowledge'}: {x.what}</a
												>
											{/each}
										</span>
									</li>
								{/each}
							</ul>
						{:else}
							<p class="ok-text">
								Every enabled activity is routed and has its knowledge. Nothing blocks a release.
							</p>
						{/if}
					</div>
				</div>
			</Card>

			{#if j.pending.length}
				<Callout
					kind="gap"
					title="{j.pending.length} journey refinement{j.pending.length === 1
						? ''
						: 's'} not in a release yet"
				>
					<ul class="plain">
						{#each j.pending as p, i (i)}<li>
								{p.what} <span class="muted">· {p.by} · {when(p.at)}</span>
							</li>{/each}
					</ul>
					Conflicts with newer template tags are flagged when the next release is cut — see
					<a href="{base}/release">Release</a>.
				</Callout>
			{/if}

			{#if red.length}
				<Callout
					kind="err"
					title="{red.length} stage{red.length === 1 ? '' : 's'} without an enabled activity"
				>
					{red.map((f) => f.message).join(' · ')}. Show a hidden activity again or add an extension activity;
					only the platform can mark a stage out of scope.
				</Callout>
			{/if}

			<!-- 3. The grid itself, read through one lens at a time. -->
			<Card
				title="The journey grid"
				sub="Columns are the stages a customer moves through; rows are who does what, what they say, and what the agent must touch or know. Click an activity for its use case and to refine it."
			>
				<div class="j4-lensbar">
					<span class="j4-step">Read the grid as</span>
					<Seg items={LENSES} bind:active={lens} label="Grid lens" />
				</div>
				<div class="j4-legend" data-testid="journey-lens-legend">
					{#if lens === 'ready'}
						<span class="j4-key ready">ready</span>
						<span class="j4-key gap">gap — click for what is missing</span>
					{:else if lens === 'refined'}
						<span class="j4-key inherited">inherited from the template</span>
						<span class="j4-key refined">refined by you</span>
					{:else if j.outcomes}
						<span class="j4-key ready">≥ 50% contained by the agent</span>
						<span class="j4-key gap">mostly handed to staff</span>
						<span class="j4-key idle">no conversations</span>
						<span class="muted">· {j.outcomes.window}</span>
					{:else}
						<span class="muted"
							>No live traffic yet — outcomes appear once a release is applied and customers start chatting.</span
						>
					{/if}
					<span class="muted"
						>· <span class="j4-struck">struck</span> = hidden by you · “tenant” = your extension</span
					>
				</div>
				<JourneyGrid
					stages={j.stages}
					themes={j.themes}
					channels={j.channels}
					activities={j.activities}
					messages={j.messages}
					kinds={j.knowledge_kinds}
					findings={j.findings}
					onActivity={openActivity}
					onAdd={openAdd}
					addLabel="+ extension"
					addDisabled={!can}
					addTitle={can ? undefined : writeHint('job_designer')}
					activityTone={(a) => toneOf(lens, a as JobJourneyActivity, j.knowledge_kinds, outcomes.get(a.key))}
				>
					{#snippet stageMeta(s)}
						{@const st = stages.find((x) => x.stage.key === s.key)}
						{#if st && !s.out_of_scope}
							<span class="j4-stagemeta">
								{#if lens === 'ready'}
									{#if st.gaps}<span class="chip warn">{st.gaps} gap{st.gaps === 1 ? '' : 's'}</span
										>{:else}<span class="chip ok">ready</span>{/if}
								{:else if lens === 'refined'}
									{st.enabled} on{st.hidden ? ` · ${st.hidden} hidden` : ''}{st.refined
										? ` · ${st.refined} refined`
										: ''}
								{:else if j.outcomes}
									{st.conversations} conversations
								{/if}
							</span>
						{/if}
					{/snippet}
					{#snippet activityMeta(a)}
						{@const ja = a as JobJourneyActivity}
						{#if a.refinement !== 'hidden'}
							{#if lens === 'ready'}
								{#each activityGaps(ja, j.knowledge_kinds) as g (g.what)}<span class="j4-gapline"
										>{g.kind === 'route' ? 'no route' : 'missing'}: {g.what}</span
									>{/each}
							{:else if lens === 'live'}
								{@const o = outcomes.get(a.key)}
								{#if o}{o.conversations} · {pct(containment(o))} contained{/if}
							{/if}
						{/if}
					{/snippet}
				</JourneyGrid>
			</Card>
		</div>
	{/snippet}
</LoadState>

<Drawer bind:open title={act?.label ?? 'Activity'}>
	{#snippet sub()}<span class="mono">{act?.key}</span>{/snippet}
	{#if act && jd}
		{@const j = jd}
		{@const gaps = activityGaps(act, j.knowledge_kinds)}
		{@const o = j.outcomes?.rows.find((r) => r.activity === act!.key)}
		<div class="stack">
			<div class="row">
				<span class="chip {act.actor === 'company' ? 'work' : 'cust'}">{act.actor}</span>
				{#if act.refinement === 'base'}<span class="chip muted">inherited from the template</span>{/if}
				{#if act.refinement === 'hidden'}<span class="chip dead">hidden by you</span>{/if}
				{#if act.refinement === 'extension'}<span class="chip cust">tenant extension</span>{/if}
				{#if act.refinement === 'relabelled'}<span class="chip work"
						>relabelled · template: {act.base_label}</span
					>{/if}
				{#if act.pending}<span class="chip warn">draft · not published</span>{/if}
			</div>
			<p class="muted">Trigger: {act.trigger || '—'}</p>

			<section class="j4-dsec">
				<h3>Readiness</h3>
				{#if act.refinement === 'hidden'}
					<p class="muted">
						Hidden in the draft: the test chat no longer offers it; customers stop seeing it once you publish.
					</p>
				{:else if gaps.length}
					<ul class="plain">
						{#each gaps as g (g.what)}
							<li>
								{#if g.kind === 'route'}Required <span class="mono">{g.what}</span> has no route —
									<a href="{base}/operations">route it</a>
								{:else}Knowledge kind <span class="mono">{g.what}</span> is not in your store —
									<a href="{base}/knowledge-kinds">add it</a>{/if}
							</li>
						{/each}
					</ul>
				{:else}
					<p class="ok-text">Ready: {act.routing?.required ?? 0} required operation(s) routed.</p>
				{/if}
				<div class="chips">
					{#each act.operations as op (opLabel(op))}<span
							class="chip {op.required ? 'accent' : 'muted'}"
							title={op.required ? 'Required' : 'Optional'}>{opLabel(op)} {op.required ? 'R' : 'O'}</span
						>{/each}
				</div>
			</section>

			<section class="j4-dsec">
				<h3>Live outcome</h3>
				{#if o}
					<p>
						{o.conversations} conversations · {pct(containment(o))} contained · {o.escalated} handed to staff
						<span class="muted">({j.outcomes?.window})</span>
					</p>
				{:else}
					<p class="muted">No conversations for this activity yet.</p>
				{/if}
			</section>

			<a class="btn" href="{base}/journey/{encodeURIComponent(act.key)}"
				>Open use case (objects × operations, dialogue)</a
			>
			<fieldset class="editor-fields">
				<legend>Refine (saved to draft, published with a release)</legend>
				<div class="field">
					<label for="rl">Label shown to staff and in the GA gate</label>
					<div class="row">
						<input id="rl" class="input" bind:value={relabel} disabled={!can} />
						<button
							class="btn"
							type="button"
							{...writeAttrs(session, 'job_designer')}
							onclick={() => act && write(() => api().relabelJobActivity(act!.key, relabel), 'Label saved.')}
							>Relabel</button
						>
					</div>
				</div>
				<div class="row">
					{#if act.refinement === 'base'}
						<button
							class="btn danger"
							type="button"
							{...writeAttrs(session, 'job_designer')}
							onclick={() => act && write(() => api().hideJobActivity(act!.key), `${act!.label} hidden.`)}
							>Hide activity</button
						>
					{:else if act.refinement === 'hidden' || act.refinement === 'relabelled'}
						<button
							class="btn"
							type="button"
							{...writeAttrs(session, 'job_designer')}
							onclick={() =>
								act && write(() => api().restoreJobActivity(act!.key), 'Template version restored.')}
							>{act.refinement === 'hidden' ? 'Show again' : 'Reset label'}</button
						>
					{:else}
						<button
							class="btn danger"
							type="button"
							{...writeAttrs(session, 'job_designer')}
							onclick={() =>
								act && write(() => api().removeJobExtensionActivity(act!.key), 'Extension removed.')}
							>Remove extension</button
						>
					{/if}
				</div>
				{#if !can}<p class="note">{writeHint('job_designer')}</p>{/if}
				<p class="note">
					A required operation can never be hidden away (K48): hiding refuses if it drops one.
				</p>
			</fieldset>
		</div>
	{/if}
</Drawer>

<Drawer bind:open={addOpen} title="Add extension activity">
	<div class="stack">
		<p class="muted">
			An extension is something your customers ask that the template does not cover. It is your refinement
			(D8): it starts with <code>knowledge.search</code> on your store, is saved to draft until you publish, and
			is kept when the template is upgraded.
		</p>
		<div class="form-grid">
			<div class="field">
				<label for="x-stage">Stage</label>
				<select id="x-stage" class="input" bind:value={nStage}>
					{#each STAGE_KEYS as k (k)}<option value={k}>{STAGE_LABEL[k]}</option>{/each}
				</select>
			</div>
			<div class="field">
				<label for="x-actor">Actor</label>
				<select id="x-actor" class="input" bind:value={nActor}>
					<option value="customer">customer</option>
					<option value="company">company</option>
					<option value="employee">employee</option>
				</select>
			</div>
		</div>
		<div class="field">
			<label for="x-slug">Key</label>
			<div class="row">
				<span class="mono muted">{nStage}:</span><input
					id="x-slug"
					class="input"
					placeholder="gift_wrapping"
					bind:value={nSlug}
				/>
			</div>
		</div>
		<div class="field">
			<label for="x-label">Label</label>
			<input id="x-label" class="input" bind:value={nLabel} />
		</div>
		<div class="field">
			<label for="x-trig">Trigger</label>
			<input id="x-trig" class="input" placeholder="“can you gift wrap it?”" bind:value={nTrigger} />
		</div>
		<div class="row">
			<button class="btn primary" type="button" {...writeAttrs(session, 'job_designer')} onclick={add}
				>Add extension</button
			>
			<button class="btn" type="button" onclick={() => (addOpen = false)}>Cancel</button>
		</div>
	</div>
</Drawer>

<style>
	.small {
		font-size: 12px;
	}
	.plain {
		margin: 4px 0;
		padding-left: 18px;
	}
	.warn-text {
		color: var(--warn);
	}
	.ok-text {
		color: var(--ok);
		margin: 0;
	}
	/* Lineage: template + refinements = effective → customers. */
	.j4-lineage {
		display: grid;
		grid-template-columns: minmax(0, 1.3fr) auto minmax(0, 1fr) auto minmax(0, 1fr) auto minmax(0, 0.9fr);
		align-items: stretch;
		gap: 8px;
	}
	.j4-node {
		display: flex;
		flex-direction: column;
		gap: 6px;
		padding: 12px 14px;
		border: 1px solid var(--line);
		border-radius: var(--r);
		background: var(--panel);
		min-width: 0;
	}
	.j4-node.inherited {
		background: var(--sunk);
		border-style: dashed;
	}
	.j4-node.refined {
		border-color: color-mix(in srgb, var(--accent) 45%, var(--line));
		background: color-mix(in srgb, var(--accent) 6%, var(--panel));
	}
	.j4-node.live {
		border-color: color-mix(in srgb, var(--ok) 45%, var(--line));
	}
	.j4-step {
		font: 600 11px var(--cond);
		letter-spacing: 0.06em;
		text-transform: uppercase;
		color: var(--faint);
	}
	.j4-pins {
		display: flex;
		flex-wrap: wrap;
		gap: 4px;
	}
	.j4-pins .chip {
		font-size: 11px;
		overflow-wrap: anywhere;
	}
	.j4-desc {
		font-size: 12.5px;
		color: var(--muted);
	}
	.j4-link {
		font-size: 12.5px;
		margin-top: auto;
	}
	.j4-links {
		display: flex;
		flex-wrap: wrap;
		gap: 4px 12px;
		margin-top: auto;
	}
	.j4-arrow {
		align-self: center;
		font: 600 18px var(--sans);
		color: var(--faint);
	}
	/* Readiness band. */
	.j4-ready {
		display: grid;
		grid-template-columns: minmax(200px, 280px) 1fr;
		gap: 20px;
	}
	.j4-score {
		display: flex;
		flex-direction: column;
		gap: 6px;
	}
	.j4-score .note {
		margin: 4px 0 0;
	}
	.j4-big {
		font: 600 30px var(--sans);
		line-height: 1;
	}
	.j4-big .muted {
		font-size: 18px;
	}
	.j4-gaplist {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 8px;
	}
	.j4-gaplist li {
		display: flex;
		flex-wrap: wrap;
		gap: 6px 10px;
		align-items: center;
		padding: 8px 10px;
		border: 1px solid color-mix(in srgb, var(--warn) 40%, var(--line));
		border-radius: var(--r);
		background: color-mix(in srgb, var(--warn) 6%, var(--panel));
	}
	.j4-gaplist a.chip {
		text-decoration: none;
	}
	.linkish {
		background: none;
		border: 0;
		padding: 0;
		font: 600 13px var(--sans);
		color: var(--accent);
		cursor: pointer;
		text-align: left;
	}
	.j4-lensbar {
		display: flex;
		flex-wrap: wrap;
		gap: 6px 10px;
		align-items: center;
		margin-bottom: 8px;
		max-width: 100%;
		overflow-x: auto;
	}
	/* Lens legend. */
	.j4-legend {
		display: flex;
		flex-wrap: wrap;
		gap: 6px 12px;
		align-items: center;
		font-size: 12px;
		margin-bottom: 10px;
	}
	.j4-key {
		display: inline-flex;
		align-items: center;
		gap: 6px;
	}
	.j4-key::before {
		content: '';
		width: 14px;
		height: 12px;
		border-radius: 3px;
		border: 1px solid var(--line);
		border-left-width: 3px;
	}
	.j4-key.ready::before {
		border-left-color: var(--ok);
	}
	.j4-key.gap::before {
		border-color: var(--warn);
		background: color-mix(in srgb, var(--warn) 12%, transparent);
	}
	.j4-key.inherited::before {
		background: var(--sunk);
	}
	.j4-key.refined::before {
		border-color: var(--accent);
		background: color-mix(in srgb, var(--accent) 12%, transparent);
	}
	.j4-key.idle::before {
		border-left-color: var(--line);
	}
	.j4-stagemeta {
		font-size: 11px;
		color: var(--muted);
		font-weight: 400;
		text-transform: none;
		letter-spacing: 0;
	}
	.j4-struck {
		text-decoration: line-through;
	}
	.j4-gapline {
		color: var(--warn);
		font-weight: 600;
	}
	.j4-dsec h3 {
		font: 600 12px var(--cond);
		letter-spacing: 0.06em;
		text-transform: uppercase;
		color: var(--faint);
		margin: 0 0 6px;
	}
	.j4-dsec p {
		margin: 0 0 6px;
	}
	@media (max-width: 1100px) {
		.j4-lineage {
			grid-template-columns: 1fr;
		}
		.j4-arrow {
			justify-self: center;
			line-height: 1;
		}
		.j4-ready {
			grid-template-columns: 1fr;
		}
	}
</style>
