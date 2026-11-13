<script lang="ts">
	// T2 Workspace home. It answers, in order: which agents are live (the one status, `agentStatus()` per agent), what
	// needs me (the one inbox, `inbox()`, each item linked to where it is decided), how my agents did this week, and
	// whether setup is complete. The Integrations panel below keeps Retry (tenant_admin + super_admin) and Cancel
	// (super_admin).
	import { onMount } from 'svelte';
	import { api, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { writeAttrs } from '$lib/auth/roles';
	import { Resource } from '$lib/load.svelte';
	import { jobPath } from '$lib/scope';
	import { fmt } from '$lib/format';
	import type { IntegrationRow, ProvisionRow, TenantHome, TenantOverview } from '$lib/api/types/tenant';
	import type { JobSummary } from '$lib/api/types/core';
	import type { AgentStatus, Inbox as InboxData } from '$lib/api/types/status';
	import { visibleItems } from '$lib/status/inbox';
	import { day, integrationChip, TARGET_LABEL, when } from '$lib/tenant/ui';
	import { L, releaseLabel, subjectLabel } from '$lib/ui/labels';
	import { prefs } from '$lib/ui/prefs.svelte';
	import TechDetail from '$lib/ui/TechDetail.svelte';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import KeyValues from '$lib/ui/KeyValues.svelte';
	import MilestoneChip from '$lib/ui/MilestoneChip.svelte';
	import Freshness from '$lib/ui/Freshness.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';
	import Inbox from '$lib/ui/Inbox.svelte';
	import StatusChip from '$lib/ui/StatusChip.svelte';
	import CalendarCard from './CalendarCard.svelte';

	const res = new Resource<TenantOverview>(
		() => api().tenantOverview(),
		() => false,
	);
	const home = new Resource<TenantHome>(
		() => api().tenantHome(),
		() => false,
	);
	/** Everything that needs a person, across all agents (`GET /api/inbox`). */
	const box = new Resource<InboxData>(
		() => api().inbox(),
		() => false,
	);
	/** Each agent's one status (`GET /api/jobs/{job}/status`). */
	const agents = new Resource<{ job: JobSummary; status: AgentStatus }[]>(
		async () => {
			const jobs = (await api().jobs()).items;
			return Promise.all(jobs.map(async (job) => ({ job, status: await api().agentStatus(job.key) })));
		},
		() => false,
	);
	onMount(() => {
		void res.refresh();
		void home.refresh();
		void box.refresh();
		void agents.refresh();
	});
	const admin = $derived(writeAttrs(session, 'tenant_admin').disabled === false);
	const homeData = $derived(home.value.state === 'ok' ? home.value.data : null);
	let busy = $state('');
	let openHistory = $state<string | null>(null);

	const canRetry = (r: ProvisionRow | null) =>
		!!r && (r.status === 'pending' || r.status === 'failed' || r.status === 'dead');

	async function refreshAll() {
		await Promise.all([res.refresh(), home.refresh(), box.refresh(), agents.refresh()]);
	}
	async function retry(row: IntegrationRow) {
		if (!row.current) return;
		busy = row.target;
		await guard(
			() => api().retryProvision(row.current!.id),
			`${row.label}: queued again (attempts reset).`,
			'Not retried',
		);
		busy = '';
		await refreshAll();
	}
	async function cancel(row: IntegrationRow) {
		if (!row.current) return;
		busy = row.target;
		await guard(
			() => api().cancelProvision(row.current!.id),
			`${row.label}: provisioning cancelled.`,
			'Not cancelled',
		);
		busy = '';
		await refreshAll();
	}
	const licenceVariant = (s: string) => (s === 'active' ? 'ok' : s === 'none' ? 'muted' : 'err');
	const targetNames = (t: string[]) => t.map((x) => TARGET_LABEL[x] ?? x).join(', ');
	const histVariant = (s: string) =>
		s === 'succeeded' ? 'ok' : s === 'pending' ? 'warn' : s === 'cancelled' ? 'muted' : 'err';
</script>

<PageHead id="T2" sub="Which agents are live, what needs you, and how your agents did this week.">
	{#snippet actions()}
		<Freshness resource={res} />
		<a class="btn" href="/admin/users">Invite teammate</a>
		{#if admin}<a class="btn primary" href="/admin/jobs">New agent</a>{/if}
	{/snippet}
</PageHead>

<LoadState resource={res}>
	{#snippet children(o)}
		{@const rows = agents.value.state === 'ok' ? agents.value.data : []}
		{@const names = Object.fromEntries(o.jobs.map((j) => [j.key, j.label]))}
		{@const mine = box.value.state === 'ok' ? visibleItems(box.value.data.items, session.effectiveRoles) : []}
		{@const need = mine.filter((i) => i.severity !== 'info').length}
		{@const live = rows.filter(
			(r) => r.status.state === 'live' || r.status.state === 'live_with_issues',
		).length}
		{@const failedRows = o.integrations.filter((r) => r.chip === 'failed' || r.chip === 'dead')}
		{@const settingUp = o.integrations.filter((r) => r.chip === 'pending')}
		{@const setupOk = o.licence.status === 'active' && failedRows.length === 0 && settingUp.length === 0}

		{@const summary = [
			`${o.tenant.name}${o.tenant.brand ? ` · ${o.tenant.brand}` : ''}`,
			o.jobs.length === 0
				? 'no agents yet'
				: `${live} of ${o.jobs.length} ${o.jobs.length === 1 ? 'agent is' : 'agents are'} live`,
			need === 0 ? 'nothing needs you' : `${need} ${need === 1 ? 'item needs' : 'items need'} you`,
			...(o.tenant.residency ? [`data stored in the ${o.tenant.residency.toUpperCase()}`] : []),
		].join(' · ')}
		<p class="ws-summary muted" data-testid="ws-summary">{summary}</p>

		<div class="ws-grid">
			<Card title="Needs you" sub="Across all agents · most urgent first">
				{#if box.value.state === 'loading'}
					<p class="muted">Loading…</p>
				{:else if box.value.state === 'unavailable'}
					<Unavailable reason={box.value.reason} retry={box.retry} />
				{:else}
					<Inbox
						items={box.value.state === 'ok' ? box.value.data.items : []}
						jobLabel={(k) => names[k] ?? k}
						empty="Nothing is waiting on you. Your agents are running on their live versions."
					/>
				{/if}
			</Card>

			<Card
				title="This week"
				sub={homeData ? `${day(homeData.activity.from)} – ${day(homeData.activity.to)}` : ''}
			>
				{#snippet actions()}<a class="btn sm" href="/admin/usage">Usage</a>{/snippet}
				{#if home.value.state === 'loading'}
					<p class="muted">Loading…</p>
				{:else if home.value.state === 'unavailable'}
					<Unavailable reason={home.value.reason} retry={home.retry} />
				{:else if homeData}
					{@const a = homeData.activity}
					{@const ok = a.outcomes.success}
					{@const all = (Object.values(a.outcomes) as number[]).reduce((n, v) => n + v, 0)}
					<div class="ws-nums" data-testid="ws-week">
						<div class="ws-num"><b>{fmt.n(a.cases)}</b><span>conversations</span></div>
						<div class="ws-num">
							<b>{fmt.pct(a.containment, 0)}</b><span>{L.contained.toLowerCase()}</span>
						</div>
						<div class="ws-num"><b>{fmt.k(a.turns)}</b><span>customer messages answered</span></div>
						<div class="ws-num">
							<b
								>{fmt.n(ok)}{#if all}<small>/{fmt.n(all)}</small>{/if}</b
							><span>actions on your systems succeeded</span>
						</div>
					</div>
					{#if !a.attributed}
						<p class="note">Usage is not attributed to this workspace until the licence is issued.</p>
					{/if}
				{/if}
			</Card>
		</div>

		<section id="jobs" class="t2-anchor ws-agents">
			<Card title={L.jobs}>
				{#snippet actions()}<a class="btn sm" href="/admin/jobs">All agents</a>{/snippet}
				{#if o.jobs.length === 0}
					<div class="ws-empty" data-testid="ws-empty">
						<h2>No agents yet</h2>
						{#if admin}
							<p class="muted">Create your first agent from a template.</p>
							<a class="btn primary" href="/admin/jobs">New agent</a>
						{:else}
							<p class="muted">Ask your admin to create the first agent.</p>
						{/if}
					</div>
				{:else if agents.value.state === 'loading'}
					<p class="muted">Loading…</p>
				{:else if agents.value.state === 'unavailable'}
					<Unavailable reason={agents.value.reason} retry={agents.retry} />
				{:else}
					<div class="tbl-wrap">
						<table class="tbl" data-testid="ws-agents">
							<thead>
								<tr
									><th>Agent</th><th>Status</th><th>Live version</th><th class="r">Open cases</th><th
										class="r">{L.contained} (30 d)</th
									><th></th></tr
								>
							</thead>
							<tbody>
								{#each rows as r (r.job.key)}
									{@const c = r.status.open_cases}
									<tr data-job={r.job.key}>
										<td
											><a href={jobPath(r.job.key)}><b>{r.job.label}</b></a>
											<TechDetail><span class="cell-sub mono">{r.job.key}</span></TechDetail></td
										>
										<td><StatusChip status={r.status} /></td>
										<td>
											{#if r.status.live_release}<span data-testid="live-release"
													>{releaseLabel(r.status.live_release.id)}</span
												>{:else}<span class="muted">Not live yet</span>{/if}
										</td>
										<td class="r"
											>{c.agent + c.staff + c.waiting + c.failed}{#if c.staff_tests}
												<span class="muted">({c.staff_tests} staff tests)</span>{/if}</td
										>
										<td class="r">{fmt.pct(r.status.resolved_by_agent_30d, 0)}</td>
										<td class="r"><a class="btn sm" href={jobPath(r.job.key)}>Open</a></td>
									</tr>
								{/each}
							</tbody>
						</table>
					</div>
				{/if}
			</Card>
		</section>

		<div class="ws-setup" data-testid="ws-setup">
			<Chip text={setupOk ? 'Setup complete' : 'Setup needs attention'} variant={setupOk ? 'ok' : 'warn'} />
			<span class="grow muted">
				Licence {o.licence.status} · {o.integrations.filter((r) => r.chip === 'ready').length} of {o
					.integrations.length} connected services ready{#if failedRows.length}
					· {failedRows.map((r) => r.label).join(', ')} stopped{/if}{#if settingUp.length}
					· {settingUp.map((r) => r.label).join(', ')} still being set up{/if}
				<TechDetail>
					{#each o.blocks as b (b.code)}<code>{b.status} {b.code}</code>{/each}
				</TechDetail>
			</span>
			<a class="btn sm" href="#integrations">Details</a>
		</div>

		<div class="grid g-main">
			<section id="integrations" class="t2-anchor">
				<Card
					title="Integrations"
					sub="Provisioned down from this app: your licence, knowledge search and document ingest."
				>
					{#snippet actions()}
						{#if o.tenant.residency}<span
								data-testid="t2-residency"
								title="Data residency, set at provisioning (D59): pins your stores, LLM providers and logs"
								><Chip text={`data in ${o.tenant.residency.toUpperCase()}`} variant="tool" /></span
							>{/if}
						<MilestoneChip milestone="M3" />
					{/snippet}
					<div class="tbl-wrap">
						<table class="tbl" data-testid="integrations">
							<thead>
								<tr
									><th>Target</th><th>Status</th><th class="r">Attempts</th><th>Last success</th><th
										>Last error</th
									><th aria-label="Row actions"></th></tr
								>
							</thead>
							<tbody>
								{#each o.integrations as r (r.target)}
									{@const chip = integrationChip(r)}
									<tr data-target={r.target}>
										<td
											><b>{r.label}</b><span class="cell-sub mono"
												>{r.current ? `${r.current.op}` : r.target}</span
											></td
										>
										<td>
											<Chip text={chip.text} variant={chip.variant} />
											{#each r.drift as d (d.slot)}
												<span class="cell-sub"
													><Chip
														text={`drift: ${d.slot} v${d.local_version} → v${d.remote_version}`}
														variant="warn"
														title="Another app changed this slot. Nothing was overwritten."
													/></span
												>
											{/each}
										</td>
										<td class="r mono"
											>{r.current ? `${r.current.attempts} / ${r.current.max_attempts}` : '—'}</td
										>
										<td>{when(r.last_success_at)}</td>
										<td>
											{#if r.current?.last_error}
												<span class="mono"
													>{r.current.last_http_status ?? ''} {r.current.last_error_code}</span
												>
												<span class="cell-sub">{r.current.last_error}</span>
											{:else}<span class="muted">—</span>{/if}
										</td>
										<td class="r">
											<div class="row-actions">
												{#if canRetry(r.current)}
													<button
														class="btn sm"
														type="button"
														{...writeAttrs(session, 'tenant_admin')}
														disabled={writeAttrs(session, 'tenant_admin').disabled || busy !== ''}
														onclick={() => retry(r)}>Retry</button
													>
													<button
														class="btn sm"
														type="button"
														{...writeAttrs(session, 'tenant_admin')}
														disabled={writeAttrs(session, 'tenant_admin').disabled || busy !== ''}
														onclick={() => cancel(r)}>Cancel</button
													>
												{/if}
												<button
													class="btn sm"
													type="button"
													aria-expanded={openHistory === r.target}
													onclick={() => (openHistory = openHistory === r.target ? null : r.target)}
													>History</button
												>
											</div>
										</td>
									</tr>
									{#if openHistory === r.target}
										<tr class="history">
											<td colspan="6">
												<table class="tbl dense">
													<thead
														><tr
															><th>Created</th><th>Op</th><th>Status</th><th class="r">Attempts</th><th>By</th
															><th>Error</th></tr
														></thead
													>
													<tbody>
														{#each r.history as h (h.id)}
															<tr>
																<td class="mono nowrap">{when(h.created_at)}</td>
																<td class="mono">{h.dedupe_key}</td>
																<td><Chip text={h.status} variant={histVariant(h.status)} /></td>
																<td class="r">{h.attempts}</td>
																<td class="mono">{h.created_by}</td>
																<td>{h.last_error_code ?? '—'}</td>
															</tr>
														{:else}
															<tr><td colspan="6" class="muted">No provisioning rows yet.</td></tr>
														{/each}
													</tbody>
												</table>
											</td>
										</tr>
									{/if}
								{/each}
							</tbody>
						</table>
					</div>
					<p class="note">
						Each connection is set up for you in the background and retries on its own. Retry starts a stuck
						one again (workspace admins and super admins); Cancel is for super admins only. A setting changed
						by another app shows as “{L.drift.toLowerCase()}” and is never overwritten.
					</p>
				</Card>
			</section>

			<section id="licence" class="t2-anchor">
				<Card
					title="Licence"
					sub="Checked on every customer turn. The token is never shown, only its key id."
				>
					<p><Chip text={o.licence.status} variant={licenceVariant(o.licence.status)} /></p>
					<KeyValues
						pairs={[
							[L.tenant, `${o.tenant.name}${o.tenant.brand ? ` · ${o.tenant.brand}` : ''}`],
							...(prefs.showTech ? [['Tenant id', o.tenant.id] as [string, string]] : []),
							[
								'Data residency',
								o.tenant.residency
									? `${o.tenant.residency.toUpperCase()} · set at provisioning; pins stores, LLM providers and logs`
									: 'not set',
							],
							['App', o.licence.app],
							['Plan', o.licence.plan],
							...(prefs.showTech
								? [['Key id', o.licence.kid ? `v2:${o.licence.kid}` : '—'] as [string, string]]
								: []),
							['Holder', o.licence.holder_sub ? subjectLabel(o.licence.holder_sub) : '—'],
							['Verdict checked', when(o.licence.checked_at)],
							['Reason', o.licence.reason ?? '—'],
							['Owner', o.licence.platform_owned ? 'Platform-owned licence' : 'Tenant licence'],
							[
								'Members',
								`${o.members.total} · ${o.members.admins} admins · ${o.members.invited} invited · ${o.members.disabled} disabled`,
							],
						]}
					/>
					<p class="note">
						<a href="/admin/users">Manage members</a> · <a href="/admin/channels">Channels</a>
					</p>
				</Card>
			</section>

			<!-- F-14 (B4 T16a): the tenant holiday calendar, a D54 draft like every design change -->
			<section id="calendar" class="t2-anchor">
				<CalendarCard />
			</section>
		</div>
	{/snippet}
</LoadState>

<style>
	.ws-grid {
		display: grid;
		grid-template-columns: minmax(0, 3fr) minmax(0, 2fr);
		gap: 16px;
		margin-bottom: 16px;
	}
	.ws-grid > :global(*) {
		min-width: 0;
	}
	.ws-summary {
		margin: 0 0 16px;
	}
	.ws-nums {
		display: grid;
		grid-template-columns: repeat(2, minmax(0, 1fr));
		gap: 16px;
	}
	.ws-num b {
		display: block;
		font-size: 1.6rem;
		line-height: 1.1;
	}
	.ws-num small {
		font-size: 0.9rem;
		color: var(--muted);
	}
	.ws-num span {
		color: var(--muted);
		font-size: 0.85rem;
	}
	.ws-agents,
	.ws-setup {
		margin-bottom: 16px;
	}
	.ws-setup {
		display: flex;
		flex-wrap: wrap;
		gap: 10px 14px;
		align-items: center;
		padding: 12px 16px;
		border: 1px solid var(--line);
		border-radius: var(--r);
		background: var(--panel);
	}
	.ws-setup .grow {
		flex: 1 1 24ch;
		min-width: 0;
	}
	.ws-empty {
		display: grid;
		gap: 8px;
		justify-items: start;
	}
	.ws-empty h2 {
		margin: 0;
		font-size: 1.1rem;
	}
	.t2-anchor {
		scroll-margin-top: 70px;
		min-width: 0;
	}
	.row-actions {
		display: inline-flex;
		gap: 6px;
		flex-wrap: wrap;
		justify-content: flex-end;
	}
	.history > td {
		background: var(--sunk);
	}
	@media (max-width: 760px) {
		.ws-grid {
			grid-template-columns: minmax(0, 1fr);
		}
	}
</style>
