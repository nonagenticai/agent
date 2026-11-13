<script lang="ts">
	// The shell: sign-in (Keycloak), the scoped sidebar (switchers + nav built from the page registry), a top bar with
	// breadcrumbs and the user menu, blocked shells, the route access guard and the view-as banner. Shell parts live
	// in $lib/shell.
	import '../app.css';
	import { onMount } from 'svelte';
	import { page } from '$app/state';
	import { logout, session } from '$lib/auth/session.svelte';
	import {
		api,
		app,
		bootKeycloak,
		enterSuspended,
		exitViewAs,
		retryLicence,
		switchTenant,
		viewAs,
	} from '$lib/app.svelte';
	import { ROLE_KEYS, rolesText, sortRoles, type Role, type RoleInfo } from '$lib/api/types/core';
	import RoleSetEditor from '$lib/tenant/RoleSetEditor.svelte';
	import { PUBLIC_KEYCLOAK_URL, PUBLIC_KEYCLOAK_REALM, PUBLIC_KEYCLOAK_CLIENT_ID } from '$app/env/public';
	import { authConfigFrom, type AuthConfig } from '$lib/auth/config';
	import Sidebar from '$lib/shell/Sidebar.svelte';
	import UserMenu from '$lib/shell/UserMenu.svelte';
	import Crumbs from '$lib/shell/Crumbs.svelte';
	import CommandPalette from '$lib/shell/CommandPalette.svelte';
	import { crumbsFor } from '$lib/shell/crumbs';
	import { accessFor, navFor, scopeForRoute, titleForRoute } from '$lib/pages';
	import { loadPrefs } from '$lib/ui/prefs.svelte';
	import Icon from '$lib/ui/Icon.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import Toast from '$lib/ui/Toast.svelte';
	import TenantPicker from '$lib/ui/TenantPicker.svelte';
	import LicenceBlocked from '$lib/licence/LicenceBlocked.svelte';
	import { tenantName } from '$lib/tenants';
	import { badgesFor } from '$lib/status/inbox';
	import { jobPath } from '$lib/scope';
	import type { AttentionItem } from '$lib/api/types/tenant';

	let { children, data }: { children: import('svelte').Snippet; data?: { config?: AuthConfig } } = $props();
	let bootError = $state('');
	let navOpen = $state(false);
	let collapsed = $state(false);
	let tenantOpen = $state(false);

	onMount(() => {
		loadPrefs();
		// An unknown path (the 404 page) has no layout data: read the same public env the server load reads.
		bootKeycloak(
			data?.config ??
				authConfigFrom({
					PUBLIC_KEYCLOAK_URL,
					PUBLIC_KEYCLOAK_REALM,
					PUBLIC_KEYCLOAK_CLIENT_ID,
				}),
		).catch((e) => (bootError = (e as Error).message));
	});

	const jobLabel = $derived(app.jobs.find((j) => j.key === app.job)?.label ?? app.job);
	const tenantLabel = $derived(
		app.platformOnly
			? 'Platform (no tenant)'
			: (session.me?.tenant?.name ?? tenantName(app.tenants, app.activeTenant) ?? '—'),
	);
	const roleLabel = $derived(
		session.superAdmin
			? 'Super admin'
			: !session.member
				? 'No role'
				: session.roles.length === ROLE_KEYS.length
					? 'All roles'
					: rolesText(session.roles),
	);

	// ---- D79 view-as: narrow my own roles (avatar menu → "View as…"; banner with Exit while it is on) ----------
	let viewAsOpen = $state(false);
	let viewAsCatalog = $state<RoleInfo[]>([]);
	let viewAsDraft = $state<Role[]>([]);
	let viewAsTtl = $state(3600);
	let viewAsError = $state('');
	let viewAsBusy = $state(false);
	async function openViewAs() {
		viewAsError = '';
		viewAsDraft = sortRoles(session.viewAs ? session.effectiveRoles : session.roles);
		viewAsOpen = true;
		try {
			viewAsCatalog = await api().roleCatalog();
		} catch (e) {
			viewAsError = (e as Error).message;
		}
	}
	async function applyViewAs() {
		viewAsBusy = true;
		const err = await viewAs(viewAsDraft, viewAsTtl);
		viewAsBusy = false;
		if (err) viewAsError = err;
		else viewAsOpen = false;
	}
	async function leaveViewAs() {
		viewAsOpen = false;
		await exitViewAs();
	}
	const crumbs = $derived(
		crumbsFor(page.route.id, tenantLabel, app.jobs.length ? jobLabel : null, app.job || null),
	);
	const nav = $derived(navFor(session, app.job, scopeForRoute(page.route.id)));
	const routeId = $derived(page.route.id);
	const access = $derived(accessFor(routeId, session));
	const displayName = $derived(session.me?.display_name ?? session.user?.name ?? '');
	const currentTitle = $derived(titleForRoute(routeId));
	/**
	 * Pages remount when the data, tenant or job they read changes. `/support` is tenant-wide (every call
	 * names its job), so a job change never remounts it — its tab, filters and stream survive.
	 */
	const tenantWide = $derived(routeId === '/support' || (routeId ?? '').startsWith('/support/'));
	// The tenant part is the SETTLED scope (`me.tenant`, set with `platformOnly` in one step), never the header being
	// switched: remounting on `app.activeTenant` re-ran the old tenant's page with no tenant header (409s).
	const scopeKey = $derived(`${app.epoch}|${session.me?.tenant?.id ?? ''}|${tenantWide ? '*' : app.job}`);
	const footer = ['Live data', 'agent-api'];
	/**
	 * The inbox behind the sidebar badges: what needs a person across the workspace (`GET /api/inbox`). Loaded once
	 * per scope; the badges narrow it to what the caller's effective roles can act on, so view-as needs no reload.
	 */
	let inboxItems = $state<AttentionItem[]>([]);
	$effect(() => {
		void scopeKey;
		const platform = (routeId ?? '').startsWith('/platform');
		if (!session.member || platform || access !== 'ok' || licenceHere) {
			inboxItems = [];
			return;
		}
		let current = true;
		api()
			.inbox()
			.then((i) => {
				if (current) inboxItems = i.items;
			})
			.catch(() => {
				if (current) inboxItems = [];
			});
		return () => {
			current = false;
		};
	});
	const badges = $derived(
		badgesFor(inboxItems, session.effectiveRoles, app.job || null, {
			workspace: '/admin',
			agent: app.job ? jobPath(app.job) : null,
		}),
	);
	/** Licence gate 2 refused the tenant: tenant pages give way to the licence state (platform routes are exempt). */
	const licenceHere = $derived(app.licence && !(routeId ?? '').startsWith('/platform') ? app.licence : null);
	const suspendedEntered = $derived(
		session.superAdmin && session.me?.tenant?.status === 'suspended' && app.enteredSuspended !== null,
	);

	/** Below this width the nav is an off-canvas drawer; keep in sync with app.css. */
	const PHONE = '(max-width: 860px)';
	let narrow = $state(false);
	let dark = $state(false);
	const THEME_KEY = 'agent-theme';
	$effect(() => {
		const mq = matchMedia(PHONE);
		const onChange = () => {
			narrow = mq.matches;
			if (!narrow) navOpen = false;
		};
		onChange();
		mq.addEventListener('change', onChange);
		const scheme = matchMedia('(prefers-color-scheme: dark)');
		const syncDark = () =>
			(dark = document.documentElement.dataset.theme
				? document.documentElement.dataset.theme === 'dark'
				: scheme.matches);
		syncDark();
		scheme.addEventListener('change', syncDark);
		return () => {
			mq.removeEventListener('change', onChange);
			scheme.removeEventListener('change', syncDark);
		};
	});
	function navigationToggle() {
		if (narrow) navOpen = !navOpen;
		else collapsed = !collapsed;
	}
	function toggleTheme() {
		const root = document.documentElement;
		const isDark = root.dataset.theme
			? root.dataset.theme === 'dark'
			: matchMedia('(prefers-color-scheme: dark)').matches;
		const next = isDark ? 'light' : 'dark';
		root.dataset.theme = next;
		dark = next === 'dark';
		try {
			localStorage.setItem(THEME_KEY, next);
		} catch {
			// Storage blocked (private mode, policy): the choice lasts for this page only.
		}
	}
</script>

<svelte:head><title>{currentTitle ? `${currentTitle} · Agent` : 'Agent'}</title></svelte:head>

{#snippet blockedShell()}
	{#if app.blocked === 'not_provisioned'}
		<div class="callout err" data-testid="blocked-not-provisioned">
			<strong>Your account has no Agent tenant yet</strong>
			<span
				>You are signed in as {session.me?.display_name ?? session.user?.name}, but Agent has no tenant for
				this account. Ask your organisation's administrator to add you, then sign in again.</span
			>
		</div>
	{:else if app.blocked === 'tenant_suspended'}
		<div class="callout err" data-testid="blocked-tenant-suspended">
			<strong>{session.me?.tenant?.name ?? 'This tenant'} is suspended</strong>
			<span
				>Agent is switched off for this tenant: staff pages are closed and customers get the polite
				“unavailable” reply. Contact your administrator.</span
			>
		</div>
		{#if session.superAdmin}
			<div class="row">
				<button class="btn primary" type="button" onclick={() => enterSuspended()}
					>Enter as super admin</button
				>
				<button class="btn" type="button" onclick={() => switchTenant(null)}>Back to platform</button>
			</div>
		{/if}
	{:else if app.blocked === 'tenant_archived'}
		<div class="callout err" data-testid="blocked-tenant-archived">
			<strong>{session.me?.tenant?.name ?? 'This tenant'} is archived</strong>
			<span
				>The tenant was offboarded. Archived is terminal: nobody can act in it, its licence is revoked, and
				its knowledge data is retained.</span
			>
		</div>
		{#if session.superAdmin}
			<div class="row">
				<button class="btn" type="button" onclick={() => switchTenant(null)}>Back to platform</button>
			</div>
		{/if}
	{/if}
{/snippet}

{#snippet previewBanner()}
	{#if session.viewAs}
		<div class="viewas-banner" role="note" data-testid="view-as-banner">
			<span>Viewing as: {rolesText(session.effectiveRoles)}</span>
			<span aria-hidden="true">·</span>
			<button class="viewas-exit" type="button" data-testid="view-as-exit" onclick={() => leaveViewAs()}
				>Exit</button
			>
		</div>
	{/if}
{/snippet}

{#if bootError}
	<main class="content">
		<div class="view">
			<div class="callout err">
				<strong>Sign-in failed</strong><span>{bootError}</span>
			</div>
		</div>
	</main>
{:else if !app.ready}
	<div class="shell skeleton" data-testid="boot-skeleton" aria-busy="true">
		<aside class="side" aria-hidden="true">
			<div class="brand">
				<span class="brand-mark"><i></i><i></i><i></i></span>
				<div><b>Agent</b><span class="sk sk-s"></span></div>
			</div>
			<div class="sk-nav">
				{#each [60, 80, 70, 90, 65, 75, 55] as w, i (i)}<span class="sk sk-bar" style="width:{w}%"
					></span>{/each}
			</div>
		</aside>
		<div class="main">
			<div class="topbar" aria-hidden="true"><span class="sk sk-crumb"></span></div>
			<main class="content">
				<div class="view">
					<p class="muted" role="status">Signing in…</p>
				</div>
			</main>
		</div>
	</div>
{:else}
	<div class="shell" class:nav-collapsed={collapsed && !narrow}>
		{#if !narrow}
			<aside class="side" id="main-navigation" aria-label="Main navigation">
				<Sidebar {nav} {tenantLabel} {footer} {badges} onpickworkspace={() => (tenantOpen = true)} />
			</aside>
		{/if}
		<div class="main">
			<div class="topbar">
				<button
					class="icon-btn menu-btn"
					type="button"
					aria-label={narrow ? 'Open navigation' : collapsed ? 'Show navigation' : 'Hide navigation'}
					aria-expanded={narrow ? navOpen : !collapsed}
					aria-controls={narrow ? 'mobile-navigation' : 'main-navigation'}
					onclick={navigationToggle}><Icon name="menu" /></button
				>
				<Crumbs {crumbs} />
				<CommandPalette {session} job={app.job} />
				{#if !narrow}
					<UserMenu
						{displayName}
						{roleLabel}
						{dark}
						ontheme={toggleTheme}
						onviewas={openViewAs}
						onleaveviewas={leaveViewAs}
					/>
				{/if}
			</div>
			<main class="content">
				<div class="view">
					{@render previewBanner()}
					{#if suspendedEntered}
						<div class="callout gap" data-testid="suspended-entered">
							<strong>{session.me?.tenant?.name} is suspended</strong>
							<span>You entered as super admin. Members are blocked and customers get the refusal text.</span>
						</div>
					{/if}
					{#if app.blocked}
						{@render blockedShell()}
					{:else if licenceHere}
						<LicenceBlocked
							code={licenceHere.code}
							tenantName={session.me?.tenant?.name ?? null}
							retry={retryLicence}
							backToPlatform={session.superAdmin ? () => void switchTenant(null) : undefined}
						/>
					{:else if access === 'super_admin_only'}
						<div class="callout err" data-testid="super-admin-only">
							<strong>Super-admin only</strong>
							<span>Platform pages are for platform super admins. Nothing was loaded.</span>
						</div>
					{:else if access === 'pick_tenant'}
						<div class="callout info" data-testid="pick-tenant">
							<strong>Choose a tenant to see this page</strong>
							<span
								>You are a super admin in the platform view. {currentTitle ?? 'This page'} acts in one tenant:
								pick it here.</span
							>
						</div>
						<TenantPicker />
					{:else if access === 'no_access'}
						<div class="callout err">
							<strong>No access</strong><span>You have no role in a tenant.</span>
						</div>
					{:else}
						{#key scopeKey}{@render children()}{/key}
					{/if}
				</div>
			</main>
		</div>
	</div>
	{#if session.superAdmin}
		<Drawer bind:open={tenantOpen} title="Act in tenant">
			{#if tenantOpen}<TenantPicker onpicked={() => (tenantOpen = false)} />{/if}
		</Drawer>
	{/if}
	{#if session.member}
		<Drawer bind:open={viewAsOpen} title="View as…">
			{#if viewAsOpen}
				<div class="viewas-panel" data-testid="view-as-panel">
					<p class="muted">
						See and use the app with only some of your roles — to check what a colleague with those roles can
						do. It can only narrow what you hold, ends by itself, and is recorded in the audit log.
					</p>
					{#if viewAsCatalog.length}
						<RoleSetEditor
							catalog={viewAsCatalog}
							bind:value={viewAsDraft}
							only={session.roles}
							name="view-as"
							legend="View as"
							compact
						/>
					{/if}
					<label class="field"
						><span class="label">For</span>
						<select class="input" bind:value={viewAsTtl}>
							<option value={900}>15 minutes</option>
							<option value={3600}>1 hour</option>
							<option value={28800}>8 hours</option>
						</select></label
					>
					{#if viewAsError}<p class="err-text" role="alert">{viewAsError}</p>{/if}
					<div class="row">
						<button
							class="btn primary"
							type="button"
							data-testid="view-as-apply"
							disabled={viewAsBusy || !viewAsCatalog.length}
							onclick={() => applyViewAs()}>View as {rolesText(viewAsDraft)}</button
						>
						{#if session.viewAs}<button class="btn" type="button" onclick={() => leaveViewAs()}
								>Exit view-as</button
							>{/if}
					</div>
				</div>
			{/if}
		</Drawer>
	{/if}
	{#if narrow}
		<Drawer bind:open={navOpen} title="Navigation">
			<div id="mobile-navigation" class="mobile-nav">
				<Sidebar
					{nav}
					{tenantLabel}
					{footer}
					{badges}
					inDrawer
					onpickworkspace={() => {
						navOpen = false;
						tenantOpen = true;
					}}
					onnavigate={() => (navOpen = false)}
				/>
				<UserMenu
					inline
					{displayName}
					{roleLabel}
					{dark}
					ontheme={toggleTheme}
					onviewas={openViewAs}
					onleaveviewas={leaveViewAs}
					onnavigate={() => (navOpen = false)}
				/>
			</div>
		</Drawer>
	{/if}
{/if}
<Toast />

<style>
	.viewas-banner {
		margin: 0 0 16px;
		padding: 8px 12px;
		border-radius: 6px;
		border: 1px solid color-mix(in srgb, var(--accent) 35%, var(--line));
		background: color-mix(in srgb, var(--accent) 10%, var(--panel));
		color: var(--accent);
		font-size: 13px;
		font-weight: 500;
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		align-items: center;
	}
	.viewas-exit {
		border: 0;
		background: none;
		padding: 0;
		color: inherit;
		font: inherit;
		text-decoration: underline;
		cursor: pointer;
	}
	.viewas-panel {
		display: grid;
		gap: 14px;
	}
	.viewas-panel .muted {
		margin: 0;
	}
	.err-text {
		margin: 0;
		color: var(--err);
	}
	.mobile-nav {
		display: flex;
		flex-direction: column;
		min-height: 100%;
	}
	/* Boot skeleton: same grid as the shell, so nothing jumps when the app is ready. */
	.sk {
		display: block;
		border-radius: 4px;
		background: var(--sunk);
	}
	.side .sk {
		background: var(--side-hi);
	}
	.sk-s {
		width: 90px;
		height: 10px;
		margin-top: 4px;
	}
	.sk-nav {
		display: grid;
		gap: 12px;
		padding: 18px 20px;
	}
	.sk-bar {
		height: 12px;
	}
	.sk-crumb {
		width: 200px;
		max-width: 50%;
		height: 14px;
	}
</style>
