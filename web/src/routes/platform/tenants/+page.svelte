<script lang="ts">
	// P1 Tenants — the super_admin's portfolio. Leads with the lifecycle (create → provision down → active ⇄
	// suspended → offboarded/archived), then one card per live tenant answering "does this tenant need me?":
	// owner, provision-down per target, licence, jobs (release, pins behind, generator run), 30-day usage.
	import { superAdminAttrs } from '$lib/auth/roles';
	import { onMount } from 'svelte';
	import { goto } from '$app/navigation';
	import { api, guard, switchTenant } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { ApiError } from '$lib/api/errors';
	import { Resource } from '$lib/load.svelte';
	import type { Paged, Residency } from '$lib/api/types/core';
	import type {
		CreateTenantResult,
		ImportTenantResult,
		OffboardResult,
		PlatformTenantRow,
		TenantLifecycleStage,
		TenantPortfolio,
	} from '$lib/api/types/platform';
	import TenantsLifecycleP1 from './TenantsLifecycleP1.svelte';
	import TenantCardP1 from './TenantCardP1.svelte';
	import PageHead from '$lib/ui/PageHead.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Tabs from '$lib/ui/Tabs.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Pagination from '$lib/ui/Pagination.svelte';
	import MilestoneChip from '$lib/ui/MilestoneChip.svelte';
	import { utc } from '$lib/platform/ui';
	import { toast } from '$lib/ui/toaster.svelte';
	import OffboardDrawer from '$lib/platform/OffboardDrawer.svelte';

	const liveRes = new Resource<Paged<PlatformTenantRow>>(() => api().platformTenants());
	const archivedRes = new Resource<Paged<PlatformTenantRow>>(() => api().platformTenants({ archived: true }));
	const portfolioRes = new Resource<TenantPortfolio[]>(() => api().platformTenantPortfolio());
	let view = $state<'live' | 'archived'>('live');
	let stage = $state<TenantLifecycleStage | null>(null);
	const portfolio = $derived(
		new Map(
			portfolioRes.value.state === 'ok' ? portfolioRes.value.data.map((p) => [p.tenant_id, p] as const) : [],
		),
	);
	const allRows = $derived([
		...(liveRes.value.state === 'ok' ? liveRes.value.data.items : []),
		...(archivedRes.value.state === 'ok' ? archivedRes.value.data.items : []),
	]);
	const stages = $derived.by(() => {
		const out: Record<TenantLifecycleStage, string[]> = {
			provisioning: [],
			active: [],
			suspended: [],
			archived: [],
		};
		for (const t of allRows) out[portfolio.get(t.id)?.stage ?? t.status].push(t.name);
		return out;
	});
	const stageOf = (t: PlatformTenantRow) => portfolio.get(t.id)?.stage ?? t.status;
	$effect(() => {
		if (stage === 'archived') view = 'archived';
		else if (stage) view = 'live';
	});
	const attentionCount = $derived(
		[...portfolio.values()].filter((p) => p.attention.some((a) => a.severity !== 'info')).length,
	);
	const count = (r: Resource<Paged<PlatformTenantRow>>) =>
		r.value.state === 'ok' || r.value.state === 'empty' ? r.value.data.items.length : undefined;
	const tabs = $derived<[string, string, number?][]>([
		['live', 'Live', count(liveRes)],
		['archived', 'Archived', count(archivedRes)],
	]);

	function reload() {
		void liveRes.refresh();
		void archivedRes.refresh();
		void portfolioRes.refresh();
	}
	onMount(reload);

	// ---- create / import --------------------------------------------------------------------------------------
	let createOpen = $state(false);
	let newName = $state('');
	let newAdmin = $state('');
	/** Both required: with them the first admin's invite link asks only for a password. */
	let newAdminFirst = $state('');
	let newAdminLast = $state('');
	/** D59: data residency, fixed at provisioning. */
	let newResidency = $state<Residency>('eu');
	let saving = $state(false);
	let formError = $state<{ code: string; message: string } | null>(null);
	let created = $state<CreateTenantResult | null>(null);

	let importOpen = $state(false);
	let importId = $state('');
	let importName = $state('');
	let importAdmin = $state('');
	let importAdminFirst = $state('');
	let importAdminLast = $state('');
	const importByEmail = $derived(importAdmin.includes('@'));
	/** D66: residency is chosen at import too (an import creates the tenant here). */
	let importResidency = $state<Residency>('eu');
	let imported = $state<ImportTenantResult | null>(null);

	function fault(e: unknown) {
		return e instanceof ApiError
			? { code: e.code, message: e.message }
			: { code: 'error', message: (e as Error).message };
	}

	async function create(e: SubmitEvent) {
		e.preventDefault();
		saving = true;
		formError = null;
		try {
			created = await api().createTenant({
				name: newName,
				first_admin_email: newAdmin,
				first_admin_first_name: newAdminFirst,
				first_admin_last_name: newAdminLast,
				residency: newResidency,
			});
			toast(`Created ${created.tenant.name}.`);
			createOpen = false;
			newName = '';
			newAdmin = '';
			newAdminFirst = '';
			newAdminLast = '';
			reload();
		} catch (err) {
			formError = fault(err);
			toast(`Not created: ${formError.message}`, 'err');
		} finally {
			saving = false;
		}
	}

	async function importTenant(e: SubmitEvent) {
		e.preventDefault();
		saving = true;
		formError = null;
		try {
			imported = await api().importTenant({
				id: importId,
				name: importName,
				first_admin: importAdmin,
				...(importByEmail
					? { first_admin_first_name: importAdminFirst, first_admin_last_name: importAdminLast }
					: {}),
				residency: importResidency,
			});
			toast(
				imported.imported
					? `Imported ${imported.tenant.name}.`
					: `${imported.tenant.name} was imported already.`,
			);
			importOpen = false;
			importId = '';
			importName = '';
			importAdmin = '';
			importAdminFirst = '';
			importAdminLast = '';
			reload();
		} catch (err) {
			formError = fault(err);
			toast(`Not imported: ${formError.message}`, 'err');
		} finally {
			saving = false;
		}
	}

	// ---- suspend / resume / act in / offboard ---------------------------------------------------------------------
	let busy = $state<string | null>(null);
	async function setStatus(t: PlatformTenantRow, status: 'active' | 'suspended') {
		busy = t.id;
		const ok = await guard(
			() => api().setTenantStatus(t.id, status),
			status === 'suspended'
				? `${t.name} suspended: members are refused, the edge stops serving it.`
				: `${t.name} resumed.`,
		);
		busy = null;
		if (ok) reload();
	}

	async function actIn(t: PlatformTenantRow) {
		busy = t.id;
		const err = await switchTenant(t.id);
		busy = null;
		if (err) toast(err, 'err');
		else void goto('/admin');
	}

	let target = $state<PlatformTenantRow | null>(null);
	let offboardOpen = $state(false);
	let offboarded = $state<{ name: string; result: OffboardResult } | null>(null);
	function openOffboard(t: PlatformTenantRow) {
		target = t;
		offboardOpen = true;
	}
</script>

<PageHead
	id="P1"
	sub="Every customer organisation on the platform and where it is in its life. Creating one provisions it down (licence, knowledge store, ingest connections); suspending is a reversible pause; offboarding archives it for good."
>
	{#snippet actions()}
		<button
			class="btn"
			{...superAdminAttrs(session)}
			onclick={() => {
				formError = null;
				importOpen = true;
			}}>Import with fixed id</button
		>
		<button
			class="btn primary"
			{...superAdminAttrs(session)}
			onclick={() => {
				formError = null;
				createOpen = true;
			}}>Create tenant</button
		>
	{/snippet}
</PageHead>

{#if created}
	<Callout kind="info" title="Created {created.tenant.name}">
		{#if created.licence}Licence <code class="mono">{created.licence.id}</code>{#if created.licence.kid}
				sealed with key <code class="mono">{created.licence.kid}</code>{/if}{:else}No licence yet (the licence
			tick issues it){/if}{created.invited ? '; an invite went to the first admin' : ''}. Provisioning: {created
			.provisioning.length} rows pending (licences, knowledge, ingest) — the drainer runs them asynchronously.
		<a href={`/platform/tenants/${created.tenant.id}`}>Open {created.tenant.name}</a>
	</Callout>
{/if}
{#if imported}
	<Callout kind="info" title={imported.imported ? `Imported ${imported.tenant.name}` : 'Already imported'}>
		{imported.imported
			? `It keeps its id ${imported.tenant.id}; licence issued and provisioning queued.`
			: `${imported.tenant.name} (${imported.tenant.id}) exists already; nothing changed (import is idempotent).`}
		<a href={`/platform/tenants/${imported.tenant.id}`}>Open it</a>
	</Callout>
{/if}
{#if offboarded}
	<Callout kind="gap" title="Offboarded {offboarded.name}">
		Licence revoked, workspace archived, {offboarded.result.users_disabled} user{offboarded.result
			.users_disabled === 1
			? ''
			: 's'} disabled. Knowledge and ingest data are <strong>retained</strong> by strata_rag / strata_ingest (<code
			class="mono">complete: false</code
		>); their licence revokes are queued.
		<a href={`/platform/tenants/${offboarded.result.tenant_id}`}>See the offboard report</a>
	</Callout>
{/if}

<Card
	title="Lifecycle"
	sub="Click a stage to filter. Hover a stage for what it means for the tenant's members."
>
	<TenantsLifecycleP1 {stages} bind:selected={stage} />
</Card>

<Card cls="flush">
	<Tabs items={tabs} bind:active={view} label="Tenant view">
		{#if view === 'live'}
			<LoadState resource={liveRes} empty="No live tenants." loading="Loading tenants…">
				{#snippet children(page)}
					{@const shown = page.items.filter((t) => !stage || stageOf(t) === stage)}
					<div class="p1-bar">
						<span class="p1-bar-text">
							{#if stage}
								Showing <strong>{stage}</strong> tenants ({shown.length}).
								<button class="linkish" onclick={() => (stage = null)}>Show all</button>
							{:else if portfolioRes.value.state === 'ok'}
								{attentionCount
									? `${attentionCount} of ${page.items.length} tenants need attention — the coloured edge and the list at the bottom of each card say why.`
									: 'Every live tenant is provisioned, licensed and serving.'}
							{:else if portfolioRes.value.state === 'unavailable'}
								Owners, jobs and usage are unavailable right now; the registry below is still current.
								<button class="linkish" onclick={() => portfolioRes.refresh()}>Retry</button>
							{/if}
						</span>
					</div>
					{#if shown.length}
						<div class="p1-cards">
							{#each shown as t (t.id)}
								<TenantCardP1
									{t}
									pf={portfolio.get(t.id) ?? null}
									busy={busy === t.id}
									onactin={() => actIn(t)}
									onstatus={(s) => setStatus(t, s)}
									onoffboard={() => openOffboard(t)}
								/>
							{/each}
						</div>
					{:else}
						<p class="note p1-empty">No live tenant is {stage} right now.</p>
					{/if}
					<Pagination next={page.next} count={page.items.length} />
				{/snippet}
			</LoadState>
		{:else}
			<LoadState resource={archivedRes} empty="No archived tenants." loading="Loading archived tenants…">
				{#snippet children(page)}
					<p class="note archived-note">
						Offboarded tenants stay on record, never deleted: licence revoked, users disabled, everyone gets
						403
						<code>tenant_archived</code>. Their knowledge and ingest data are retained by strata_rag /
						strata_ingest — the offboard report on each tenant lists what.
					</p>
					<div class="tbl-wrap">
						<table class="tbl">
							<thead>
								<tr>
									<th>Tenant</th>
									<th>Archived</th>
									<th>Licence</th>
									<th class="r">Jobs</th>
									<th><span class="sr-only">Actions</span></th>
								</tr>
							</thead>
							<tbody>
								{#each page.items as t (t.id)}
									<tr class="archived" data-testid="archived-row">
										<td>
											<a class="cell-title" href={`/platform/tenants/${t.id}`}>{t.name}</a>
											<span class="cell-sub">{t.description}</span>
											<span class="cell-sub mono">{t.id}</span>
										</td>
										<td class="mono nowrap">{utc(t.archived_at)}</td>
										<td><Chip text={t.licence.status} variant="err" /></td>
										<td class="r">{t.jobs}</td>
										<td>
											<div class="actions row-actions">
												<a class="btn sm" href={`/platform/tenants/${t.id}`}>Offboard report</a>
												<button class="btn sm" disabled title="Archived tenants are read-only">Resume</button>
											</div>
										</td>
									</tr>
								{/each}
							</tbody>
						</table>
					</div>
				{/snippet}
			</LoadState>
		{/if}
	</Tabs>
</Card>

<Drawer
	bind:open={createOpen}
	title="Create tenant"
	dirty={!!(newName || newAdmin || newAdminFirst || newAdminLast)}
>
	{#snippet sub()}<MilestoneChip milestone="M1" note="M1 creates by sub; email invite is M2" />{/snippet}
	<form class="stack" onsubmit={create}>
		<div class="field">
			<label for="new-name">Tenant name</label>
			<input id="new-name" class="input" bind:value={newName} required minlength="2" maxlength="120" />
		</div>
		<div class="field">
			<label for="new-admin">First admin email</label>
			<input id="new-admin" class="input" type="email" bind:value={newAdmin} required />
			<span class="note"
				>Not in Keycloak yet? They get an invite. Someone who already has a tenant → 409.</span
			>
		</div>
		<div class="field">
			<label for="new-admin-first">First admin first name</label>
			<input
				id="new-admin-first"
				class="input"
				bind:value={newAdminFirst}
				required
				maxlength="80"
				autocomplete="off"
			/>
		</div>
		<div class="field">
			<label for="new-admin-last">First admin last name</label>
			<input
				id="new-admin-last"
				class="input"
				bind:value={newAdminLast}
				required
				maxlength="80"
				autocomplete="off"
			/>
			<span class="note">With both names the invite asks them only to set a password.</span>
		</div>
		<fieldset class="field residency" data-testid="new-residency">
			<legend class="label">Data residency</legend>
			<label class="row"><input type="radio" bind:group={newResidency} value="eu" /> EU</label>
			<label class="row"><input type="radio" bind:group={newResidency} value="us" /> US</label>
			<span class="note"
				>Fixed at provisioning; changing it later is a support migration (D66). Pins the tenant’s knowledge
				stores, LLM providers and logs to this region. A tenant setting, not a job choice (D59).</span
			>
		</fieldset>
		{#if formError}
			<Callout kind="err" title={formError.code}>{formError.message}</Callout>
		{/if}
		<div class="actions">
			<button class="btn primary" type="submit" disabled={saving || !session.superAdmin}
				>{saving ? 'Creating…' : 'Create tenant'}</button
			>
		</div>
	</form>
</Drawer>

<Drawer bind:open={importOpen} title="Import a tenant with a fixed id" dirty={!!(importId || importName)}>
	{#snippet sub()}<span
			>K30 — aligns a tenant that exists elsewhere (configurator, w2v) under its own id.</span
		>{/snippet}
	<form class="stack" onsubmit={importTenant}>
		<div class="field">
			<label for="import-id">Tenant id</label>
			<input
				id="import-id"
				class="input mono"
				bind:value={importId}
				required
				placeholder="0195f48b-d000-7000-8000-0000000000e5"
			/>
		</div>
		<div class="field">
			<label for="import-name">Tenant name</label>
			<input id="import-name" class="input" bind:value={importName} required />
		</div>
		<div class="field">
			<label for="import-admin">First admin (email or user id)</label>
			<input id="import-admin" class="input" bind:value={importAdmin} required />
		</div>
		{#if importByEmail}
			<div class="field">
				<label for="import-admin-first">First admin first name</label>
				<input
					id="import-admin-first"
					class="input"
					bind:value={importAdminFirst}
					required
					maxlength="80"
					autocomplete="off"
				/>
			</div>
			<div class="field">
				<label for="import-admin-last">First admin last name</label>
				<input
					id="import-admin-last"
					class="input"
					bind:value={importAdminLast}
					required
					maxlength="80"
					autocomplete="off"
				/>
			</div>
		{/if}
		<fieldset class="field residency" data-testid="import-residency">
			<legend class="label">Data residency</legend>
			<label class="row"><input type="radio" bind:group={importResidency} value="eu" /> EU</label>
			<label class="row"><input type="radio" bind:group={importResidency} value="us" /> US</label>
			<span class="note">Fixed at import; changing it later is a support migration (D66).</span>
		</fieldset>
		{#if formError}
			<Callout kind="err" title={formError.code}>{formError.message}</Callout>
		{/if}
		<div class="actions">
			<button class="btn primary" type="submit" disabled={saving || !session.superAdmin}
				>{saving ? 'Importing…' : 'Import tenant'}</button
			>
		</div>
		<p class="note">
			Importing an id that exists already is a no-op (idempotent). A malformed id is refused and audited.
		</p>
	</form>
</Drawer>

<OffboardDrawer
	bind:open={offboardOpen}
	tenant={target}
	ondone={(result, name) => {
		offboarded = { name, result };
		reload();
	}}
/>

<style>
	.p1-bar {
		padding: 4px 16px 10px;
		font-size: 13px;
		color: var(--muted);
	}
	.linkish {
		all: unset;
		cursor: pointer;
		color: var(--accent);
		text-decoration: underline;
	}
	.p1-cards {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(min(100%, 460px), 1fr));
		gap: 14px;
		padding: 0 16px 6px;
	}
	.p1-empty {
		padding: 0 16px;
	}
	.row-actions {
		justify-content: flex-end;
		flex-wrap: nowrap;
	}
	tr.archived td {
		color: var(--muted);
	}
	.archived-note {
		padding: 0 16px;
	}
	.residency {
		border: 1px solid var(--line);
		border-radius: 6px;
		padding: 8px 12px;
		display: grid;
		gap: 4px;
		margin: 0;
	}
	.sr-only {
		position: absolute;
		width: 1px;
		height: 1px;
		overflow: hidden;
		clip: rect(0 0 0 0);
	}
</style>
