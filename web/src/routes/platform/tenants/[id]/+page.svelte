<script lang="ts">
	// P2 Tenant detail — one tenant end to end, for the super_admin who has to answer "can this tenant serve its
	// customers right now, and if not, what do I fix?" (04 §10, 06 §4.6, K22). Reads top-down:
	//  1. serving verdict = the four gates a customer turn passes (status → gate-2 verdict → release → provisioning);
	//  2. provision-down = what this app owns vs. what the outbox pushed to Licences / Knowledge / Ingest, with
	//     Retry/Cancel where a row is stuck, and what is never pushed (Keycloak, sources);
	//  3. jobs (template pins base → industry → department → job, D15) and people (soft disable, re-home K43);
	//  4. lifecycle (K42 state machine; suspend/resume/offboard with their consequences, offboard report);
	//  5. licence & metering (key id only, never the token), and the tenant's lifecycle history (provenance).
	import { superAdminAttrs } from '$lib/auth/roles';
	import { onMount } from 'svelte';
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { api, guard, switchTenant } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { ApiError } from '$lib/api/errors';
	import { Resource } from '$lib/load.svelte';
	import { pinTag, rolesText, type TenantSummary, type TenantUser } from '$lib/api/types/core';
	import type {
		FootprintSlot,
		JobTemplateTag,
		ProvisionRow,
		ReissueResult,
		TenantDetail,
		TenantFootprint,
	} from '$lib/api/types/platform';
	import PageHead from '$lib/ui/PageHead.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import KeyValues from '$lib/ui/KeyValues.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import MilestoneChip from '$lib/ui/MilestoneChip.svelte';
	import StateDiagram from '$lib/ui/StateDiagram.svelte';
	import { toast } from '$lib/ui/toaster.svelte';
	import OffboardDrawer from '$lib/platform/OffboardDrawer.svelte';
	import InvitesPanel from '$lib/tenant/InvitesPanel.svelte';
	import { ARCHIVED_HINT, PROVISION_CHIP, TARGET_LABEL, utc } from '$lib/platform/ui';
	import {
		LIFECYCLE_STATES,
		LIFECYCLE_TRANSITIONS,
		collapseHistory,
		eventTone,
		latestPerTarget,
		pinKind,
		provLabel,
		servingOf,
	} from '$lib/platform/TenantDetailP2';

	const id = $derived(page.params.id ?? '');
	const res = new Resource<TenantDetail>(() => api().platformTenant(id));
	const fpRes = new Resource<TenantFootprint>(
		() => api().platformTenantFootprint(id),
		() => false,
	);
	onMount(() => {
		void res.refresh();
		void fpRes.refresh();
	});

	const detail = $derived(res.value.state === 'ok' ? res.value.data : null);
	const fp = $derived(fpRes.value.state === 'ok' ? fpRes.value.data : null);
	const archived = $derived(detail?.tenant.status === 'archived');
	/** Write props: super_admin and a non-archived tenant. */
	const w = $derived(archived ? { disabled: true, title: ARCHIVED_HINT } : superAdminAttrs(session));

	let busy = $state<string | null>(null);
	/** Attachment storage, in MB as typed; empty = the platform default. */
	let quotaMb = $state('');
	let quotaFor = '';
	$effect(() => {
		const t = detail?.tenant;
		if (t && quotaFor !== `${t.id}|${t.attachment_quota_bytes ?? ''}`) {
			quotaFor = `${t.id}|${t.attachment_quota_bytes ?? ''}`;
			quotaMb = t.attachment_quota_bytes ? String(Math.round(t.attachment_quota_bytes / 1048576)) : '';
		}
	});
	const quotaBytes = $derived(quotaMb.trim() === '' ? null : Math.round(Number(quotaMb) * 1048576));
	const quotaBad = $derived(quotaBytes !== null && (!Number.isFinite(quotaBytes) || quotaBytes < 1));
	async function run(key: string, fn: () => Promise<unknown>, ok: string) {
		busy = key;
		const done = await guard(fn, ok);
		busy = null;
		if (done) await Promise.all([res.refresh(), fpRes.refresh()]);
	}

	async function actIn() {
		if (!detail) return;
		const err = await switchTenant(detail.tenant.id);
		if (err) toast(err, 'err');
		else void goto('/admin');
	}

	const MARK = { ok: '✓', warn: '!', err: '✕', off: '–' } as const;
	const TONE_WORD = { ok: 'ok', warn: 'attention', err: 'blocked', off: 'off' } as const;

	/** What each target holds for this tenant, and the one-line "why it exists". */
	const TARGET_WHAT = {
		licences: 'Per-app licences from the bundle, so this app can mint rag/ingest/LLM tokens for the tenant.',
		strata_rag: 'Retrieval: embed, rerank and chat slots, plus the stores knowledge is indexed in.',
		strata_ingest: 'Upload pipeline: OCR, vision and transcription slots.',
	} as const;
	const slotsOf = (f: TenantFootprint | null, service: FootprintSlot['service']) =>
		f?.slots.filter((s) => s.service === service) ?? [];
	const historyRows = $derived(collapseHistory(fp?.history ?? []));
	let allHistory = $state(false);

	function stuck(r: ProvisionRow) {
		return ['pending', 'failed', 'dead'].includes(r.status) && !archived;
	}

	// ---- create job from template ----
	let jobOpen = $state(false);
	let templates = $state<JobTemplateTag[]>([]);
	let template = $state('');
	let jobKey = $state('');
	let jobLabel = $state('');
	let jobError = $state<{ code: string; message: string } | null>(null);
	const chosen = $derived(templates.find((t) => `${t.layer}@v${t.version}` === template));
	async function openJob() {
		jobError = null;
		jobKey = '';
		jobLabel = '';
		jobOpen = true;
		try {
			templates = await api().platformJobTemplates();
			template ||= templates[0] ? `${templates[0].layer}@v${templates[0].version}` : '';
		} catch (e) {
			jobError = { code: 'unavailable', message: (e as Error).message };
		}
	}
	async function createJob(e: SubmitEvent) {
		e.preventDefault();
		if (!detail) return;
		jobError = null;
		try {
			const job = await api().platformCreateJob(detail.tenant.id, { template, key: jobKey, label: jobLabel });
			toast(`Job ${job.key} created, pinned to ${job.pins.map(pinTag).join(', ')}.`);
			jobOpen = false;
			await Promise.all([res.refresh(), fpRes.refresh()]);
		} catch (err) {
			jobError =
				err instanceof ApiError
					? { code: err.code, message: err.message }
					: { code: 'error', message: String(err) };
		}
	}
	$effect(() => {
		if (!jobKey && chosen)
			jobKey =
				chosen.layer.split('/').at(-1) === 'billing'
					? 'billing_assistant'
					: (chosen.layer.split('/').at(-1) ?? '');
	});

	// ---- re-home ----
	let rehomeOpen = $state(false);
	let rehomeUser = $state<TenantUser | null>(null);
	let rehomeTarget = $state('');
	let liveTenants = $state<TenantSummary[]>([]);
	async function openRehome(u: TenantUser) {
		rehomeUser = u;
		rehomeTarget = '';
		rehomeOpen = true;
		try {
			liveTenants = (await api().tenants()).items.filter(
				(t) => t.status === 'active' && t.id !== u.tenant_id,
			);
		} catch {
			liveTenants = [];
		}
	}
	async function rehome(e: SubmitEvent) {
		e.preventDefault();
		if (!rehomeUser || !rehomeTarget) return;
		const u = rehomeUser;
		const to = liveTenants.find((t) => t.id === rehomeTarget)?.name ?? rehomeTarget;
		const ok = await guard(
			() => api().platformRehomeUser(u.sub, rehomeTarget),
			`${u.display_name} moved to ${to} as an observer (no role) (audited in both tenants).`,
		);
		if (ok) {
			rehomeOpen = false;
			await Promise.all([res.refresh(), fpRes.refresh()]);
		}
	}

	let offboardOpen = $state(false);

	// ---- licence re-issue (F-31): revoke the old licence, issue a new one in the residency region ----
	let reissueOpen = $state(false);
	let reissuing = $state(false);
	let reissueError = $state<{ code: string; message: string } | null>(null);
	let reissued = $state<ReissueResult | null>(null);
	function openReissue() {
		reissueError = null;
		reissued = null;
		reissueOpen = true;
	}
	async function reissue(e: SubmitEvent) {
		e.preventDefault();
		if (!detail) return;
		reissuing = true;
		reissueError = null;
		try {
			const a = await api().reissueTenantLicence(detail.tenant.id, { tenantLicence: true });
			reissued = a.results.find((r) => r.holder === 'tenant') ?? a.results[0] ?? null;
			toast(
				reissued?.status === 'reissued'
					? `New licence ${reissued.new_licence_id} issued.`
					: 'Re-issue pending; the provisioning tick finishes it.',
			);
			await Promise.all([res.refresh(), fpRes.refresh()]);
		} catch (err) {
			reissueError =
				err instanceof ApiError
					? { code: err.code, message: err.message }
					: { code: 'error', message: (err as Error).message };
		} finally {
			reissuing = false;
		}
	}
</script>

<PageHead
	id="P2"
	title={detail?.tenant.name ?? 'Tenant detail'}
	sub={detail
		? `${detail.tenant.brand ? `Trades as ${detail.tenant.brand} · ` : ''}${detail.tenant.description}`
		: undefined}
>
	{#snippet actions()}
		<a class="btn" href="/platform/tenants">All tenants</a>
		{#if detail && !archived}
			<button class="btn" onclick={actIn} title="Switch into this tenant as super_admin (audited)"
				>Act in tenant</button
			>
		{/if}
	{/snippet}
</PageHead>

<LoadState resource={res} loading="Loading tenant…">
	{#snippet children(d)}
		{@const s = servingOf(d)}
		{@const rows = latestPerTarget(d.provisioning)}
		{@const active = d.users.filter((u) => !u.disabled_at)}
		<p class="p2-id muted">
			<span class="mono">{d.tenant.id}</span> · {d.tenant.industry.replace('_', ' ')} · created {utc(
				d.tenant.created_at,
			)}
		</p>

		<!-- 1. Serving verdict -->
		<section class="p2-serving tone-{s.tone}" data-testid="p2-serving" aria-labelledby="p2-serving-h">
			<div class="p2-serving-head">
				<span class="label">Can this tenant serve its customers right now?</span>
				<h2 id="p2-serving-h">{s.headline}</h2>
				<p class="muted">A customer message passes these gates in order; the first red one stops it.</p>
			</div>
			<ol class="p2-gates">
				{#each s.gates as g, i (g.key)}
					<li class="p2-gate {g.tone}" data-testid="p2-gate">
						<span class="p2-mark" aria-hidden="true">{MARK[g.tone]}</span>
						<span class="p2-gate-text">
							<b>{i + 1}. {g.label}</b>
							<span class="sr-only">({TONE_WORD[g.tone]})</span>
							<span class="p2-gate-detail">{g.detail}</span>
						</span>
					</li>
				{/each}
			</ol>
		</section>

		{#if archived}
			<Callout kind="gap" title="Archived {utc(d.tenant.archived_at)} — read-only">
				Everyone gets 403 <code>tenant_archived</code>. Nothing here can change; the offboard report is final.
			</Callout>
		{/if}

		<div class="grid g-main p2-grid">
			<div class="stack">
				<!-- 2. Provision-down -->
				<Card
					title="Provision-down"
					sub="Creating the tenant here pushed its slice to three systems through the outbox. Each target retries on its own; a failure never undoes the tenant (K22). Remote changes are shown as drift, never overwritten."
					cls="prov-card"
				>
					{#snippet actions()}<MilestoneChip milestone="M3" />{/snippet}
					<div class="p2-pd">
						<div class="p2-src">
							<span class="label">Owned here (agent)</span>
							<ul class="p2-src-list">
								<li><span>Workspace row</span><span class="mono">{d.tenant.status}</span></li>
								<li>
									<span>People</span><span class="mono"
										>{active.length} active{d.users.length > active.length
											? ` · ${d.users.length - active.length} disabled`
											: ''}</span
									>
								</li>
								<li>
									<span>Agent licence</span><span class="mono"
										>{d.licence?.kid ? `v2:${d.licence.kid} sealed` : 'none'}</span
									>
								</li>
								<li><span>Jobs</span><span class="mono">{d.jobs.length}</span></li>
								<li data-testid="p2-residency">
									<span>Data residency</span><span class="mono">{d.tenant.residency.toUpperCase()}</span>
								</li>
							</ul>
							<p class="note">
								Residency is chosen at creation and immutable after (D59/D66): the Knowledge and Ingest
								targets below create the tenant’s stores in {d.tenant.residency.toUpperCase()}, and its LLM
								providers and logs are pinned there too.
							</p>
							<p class="note" data-testid="p2-residency-note">
								Changing residency is a support migration: stores, providers and logs move region, so there is
								no edit here.
							</p>
							<p class="note">
								Sign-in is the shared portal realm (client <code>agent</code>); tenant roles live here, never
								in Keycloak (D39).
							</p>
						</div>
						<div class="p2-flow" aria-hidden="true">
							<span>provision_outbox</span>
							<span class="p2-flow-arrow">→</span>
						</div>
						<ul class="p2-targets list-plain">
							{#each rows as r (r.id)}
								<li data-testid="prov-row" class="p2-target st-{r.status}">
									<div class="p2-target-body">
										<div class="p2-target-head">
											<strong>{TARGET_LABEL[r.target]}</strong>
											<span class="muted mono">{r.target} · {r.op}</span>
										</div>
										<span class="cell-sub">{TARGET_WHAT[r.target]}</span>
										<div class="chips">
											<Chip text={provLabel(r)} variant={PROVISION_CHIP[r.status]} />
											<Chip text={`${r.attempts} attempt${r.attempts === 1 ? '' : 's'}`} />
											{#if r.drift}<Chip text="drift" variant="warn" title={r.drift} />{/if}
										</div>
										{#if r.target === 'licences' && fp?.bundle && r.op === 'licence_ensure'}
											<div class="chips p2-holds">
												<span class="muted">bundle {fp.bundle.key} →</span>
												{#each fp.bundle.apps as a (a)}<Chip text={a} variant="tool" />{/each}
											</div>
										{:else if r.target !== 'licences' && fp}
											{@const slots = slotsOf(fp, r.target)}
											{#if slots.length}
												<div class="chips p2-holds" aria-label="Provider slots">
													{#each slots as sl (sl.slot)}
														<Chip
															text={sl.configured
																? `${sl.slot} · ${sl.model ?? sl.kind} v${sl.version}${sl.drift ? ' ⚠' : ''}`
																: `${sl.slot} · unset`}
															variant={!sl.configured ? 'muted' : sl.drift ? 'warn' : 'ok'}
															title={sl.drift
																? 'Moved by another app — shown, never overwritten'
																: sl.configured
																	? `${sl.label}: ${sl.kind}`
																	: `${sl.label}: not configured`}
														/>
													{/each}
												</div>
											{/if}
											{#if r.target === 'strata_rag' && fp.stores.leaves}
												<span class="cell-sub">
													Stores: {fp.stores.bound} of {fp.stores.leaves} bound in strata_rag ·
													{fp.stores.documents.toLocaleString('en')} documents{fp.stores.unbound.length
														? ` · not created yet: ${fp.stores.unbound.join(', ')}`
														: ''}
												</span>
											{/if}
										{/if}
										{#if r.last_error}<span class="p2-err">{r.last_error.message}</span>{/if}
										{#if r.drift}<span class="cell-sub">{r.drift}</span>{/if}
										<span class="cell-sub">last success {utc(r.last_success_at)}</span>
									</div>
									{#if stuck(r)}
										<span class="actions">
											<button
												class="btn sm"
												{...w}
												disabled={w.disabled || busy === r.id}
												onclick={() =>
													run(
														r.id,
														() => api().platformRetryProvision(r.tenant_id, r.id),
														`${TARGET_LABEL[r.target]} ${r.op} queued again.`,
													)}>Retry</button
											>
											<button
												class="btn sm"
												{...w}
												disabled={w.disabled || busy === r.id}
												onclick={() =>
													run(
														r.id,
														() => api().platformCancelProvision(r.tenant_id, r.id),
														`${TARGET_LABEL[r.target]} ${r.op} cancelled.`,
													)}>Cancel</button
											>
										</span>
									{/if}
								</li>
							{:else}
								<li class="muted">Nothing queued yet.</li>
							{/each}
						</ul>
					</div>
					<p class="note">
						Never pushed per tenant: sources and connector credentials (inline per ingest job, K45). Provider
						keys are written synchronously from the tenant's Providers page, never queued.
					</p>
				</Card>

				<!-- 3a. Jobs -->
				<Card
					title="Jobs"
					sub="Each job is pinned to template tags, base → industry → department → job (D15). Only a super_admin creates jobs (K11)."
				>
					{#snippet actions()}
						<button class="btn sm primary" {...w} onclick={openJob}>Create job from template</button>
					{/snippet}
					{#if d.jobs.length}
						<ul class="list-plain p2-jobs">
							{#each d.jobs as j (j.key)}
								{@const pj = fp?.portfolio.jobs.find((x) => x.key === j.key)}
								<li data-testid="p2-job">
									<div class="p2-job">
										<div class="p2-job-head">
											<span>
												<span class="cell-title">{j.label}</span>
												<span class="cell-sub mono">{j.key}</span>
											</span>
											<span class="chips">
												<Chip text={j.on_bus ? 'on bus' : 'off bus'} variant={j.on_bus ? 'cust' : 'muted'} />
												<Chip
													text={j.current_release
														? `${j.current_release.id} · ${j.current_release.hash}`
														: 'no release yet'}
													variant={j.current_release ? 'ok' : 'warn'}
												/>
											</span>
										</div>
										<ol class="p2-pins" aria-label="Template pins">
											{#each j.pins as p, i (p.layer)}
												{@const behind = pj?.behind.find((b) => b.layer === p.layer)}
												<li>
													{#if i}<span class="p2-sep" aria-hidden="true">›</span>{/if}
													<span class="p2-pin">
														<span class="p2-pin-kind">{pinKind(p)}</span>
														<Chip text={pinTag(p)} variant="tool" />
														{#if behind}<span class="p2-behind">v{behind.latest} tagged</span>{/if}
													</span>
												</li>
											{/each}
										</ol>
										{#if pj && (pj.cases_7d !== null || pj.generator)}
											<span class="cell-sub">
												{#if pj.cases_7d !== null}{pj.cases_7d.toLocaleString('en')} cases in 7 d{pj.containment_7d !==
													null
														? ` · ${Math.round(pj.containment_7d * 100)}% contained`
														: ''}{/if}
												{#if pj.generator}
													· <a href={`/platform/generator/${pj.generator.run}`}
														>generator run #{pj.generator.run} {pj.generator.status}</a
													>
												{/if}
											</span>
										{/if}
									</div>
								</li>
							{/each}
						</ul>
					{:else}
						<p class="empty">
							No jobs yet. A tenant without a release is valid — it serves nothing until a job is created and
							its first release is cut.
						</p>
					{/if}
				</Card>

				<!-- 3b. People -->
				<Card
					title="People"
					sub={`${active.filter((u) => u.roles.includes('tenant_admin')).length} admins · ${active.length} active · ${active.filter((u) => u.invited_at).length} invited · ${d.users.length - active.length} disabled. Removal is soft; a disabled user can be re-homed to another tenant (K43).`}
				>
					<div class="tbl-wrap">
						<table class="tbl">
							<thead
								><tr
									><th>User</th><th>Roles</th><th>State</th><th><span class="sr-only">Actions</span></th></tr
								></thead
							>
							<tbody>
								{#each d.users as u (u.sub)}
									<tr class:disabled={!!u.disabled_at} data-testid="user-row">
										<td>
											<span class="cell-title">{u.display_name}</span>
											<span class="cell-sub">{u.title ?? ''} · {u.email}</span>
										</td>
										<td
											><Chip
												text={rolesText(u.roles)}
												variant={u.roles.includes('tenant_admin') ? 'accent' : 'muted'}
											/></td
										>
										<td>
											{#if u.disabled_at}<Chip text={`disabled ${u.disabled_at.slice(0, 10)}`} />
											{:else if u.invited_at}<Chip text="invited" variant="warn" />
											{:else}<Chip text="active" variant="ok" />{/if}
										</td>
										<td class="r">
											{#if u.disabled_at && !archived}
												<button class="btn sm" {...superAdminAttrs(session)} onclick={() => openRehome(u)}
													>Re-home…</button
												>
											{/if}
										</td>
									</tr>
								{/each}
							</tbody>
						</table>
					</div>
					<!-- B1b F-01: the tenant's member invites, Resend / Revoke as super_admin. -->
					<h3 class="p2-sub">Invites</h3>
					<InvitesPanel
						load={(state, before) => api().platformTenantInvites(d.tenant.id, { state, before })}
						resend={(invite) => api().platformResendInvite(d.tenant.id, invite)}
						revoke={(invite) => api().platformRevokeInvite(d.tenant.id, invite)}
						canAct={session.superAdmin}
						resendBlocked={archived ? ARCHIVED_HINT : undefined}
						onChange={() => res.refresh()}
					/>
				</Card>
			</div>

			<div class="stack">
				<!-- 4. Lifecycle -->
				<Card
					title="Lifecycle"
					sub="Suspend is reversible; offboard is terminal. Nothing is ever deleted (K42)."
				>
					{#snippet actions()}<MilestoneChip milestone="M2" />{/snippet}
					<div class="p2-sm">
						<StateDiagram
							states={LIFECYCLE_STATES}
							transitions={LIFECYCLE_TRANSITIONS}
							current={d.tenant.status}
							title="Tenant lifecycle"
						/>
					</div>
					{#if !archived}
						<ul class="list-plain p2-life" data-testid="p2-lifecycle">
							<li>
								<div>
									{#if d.tenant.status === 'active'}
										<b>Suspend</b>
										<span class="cell-sub"
											>Members get 403 <code>tenant_suspended</code>; the edge refuses customer turns with a
											polite unavailable text. rag/ingest are untouched. You can still act in it.</span
										>
									{:else}
										<b>Resume</b>
										<span class="cell-sub"
											>Suspended since {utc(d.tenant.suspended_at)}. Resuming re-adds the job scopes; the
											verdict turns active on its next refresh (≤ 60 s).</span
										>
									{/if}
								</div>
								{#if d.tenant.status === 'active'}
									<button
										class="btn"
										{...w}
										disabled={w.disabled || busy === 'status'}
										onclick={() =>
											run(
												'status',
												() => api().setTenantStatus(d.tenant.id, 'suspended'),
												`${d.tenant.name} suspended.`,
											)}>Suspend</button
									>
								{:else}
									<button
										class="btn"
										{...w}
										disabled={w.disabled || busy === 'status'}
										onclick={() =>
											run(
												'status',
												() => api().setTenantStatus(d.tenant.id, 'active'),
												`${d.tenant.name} resumed.`,
											)}>Resume</button
									>
								{/if}
							</li>
							<li>
								<div>
									<b>Support auto-assignment</b>
									<span class="cell-sub"
										>Round-robin hands each queued handover to whoever is online on the Support workspace.
										{d.tenant.support_auto_assign
											? 'On: the workspace’s own routing setting decides.'
											: 'Off (the default): nothing is assigned automatically, whatever the workspace’s routing setting says.'}</span
									>
								</div>
								<button
									class="btn"
									{...w}
									disabled={w.disabled || busy === 'autoassign'}
									onclick={() =>
										run(
											'autoassign',
											() => api().setTenantSupportAutoAssign(d.tenant.id, !d.tenant.support_auto_assign),
											`Support auto-assignment ${d.tenant.support_auto_assign ? 'switched off' : 'switched on'} for ${d.tenant.name}.`,
										)}>{d.tenant.support_auto_assign ? 'Turn off' : 'Turn on'}</button
								>
							</li>
							<li>
								<div>
									<b>Attachment storage</b>
									<span class="cell-sub"
										>Photos and PDFs the workspace may keep, in MB. Empty = the platform default ({Math.round(
											(d.tenant.attachment_quota_default_bytes ?? 1073741824) / 1048576,
										)} MB). The workspace admin sees this read-only.</span
									>
								</div>
								<span class="row">
									<input
										class="input"
										type="number"
										min="1"
										aria-label="Attachment storage in MB"
										data-testid="tenant-quota"
										placeholder="default"
										bind:value={quotaMb}
										{...w}
									/>
									<button
										class="btn"
										data-testid="tenant-quota-save"
										{...w}
										disabled={w.disabled || quotaBad || busy === 'quota'}
										onclick={() =>
											run(
												'quota',
												() => api().setTenantAttachmentQuota(d.tenant.id, quotaBytes),
												`Attachment storage updated for ${d.tenant.name}.`,
											)}>Save</button
									>
								</span>
							</li>
							<li>
								<div>
									<b>Offboard</b>
									<span class="cell-sub"
										>Revokes the agent licence, archives the workspace for good, disables all {d.users.filter(
											(u) => !u.disabled_at,
										).length} active users and queues licence revokes for rag/ingest. Their data is
										<em>retained</em>, so the report is never complete.</span
									>
								</div>
								<button class="btn danger" {...w} onclick={() => (offboardOpen = true)}>Offboard…</button>
							</li>
						</ul>
					{/if}
					{#if d.offboard}
						<div class="p2-report">
							<div class="p2-report-head">
								<b>Offboard report</b>
								<span class="muted">{utc(d.offboard.at)} by {d.offboard.by}</span>
								<Chip text="complete: false" variant="warn" title="rag/ingest data is retained" />
							</div>
							<ul class="list-plain" data-testid="offboard-report">
								<li>
									<span>Licence revoked</span><Chip
										text={d.offboard.licence_revoked ? 'yes' : 'no'}
										variant="ok"
									/>
								</li>
								<li>
									<span>Workspace archived</span><Chip
										text={d.offboard.archived ? 'yes' : 'no'}
										variant="ok"
									/>
								</li>
								<li><span>Users disabled</span><span class="mono">{d.offboard.users_disabled}</span></li>
								{#each d.offboard.outbox as o (o.target)}
									<li>
										<span>{TARGET_LABEL[o.target]} <span class="mono muted">{o.op}</span></span><Chip
											text={o.status}
											variant={PROVISION_CHIP[o.status]}
										/>
									</li>
								{/each}
								{#each d.offboard.data_retained as r (r.app)}
									<li><span>Data retained · {r.app}</span><span class="muted">{r.what}</span></li>
								{/each}
							</ul>
						</div>
					{/if}
				</Card>

				<!-- 5a. Licence & metering -->
				<Card
					title="Licence & metering"
					sub="Gate 2: the sealed token is never shown, only the key id it is sealed with. Usage is attributed to this licence."
				>
					{#snippet actions()}
						{#if d.licence?.kid}
							<button
								class="btn sm"
								{...w}
								disabled={w.disabled || busy === 'reseal'}
								title={w.title}
								onclick={() =>
									run(
										'reseal',
										() => api().resealTenantLicence(d.tenant.id),
										'Licence token resealed under the current key.',
									)}>Reseal</button
							>
						{/if}
						{#if d.licence}
							<button class="btn sm" {...w} disabled={w.disabled || reissuing} onclick={openReissue}
								>Re-issue…</button
							>
						{/if}
					{/snippet}
					{#if d.licence}
						<KeyValues
							pairs={[
								['Licence', `${d.licence.licence_id} · ${d.licence.app} ${d.licence.plan}`],
								['Holder', d.licence.holder_sub],
								['Token', d.licence.kid ? `v2:${d.licence.kid}:•••• (sealed)` : 'none (revoked)'],
								['Introspect', d.licence.introspect_status],
								[
									'Edge verdict',
									d.verdict
										? `${d.verdict.status}${d.verdict.fails_closed ? ' · fails closed' : ''} · ${utc(d.verdict.checked_at)}`
										: '—',
								],
								['Metered as', `${d.licence.licence_id} / ${d.licence.subject_id}`],
								...(fp
									? ([
											[
												'Last 30 days',
												fp.portfolio.usage_30d.turns
													? `${fp.portfolio.usage_30d.turns.toLocaleString('en')} turns · ${fp.portfolio.usage_30d.tokens.toLocaleString('en')} tokens`
													: 'no usage',
											],
											[
												'LLM providers',
												fp.llm_providers.length
													? fp.llm_providers
															.map(
																(p) =>
																	`${p.name} (${p.token_mode === 'internal' ? 'mlx_api, internal' : 'own token'})`,
															)
															.join(', ')
													: 'none',
											],
										] as [string, string][])
									: []),
							]}
						/>
					{:else}
						<p class="empty">No licence.</p>
					{/if}
					<p class="note">
						Keyring, reseal of every tenant and the bundle: <a href="/platform/licensing"
							>Licensing &amp; keys</a
						>.
					</p>
				</Card>

				<!-- 5b. History -->
				<Card title="History" sub="How this tenant got here: its own audit log, lifecycle events only.">
					{#if fpRes.value.state === 'unavailable'}
						<p class="empty">History unavailable ({fpRes.value.reason}).</p>
					{:else if !fp}
						<p class="empty">Loading…</p>
					{:else if historyRows.length}
						<ol class="p2-hist" data-testid="p2-history">
							{#each allHistory ? historyRows : historyRows.slice(0, 8) as g, i (`${g.first.at}-${i}`)}
								{@const e = g.first}
								<li class="t-{eventTone(e)}">
									<span class="p2-dot" aria-hidden="true"></span>
									<div>
										<span class="mono">{e.action}</span>
										<span class="muted"> · {e.resource_id}</span>
										{#if g.count > 1}<Chip text={`× ${g.count}`} />{/if}
										<span class="cell-sub"
											>{g.count > 1 ? `${utc(g.last.at)} → ${utc(e.at)}` : utc(e.at)} · {e.actor_id}{e.actor_type ===
											'system'
												? ' (system)'
												: ''}{e.ok ? '' : ' · failed'}</span
										>
									</div>
								</li>
							{/each}
						</ol>
						{#if historyRows.length > 8}
							<button class="btn sm" onclick={() => (allHistory = !allHistory)}
								>{allHistory ? 'Show recent' : `Show all ${historyRows.length} entries`}</button
							>
						{/if}
					{:else}
						<p class="empty">No lifecycle events recorded.</p>
					{/if}
				</Card>
			</div>
		</div>
	{/snippet}
</LoadState>

<OffboardDrawer
	bind:open={offboardOpen}
	tenant={detail ? { id: detail.tenant.id, name: detail.tenant.name } : null}
	ondone={() => void Promise.all([res.refresh(), fpRes.refresh()])}
/>

<Drawer bind:open={reissueOpen} title="Re-issue licence">
	{#snippet sub()}<span>{detail?.tenant.name}</span>{/snippet}
	<form class="stack" onsubmit={reissue}>
		<Callout kind="info" title="Revokes the current licence">
			The current licence{detail?.licence ? ` (${detail.licence.licence_id})` : ''} is revoked, then a new one
			is issued for the same holder in the tenant's residency region ({detail?.tenant.residency.toUpperCase()}).
			Members are not disabled. If licences does not confirm the revoke in time, the provisioning tick
			finishes the re-issue.
		</Callout>
		{#if reissued}
			<Callout
				kind={reissued.status === 'reissued' ? 'info' : 'gap'}
				title={reissued.status === 'reissued' ? 'Re-issued' : 'Pending'}
			>
				<span data-testid="reissue-result"
					>{#if reissued.status === 'reissued'}New licence <span class="mono">{reissued.new_licence_id}</span
						>; {reissued.old_licence_id} revoked.{:else}Old licence {reissued.old_licence_id} revoke {reissued.old_revoke};
						the provisioning tick finishes the re-issue{reissued.code
							? ` (${reissued.code})`
							: ''}.{/if}</span
				>
			</Callout>
		{/if}
		{#if reissueError}<Callout kind="err" title={reissueError.code}>{reissueError.message}</Callout>{/if}
		<div class="actions">
			{#if reissued}
				<button class="btn" type="button" onclick={() => (reissueOpen = false)}>Close</button>
			{:else}
				<button class="btn danger" type="submit" {...w} disabled={w.disabled || reissuing}
					>{reissuing ? 'Re-issuing…' : 'Re-issue licence'}</button
				>
			{/if}
		</div>
	</form>
</Drawer>

<Drawer bind:open={jobOpen} title="Create job from template">
	{#snippet sub()}<span>{detail?.tenant.name}</span>{/snippet}
	<form class="stack" onsubmit={createJob}>
		<div class="field">
			<label for="tpl">Template tag</label>
			<select id="tpl" class="input" bind:value={template}>
				{#each templates as t (`${t.layer}@v${t.version}`)}
					<option value={`${t.layer}@v${t.version}`}
						>{t.layer}@v{t.version} — {t.label}{t.status === 'draft' ? ' (draft)' : ''}</option
					>
				{/each}
			</select>
		</div>
		{#if chosen}
			<div class="chips">
				{#each chosen.pins as p (p.layer)}<Chip text={pinTag(p)} variant="tool" />{/each}
			</div>
		{/if}
		<div class="form-grid">
			<div class="field">
				<label for="job-key">Job key</label>
				<input id="job-key" class="input mono" bind:value={jobKey} required />
			</div>
			<div class="field">
				<label for="job-label">Label</label>
				<input id="job-label" class="input" bind:value={jobLabel} placeholder={chosen?.label} />
			</div>
		</div>
		{#if jobError}<Callout kind="err" title={jobError.code}>{jobError.message}</Callout>{/if}
		<div class="actions"><button class="btn primary" type="submit" {...w}>Create job</button></div>
	</form>
</Drawer>

<Drawer bind:open={rehomeOpen} title={rehomeUser ? `Re-home ${rehomeUser.display_name}` : 'Re-home user'}>
	{#snippet sub()}<span
			>Moves a disabled user to another tenant as an observer (no role); audited as user.rehome in both.</span
		>{/snippet}
	<form class="stack" onsubmit={rehome}>
		<div class="field">
			<label for="rehome-target">New home tenant</label>
			<select id="rehome-target" class="input" bind:value={rehomeTarget} required>
				<option value="" disabled>Pick a live tenant…</option>
				{#each liveTenants as t (t.id)}<option value={t.id}>{t.name}</option>{/each}
			</select>
		</div>
		<div class="actions">
			<button class="btn primary" type="submit" {...superAdminAttrs(session)} disabled={!rehomeTarget}
				>Re-home</button
			>
		</div>
	</form>
</Drawer>

<style>
	:global(.card.prov-card > .card-head) {
		flex-wrap: nowrap;
	}
	.p2-sub {
		font-size: 14px;
		margin: 16px 0 8px;
	}
	.p2-id {
		margin: -6px 0 12px;
		font-size: 12.5px;
		overflow-wrap: anywhere;
	}
	.sr-only {
		position: absolute;
		width: 1px;
		height: 1px;
		overflow: hidden;
		clip: rect(0 0 0 0);
	}

	/* 1. serving verdict */
	.p2-serving {
		--tone: var(--ok);
		background: var(--panel);
		border: 1px solid var(--line);
		border-left: 4px solid var(--tone);
		border-radius: 8px;
		padding: 14px 16px;
		margin-bottom: 16px;
		display: grid;
		gap: 12px;
	}
	.p2-serving.tone-warn {
		--tone: var(--warn);
	}
	.p2-serving.tone-err {
		--tone: var(--err);
	}
	.p2-serving.tone-off {
		--tone: var(--faint);
	}
	.p2-serving h2 {
		margin: 4px 0 2px;
		font: 600 18px/1.3 var(--cond);
		color: var(--fg);
	}
	.p2-serving-head p {
		margin: 0;
		font-size: 12.5px;
	}
	.p2-gates {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		grid-template-columns: repeat(4, minmax(0, 1fr));
		gap: 8px;
	}
	.p2-gate {
		--g: var(--ok);
		display: flex;
		gap: 10px;
		align-items: flex-start;
		padding: 10px 12px;
		border-radius: 6px;
		border: 1px solid color-mix(in srgb, var(--g) 35%, var(--line));
		background: color-mix(in srgb, var(--g) 7%, var(--panel));
		position: relative;
	}
	.p2-gate.warn {
		--g: var(--warn);
	}
	.p2-gate.err {
		--g: var(--err);
	}
	.p2-gate.off {
		--g: var(--faint);
	}
	.p2-mark {
		flex: none;
		display: inline-grid;
		place-items: center;
		width: 22px;
		height: 22px;
		border-radius: 50%;
		background: var(--g);
		color: var(--panel);
		font: 700 12px/1 var(--sans);
	}
	.p2-gate-text {
		display: grid;
		gap: 2px;
		min-width: 0;
	}
	.p2-gate-text b {
		font-size: 13px;
	}
	.p2-gate-detail {
		font-size: 12px;
		color: var(--muted);
		overflow-wrap: anywhere;
	}

	/* 2. provision-down */
	.p2-pd {
		display: grid;
		grid-template-columns: minmax(150px, 0.8fr) auto minmax(0, 2fr);
		gap: 12px;
		align-items: start;
	}
	.p2-src {
		border: 1px dashed var(--line);
		border-radius: 6px;
		padding: 10px 12px;
		background: var(--sunk);
	}
	.p2-src-list {
		list-style: none;
		margin: 8px 0 0;
		padding: 0;
		display: grid;
		gap: 6px;
		font-size: 12.5px;
	}
	.p2-src-list li {
		display: grid;
		gap: 1px;
	}
	.p2-src-list li > span:first-child {
		color: var(--muted);
		font-size: 12px;
	}
	.p2-src .note {
		margin-top: 8px;
	}
	.p2-flow {
		align-self: center;
		display: grid;
		justify-items: center;
		gap: 2px;
		font: 500 10.5px/1.2 var(--mono);
		color: var(--faint);
	}
	.p2-flow-arrow {
		font-size: 22px;
		line-height: 1;
	}
	.p2-target {
		border-left: 3px solid var(--line);
		padding-left: 10px !important;
	}
	.p2-target.st-succeeded {
		border-left-color: var(--ok);
	}
	.p2-target.st-pending,
	.p2-target.st-in_flight {
		border-left-color: var(--warn);
	}
	.p2-target.st-failed,
	.p2-target.st-dead {
		border-left-color: var(--err);
	}
	.p2-target-body {
		display: grid;
		gap: 4px;
		min-width: 0;
		flex: 1;
	}
	.p2-target-head {
		display: flex;
		gap: 8px;
		align-items: baseline;
		flex-wrap: wrap;
	}
	.p2-holds {
		align-items: center;
		font-size: 12px;
	}
	.p2-err {
		color: var(--err);
		font-size: 12.5px;
	}

	/* 3. jobs */
	.p2-job {
		display: grid;
		gap: 8px;
		width: 100%;
	}
	.p2-job-head {
		display: flex;
		justify-content: space-between;
		gap: 8px;
		flex-wrap: wrap;
	}
	.p2-pins {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		align-items: flex-end;
	}
	.p2-pins li {
		display: flex;
		align-items: flex-start;
		gap: 6px;
		min-width: 0;
	}
	.p2-sep {
		color: var(--faint);
		padding-top: 14px;
	}
	.p2-pin {
		display: grid;
		gap: 3px;
		justify-items: start;
		min-width: 0;
	}
	.p2-pin-kind {
		font: 600 10px/1 var(--cond);
		letter-spacing: 0.06em;
		text-transform: uppercase;
		color: var(--faint);
	}
	.p2-behind {
		font-size: 11px;
		color: var(--warn);
	}
	tr.disabled td {
		color: var(--faint);
	}

	/* 4. lifecycle */
	.p2-sm {
		overflow-x: auto;
		margin-bottom: 4px;
	}
	.p2-life li > div {
		display: grid;
		gap: 2px;
		flex: 1;
		min-width: 0;
	}
	.p2-life li {
		align-items: center;
	}
	.p2-report {
		margin-top: 8px;
		border-top: 1px solid var(--line-2);
		padding-top: 10px;
	}
	.p2-report-head {
		display: flex;
		gap: 8px;
		align-items: center;
		flex-wrap: wrap;
	}

	/* 5b. history */
	.p2-hist {
		list-style: none;
		margin: 0 0 8px;
		padding: 0;
		display: grid;
		gap: 0;
	}
	.p2-hist li {
		--d: var(--faint);
		display: grid;
		grid-template-columns: 14px minmax(0, 1fr);
		gap: 8px;
		padding: 6px 0;
		border-left: 1px solid var(--line);
		margin-left: 5px;
		padding-left: 0;
		font-size: 12.5px;
		overflow-wrap: anywhere;
	}
	.p2-hist li.t-ok {
		--d: var(--ok);
	}
	.p2-hist li.t-warn {
		--d: var(--warn);
	}
	.p2-hist li.t-err {
		--d: var(--err);
	}
	.p2-dot {
		width: 9px;
		height: 9px;
		border-radius: 50%;
		background: var(--d);
		margin: 4px 0 0 -5px;
	}

	@media (max-width: 1100px) {
		.p2-gates {
			grid-template-columns: repeat(2, minmax(0, 1fr));
		}
		.p2-pd {
			grid-template-columns: minmax(0, 1fr);
		}
		.p2-flow {
			justify-items: start;
			grid-auto-flow: column;
			gap: 6px;
		}
		.p2-flow-arrow {
			transform: rotate(90deg);
		}
	}
	@media (max-width: 560px) {
		.p2-gates {
			grid-template-columns: minmax(0, 1fr);
		}
	}
</style>
