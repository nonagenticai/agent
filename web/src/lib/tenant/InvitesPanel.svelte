<script lang="ts">
	// B1b F-01 member invites: one state at a time (Pending / Expired / Accepted / Revoked; `expired` is computed on
	// read by the API), with Resend and Revoke. Used by /admin/users (the active tenant, tenant_admin) and by
	// /platform/tenants/[id] (any tenant, super_admin). Resend after a 429 `invite_resend_cooldown` stays disabled
	// with a countdown from `retry_after_s`; Revoke asks first. The actions are HIDDEN without the right role (U-03).
	import { onDestroy, untrack } from 'svelte';
	import { ApiError } from '$lib/api/errors';
	import type { Paged } from '$lib/api/types/core';
	import { rolesText } from '$lib/api/types/core';
	import type { InviteState, MemberInvite } from '$lib/api/types/tenant';
	import { INVITE_STATES } from '$lib/api/types/tenant';
	import { toast } from '$lib/ui/toaster.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import { shortDate } from './T3-members';

	let {
		load,
		resend,
		revoke,
		canAct,
		resendBlocked,
		onChange,
	}: {
		load: (state: InviteState, before: string | null) => Promise<Paged<MemberInvite>>;
		resend: (id: string) => Promise<MemberInvite>;
		revoke: (id: string) => Promise<MemberInvite>;
		/** The caller may resend / revoke (effective tenant_admin, or super_admin on the platform page). */
		canAct: boolean;
		/** Why Resend is refused for every row (e.g. the tenant is archived); undefined = allowed. */
		resendBlocked?: string;
		/** After a write (a revoke disables the never-signed-in member). */
		onChange?: () => unknown;
	} = $props();

	const LABEL: Record<InviteState, string> = {
		pending: 'Pending',
		expired: 'Expired',
		accepted: 'Accepted',
		revoked: 'Revoked',
	};
	const VARIANT = { pending: 'cust', expired: 'warn', accepted: 'ok', revoked: 'muted' } as const;

	let shown = $state<InviteState>('pending');
	let rows = $state<MemberInvite[]>([]);
	let next = $state<string | null>(null);
	let loading = $state(true);
	let error = $state('');
	let busy = $state<string | null>(null);
	let confirming = $state<string | null>(null);
	/** Invite id → when Resend opens again (from a 429's `retry_after_s`). */
	let cooldown = $state<Record<string, number>>({});
	let now = $state(Date.now());
	const tick = setInterval(() => (now = Date.now()), 1000);
	onDestroy(() => clearInterval(tick));

	async function refresh(more = false) {
		loading = !more;
		error = '';
		try {
			const p = await load(shown, more ? next : null);
			rows = more ? [...rows, ...p.items] : p.items;
			next = p.next;
		} catch (e) {
			rows = more ? rows : [];
			error = e instanceof Error ? e.message : String(e);
		} finally {
			loading = false;
		}
	}
	$effect(() => {
		void shown;
		untrack(() => void refresh());
	});

	const left = (id: string) => Math.max(0, Math.ceil(((cooldown[id] ?? 0) - now) / 1000));
	const clock = (s: number) => `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;

	async function doResend(i: MemberInvite) {
		busy = i.id;
		try {
			const r = await resend(i.id);
			toast(`Invite re-sent to ${i.email}; the link is valid until ${shortDate(r.expires_at)}.`);
			await refresh();
			await onChange?.();
		} catch (e) {
			if (e instanceof ApiError && e.code === 'invite_resend_cooldown') {
				const s = Number(e.details.retry_after_s) || 60;
				cooldown = { ...cooldown, [i.id]: Date.now() + s * 1000 };
				toast(`Sent recently. You can resend to ${i.email} in ${clock(s)} (${s} s).`, 'err');
			} else toast(`Not re-sent: ${e instanceof Error ? e.message : String(e)}`, 'err');
		} finally {
			busy = null;
		}
	}

	async function doRevoke(i: MemberInvite) {
		busy = i.id;
		try {
			await revoke(i.id);
			confirming = null;
			toast(`Invite for ${i.email} revoked; the link no longer works.`);
			await refresh();
			await onChange?.();
		} catch (e) {
			toast(`Not revoked: ${e instanceof Error ? e.message : String(e)}`, 'err');
		} finally {
			busy = null;
		}
	}
</script>

<section class="inv" aria-label="Member invites" data-testid="invites">
	<div class="inv-states" role="group" aria-label="Invite state">
		{#each INVITE_STATES as s (s)}
			<button
				type="button"
				class="tab"
				class:on={shown === s}
				aria-pressed={shown === s}
				data-testid={`invite-state-${s}`}
				onclick={() => (shown = s)}>{LABEL[s]}</button
			>
		{/each}
	</div>

	{#if error}
		<p class="err" role="alert">Invites could not be loaded: {error}</p>
	{:else if loading}
		<p class="muted" role="status">Loading invites…</p>
	{:else if !rows.length}
		<p class="muted" data-testid="invites-empty">No {LABEL[shown].toLowerCase()} invites.</p>
	{:else}
		<div class="tbl-wrap">
			<table class="tbl inv-tbl">
				<thead>
					<tr
						><th>Invitee</th><th>Roles</th><th>State</th><th>Invited by</th><th aria-label="Invite actions"
						></th></tr
					>
				</thead>
				<tbody>
					{#each rows as i (i.id)}
						{@const wait = left(i.id)}
						<tr data-testid="invite-row" data-state={i.state} data-email={i.email}>
							<td><b class="mono">{i.email}</b></td>
							<td data-label="Roles">{rolesText(i.roles)}</td>
							<td data-label="State">
								<Chip text={LABEL[i.state].toLowerCase()} variant={VARIANT[i.state]} />
								<span class="cell-sub">
									{#if i.state === 'accepted'}accepted {shortDate(i.accepted_at)}
									{:else if i.state === 'revoked'}revoked {shortDate(i.revoked_at)}
									{:else if i.state === 'expired'}expired {shortDate(i.expires_at)}
									{:else}expires {shortDate(i.expires_at)}{/if}
									· sent {i.send_count}×{i.last_sent_at ? `, last ${shortDate(i.last_sent_at)}` : ''}
								</span>
							</td>
							<td data-label="Invited by">
								{i.invited_by_name ?? i.invited_by}
								{#if i.via === 'signup'}<span data-testid="invite-via-signup"
										><Chip text="via sign-up" variant="accent" title="Recorded by a signup approval" /></span
									>{/if}
								<span class="cell-sub">{shortDate(i.created_at)}</span>
							</td>
							<td class="r inv-act">
								{#if canAct && (i.state === 'pending' || i.state === 'expired')}
									{#if confirming === i.id}
										<span class="inv-confirm" role="group" aria-label={`Revoke the invite for ${i.email}?`}>
											<span class="cell-sub">Revoke? The link stops working.</span>
											<button
												class="btn sm danger"
												type="button"
												data-testid="invite-revoke-confirm"
												disabled={busy === i.id}
												onclick={() => doRevoke(i)}>Revoke invite</button
											>
											<button class="btn sm" type="button" onclick={() => (confirming = null)}>Cancel</button>
										</span>
									{:else}
										<button
											class="btn sm"
											type="button"
											data-testid="invite-resend"
											disabled={busy === i.id || wait > 0 || !!resendBlocked}
											title={resendBlocked ??
												(wait > 0 ? `Sent recently; resend in ${clock(wait)}` : undefined)}
											onclick={() => doResend(i)}>{wait > 0 ? `Resend in ${clock(wait)}` : 'Resend'}</button
										>
										<button
											class="btn sm danger"
											type="button"
											data-testid="invite-revoke"
											disabled={busy === i.id}
											onclick={() => (confirming = i.id)}>Revoke…</button
										>
									{/if}
								{/if}
							</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
		{#if next}<button class="btn sm" type="button" onclick={() => refresh(true)}>Load more</button>{/if}
	{/if}
</section>

<style>
	.inv {
		display: grid;
		gap: 12px;
	}
	.inv-states {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
	}
	.inv-act {
		white-space: nowrap;
	}
	.inv-confirm {
		display: inline-flex;
		flex-wrap: wrap;
		gap: 6px;
		align-items: center;
		justify-content: flex-end;
	}
	.err {
		color: var(--err);
	}
	@media (max-width: 640px) {
		.inv-act {
			white-space: normal;
		}
	}
</style>
