<script lang="ts">
	// J19 Cases — the job's live caseload. Every case is held by someone: the agent, the customer (verify / confirm),
	// staff (after a handover) — or it failed. The page answers, in order: which cases need a person now (claim, check
	// an unknown write outcome, chase an overdue SLA); who holds the open cases; and the full history with the release
	// each case ran on (03 §2.7) and its environment (D54: test cases ran on the unpublished draft). Live: refreshes on a
	// timer and marks what moved. J20 opens one case; J5 explains its activity. Spec 14 §2 J19, 03 §2.7.
	import { onMount } from 'svelte';
	import { page } from '$app/state';
	import { goto } from '$app/navigation';
	import { api, app, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { writeAttrs } from '$lib/auth/roles';
	import { SUPPORT_ROLES_NOTE } from '$lib/runtime/support-D55';
	import { Resource } from '$lib/load.svelte';
	import { setQuery } from '$lib/query';
	import { jobPath } from '$lib/scope';
	import { workspaceHref } from '$lib/support/links';
	import type { CaseRow, CaseStatus, Env } from '$lib/api/types/runtime';
	import { CASE_STATUSES, STATUS_LABEL, STATUS_WORD, activityLabel, plural, when } from '$lib/runtime/labels';
	import {
		LANES,
		URGENCY_LABEL,
		byLane,
		changedIds,
		duration,
		holderOf,
		identityText,
		laneOfStatus,
		minutesBetween,
		needsPerson,
		releaseNote,
		slaOf,
	} from '$lib/runtime/J19-caseload';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Freshness from '$lib/ui/Freshness.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Status from '$lib/ui/Status.svelte';
	import Pagination from '$lib/ui/Pagination.svelte';

	const LIVE_MS = 5000;
	const LANE_CAP = 5;

	const status = $derived((page.url.searchParams.get('status') ?? 'all') as CaseStatus | 'all');
	const env = $derived((page.url.searchParams.get('env') ?? 'all') as Env | 'all');
	const q = $derived(page.url.searchParams.get('q') ?? '');
	const before = $derived(page.url.searchParams.get('before') ?? '');
	const filtered = $derived(status !== 'all' || env !== 'all' || q !== '');
	const focusLane = $derived(laneOfStatus(status));

	const load = new Resource(
		() => api().caseload({ env }),
		() => false,
	);
	const list = new Resource(() => api().cases({ status, env, q, before: before || null }));
	$effect(() => {
		void env;
		load.value = { state: 'loading' };
		void load.refresh();
	});
	$effect(() => {
		void status;
		void env;
		void q;
		void before;
		list.value = { state: 'loading' };
		void list.refresh();
	});

	// Live: a fake timer re-reads the caseload (cases opened from /chat or /console appear on their own) and marks
	// the cases that are new or moved since the previous read.
	let live = $state(true);
	let fresh = $state<Set<string>>(new Set());
	let lastMove = $state<{ n: number; at: string } | null>(null);
	let snapshot: CaseRow[] | null = null;
	let snapshotEnv: string | null = null;
	$effect(() => {
		const v = load.value;
		if (v.state !== 'ok') return;
		const rows = v.data.active;
		const ids = snapshotEnv === env ? changedIds(snapshot, rows) : [];
		snapshot = rows;
		snapshotEnv = env;
		if (ids.length) {
			fresh = new Set(ids);
			lastMove = { n: ids.length, at: new Date().toLocaleTimeString('en-GB', { hour12: false }) };
		}
	});
	onMount(() => {
		const t = setInterval(() => {
			if (!live) return;
			void load.refresh();
			if (!before) void list.refresh();
		}, LIVE_MS);
		return () => clearInterval(t);
	});

	let search = $state(page.url.searchParams.get('q') ?? '');
	const href = (sub: string) => jobPath(app.job, sub);
	const caseHref = (id: string) => href(`/cases/${encodeURIComponent(id)}`);
	const activityHref = (a: string) => href(`/journey/${encodeURIComponent(a)}`);
	const open = (id: string) => goto(caseHref(id));
	const setEnv = (v: string) => void setQuery({ env: v === 'all' ? '' : v, before: '', previous: '' });
	const showLane = (statusKey: string) => void setQuery({ status: statusKey, before: '', previous: '' });

	const ENVS: [Env | 'all', string][] = [
		['all', 'All'],
		['prod', 'Production'],
		['test', 'Test'],
	];
	const FIRST_STATUS = { agent: 'open', customer: 'identity_required', staff: 'handover', failed: 'failed' };
</script>

<PageHead
	id="J19"
	sub="Who holds each of this job's cases right now, the agent, the customer or staff, and which ones need a person."
>
	{#snippet actions()}
		<button
			class="btn sm j19-live"
			class:on={live}
			type="button"
			aria-pressed={live}
			title={live ? `Re-reads every ${LIVE_MS / 1000} s` : 'Live updates paused'}
			onclick={() => (live = !live)}
			><span class="j19-pulse" aria-hidden="true"></span>{live ? 'Live' : 'Paused'}</button
		>
		<Freshness resource={load} />
		<div class="seg" role="group" aria-label="Environment">
			{#each ENVS as [id, label] (id)}
				<button
					type="button"
					class="tab"
					class:on={id === env}
					aria-pressed={id === env}
					onclick={() => setEnv(id)}>{label}</button
				>
			{/each}
		</div>
	{/snippet}
</PageHead>

<LoadState resource={load}>
	{#snippet children(cl)}
		{@const asOf = cl.as_of}
		{@const people = needsPerson(cl.active, asOf)}
		{@const lanes = byLane(cl.active)}
		{@const fleet = cl.fleet}
		<p class="j19-sr" aria-live="polite">
			{lastMove ? `${plural(lastMove.n, 'case')} moved at ${lastMove.at}` : ''}
		</p>

		<!-- 1 · what needs a person now -->
		<section class="j19-now" data-testid="needs-person" aria-labelledby="j19-now-h">
			<header class="j19-sec-head">
				<h2 id="j19-now-h">Needs a person now</h2>
				<span class="muted"
					>{people.length
						? `${plural(people.length, 'case')} the agent cannot move on its own`
						: 'Nothing waits on staff'}{lastMove
						? ` · ${plural(lastMove.n, 'case')} moved at ${lastMove.at}`
						: ''}</span
				>
			</header>
			{#if people.length && writeAttrs(session, 'support_agent').disabled}
				<p class="muted small" data-testid="support-roles-note">{SUPPORT_ROLES_NOTE}</p>
			{/if}
			{#if people.length}
				<ol class="j19-people list-plain">
					{#each people as p (p.c.case_id)}
						<li class="j19-person u-{p.urgency}" class:j19-fresh={fresh.has(p.c.case_id)}>
							<span class="j19-urg" title="Urgency">{URGENCY_LABEL[p.urgency]}</span>
							<div class="j19-person-main">
								<div class="j19-person-title">
									<a class="mono" href={caseHref(p.c.case_id)}>{p.c.case_id}</a>
									<span class="cell-title">{p.c.title}</span>
									{#if p.c.env === 'test'}<Chip text="test" variant="warn" />{/if}
								</div>
								<span class="j19-why">{p.why}</span>
								<span class="cell-sub"
									>{p.c.customer_name} · {identityText(p.c.identity_level)} ·
									<a href={activityHref(p.c.activity)}>{activityLabel(p.c.activity)}</a></span
								>
							</div>
							<div class="j19-person-side">
								{#if p.sla}
									<span class="j19-sla s-{p.sla.kind}">{p.sla.text}</span>
								{:else}
									<span class="cell-sub">since {when(p.c.updated_at)}</span>
								{/if}
								{#if p.action === 'claim'}
									<a
										class="btn sm primary"
										href={p.c.conversation_id
											? workspaceHref(app.job, p.c.conversation_id)
											: caseHref(p.c.case_id)}>{p.c.conversation_id ? 'Open in workspace' : 'Open'}</a
									>
								{:else if p.action === 'check'}
									<a class="btn sm" href={caseHref(p.c.case_id)}>Check the timeline</a>
								{:else}
									<a class="btn sm" href={caseHref(p.c.case_id)}>Open</a>
								{/if}
							</div>
						</li>
					{/each}
				</ol>
			{:else}
				<p class="j19-calm">
					No unassigned handovers, no failed writes. The agent and the customers hold every open case.
				</p>
			{/if}
		</section>

		<!-- 2 · who holds the open cases -->
		<section class="j19-board-wrap" aria-labelledby="j19-board-h">
			<header class="j19-sec-head">
				<h2 id="j19-board-h">Who holds the open cases</h2>
				<span class="muted"
					>A case moves left to right as it runs: the agent works it, may wait on the customer, hands it to
					staff, or stops.</span
				>
			</header>
			<div class="j19-board" data-testid="case-lanes">
				{#each LANES as l (l.key)}
					{@const rows = lanes[l.key]}
					<section
						class="j19-lane t-{l.tone}"
						class:focus={focusLane === l.key}
						aria-label="{l.label}: {rows.length}"
						data-lane={l.key}
					>
						<header class="j19-lane-head">
							<span class="j19-dot" aria-hidden="true"></span>
							<strong>{l.label}</strong>
							<span class="j19-count">{rows.length}</span>
						</header>
						<p class="j19-lane-hint">{l.hint} <span class="muted">{l.next}</span></p>
						{#each rows.slice(0, LANE_CAP) as c (c.case_id)}
							{@const rel = releaseNote(c, fleet)}
							{@const sla = slaOf(c, asOf)}
							<a class="j19-card" class:j19-fresh={fresh.has(c.case_id)} href={caseHref(c.case_id)}>
								<span class="j19-card-top">
									<span class="mono">{c.case_id}</span>
									{#if c.env === 'test'}<Chip text="test · draft" variant="warn" title={rel.title} />{/if}
									{#if rel.kind === 'older'}<Chip text={rel.text} variant="muted" title={rel.title} />{/if}
								</span>
								<span class="j19-card-title">{c.title}</span>
								<span class="cell-sub">{c.customer_name} · L{c.identity_level}</span>
								<span class="j19-card-foot">
									{#if l.key === 'staff'}
										<span>{c.assignee_name ? `Assigned · ${c.assignee_name}` : 'Unassigned'}</span>
										{#if sla}<span class="j19-sla s-{sla.kind}">{sla.text}</span>{/if}
									{:else if l.key === 'customer'}
										<span>{STATUS_LABEL[c.status]}</span>
										<span>waiting {duration(minutesBetween(c.updated_at, asOf))}</span>
									{:else if l.key === 'failed'}
										<span class="mono">{c.alert ?? c.outcome ?? 'failed'}</span>
									{:else}
										<span>{activityLabel(c.activity)}</span>
										<span>{when(c.updated_at)}</span>
									{/if}
								</span>
							</a>
						{:else}
							<p class="j19-lane-empty">None</p>
						{/each}
						{#if rows.length > LANE_CAP}
							<button class="btn sm" type="button" onclick={() => showLane(FIRST_STATUS[l.key])}
								>{rows.length - LANE_CAP} more in the list</button
							>
						{/if}
					</section>
				{/each}
			</div>
			<p class="j19-runs">
				<span
					><strong>Production</strong> runs the release
					<span class="mono">{fleet.production.release?.id ?? '(none yet)'}</span></span
				>
				<span
					><strong>Test</strong> runs the draft{fleet.test.unpublished
						? `: ${plural(fleet.test.unpublished, 'unpublished change')}`
						: ' (no unpublished changes)'}</span
				>
				<span class="j19-runs-links">
					<a href="/chat">Test in test chat</a>
					<a href={href('/release')}>Review &amp; publish</a>
				</span>
			</p>
		</section>

		<!-- 3 · every case -->
		<section aria-labelledby="j19-all-h">
			<header class="j19-sec-head j19-all-head">
				<div>
					<h2 id="j19-all-h">All cases</h2>
					<span class="muted"
						>Closed in the last 7 days: {cl.closed_7d.resolved} resolved · {cl.closed_7d.not_eligible} not eligible{cl
							.closed_7d.cancelled_offboard
							? ` · ${cl.closed_7d.cancelled_offboard} cancelled (offboard)`
							: ''}. Each case keeps the release it started on.</span
					>
				</div>
				<div class="j19-filters">
					<label class="field">
						<span class="label">Status</span>
						<select
							class="input"
							value={status}
							onchange={(e) =>
								setQuery({
									status: e.currentTarget.value === 'all' ? '' : e.currentTarget.value,
									before: '',
									previous: '',
								})}
						>
							<option value="all">All statuses</option>
							{#each CASE_STATUSES as s (s)}<option value={s}>{STATUS_LABEL[s]}</option>{/each}
						</select>
					</label>
					<form
						class="field"
						role="search"
						onsubmit={(e) => {
							e.preventDefault();
							void setQuery({ q: search.trim(), before: '', previous: '' });
						}}
					>
						<span class="label">Search</span>
						<input
							class="input j19-search"
							type="search"
							placeholder="Case, customer, activity…"
							bind:value={search}
						/>
					</form>
				</div>
			</header>
		</section>
	{/snippet}
</LoadState>

<LoadState resource={list}>
	{#snippet empty()}
		{#if filtered || before}
			No cases match these filters.
			<button
				class="btn sm"
				type="button"
				onclick={() => {
					search = '';
					void setQuery({ status: '', env: '', q: '', before: '', previous: '' });
				}}>Clear filters</button
			>
		{:else}
			No cases yet. They appear when a customer (or a staff test chat) writes to this job.
		{/if}
	{/snippet}
	{#snippet children(p)}
		{@const fleet = load.value.state === 'ok' ? load.value.data.fleet : null}
		<Card cls="flush">
			<div class="tbl-wrap">
				<table class="tbl clickable" data-testid="cases-table">
					<thead>
						<tr
							><th>Case</th><th>Request</th><th>Held by · status</th><th>Ran on</th><th>Identity</th><th
								>Updated</th
							></tr
						>
					</thead>
					<tbody>
						{#each p.items as c (c.case_id)}
							{@const h = holderOf(c)}
							{@const rel = releaseNote(c, fleet)}
							<tr onclick={() => open(c.case_id)} class:j19-fresh={fresh.has(c.case_id)}>
								<td
									><a
										class="table-action mono"
										data-case-link
										href={caseHref(c.case_id)}
										onclick={(e) => e.stopPropagation()}>{c.case_id}</a
									><span class="cell-sub">{c.customer_name}</span></td
								>
								<td
									><span class="cell-title">{c.title}</span><a
										class="cell-sub j19-act"
										href={activityHref(c.activity)}
										title="What this activity does (J5)"
										onclick={(e) => e.stopPropagation()}>{activityLabel(c.activity)}</a
									></td
								>
								<td>
									<span class="j19-held h-{h}"
										>{h === 'closed' ? 'Closed' : LANES.find((l) => l.key === h)?.label}</span
									>
									<Status word={STATUS_WORD[c.status]} label={STATUS_LABEL[c.status]} />
									{#if c.alert}<Chip
											text={c.alert}
											variant="err"
											title="Alert: never retried automatically"
										/>{/if}
									{#if c.outcome && !['ok', 'handover', c.status].includes(c.outcome)}<span
											class="cell-sub mono">{c.outcome}</span
										>{/if}
									{#if c.status === 'handover'}<span class="cell-sub"
											>{c.assignee_name ? `assigned · ${c.assignee_name}` : 'unassigned'}{c.sla
												? ` · due ${when(c.sla.due_at)}`
												: ''}</span
										>{/if}
								</td>
								<td>
									<span class="j19-ran">
										<Chip
											text={c.env === 'prod' ? 'prod' : 'test · draft'}
											variant={c.env === 'prod' ? 'ok' : 'warn'}
											title={c.env === 'test' ? rel.title : undefined}
										/>
										{#if rel.kind === 'older'}<Chip text="older" variant="muted" title={rel.title} />{/if}
									</span>
									<span class="cell-sub mono" title={rel.title}
										>{c.release_id ?? `draft rev ${c.draft_rev ?? '?'}`}</span
									>
									<span class="cell-sub">{c.channel === 'staff' ? 'staff test chat' : 'Telegram'}</span>
								</td>
								<td class="j19-id">{identityText(c.identity_level)}</td>
								<td>{when(c.updated_at)}<span class="cell-sub">{plural(c.op_calls, 'op call')}</span></td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		</Card>
		<Pagination next={p.next} count={p.items.length} busy={list.refreshing} />
	{/snippet}
</LoadState>

<style>
	.j19-sr {
		position: absolute;
		width: 1px;
		height: 1px;
		overflow: hidden;
		clip: rect(0 0 0 0);
		white-space: nowrap;
	}
	.j19-live {
		display: inline-flex;
		align-items: center;
		gap: 6px;
	}
	.j19-pulse {
		width: 8px;
		height: 8px;
		border-radius: 50%;
		background: var(--faint);
	}
	.j19-live.on .j19-pulse {
		background: var(--ok);
		animation: j19-pulse 2s ease-in-out infinite;
	}
	@keyframes j19-pulse {
		50% {
			opacity: 0.35;
		}
	}
	.j19-sec-head {
		display: flex;
		flex-wrap: wrap;
		align-items: baseline;
		gap: 4px 12px;
		margin: 0 0 10px;
	}
	.j19-sec-head h2 {
		font-size: 1.05rem;
		margin: 0;
	}
	section + section,
	.j19-board-wrap {
		margin-top: 22px;
	}

	/* 1 · needs a person */
	.j19-now {
		border: 1px solid var(--line);
		border-left: 4px solid var(--err);
		background: var(--panel);
		border-radius: var(--r);
		padding: 14px 16px;
	}
	.j19-people {
		display: grid;
		gap: 0;
		margin: 0;
	}
	.j19-person {
		display: grid;
		grid-template-columns: 64px minmax(0, 1fr) auto;
		gap: 4px 14px;
		align-items: start;
		padding: 10px 0;
		border-top: 1px solid var(--line-2);
	}
	.j19-person:first-child {
		border-top: 0;
	}
	.j19-urg {
		font-family: var(--cond);
		font-size: 0.72rem;
		font-weight: 600;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		text-align: center;
		padding: 3px 0;
		border-radius: 4px;
		color: var(--warn);
		background: color-mix(in srgb, var(--warn) 14%, transparent);
	}
	.u-now .j19-urg {
		color: var(--err);
		background: color-mix(in srgb, var(--err) 14%, transparent);
	}
	.u-chase .j19-urg {
		color: var(--muted);
		background: var(--sunk);
	}
	.j19-person-main {
		display: grid;
		gap: 2px;
		min-width: 0;
	}
	.j19-person-title {
		display: flex;
		flex-wrap: wrap;
		align-items: baseline;
		gap: 4px 8px;
	}
	.j19-why {
		color: var(--fg);
	}
	.j19-person-side {
		display: grid;
		justify-items: end;
		gap: 6px;
	}
	.j19-sla {
		font-size: 0.8rem;
		white-space: nowrap;
		color: var(--muted);
	}
	.j19-sla.s-overdue {
		color: var(--err);
		font-weight: 600;
	}
	.j19-sla.s-due_soon {
		color: var(--warn);
		font-weight: 600;
	}
	.j19-calm {
		margin: 0;
		color: var(--muted);
	}

	/* 2 · lanes */
	.j19-board {
		display: grid;
		grid-template-columns: repeat(4, minmax(0, 1fr));
		gap: 12px;
	}
	.j19-lane {
		--tone: var(--accent);
		display: flex;
		flex-direction: column;
		gap: 8px;
		background: var(--sunk);
		border: 1px solid var(--line-2);
		border-top: 3px solid var(--tone);
		border-radius: var(--r);
		padding: 10px;
		min-width: 0;
	}
	.j19-lane.t-warn {
		--tone: var(--warn);
	}
	.j19-lane.t-work {
		--tone: var(--work);
	}
	.j19-lane.t-err {
		--tone: var(--err);
	}
	.j19-lane.focus {
		outline: 2px solid var(--tone);
		outline-offset: 2px;
	}
	.j19-lane-head {
		display: flex;
		align-items: center;
		gap: 8px;
	}
	.j19-dot {
		width: 9px;
		height: 9px;
		border-radius: 50%;
		background: var(--tone);
	}
	.j19-count {
		margin-left: auto;
		font-family: var(--mono);
		font-weight: 600;
		font-size: 1.05rem;
	}
	.j19-lane-hint {
		margin: 0;
		font-size: 0.8rem;
		line-height: 1.35;
	}
	.j19-lane-empty {
		margin: 0;
		font-size: 0.82rem;
		color: var(--faint);
	}
	.j19-card {
		display: grid;
		gap: 2px;
		padding: 8px 10px;
		background: var(--panel);
		border: 1px solid var(--line);
		border-radius: var(--r);
		color: var(--fg);
		text-decoration: none;
		min-width: 0;
	}
	.j19-card:hover,
	.j19-card:focus-visible {
		border-color: var(--tone);
	}
	.j19-card-top {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 4px 6px;
		font-size: 0.8rem;
		color: var(--accent);
	}
	.j19-card-title {
		font-weight: 500;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.j19-card-foot {
		display: flex;
		flex-wrap: wrap;
		justify-content: space-between;
		gap: 2px 8px;
		font-size: 0.78rem;
		color: var(--muted);
	}
	.j19-runs {
		display: flex;
		flex-wrap: wrap;
		gap: 4px 18px;
		margin: 10px 0 0;
		font-size: 0.85rem;
		color: var(--muted);
	}
	.j19-runs strong {
		color: var(--fg);
	}
	.j19-runs-links {
		display: inline-flex;
		gap: 12px;
		margin-left: auto;
	}

	/* 3 · all cases */
	.j19-all-head {
		justify-content: space-between;
		align-items: flex-end;
	}
	.j19-all-head > div:first-child {
		display: grid;
		gap: 2px;
	}
	.j19-filters {
		display: flex;
		flex-wrap: wrap;
		gap: 8px 12px;
	}
	.j19-search {
		min-width: 220px;
	}
	.j19-act {
		color: var(--accent);
	}
	.j19-held {
		display: block;
		font-size: 0.75rem;
		font-family: var(--cond);
		text-transform: uppercase;
		letter-spacing: 0.03em;
		color: var(--muted);
	}
	.j19-held.h-failed {
		color: var(--err);
	}
	.j19-held.h-staff {
		color: var(--work);
	}
	.j19-held.h-customer {
		color: var(--warn);
	}
	.j19-held.h-agent {
		color: var(--accent);
	}
	.j19-ran {
		display: inline-flex;
		gap: 4px;
	}
	.j19-id {
		white-space: nowrap;
	}
	.j19-fresh {
		animation: j19-flash 2.4s ease-out;
	}
	@keyframes j19-flash {
		from {
			box-shadow: 0 0 0 2px var(--accent);
			background: color-mix(in srgb, var(--accent) 12%, var(--panel));
		}
	}

	@media (max-width: 1100px) {
		.j19-board {
			grid-template-columns: repeat(2, minmax(0, 1fr));
		}
	}
	@media (max-width: 640px) {
		.j19-board {
			grid-template-columns: minmax(0, 1fr);
		}
		.j19-person {
			grid-template-columns: minmax(0, 1fr);
		}
		.j19-urg {
			justify-self: start;
			padding: 2px 8px;
		}
		.j19-person-side {
			justify-items: start;
			grid-auto-flow: column;
			align-items: center;
			justify-content: start;
		}
		.j19-runs-links {
			margin-left: 0;
		}
		.j19-search {
			min-width: 0;
			width: 100%;
		}
	}
</style>
