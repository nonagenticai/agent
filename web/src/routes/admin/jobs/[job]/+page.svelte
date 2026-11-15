<script lang="ts">
	import { prefs } from '$lib/ui/prefs.svelte';
	// J1 Agent home. An agent (a job) is one deployment (template layers + this tenant's refinements) that reaches
	// customers only through a release. The page answers, in order: where the agent stands (the lifecycle strip over the
	// one `agentStatus()`), what needs a person (the one `inbox(job)`), what it is doing right now (open cases by
	// holder, signals), what is live and what is pending (template pins → refinements → approved release → live), and
	// what changed recently. Spec 07 §3a, 05 §4.7, 03; vocabulary shared with the Agents board.
	import { onMount } from 'svelte';
	import { api, app, guard, switchTenant } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { canTenantAdmin, isSuperAdmin, writeHint } from '$lib/auth/roles';
	import { Resource } from '$lib/load.svelte';
	import { fmt } from '$lib/format';
	import { jobPath } from '$lib/scope';
	import { when } from '$lib/runtime/labels';
	import { layerName } from '$lib/tenant/jobs-T1';
	import { changesOf, lanesOf, releaseFlow } from '$lib/runtime/J1-home';
	import { kpiDelta } from './analytics/series';
	import { L, layerLabel, plain, releaseLabel } from '$lib/ui/labels';
	import TechDetail from '$lib/ui/TechDetail.svelte';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Freshness from '$lib/ui/Freshness.svelte';
	import Kpi from '$lib/ui/Kpi.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Status from '$lib/ui/Status.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';
	import Inbox from '$lib/ui/Inbox.svelte';
	import Lifecycle from '$lib/ui/Lifecycle.svelte';
	import { LANGUAGE_LABEL, type JobAudience } from '$lib/api/types/core';
	import type { JobLanguagesView } from '$lib/api/types/release';
	import JobLanguages from './refinements/JobLanguages.svelte';
	import type { Inbox as InboxData } from '$lib/api/types/status';

	const overview = new Resource(
		() => api().jobOverview(),
		() => false,
	);
	/** The one status of this agent, and the one inbox of this agent (the same sources as Workspace home). */
	const status = new Resource(
		() => api().agentStatus(),
		() => false,
	);
	const box = new Resource<InboxData>(
		() => api().inbox(app.job),
		() => false,
	);
	const audit = new Resource(() => api().tenantAudit({ job: app.job, view: 'changes', limit: 12 }));
	/** D54 draft ledger: the job's unpublished changes (the refinements node counts them). */
	let draftCount = $state<number | null>(null);
	/** D59: the languages the job serves (a job setting), live and draft. */
	let langs = $state<JobLanguagesView | null>(null);
	const langName = (k: string) => LANGUAGE_LABEL[k] ?? k;
	function loadLangs() {
		api()
			.jobLanguages()
			.then((l) => (langs = l))
			.catch(() => (langs = null));
	}
	onMount(() => {
		void overview.refresh();
		void status.refresh();
		void box.refresh();
		void audit.refresh();
		loadLangs();
		api()
			.draftSummary()
			.then((d) => (draftCount = d.count))
			.catch(() => (draftCount = null));
	});

	const HEALTH_LABEL = {
		ok: 'Healthy',
		down: 'Down',
		degraded: 'Degraded',
		unconfigured: 'Not configured',
	} as const;
	const FLOW_CHIP = {
		done: ['current', 'ok'],
		live: ['live', 'ok'],
		pending: ['pending', 'accent'],
		blocked: ['blocked', 'err'],
		none: ['—', 'muted'],
		behind: ['newer tag', 'warn'],
	} as const;
	const href = (sub: string) => jobPath(app.job, sub);
	const sa = $derived(isSuperAdmin(session));

	// D123.4: a tenant_admin renames the job (`PATCH /api/jobs/{job}`); the key never changes.
	const jobLabel = $derived(app.jobs.find((j) => j.key === app.job)?.label ?? app.job);
	const mayRename = $derived(canTenantAdmin(session));
	let renaming = $state(false);
	let nameDraft = $state('');
	let nameBusy = $state(false);
	// D129: who the job serves, editable next to the name.
	const jobRegion = $derived(app.jobs.find((j) => j.key === app.job)?.region ?? null);
	const jobAudience = $derived(app.jobs.find((j) => j.key === app.job)?.audience ?? 'customers');
	let audienceBusy = $state(false);
	async function saveAudience(audience: JobAudience) {
		if (audience === jobAudience || audienceBusy) return;
		audienceBusy = true;
		const ok = await guard(
			() => api().renameJob(app.job, { audience }),
			'Audience saved.',
			'Audience not saved',
		);
		audienceBusy = false;
		if (ok) await switchTenant(app.activeTenant);
	}
	function startRename() {
		nameDraft = jobLabel;
		renaming = true;
	}
	async function saveName() {
		const label = nameDraft.trim();
		if (!label || nameBusy) return;
		nameBusy = true;
		const ok = await guard(() => api().renameJob(app.job, { label }), 'Job renamed.', 'Not renamed');
		nameBusy = false;
		if (ok) {
			renaming = false;
			await switchTenant(app.activeTenant);
		}
	}

	const releasedAt = $derived(
		overview.value.state === 'ok' ? (overview.value.data.current_release?.released_at ?? null) : null,
	);
	const changes = $derived(audit.value.state === 'ok' ? changesOf(audit.value.data.items, releasedAt) : []);
	const unreleased = $derived(changes.filter((c) => c.unreleased).length);
</script>

<PageHead id="J1" sub="Where this agent stands, what needs you, and what it is doing right now.">
	{#snippet actions()}
		<Freshness resource={overview} />
		<a class="btn" href="/chat">Try in test chat</a>
		<a class="btn primary" href="/console">Open live console</a>
	{/snippet}
</PageHead>

<section class="j1-name" data-testid="job-name" aria-label="Job name">
	{#if renaming}
		<form
			class="j1-name-form"
			onsubmit={(e) => {
				e.preventDefault();
				void saveName();
			}}
		>
			<input class="input" aria-label="Job name" bind:value={nameDraft} maxlength="120" />
			<button class="btn primary sm" type="submit" disabled={nameBusy || !nameDraft.trim()}
				>{nameBusy ? 'Saving…' : 'Save name'}</button
			>
			<button class="btn sm" type="button" onclick={() => (renaming = false)}>Cancel</button>
		</form>
	{:else}
		<b data-testid="job-label">{jobLabel}</b>
		{#if prefs.showTech}<span class="mono muted">{app.job}</span>{/if}
		<button
			class="btn sm"
			type="button"
			disabled={!mayRename}
			title={mayRename ? undefined : writeHint('tenant_admin')}
			onclick={startRename}>Rename</button
		>
	{/if}
	<label class="j1-aud"
		>Serves
		<select
			class="input"
			data-testid="job-audience"
			aria-label="Who does this job serve?"
			value={jobAudience}
			disabled={!mayRename || audienceBusy}
			title={mayRename ? undefined : writeHint('tenant_admin')}
			onchange={(e) => void saveAudience(e.currentTarget.value as JobAudience)}
		>
			<option value="customers">Customers</option>
			<option value="employees">Employees</option>
		</select></label
	>
	<span class="muted small" data-testid="job-region-shown">Region <b>{jobRegion ?? 'none'}</b></span>
</section>

<LoadState resource={status}>
	{#snippet children(st)}
		<Lifecycle status={st} job={app.job} />

		<div class="grid j1-now">
			<Card title="Needs you" sub="{st.urgent} urgent · {st.review} to review">
				{#if box.value.state === 'loading'}
					<p class="muted">Loading…</p>
				{:else if box.value.state === 'unavailable'}
					<Unavailable reason={box.value.reason} retry={box.retry} />
				{:else if box.value.state === 'ok'}
					<Inbox
						items={box.value.data.items}
						empty="Nothing needs you right now. The agent is running on its live version."
					/>
				{/if}
			</Card>

			<Card title="Right now" sub="Open cases by who is holding them">
				{#snippet actions()}
					<a class="btn sm" href={href('/cases')}>All cases</a>
				{/snippet}
				{@const c = st.open_cases}
				{@const lanes = lanesOf(c)}
				{@const customers = c.agent + c.staff + c.waiting}
				{@const openTotal = customers + c.staff_tests}
				{#if !st.live_release && openTotal === 0}
					<div class="empty">Nothing is live yet, so no customer is waiting on this agent.</div>
				{:else if openTotal === 0}
					<div class="empty">No open cases — nobody is waiting on this agent.</div>
				{:else}
					<div class="j1-open">
						<b class="j1-big" data-testid="open-cases">{openTotal}</b>
						<span class="muted"
							>open · {customers} with customers{#if c.staff_tests}, {c.staff_tests} staff tests{/if}</span
						>
					</div>
					<div class="j1-bar" role="img" aria-label="Open cases by holder">
						{#each lanes.filter((l) => l.count && l.key !== 'failed') as l (l.key)}
							<span class="seg t-{l.tone}" style="flex:{l.count}" title="{l.label}: {l.count}"></span>
						{/each}
					</div>
					<ul class="list-plain j1-lanes">
						{#each lanes as l (l.key)}
							<li>
								<a href={href(`/cases?status=${l.statuses[0]}`)} class="j1-lane">
									<span class="sw t-{l.tone}" aria-hidden="true"></span>
									<span class="cell-title">{l.label}</span>
									<span class="cell-sub">{l.hint}</span>
									<b class="mono">{l.count}</b>
								</a>
							</li>
						{/each}
					</ul>
				{/if}
				{#if overview.value.state === 'ok' && overview.value.data.pulse && st.live_release}
					{@const p = overview.value.data.pulse}
					<div class="j1-signals">
						<a href={href('/handover')} class:hot={p.unclaimed_handovers > 0}>
							<b>{p.unclaimed_handovers}</b><span>handovers unclaimed</span>
						</a>
						<a href={href('/cases')} class:hot={p.alerts > 0}>
							<b>{p.alerts}</b><span>outcome unknown</span>
						</a>
						<a href={href('/agents')} class:hot={p.agents.behind > 0}>
							<b>{p.agents.ok}/{p.agents.total}</b><span
								>specialists healthy{#if p.agents.behind}
									· {p.agents.behind} behind{/if}</span
							>
						</a>
						<a href={href('/buses?tab=dead')} class:hot={p.dead_letters > 0}>
							<b>{p.dead_letters}</b><span>{L.dead_letters.toLowerCase()}</span>
						</a>
					</div>
					{#if p.failing_ops.length}
						<div class="j1-failing">
							<span class="label">Actions failing · 30 d</span>
							<ul class="list-plain">
								{#each p.failing_ops as f (f.op)}
									<li>
										<a href={href(`/operation-calls?op=${encodeURIComponent(f.op)}`)}
											><span class="mono">{f.op}</span>
											<span class="cell-sub mono">{f.instance ?? ''}</span></a
										>
										<span class="mono j1-rate">{fmt.pct(f.rate, 0)} of {fmt.n(f.calls)}</span>
									</li>
								{/each}
							</ul>
						</div>
					{/if}
				{/if}
			</Card>
		</div>
	{/snippet}
</LoadState>

<LoadState resource={overview}>
	{#snippet children(o)}
		{@const r = o.release_status}
		{@const flow = releaseFlow(o, draftCount ?? unreleased)}

		{#if langs}
			<p class="muted j1-line" data-testid="j1-languages">
				Serves <b>{langs.live.map(langName).join(', ') || 'no language'}</b>{#if langs.unpublished}
					· draft <a href="#job-settings">{langs.draft.map(langName).join(', ')}</a
					>{/if}{#if o.kpis.containment != null}
					· {fmt.pct(o.kpis.containment, 0)} finished without a person (30 d){/if}
				· <a href="#job-settings" data-testid="j1-edit-languages">Edit languages</a>
			</p>
		{/if}
		<!-- D59/D132.4: the languages editor lives here (the old /refinements route only redirects to Releases). -->
		<div id="job-settings" data-testid="j1-languages-editor"><JobLanguages onchange={loadLangs} /></div>

		<!-- 3 · What is live, what is next -->
		<Card
			title="What customers get — and what comes next"
			sub="Template layers and your refinements reach customers only when a release is cut, approved and applied."
		>
			{#snippet actions()}
				<a class="btn sm" href={href('/release')}
					>{o.current_release ? 'Release history & apply' : 'Approve a release'}</a
				>
			{/snippet}
			<ol class="j1-flow" aria-label="Release pipeline">
				{#each flow as n (n.key)}
					<li class="j1-node st-{n.state}">
						<a href={href(n.sub)}>
							<span class="label">{n.title}</span>
							{#if n.key === 'live' && o.current_release}
								<b data-testid="current-release">{releaseLabel(o.current_release.id)}</b>
							{:else if n.key === 'pending' && r.approved_waiting}
								<b data-testid="approved-waiting">{releaseLabel(r.approved_waiting.id)}</b>
							{:else}
								<b>{plain(n.value)}</b>
							{/if}
							<span class="cell-sub">{plain(n.note)}</span>
							<Chip text={FLOW_CHIP[n.state][0]} variant={FLOW_CHIP[n.state][1]} />
						</a>
					</li>
				{/each}
			</ol>

			{#if !o.current_release}
				<div class="empty" data-testid="no-release">
					No release yet. Approve one on the <a href={href('/release')}>Release page</a> — customers get the “unavailable”
					reply until then.
				</div>
			{/if}

			<div class="j1-rel-detail">
				<div>
					<span class="label">Inherited template layers</span>
					<ul class="list-plain j1-pins">
						{#each o.pins ?? [] as pin (pin.layer)}
							<li>
								<span
									>{layerLabel(`${pin.layer}@v${pin.pinned}`)}
									<TechDetail><span class="mono">{layerName(pin.layer)}@v{pin.pinned}</span></TechDetail
									></span
								>
								{#if pin.latest != null}
									<Chip
										text="Base changed since your release"
										variant="warn"
										title="{layerLabel(`${pin.layer}@v${pin.latest}`)} was tagged {when(
											pin.tagged_at,
										)}; your release uses v{pin.pinned}"
									/>
								{:else}
									<span class="cell-sub">current</span>
								{/if}
							</li>
						{/each}
					</ul>
					<p class="cell-sub">
						Read-only here; refine on <a href={href('/refinements')}>Base & refinements</a>{#if sa}, or open
							the <a href="/platform/templates">template catalogue</a>{/if}.
					</p>
				</div>
				<div>
					{#if o.current_release}
						{@const cr = o.current_release}
						<span class="label">Live release</span>
						<p class="j1-rel-line">
							<b>{releaseLabel(cr.id)}</b>
							<TechDetail><span class="mono muted">#{cr.hash}</span></TechDetail> · released by
							{cr.released_by ?? '—'}
							{when(cr.released_at)}
						</p>
					{/if}
					{#if r.approved_waiting}
						{@const w = r.approved_waiting}
						<span class="label">Approved, waiting to apply</span>
						<p class="j1-rel-line">
							<b>{releaseLabel(w.id)}</b>
							<TechDetail><span class="mono muted">#{w.hash}</span></TechDetail> · approved by {w.approved_by}
							{when(w.approved_at)}
						</p>
					{/if}
					{#each r.base_changed as b (b.layer)}
						<p class="j1-rel-line">
							{layerLabel(`${b.layer}@v${b.latest}`)}
							<TechDetail><span class="mono">{b.layer}@v{b.latest}</span></TechDetail> was tagged {when(
								b.tagged_at,
							)}; you run v{b.pinned}.
							<a href="{href('/release')}#conflicts">Review the conflicts</a> before the next cut.
						</p>
					{/each}
					{#if r.missing_slots.length}
						<Callout kind="gap" title={L.release_not_ready}>
							<ul class="miss">
								{#each r.missing_slots as m (`${m.dependency}.${m.slot}`)}<li>
										{#if m.href}<a href={m.href}>{plain(m.detail)}</a>{:else}{plain(m.detail)}{/if}
									</li>{/each}
							</ul>
							<a href={href('/readiness')}>Open {L.readiness.toLowerCase()}</a>
						</Callout>
					{/if}
				</div>
			</div>
		</Card>

		<!-- 4 · How customers fared -->
		{@const k = o.kpis}
		<div class="j1-kpi-head">
			<h2>How customers fared · last 30 days</h2>
			<a href={href('/analytics')}>Analytics →</a>
		</div>
		<div class="kpis">
			<Kpi
				label="North star · {k.north_star.label}"
				value={k.north_star.value == null
					? null
					: k.north_star.unit === 'pct'
						? fmt.pct(k.north_star.value, 0)
						: fmt.n(k.north_star.value)}
				unavailable={k.north_star.value == null ? 'not measured yet' : ''}
				note="what this job is for"
			/>
			<Kpi
				label="Containment"
				value={k.containment == null ? null : fmt.pct(k.containment, 0)}
				delta={kpiDelta(k.containment, k.containment_prev)}
				unavailable={k.containment == null ? 'no cases yet' : ''}
				note="finished without a person"
			/>
			<Kpi
				label="Escalation"
				value={k.escalation == null ? null : fmt.pct(k.escalation, 0)}
				delta={kpiDelta(k.escalation, k.escalation_prev)}
				good="down"
				unavailable={k.escalation == null ? 'no cases yet' : ''}
				note="handed to staff"
			/>
			<Kpi
				label="Turns"
				value={fmt.n(k.turns_30d)}
				delta={k.turns_prev ? (k.turns_30d - k.turns_prev) / k.turns_prev : null}
				note="customer messages answered"
			/>
		</div>

		<!-- 5 · What changed, what it depends on -->
		<div class="grid j1-now">
			<Card title="What changed recently" sub="Changes to this job, newest first">
				{#snippet actions()}
					<a class="btn sm" href="/admin/audit?job={encodeURIComponent(app.job)}">Audit log</a>
				{/snippet}
				<LoadState resource={audit} empty="No changes recorded for this job yet.">
					{#snippet children()}
						{#if unreleased}
							<p class="j1-unrel-note">
								<Chip text="not live yet" variant="accent" />
								{unreleased === 1 ? 'One design change was' : `${unreleased} design changes were`} saved after the
								live release: draft (or approved, not yet applied). Customers see
								{unreleased === 1 ? 'it' : 'them'} once published —
								<a href="/chat">test in test chat</a> ·
								<a href={href('/release')}>review &amp; publish</a>.
							</p>
						{/if}
						<ul class="list-plain j1-changes" data-testid="j1-changes">
							{#each changes.slice(0, 7) as c (c.id)}
								<li>
									<span class="mono j1-when">{when(c.at)}</span>
									<span class="j1-change"
										><b>{c.who}</b>
										{plain(c.what)} <span class="mono">{plain(c.object)}</span>
										{#if c.failed}<Chip text="failed" variant="err" />{/if}
										{#if c.unreleased}<Chip text="not live yet" variant="accent" />{/if}</span
									>
									<span class="j1-sec">{c.section}</span>
								</li>
							{/each}
						</ul>
					{/snippet}
				</LoadState>
			</Card>
			<Card title="Systems it depends on" sub="Health of the message flow, stores and connected systems">
				{#if o.health.length === 0}<div class="empty">No systems connected yet.</div>{/if}
				<ul class="list-plain j1-health">
					{#each o.health as h (h.name)}
						<li>
							<span
								><span class="cell-title">{h.name}</span>{#if h.detail}<span class="cell-sub mono"
										>{plain(h.detail)}</span
									>{/if}</span
							>
							<Status
								word={h.status === 'unconfigured' ? 'draft' : h.status}
								label={HEALTH_LABEL[h.status]}
							/>
						</li>
					{/each}
				</ul>
			</Card>
		</div>
	{/snippet}
</LoadState>

<style>
	.j1-name {
		display: flex;
		align-items: center;
		gap: 10px;
		flex-wrap: wrap;
		margin-bottom: 12px;
	}
	.j1-aud {
		display: inline-flex;
		align-items: center;
		gap: 6px;
	}
	.j1-name-form {
		display: flex;
		gap: 8px;
		align-items: center;
	}
	.j1-line {
		margin: 0 0 16px;
	}
	.j1-now {
		grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
		margin-bottom: 16px;
	}
	/* Grid children default to min-width:auto, which lets a long id or source tag push the column past the card. */
	.j1-now > :global(*),
	.j1-rel-detail > :global(*) {
		min-width: 0;
	}
	.j1-rel-detail :global(.mono),
	.j1-pins :global(.mono),
	.j1-changes :global(.mono),
	:global(.cell-sub.mono) {
		overflow-wrap: anywhere;
	}
	/* a health detail can be one unbroken word (a sha256): the flex row must let it shrink and wrap */
	.j1-health li > span {
		min-width: 0;
	}
	.j1-open {
		display: flex;
		align-items: baseline;
		gap: 8px;
	}
	.j1-big {
		font-size: 30px;
		line-height: 1.1;
	}
	.j1-bar {
		display: flex;
		height: 10px;
		gap: 2px;
		margin: 8px 0 10px;
		border-radius: 5px;
		overflow: hidden;
		background: var(--sunk);
	}
	.seg,
	.sw {
		background: var(--muted);
	}
	.t-accent {
		background: var(--accent);
	}
	.t-warn {
		background: var(--warn);
	}
	.t-work {
		background: var(--work);
	}
	.t-err {
		background: var(--err);
	}
	.j1-lane {
		display: grid;
		grid-template-columns: 10px auto 1fr auto;
		gap: 8px;
		align-items: baseline;
		width: 100%;
		color: inherit;
		text-decoration: none;
	}
	.j1-lane .cell-sub {
		margin: 0;
	}
	.j1-lane:hover .cell-title {
		color: var(--accent);
	}
	.sw {
		width: 10px;
		height: 10px;
		border-radius: 2px;
		align-self: center;
	}
	.j1-signals {
		display: grid;
		grid-template-columns: repeat(4, minmax(0, 1fr));
		gap: 8px;
		margin-top: 12px;
	}
	.j1-signals a {
		display: flex;
		flex-direction: column;
		padding: 8px 10px;
		border: 1px solid var(--line);
		border-radius: var(--r);
		color: inherit;
		text-decoration: none;
		font-size: 12px;
	}
	.j1-signals a span {
		color: var(--muted);
	}
	.j1-signals b {
		font-size: 18px;
	}
	.j1-signals a.hot {
		border-color: color-mix(in srgb, var(--warn) 60%, var(--line));
		background: color-mix(in srgb, var(--warn) 8%, transparent);
	}
	.j1-signals a:hover {
		border-color: var(--accent);
	}
	.j1-failing {
		margin-top: 12px;
	}
	.j1-failing li {
		display: flex;
		justify-content: space-between;
		gap: 8px;
	}
	.j1-rate {
		color: var(--err);
		white-space: nowrap;
	}

	.j1-sec {
		flex: none;
		font-size: 11px;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		color: var(--faint);
	}

	.j1-flow {
		display: grid;
		grid-template-columns: repeat(4, minmax(0, 1fr));
		gap: 22px;
		list-style: none;
		padding: 0;
		margin: 0 0 14px;
		counter-reset: none;
	}
	.j1-node {
		position: relative;
	}
	.j1-node:not(:last-child)::after {
		content: '→';
		position: absolute;
		right: -17px;
		top: 50%;
		transform: translateY(-50%);
		color: var(--faint);
		font-size: 16px;
	}
	.j1-node {
		min-width: 0;
	}
	.j1-node a {
		min-width: 0;
		display: flex;
		flex-direction: column;
		align-items: flex-start;
		gap: 3px;
		height: 100%;
		padding: 10px 12px;
		border: 1px solid var(--line);
		border-radius: var(--r);
		background: var(--sunk);
		color: inherit;
		text-decoration: none;
	}
	.j1-node a:hover {
		border-color: var(--accent);
	}
	.j1-node b {
		font-size: 15px;
		overflow-wrap: anywhere;
	}
	.j1-node .cell-sub {
		margin: 0;
	}
	.j1-node.st-live a {
		border-color: color-mix(in srgb, var(--ok) 60%, var(--line));
		background: color-mix(in srgb, var(--ok) 9%, var(--panel));
	}
	.j1-node.st-blocked a {
		border-color: color-mix(in srgb, var(--err) 50%, var(--line));
	}
	.j1-node.st-pending a {
		border-style: dashed;
		border-color: var(--accent);
	}
	.j1-node.st-behind a {
		border-color: color-mix(in srgb, var(--warn) 60%, var(--line));
	}
	.j1-rel-detail {
		display: grid;
		grid-template-columns: minmax(0, 1fr) minmax(0, 1.4fr);
		gap: 20px;
	}
	.j1-pins li {
		display: flex;
		justify-content: space-between;
		gap: 8px;
		align-items: center;
	}
	.j1-kind {
		display: inline-block;
		min-width: 82px;
		color: var(--muted);
		font-size: 12px;
	}
	.j1-rel-line {
		margin: 2px 0 10px;
		overflow-wrap: anywhere;
	}
	.miss {
		margin: 4px 0 6px;
		padding-left: 18px;
	}

	.j1-kpi-head {
		display: flex;
		justify-content: space-between;
		align-items: baseline;
		margin: 18px 0 8px;
	}
	.j1-kpi-head h2 {
		margin: 0;
		font-size: 15px;
	}
	.kpis {
		margin-bottom: 16px;
	}

	.j1-unrel-note {
		margin: 0 0 8px;
		font-size: 13px;
	}
	.j1-changes li {
		display: grid;
		grid-template-columns: 92px minmax(0, 1fr) auto;
		gap: 8px;
		align-items: baseline;
	}
	.j1-when {
		color: var(--muted);
		font-size: 12px;
	}
	.j1-change {
		overflow-wrap: anywhere;
	}

	@media (max-width: 900px) {
		.j1-now,
		.j1-rel-detail {
			grid-template-columns: minmax(0, 1fr);
		}
		.j1-flow {
			grid-template-columns: repeat(2, minmax(0, 1fr));
		}
		.j1-node:nth-child(2)::after {
			display: none;
		}
	}
	@media (max-width: 560px) {
		.j1-flow {
			grid-template-columns: minmax(0, 1fr);
			gap: 18px;
		}
		.j1-node:not(:last-child)::after {
			content: '↓';
			display: block;
			right: auto;
			left: 16px;
			top: auto;
			bottom: -18px;
			transform: none;
		}
		.j1-signals {
			grid-template-columns: repeat(2, minmax(0, 1fr));
		}
		.j1-changes li {
			grid-template-columns: minmax(0, 1fr);
			gap: 0;
		}
	}
</style>
