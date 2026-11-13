<script lang="ts">
	// T3 Users: who may use Agent in the active tenant, and at which role. Everyone signs in through the one shared
	// portal realm; membership + role live in Agent's own users table (07 §4.2, D39). Invite by e-mail is the primary
	// form (M2, D39: 202 "invite sent", 7-day link); "Add existing user by ID" (M1, Keycloak sub) is secondary. Role
	// change and soft Disable keep the last-admin guard for every caller, super_admin included (07 §4.4). Disabled
	// rows stay, greyed (K9). D88: a member holds a SET of §R.1 roles (checkboxes from `GET /api/roles`; none =
	// observer). Writes here need the Tenant admin role; without it they are disabled, never hidden.
	import { onMount } from 'svelte';
	import { api, app, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { canWrite as canWriteFor, writeAttrs, writeHint } from '$lib/auth/roles';
	import { Resource } from '$lib/load.svelte';
	import { isLastAdmin } from '$lib/tenants';
	import type { Paged, Role, RoleInfo, TenantUser } from '$lib/api/types/core';
	import { rolesText, sortRoles } from '$lib/api/types/core';
	import RoleSetEditor from '$lib/tenant/RoleSetEditor.svelte';
	import InvitesPanel from '$lib/tenant/InvitesPanel.svelte';
	import type { MemberActivity } from '$lib/api/types/tenant';
	import {
		ago,
		isQuiet,
		memberState,
		ORIGIN_LABEL,
		QUIET_DAYS,
		shortDate,
		summarize,
		SURFACE_LABEL,
		type MemberState,
	} from '$lib/tenant/T3-members';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import Seg from '$lib/ui/Seg.svelte';
	import StateDiagram from '$lib/ui/StateDiagram.svelte';
	import MilestoneChip from '$lib/ui/MilestoneChip.svelte';

	/** The tenant the page acts in: a super_admin's chosen tenant, else the caller's own. */
	const tenant = $derived(app.activeTenant ?? session.me?.tenant?.id ?? '');
	const tenantName = $derived(session.me?.tenant?.name ?? 'this tenant');
	const users = new Resource<Paged<TenantUser>>(() => api().tenantUsers(tenant));
	const activity = new Resource<MemberActivity[]>(() => api().memberActivity());
	const catalogRes = new Resource<RoleInfo[]>(() => api().roleCatalog());
	const refreshAll = () => Promise.all([users.refresh(), activity.refresh()]);
	onMount(() => {
		refreshAll();
		catalogRes.refresh();
	});
	const catalog = $derived(catalogRes.value.state === 'ok' ? catalogRes.value.data : []);
	const now = Date.now();
	const canWrite = $derived(canWriteFor(session, 'tenant_admin'));
	const WRITE_HINT = writeHint('tenant_admin');
	const list = $derived(users.value.state === 'ok' ? users.value.data.items : []);
	const acts = $derived(
		new Map((activity.value.state === 'ok' ? activity.value.data : []).map((a) => [a.sub, a])),
	);
	const sum = $derived(summarize(list, acts, now));
	/** Admins who can still sign in: the guard counts these. */
	const signInAdmins = $derived(list.filter((u) => memberState(u) === 'active'));
	const LAST = "The tenant's last Tenant admin. Give another member the Tenant admin role first.";
	const lastAdmin = (u: TenantUser) => memberState(u) === 'active' && isLastAdmin(signInAdmins, u.sub);
	const me = $derived(session.me?.sub ?? '');

	const ORDER: Record<MemberState, number> = { active: 0, invited: 1, disabled: 2 };
	let filter = $state<'all' | MemberState>('all');
	/** B1b: the Members table, or the invites (Pending / Expired / Accepted / Revoked). */
	let tab = $state<'members' | 'invites'>('members');
	const shown = (items: TenantUser[]) =>
		items
			.filter((u) => filter === 'all' || memberState(u) === filter)
			.sort(
				(a, b) =>
					ORDER[memberState(a)] - ORDER[memberState(b)] ||
					Number(b.roles.includes('tenant_admin')) - Number(a.roles.includes('tenant_admin')) ||
					b.roles.length - a.roles.length,
			);
	/** Active holders of `r`; `null` = observers (no role). */
	const holders = (r: Role | null) =>
		list
			.filter((u) => memberState(u) === 'active' && (r ? u.roles.includes(r) : u.roles.length === 0))
			.map((u) => u.display_name);
	const initials = (name: string) =>
		name
			.split(/\s+/)
			.filter((w) => /^[A-Z]/.test(w))
			.map((w) => w[0])
			.slice(0, 2)
			.join('') || name.slice(0, 1).toUpperCase();

	let busy = $state(false);
	let email = $state('');
	/** Both required (as in commonpractice): with them Keycloak's invite link asks only for a password. */
	let firstName = $state('');
	let lastName = $state('');
	let inviteRoles = $state<Role[]>([]);
	let bySub = $state(false);
	let sub = $state('');
	let subRoles = $state<Role[]>([]);
	/** The member whose role set is being edited, and the draft set. */
	let editing = $state<string | null>(null);
	let draftRoles = $state<Role[]>([]);
	let lastInvite = $state<string | null>(null);

	async function run(action: () => Promise<unknown>, msg: string, fail: string): Promise<boolean> {
		if (busy) return false;
		busy = true;
		const ok = await guard(action, msg, fail);
		busy = false;
		await refreshAll();
		return ok;
	}
	async function invite(e: SubmitEvent) {
		e.preventDefault();
		const addr = email.trim();
		const first = firstName.trim();
		const last = lastName.trim();
		if (!addr || !first || !last) return;
		const ok = await run(
			() => api().inviteUser({ email: addr, roles: inviteRoles, first_name: first, last_name: last }),
			`Invite sent to ${addr}.`,
			'Invite not sent',
		);
		if (ok) {
			lastInvite = addr;
			email = '';
			firstName = '';
			lastName = '';
			inviteRoles = [];
		}
	}
	async function addBySub(e: SubmitEvent) {
		e.preventDefault();
		const s = sub.trim();
		if (!s) return;
		const ok = await run(() => api().addUserBySub({ sub: s, roles: subRoles }), `Added ${s}.`, 'Not added');
		if (ok) {
			sub = '';
			subRoles = [];
		}
	}
	function startEdit(u: TenantUser) {
		editing = u.sub;
		draftRoles = sortRoles(u.roles);
	}
	async function saveRoles(u: TenantUser) {
		const to = draftRoles;
		const ok = await run(
			() => api().setUserRole(u.sub, to),
			`${u.display_name}: ${rolesText(to)}.`,
			'Roles not changed',
		);
		if (ok) editing = null;
	}
	const sameSet = (a: Role[], b: Role[]) => sortRoles(a).join() === sortRoles(b).join();
	const disable = (u: TenantUser) =>
		run(
			() => api().disableUser(u.sub),
			`${u.display_name} is disabled. Their history stays.`,
			'Not disabled',
		);
	const enable = (u: TenantUser) =>
		run(() => api().enableUser(u.sub), `${u.display_name} can sign in again.`, 'Not enabled');

	const why = (u: TenantUser) =>
		!canWrite ? WRITE_HINT : lastAdmin(u) ? LAST : u.sub === me ? 'You cannot disable yourself' : undefined;

	const STATES = [
		{ id: 'invited', label: 'invited', initial: true },
		{ id: 'active', label: 'active' },
		{ id: 'disabled', label: 'disabled' },
	];
	const TRANSITIONS = [
		{ from: 'invited', to: 'active', label: 'accepts' },
		{ from: 'active', to: 'disabled', label: 'Disable' },
		{ from: 'disabled', to: 'active', label: 'Enable' },
	];
</script>

<PageHead
	id="T3"
	sub={`Who may use Agent in ${tenantName}, and what they may change. Everyone signs in with their one portal account; this page decides whether that login gets into Agent here, and as which role.`}
/>

{#if !canWrite}
	<Callout kind="info" title="Read only"
		><span
			>You can see every member, but inviting, changing roles and disabling need the Tenant admin role{session.viewAs
				? ' (you are viewing as fewer roles)'
				: ''}. The controls stay visible, disabled.</span
		></Callout
	>
{/if}

<div class="t3-top">
	<div class="t3-left">
		<section class="t3-roles" aria-label="What each role can do">
			{#each catalog as r (r.key)}
				{@const names = holders(r.key)}
				<article class="t3-role" class:admin={r.key === 'tenant_admin'} data-role={r.key}>
					<header>
						<span class="t3-role-name">{r.label}</span>
						<span class="t3-count" aria-label={`${names.length} active`}>{names.length}</span>
					</header>
					<p class="t3-role-title">{r.description}</p>
					<p class="t3-holders">
						{#if names.length}{names.join(', ')}{:else}<span class="muted">Nobody yet</span>{/if}
					</p>
				</article>
			{/each}
			{#if catalog.length}
				{@const names = holders(null)}
				<article class="t3-role" data-role="observer">
					<header>
						<span class="t3-role-name">Observer</span>
						<span class="t3-count" aria-label={`${names.length} active`}>{names.length}</span>
					</header>
					<p class="t3-role-title">
						No role: reads every page, runs test chats and dry runs, gives answer feedback. Changes nothing.
					</p>
					<p class="t3-holders">
						{#if names.length}{names.join(', ')}{:else}<span class="muted">Nobody</span>{/if}
					</p>
				</article>
			{/if}
		</section>
		<ol class="t3-flow" aria-label="From portal login to Agent">
			<li>
				<b>Portal account</b>
				<span class="cell-sub"
					>One login for every nonagentic app (Keycloak portal realm). Not managed here.</span
				>
			</li>
			<li class="here">
				<b>Membership in {tenantName}</b>
				<span class="cell-sub">This page: tenant + role, stored in Agent's own users table.</span>
			</li>
			<li>
				<b>Agent</b>
				<span class="cell-sub"
					><a href="/console">Console</a>, <a href="/chat">Test chat</a> and the admin pages.</span
				>
			</li>
		</ol>
	</div>

	<Card title="Invite a member" sub="By e-mail. They get a portal sign-up link that lands them in Agent.">
		{#snippet actions()}<MilestoneChip milestone="M2" />{/snippet}
		<form class="t3-add" onsubmit={invite}>
			<fieldset class="t3-fields" disabled={!canWrite || busy}>
				<label class="field"
					><span class="label">E-mail</span><input
						class="input"
						type="email"
						autocomplete="off"
						bind:value={email}
						placeholder="name@lumenloom.example"
					/></label
				>
				<label class="field"
					><span class="label">First name</span><input
						class="input"
						autocomplete="off"
						maxlength="80"
						required
						bind:value={firstName}
					/></label
				>
				<label class="field"
					><span class="label">Last name</span><input
						class="input"
						autocomplete="off"
						maxlength="80"
						required
						bind:value={lastName}
					/></label
				>
				<RoleSetEditor {catalog} bind:value={inviteRoles} name="invite" compact />
			</fieldset>
			<div class="t3-actions">
				<button
					class="btn primary"
					type="submit"
					{...writeAttrs(session, 'tenant_admin')}
					disabled={!canWrite || busy || !email.trim() || !firstName.trim() || !lastName.trim()}
					>Send invite</button
				>
				{#if lastInvite}<span class="muted" role="status">202 · invite sent to {lastInvite}</span>{/if}
			</div>
			<p class="cell-sub">
				The link is valid for 7 days. Until it is used the member shows as <b>invited</b> and cannot sign in.
			</p>
		</form>
		<div class="t3-secondary">
			<button class="t3-link" type="button" aria-expanded={bySub} onclick={() => (bySub = !bySub)}
				>{bySub ? '▾' : '▸'} Already has a portal account? Add by ID</button
			>
			<MilestoneChip milestone="M1" />
		</div>
		{#if bySub}
			<form class="t3-add" onsubmit={addBySub}>
				<p class="cell-sub">
					Enter their Keycloak subject (sub). They become active at once; no e-mail is sent.
				</p>
				<fieldset class="t3-fields two" disabled={!canWrite || busy}>
					<label class="field"
						><span class="label">Keycloak subject (sub)</span><input
							class="input mono"
							autocomplete="off"
							spellcheck="false"
							bind:value={sub}
							placeholder="noor-el-amrani"
						/></label
					>
				</fieldset>
				<RoleSetEditor
					{catalog}
					bind:value={subRoles}
					name="add-by-sub"
					disabled={!canWrite || busy}
					compact
				/>
				<div class="t3-actions">
					<button
						class="btn"
						type="submit"
						{...writeAttrs(session, 'tenant_admin')}
						disabled={!canWrite || busy || !sub.trim()}>Add user</button
					>
				</div>
			</form>
		{/if}
	</Card>
</div>

<div class="t3-tabs">
	<Seg
		label="Members or invites"
		bind:active={tab}
		items={[
			['members', 'Members'],
			['invites', 'Invites'],
		]}
	/>
</div>

{#if tab === 'invites'}
	<Card
		title="Invites"
		sub="Every invite sent from this workspace. An unused link expires after 7 days; Resend re-arms it, Revoke withdraws it and the seat with it."
	>
		<InvitesPanel
			load={(state, before) => api().tenantInvites({ state, before })}
			resend={(id) => api().resendInvite(id)}
			revoke={(id) => api().revokeInvite(id)}
			canAct={canWrite}
			onChange={refreshAll}
		/>
	</Card>
{:else}
	<LoadState resource={users} empty="This tenant has no members yet. Invite the first one above.">
		{#snippet children(page)}
			{@const rows = shown(page.items)}
			<Card cls="flush">
				<header class="t3-head">
					<div>
						<h2>Members</h2>
						<p class="t3-sum">
							<span><b>{sum.admins + sum.users}</b> can sign in</span>
							{#if sum.invited}<span><b>{sum.invited}</b> invite pending</span>{/if}
							{#if sum.disabled}<span><b>{sum.disabled}</b> disabled</span>{/if}
							{#if sum.quiet}<span class="warn"
									><b>{sum.quiet}</b> quiet ≥ {QUIET_DAYS} d — consider disabling</span
								>{/if}
						</p>
					</div>
					<Seg
						label="Filter members by state"
						bind:active={filter}
						items={[
							['all', `All ${page.items.length}`],
							['active', 'Active'],
							['invited', 'Invited'],
							['disabled', 'Disabled'],
						]}
					/>
				</header>
				<div class="tbl-wrap">
					<table class="tbl t3-tbl" data-testid="tenant-users">
						<thead>
							<tr
								><th>Member</th><th>Roles</th><th>State</th><th>Last activity</th><th>Added</th><th
									aria-label="Member actions"
								></th></tr
							>
						</thead>
						<tbody>
							{#each rows as u (u.sub)}
								{@const st = memberState(u)}
								{@const a = acts.get(u.sub)}
								<tr data-sub={u.sub} data-state={st} class:disabled={st === 'disabled'}>
									<td class="t3-member">
										<span class="t3-avatar" class:admin={u.roles.includes('tenant_admin')} aria-hidden="true"
											>{initials(u.display_name)}</span
										>
										<span
											><b>{u.display_name}</b>{#if u.sub === me}<span class="chip accent t3-you">you</span
												>{/if}<span class="cell-sub">{u.email}{u.title ? ` · ${u.title}` : ''}</span><span
												class="cell-sub mono">{u.sub}</span
											></span
										>
									</td>
									<td data-label="Roles" class="t3-roles-cell">
										{#if editing === u.sub}
											<RoleSetEditor
												{catalog}
												bind:value={draftRoles}
												name="edit-{u.sub}"
												legend={`Roles of ${u.display_name}`}
												disabled={busy}
												locked={lastAdmin(u) ? ['tenant_admin'] : []}
												lockedHint={LAST}
												compact
											/>
											<div class="t3-edit-actions">
												<button
													class="btn sm primary"
													type="button"
													disabled={busy || sameSet(draftRoles, u.roles)}
													onclick={() => saveRoles(u)}>Save roles</button
												>
												<button class="btn sm" type="button" onclick={() => (editing = null)}>Cancel</button>
											</div>
										{:else}
											<span class="t3-role-chips" data-testid="roles-of-{u.sub}">
												{#each u.roles as r (r)}<Chip
														text={catalog.find((c) => c.key === r)?.label ?? r}
														variant={r === 'tenant_admin' ? 'accent' : 'muted'}
													/>{:else}<Chip text="Observer" variant="muted" />{/each}
											</span>
											<button
												class="btn sm t3-change"
												type="button"
												aria-label={`Change roles of ${u.display_name}`}
												disabled={!canWrite || busy || st === 'disabled'}
												title={!canWrite
													? WRITE_HINT
													: st === 'disabled'
														? 'Enable the user first'
														: undefined}
												onclick={() => startEdit(u)}>Change roles</button
											>
										{/if}
										{#if lastAdmin(u)}<span class="cell-sub">Last admin — protected</span>{/if}
									</td>
									<td data-label="State">
										{#if st === 'disabled'}
											<Chip text="disabled" variant="muted" />
											<span class="cell-sub">since {shortDate(u.disabled_at)} · history kept</span>
										{:else if st === 'invited'}
											<Chip text="invited" variant="cust" title="Invite sent; not accepted yet" />
											<span class="cell-sub"
												>sent {shortDate(u.invited_at)}{a?.invite_expires_at
													? ` · expires ${shortDate(a.invite_expires_at)}`
													: ''}</span
											>
										{:else}
											<Chip text="active" variant="ok" />
										{/if}
									</td>
									<td data-label="Last activity">
										{#if st === 'invited'}
											<span class="muted">not signed in yet</span>
										{:else if a}
											<span class:t3-quiet={isQuiet(st, a, now)}>{ago(a.last_seen_at, now)}</span>
											{#if a.last_surface}<span class="cell-sub"
													>{SURFACE_LABEL[a.last_surface]} · {a.sign_ins_30d} sign-ins / 30 d</span
												>{/if}
											{#if isQuiet(st, a, now)}<Chip text="quiet" variant="warn" />{/if}
										{:else}
											<span class="muted">—</span>
										{/if}
									</td>
									<td data-label="Added">
										{#if a?.added_via}
											<span>{ORIGIN_LABEL[a.added_via]}{a.added_by_name ? ` by ${a.added_by_name}` : ''}</span
											>
											<span class="cell-sub">{shortDate(a.added_at)}</span>
										{:else}
											<span>{shortDate(u.created_at)}</span>
										{/if}
									</td>
									<td class="r t3-act">
										{#if st === 'disabled'}
											<button
												class="btn sm"
												type="button"
												{...writeAttrs(session, 'tenant_admin')}
												disabled={!canWrite || busy}
												onclick={() => enable(u)}>Enable</button
											>
										{:else}
											<button
												class="btn sm danger"
												type="button"
												disabled={!canWrite || busy || lastAdmin(u) || u.sub === me}
												title={why(u)}
												onclick={() => disable(u)}>Disable</button
											>
										{/if}
									</td>
								</tr>
							{:else}
								<tr><td colspan="6" class="muted">No {filter} members.</td></tr>
							{/each}
						</tbody>
					</table>
				</div>
			</Card>
		{/snippet}
	</LoadState>
{/if}

<Card title="Member lifecycle" cls="t3-how">
	<div class="t3-how-grid">
		<StateDiagram title="Member lifecycle" states={STATES} transitions={TRANSITIONS} />
		<ul class="t3-rules">
			<li>
				<b>invited</b> — the 7-day link has not been used. The member cannot sign in yet; Disable withdraws the
				seat.
			</li>
			<li>
				<b>disabled</b> — soft: no sign-in to Agent, but their portal account and other apps are untouched, and
				cases and audit keep their name. Enable brings them back with the same role.
			</li>
			<li>
				<b>last Tenant admin</b> — can neither lose that role nor be disabled, by anyone, a super_admin included.
			</li>
			<li>Every change lands in the <a href="/admin/audit?action=user">audit log</a>.</li>
		</ul>
	</div>
</Card>

<style>
	.t3-tabs {
		margin: 16px 0 8px;
	}
	.t3-top {
		display: grid;
		gap: 16px;
		grid-template-columns: minmax(0, 3fr) minmax(0, 2fr);
		align-items: start;
	}
	.t3-roles {
		display: grid;
		gap: 12px;
		grid-template-columns: repeat(auto-fit, minmax(220px, 1fr));
	}
	.t3-role {
		background: var(--panel);
		border: 1px solid var(--line);
		border-top: 3px solid var(--muted);
		border-radius: 8px;
		padding: 12px 14px;
		display: grid;
		gap: 8px;
		align-content: start;
	}
	.t3-role.admin {
		border-top-color: var(--accent);
	}
	.t3-role header {
		display: grid;
		grid-template-columns: 1fr auto;
		row-gap: 2px;
	}
	.t3-role-name {
		font-weight: 600;
	}
	.t3-role-title {
		margin: 0;
		color: var(--muted);
		font-size: 13px;
	}
	.t3-count {
		grid-column: 2;
		font-size: 26px;
		font-weight: 600;
		line-height: 1;
		align-self: center;
	}
	.t3-holders {
		margin: 0;
		padding-top: 8px;
		border-top: 1px solid var(--line-2);
		font-size: 12.5px;
	}
	.t3-add {
		display: grid;
		gap: 12px;
	}
	.t3-fields {
		border: 0;
		padding: 0;
		margin: 0;
		min-width: 0;
		display: grid;
		gap: 12px;
	}
	.t3-fields.two {
		grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));
	}
	.t3-actions {
		display: flex;
		gap: 12px;
		align-items: center;
		flex-wrap: wrap;
	}
	.t3-secondary {
		margin-top: 14px;
		padding-top: 12px;
		border-top: 1px solid var(--line);
		display: flex;
		gap: 8px;
		align-items: center;
		flex-wrap: wrap;
	}
	.t3-link {
		border: 0;
		background: none;
		color: var(--accent);
		cursor: pointer;
		padding: 0;
		font-size: 13px;
	}
	.t3-head {
		display: flex;
		justify-content: space-between;
		align-items: center;
		gap: 12px;
		flex-wrap: wrap;
		padding: 12px 14px;
		border-bottom: 1px solid var(--line);
	}
	.t3-head h2 {
		margin: 0;
		font-size: 15px;
	}
	.t3-sum {
		margin: 2px 0 0;
		display: flex;
		flex-wrap: wrap;
		gap: 4px 14px;
		font-size: 13px;
		color: var(--muted);
	}
	.t3-sum b {
		color: var(--fg);
	}
	.t3-sum .warn,
	.t3-sum .warn b {
		color: var(--warn);
	}
	.t3-member {
		display: flex;
		gap: 10px;
		align-items: flex-start;
	}
	.t3-avatar {
		flex: none;
		width: 32px;
		height: 32px;
		border-radius: 50%;
		display: grid;
		place-items: center;
		font-size: 12px;
		font-weight: 600;
		background: var(--sunk);
		color: var(--muted);
	}
	.t3-avatar.admin {
		background: color-mix(in srgb, var(--accent) 15%, var(--panel));
		color: var(--accent);
	}
	.t3-you {
		margin-left: 6px;
		font-size: 10.5px;
	}
	.t3-roles-cell {
		min-width: 220px;
	}
	.t3-role-chips {
		display: flex;
		flex-wrap: wrap;
		gap: 4px;
		margin-bottom: 6px;
	}
	.t3-edit-actions {
		display: flex;
		gap: 8px;
		margin-top: 8px;
	}
	.t3-quiet {
		color: var(--warn);
	}
	tr.disabled td {
		opacity: 0.55;
	}
	.t3-how-grid {
		display: grid;
		gap: 20px;
		grid-template-columns: minmax(0, 3fr) minmax(0, 2fr);
		align-items: start;
	}
	.t3-left {
		display: grid;
		gap: 12px;
		min-width: 0;
	}
	.t3-flow {
		list-style: none;
		margin: 0;
		padding: 12px 14px;
		display: grid;
		grid-template-columns: repeat(3, minmax(0, 1fr));
		gap: 10px;
		background: var(--panel);
		border: 1px solid var(--line);
		border-radius: 8px;
	}
	.t3-flow li {
		position: relative;
		padding-right: 18px;
	}
	.t3-flow li:not(:last-child)::after {
		content: '→';
		position: absolute;
		right: 0;
		top: 0;
		color: var(--muted);
	}
	.t3-flow li.here b {
		color: var(--accent);
	}
	.t3-rules {
		margin: 0;
		padding-left: 18px;
		display: grid;
		gap: 6px;
		font-size: 13px;
	}
	@media (max-width: 900px) {
		.t3-top,
		.t3-how-grid {
			grid-template-columns: minmax(0, 1fr);
		}
	}
	@media (max-width: 640px) {
		.t3-flow {
			grid-template-columns: 1fr;
		}
		.t3-flow li::after {
			display: none;
		}
		.t3-tbl thead {
			display: none;
		}
		.t3-tbl,
		.t3-tbl tbody,
		.t3-tbl tr,
		.t3-tbl td {
			display: block;
			width: 100%;
		}
		.t3-tbl tr {
			border-bottom: 1px solid var(--line);
			padding: 6px 0;
		}
		.t3-tbl td {
			border: 0;
			padding: 4px 14px;
		}
		.t3-tbl td[data-label] {
			display: grid;
			grid-template-columns: 96px 1fr;
			column-gap: 8px;
		}
		.t3-tbl td[data-label]::before {
			content: attr(data-label);
			grid-row: span 3;
			font-size: 11px;
			text-transform: uppercase;
			letter-spacing: 0.06em;
			color: var(--muted);
			padding-top: 3px;
		}
		.t3-tbl td.t3-act {
			text-align: left;
		}
		.t3-tbl td.t3-member {
			display: flex;
		}
		.t3-tbl td[data-label] > :global(*) {
			justify-self: start;
		}
	}
</style>
