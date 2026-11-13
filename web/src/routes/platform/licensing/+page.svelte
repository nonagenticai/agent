<script lang="ts">
	// P20 Licensing & keys. Two questions, two chains (04 §4b):
	//  · Gate 2 (C1, K35) — can each tenant's customers be served right now? The edge reads only the verdict row;
	//    missing, not active, or older than 5 min → it refuses turns (fails closed).
	//  · Keyring (K43) — is every stored token sealed under the current kid? Stage → current → reseal → drop.
	import { superAdminAttrs } from '$lib/auth/roles';
	import { onMount } from 'svelte';
	import { api, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { Resource } from '$lib/load.svelte';
	import { ApiError } from '$lib/api/errors';
	import type { LicensingOverview, ReissueResult, ResealResult } from '$lib/api/types/platform';
	import Drawer from '$lib/ui/Drawer.svelte';
	import { toast } from '$lib/ui/toaster.svelte';
	import PageHead from '$lib/ui/PageHead.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import { VERDICT_MAX_AGE_S } from '$lib/platform/model';
	import { STATUS_CHIP, age, hhmm, utc } from '$lib/platform/ui';
	import {
		AGE_SCALE_S,
		INTROSPECT_CACHE_S,
		VERDICT_REFRESH_S,
		agePos,
		edgeVerdict,
		summarise,
	} from '$lib/platform/licensing-P20';
	import GateFlow from './GateFlow.svelte';
	import KeyLifecycle from './KeyLifecycle.svelte';
	import LicenceDrawer from './LicenceDrawer.svelte';

	type Row = LicensingOverview['tenants'][number];

	const res = new Resource<LicensingOverview>(
		() => api().licensingOverview(),
		() => false,
	);
	onMount(() => void res.refresh());

	let resealed = $state<ResealResult | null>(null);
	let busy = $state(false);
	let pickedId = $state<string | null>(null);
	let drawerOpen = $state(false);

	async function reseal() {
		busy = true;
		let out: ResealResult | null = null;
		const ok = await guard(
			async () => (out = await api().resealLicences()),
			() =>
				`Resealed ${out?.resealed ?? 0} tenant token(s) under ${out?.kid}; ${out?.remaining ?? 0} remaining.`,
		);
		busy = false;
		if (ok) {
			resealed = out;
			await res.refresh();
		}
	}
	async function resealOne(id: string) {
		busy = true;
		const ok = await guard(() => api().resealTenantLicence(id), 'Token resealed under the current key.');
		busy = false;
		if (ok) await res.refresh();
	}
	async function nudge() {
		busy = true;
		const ok = await guard(() => api().nudgeLicenceVerdicts(), 'Verdict rows refreshed (licence:changed).');
		busy = false;
		if (ok) await res.refresh();
	}
	function open(t: Row) {
		pickedId = t.tenant_id;
		drawerOpen = true;
	}

	// ---- per-tenant licence replacement: re-issue (F-31) or the leaked-token re-mint (F-32, typed tenant name) ----
	let replaceOpen = $state(false);
	let replaceMode = $state<'reissue' | 'remint'>('reissue');
	let replaceRow = $state<Row | null>(null);
	let confirmName = $state('');
	let remintReason = $state('');
	let replacing = $state(false);
	let replaceError = $state<{ code: string; message: string } | null>(null);
	let replaced = $state<ReissueResult | null>(null);
	function openReplace(t: Row, mode: 'reissue' | 'remint') {
		replaceRow = t;
		replaceMode = mode;
		confirmName = '';
		remintReason = '';
		replaceError = null;
		replaced = null;
		replaceOpen = true;
	}
	const nameMatches = $derived(!!replaceRow && confirmName.trim() === replaceRow.name.trim());
	async function replace(e: SubmitEvent) {
		e.preventDefault();
		const t = replaceRow;
		if (!t) return;
		replacing = true;
		replaceError = null;
		try {
			const a =
				replaceMode === 'remint'
					? await api().remintTenantLicence(t.tenant_id, { confirmName, reason: remintReason })
					: await api().reissueTenantLicence(t.tenant_id, { tenantLicence: true });
			replaced = a.results.find((r) => r.holder === 'tenant') ?? a.results[0] ?? null;
			toast(
				replaced?.status === 'reissued'
					? `New licence ${replaced.new_licence_id} issued for ${t.name}.`
					: `${t.name}: the old licence is not revoked yet; the provisioning tick retries it.`,
			);
			await res.refresh();
		} catch (err) {
			replaceError =
				err instanceof ApiError
					? { code: err.code, message: err.message }
					: { code: 'error', message: (err as Error).message };
		} finally {
			replacing = false;
		}
	}
	const canReplace = (t: Row) => !!t.licence && t.status === 'active';
	const plural = (n: number, w: string) => `${n} ${w}${n === 1 ? '' : 's'}`;
</script>

<PageHead
	id="P20"
	sub="Can every tenant's customers be served right now, and is every stored licence token sealed under the current key? Tokens are never shown — only key ids."
>
	{#snippet actions()}
		<a class="btn" href="/platform/audit?action=license.reseal">Licence audit</a>
		<button class="btn" {...superAdminAttrs(session)} disabled={busy || !session.superAdmin} onclick={nudge}
			>Nudge verdict refresh</button
		>
	{/snippet}
</PageHead>

<LoadState resource={res} loading="Loading licensing…">
	{#snippet children(o)}
		{@const s = summarise(o)}
		{@const sealedLive = s.live - s.unsealed.length}
		<div class="p20-answers">
			<section class="p20-answer" class:bad={s.failingClosed.length > 0}>
				<span class="label">Gate 2 · can customers be served?</span>
				<p class="p20-big">
					{s.serving}<small>/ {s.live} live tenants serving</small>
				</p>
				<p class="p20-line">
					<span class="p20-n" class:err={s.failingClosed.length > 0} data-testid="fails-closed"
						>{s.failingClosed.length}</span
					>
					fail closed on a stale verdict{#if s.failingClosed.length}: <strong
							>{s.failingClosed.join(', ')}</strong
						>{/if}
				</p>
				<p class="p20-line muted">
					{s.refusedByDesign.length} refused by design{s.refusedByDesign.length
						? ` (${s.refusedByDesign.join(', ')})`
						: ''} · verdicts read at {hhmm(o.now)} UTC
				</p>
			</section>
			<section class="p20-answer" class:warn={s.behind > 0}>
				<span class="label">Keyring · are tokens on the current key?</span>
				<p class="p20-big">
					{sealedLive - s.behind}<small
						>/ {sealedLive} tokens on <span class="mono">{o.current_kid}</span></small
					>
				</p>
				<p class="p20-line">
					{#if s.behind}<strong>{plural(s.behind, 'token')}</strong> still on an older kid — reseal before dropping
						it.
					{:else}All live tokens sealed under the current kid.{/if}
				</p>
				<p class="p20-line muted">
					{#if s.droppable.length}Safe to remove from SOPS: <span class="mono">{s.droppable.join(', ')}</span>
					{:else}No kid ready to drop.{/if} · last reseal {o.last_reseal
						? `${utc(o.last_reseal.at)} → ${o.last_reseal.kid}`
						: 'never'}
				</p>
			</section>
		</div>

		{#if s.failingClosed.length}
			<Callout
				kind="err"
				title="{plural(s.failingClosed.length, 'tenant')} refused at the edge with an active licence"
			>
				{s.failingClosed.join(', ')}: the verdict row was not refreshed in the last {VERDICT_MAX_AGE_S / 60} minutes,
				so customers get the neutral "unavailable" text. agent-api refreshes every ≤{VERDICT_REFRESH_S} s; nudge
				to force it, then check why the refresh is failing.
			</Callout>
		{/if}

		<Card
			cls="flush"
			title="Gate 2 — from licence to the edge"
			sub="The edge holds no key and no token: it trusts only a fresh verdict row."
		>
			<div class="p20-pad">
				<GateFlow cacheS={INTROSPECT_CACHE_S} refreshS={VERDICT_REFRESH_S} maxAgeS={VERDICT_MAX_AGE_S} />
			</div>
			<div class="tbl-wrap">
				<table class="tbl">
					<thead>
						<tr>
							<th>Tenant</th><th>Edge</th><th>Licence · sealed under</th><th>Verdict</th><th
								class="p20-agecol"
								>Verdict age <span class="p20-legend"
									>refresh {VERDICT_REFRESH_S} s · fail {VERDICT_MAX_AGE_S / 60} min</span
								></th
							><th>Last re-issue</th><th></th>
						</tr>
					</thead>
					<tbody>
						{#each o.tenants as t (t.tenant_id)}
							{@const e = edgeVerdict(t.verdict)}
							<tr data-testid="verdict-row" class:archived={t.status === 'archived'}>
								<td>
									<a class="cell-title" href={`/platform/tenants/${t.tenant_id}`}>{t.name}</a>
									<span class="cell-sub"><Chip text={t.status} variant={STATUS_CHIP[t.status]} /></span>
								</td>
								<td><Chip text={e.label} variant={e.variant} title={e.why} /></td>
								<td>
									{#if t.licence}
										<span class="mono">{t.licence.licence_id}</span>
										<span class="cell-sub mono"
											>{#if t.licence.kid}v2:<span class:p20-behind={t.licence.kid !== o.current_kid}
													>{t.licence.kid}</span
												>:••••{:else}no token (revoked){/if}</span
										>
									{:else}<span class="muted">—</span>{/if}
								</td>
								<td>
									{#if t.verdict}
										<Chip text={t.verdict.status} variant={t.verdict.status === 'active' ? 'ok' : 'err'} />
										{#if t.verdict.reason}<span class="cell-sub">{t.verdict.reason}</span>{/if}
									{:else}<span class="muted">—</span>{/if}
								</td>
								<td class="p20-agecol">
									{#if t.verdict}
										<div
											class="p20-age"
											title={`checked ${utc(t.verdict.checked_at)} UTC`}
											aria-label={`Verdict age ${age(t.verdict.age_seconds)}`}
										>
											<span class="p20-track" style="--lim:{(VERDICT_MAX_AGE_S / AGE_SCALE_S) * 100}%">
												<i class="p20-tick" style="left:{(VERDICT_REFRESH_S / AGE_SCALE_S) * 100}%"></i>
												<i class="p20-limit" style="left:{(VERDICT_MAX_AGE_S / AGE_SCALE_S) * 100}%"></i>
												<b class="p20-dot {e.variant}" style="left:{agePos(t.verdict.age_seconds) * 100}%"
												></b>
											</span>
											<span class="mono p20-agev" class:stale={e.state === 'stale'}
												>{age(t.verdict.age_seconds)}</span
											>
										</div>
									{:else}<span class="muted">no row</span>{/if}
								</td>
								<td data-testid="last-reissue">
									{#if t.last_reissue}
										<Chip
											text={t.last_reissue.kind === 'remint' ? 're-mint' : 're-issue'}
											variant={t.last_reissue.kind === 'remint' ? 'warn' : 'muted'}
										/>
										<span class="cell-sub">{utc(t.last_reissue.at)} · {t.last_reissue.reason}</span>
										{#if t.last_reissue.old_still_valid}
											<span class="cell-sub p20-stillvalid" data-testid="old-still-valid"
												>old token still valid — retrying revoke</span
											>
										{/if}
									{:else}<span class="muted">never</span>{/if}
								</td>
								<td class="r p20-rowacts">
									{#if canReplace(t)}
										<button
											class="btn sm"
											{...superAdminAttrs(session)}
											disabled={busy || replacing || !session.superAdmin}
											onclick={() => openReplace(t, 'reissue')}
											aria-label={`Re-issue the licence of ${t.name}`}>Re-issue…</button
										>
										<button
											class="btn sm danger"
											{...superAdminAttrs(session)}
											disabled={busy || replacing || !session.superAdmin}
											onclick={() => openReplace(t, 'remint')}
											aria-label={`Re-mint the leaked licence of ${t.name}`}>Re-mint…</button
										>
									{/if}
									<button class="btn sm" onclick={() => open(t)} aria-label={`Licence details for ${t.name}`}
										>Details</button
									>
								</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		</Card>

		<Card
			title="Sealing keys — rotation lifecycle"
			sub="Stored tokens are opened by their kid and sealed with the current one (v2:<kid>:…). A key can leave the ring only when nothing is sealed under it."
		>
			{#snippet actions()}
				<button
					class="btn sm"
					class:primary={s.behind > 0}
					{...superAdminAttrs(session)}
					disabled={busy || !session.superAdmin}
					onclick={reseal}>Reseal to {o.current_kid}</button
				>
			{/snippet}
			{#if resealed}
				<Callout kind="info" title="Resealed {resealed.resealed} · {resealed.remaining} remaining">
					Every live tenant token is sealed with {resealed.kid}. Keys with 0 sealed tokens can be dropped from
					LICENSE_TOKEN_ENC_KEYS. Audited as <code>license.reseal</code> (counts only).
				</Callout>
			{/if}
			<KeyLifecycle overview={o} {busy} onreseal={reseal} />
		</Card>

		<div class="grid g-main">
			<Card title="What each action changes" sub="Read this before you rotate, drop or revoke.">
				<dl class="p20-conseq">
					<dt>Stage + make a kid current</dt>
					<dd>New seals use it immediately. Old tokens still open under their kid. No customer impact.</dd>
					<dt>Reseal</dt>
					<dd>
						One transaction per tenant moves its token to the current kid. Licence, verdict and customers are
						untouched.
					</dd>
					<dt>Drop a kid with tokens left</dt>
					<dd class="p20-danger">
						Those tokens become unreadable → 402 <code>license_unreadable</code> → verdict not active → the edge
						refuses those tenants. Always reseal to 0 first.
					</dd>
					<dt>Revoke a licence (offboard)</dt>
					<dd>
						Verdict flips to revoked at once (nudge), the edge refuses, LLM token minting stops; minted tokens
						die within their TTL.
					</dd>
					<dt>Verdict refresh stalls</dt>
					<dd>
						After {VERDICT_MAX_AGE_S / 60} min the edge fails closed even though the licence is fine — that is
						the safe default, not an outage of the licence.
					</dd>
				</dl>
			</Card>
			<Card title="What a licence carries" sub={o.bundle ? `Bundle ${o.bundle.key} — per tenant` : undefined}>
				{#if o.bundle}
					<ul class="p20-bundle">
						{#each o.bundle.members as m (m.app)}
							<li>
								<span class="p20-app mono">{m.app}</span>
								<Chip
									text={m.holding === 'sealed'
										? 'stored · sealed'
										: m.holding === 'minted'
											? `minted · ≤${m.ttl_min} min`
											: 'static (interim)'}
									variant={m.holding === 'sealed' ? 'accent' : m.holding === 'minted' ? 'ok' : 'warn'}
								/>
								<span class="cell-sub">{m.purpose} <span class="p20-ref">{m.ref}</span></span>
							</li>
						{/each}
					</ul>
					<p class="p20-note muted">
						Only <span class="mono">agent</span> is stored, so only it is on the keyring. Minted tokens come
						from
						<span class="mono">{o.bundle.minter}</span> (mint-only) and live in memory.
					</p>
				{/if}
				<p class="p20-note">
					<strong>Metering:</strong> every usage-ledger row is attributed with the licence id + subject id (<code
						>agent_licence_attribution()</code
					>, never the token). Billing emit is
					<Chip
						text={o.metering_emit_enabled ? 'on' : 'off'}
						variant={o.metering_emit_enabled ? 'ok' : 'muted'}
					/>{o.metering_emit_enabled ? '.' : ' — the ledger is written, nothing is sent.'}
				</p>
			</Card>
		</div>

		<LicenceDrawer
			bind:open={drawerOpen}
			row={o.tenants.find((t) => t.tenant_id === pickedId) ?? null}
			overview={o}
			{busy}
			onreseal={(id) => resealOne(id)}
		/>
	{/snippet}
</LoadState>

<Drawer
	bind:open={replaceOpen}
	title={replaceMode === 'remint' ? 'Leaked token — re-mint' : 'Re-issue licence'}
>
	{#snippet sub()}<span>{replaceRow?.name}</span>{/snippet}
	<form class="stack" onsubmit={replace}>
		{#if replaceMode === 'remint'}
			<Callout kind="err" title="A leaked token is re-minted, not resealed">
				Resealing changes only the envelope at rest. Re-minting issues a new licence for this tenant, stores
				its token sealed under the current kid and revokes the old licence
				{replaceRow?.licence ? `(${replaceRow.licence.licence_id})` : ''} at once, so the leaked token stops working.
				Members are not touched.
			</Callout>
			<div class="field">
				<label for="remint-name">Type <strong>{replaceRow?.name}</strong> to confirm</label>
				<input
					id="remint-name"
					class="input"
					autocomplete="off"
					bind:value={confirmName}
					data-testid="remint-confirm"
				/>
			</div>
			<div class="field">
				<label for="remint-reason">Reason (optional, audited; never paste the token)</label>
				<input id="remint-reason" class="input" maxlength="200" bind:value={remintReason} />
			</div>
		{:else}
			<Callout kind="info" title="Revokes the current licence">
				The current licence{replaceRow?.licence ? ` (${replaceRow.licence.licence_id})` : ''} is revoked, then
				a new one is issued for the same holder in the tenant's residency region. Members are not disabled. If
				licences does not confirm the revoke in time, the provisioning tick finishes the re-issue.
			</Callout>
		{/if}
		{#if replaced}
			<Callout
				kind={replaced.status === 'reissued' ? 'info' : 'err'}
				title={replaced.status === 'reissued' ? 'Done' : 'Old token still valid — retrying'}
			>
				<span data-testid="replace-result"
					>{#if replaced.status === 'reissued'}New licence <span class="mono">{replaced.new_licence_id}</span
						>;
						{replaced.old_licence_id} revoked.{:else}Licences did not confirm the revoke of {replaced.old_licence_id};
						it stays valid until the provisioning tick revokes it{replaced.code
							? ` (${replaced.code})`
							: ''}.{/if}</span
				>
			</Callout>
		{/if}
		{#if replaceError}<Callout kind="err" title={replaceError.code}>{replaceError.message}</Callout>{/if}
		<div class="actions">
			{#if replaced}
				<button class="btn" type="button" onclick={() => (replaceOpen = false)}>Close</button>
			{:else}
				<button
					class="btn danger"
					type="submit"
					{...superAdminAttrs(session)}
					disabled={replacing || !session.superAdmin || (replaceMode === 'remint' && !nameMatches)}
					>{replacing
						? 'Working…'
						: replaceMode === 'remint'
							? 'Re-mint and revoke the old licence'
							: 'Re-issue licence'}</button
				>
			{/if}
		</div>
	</form>
</Drawer>

<style>
	.p20-answers {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(280px, 1fr));
		gap: 12px;
	}
	.p20-answer {
		background: var(--panel);
		border: 1px solid var(--line);
		border-left: 4px solid var(--ok);
		border-radius: 8px;
		padding: 12px 16px;
		display: grid;
		gap: 4px;
		min-width: 0;
	}
	.p20-answer.bad {
		border-left-color: var(--err);
	}
	.p20-answer.warn {
		border-left-color: var(--warn);
	}
	.p20-big {
		margin: 0;
		font: 600 28px/1.15 var(--cond);
		font-variant-numeric: tabular-nums;
	}
	.p20-big small {
		font: 500 13px var(--sans);
		color: var(--muted);
		margin-left: 4px;
	}
	.p20-line {
		margin: 0;
		font-size: 13px;
	}
	.p20-n {
		font-weight: 700;
		font-variant-numeric: tabular-nums;
	}
	.p20-n.err {
		color: var(--err);
	}
	.p20-pad {
		padding: 4px 16px 12px;
	}
	.p20-agecol {
		min-width: 170px;
	}
	.p20-legend {
		display: block;
		font: 500 10.5px var(--sans);
		letter-spacing: 0;
		text-transform: none;
		color: var(--faint);
		margin-top: 2px;
	}
	.p20-age {
		display: flex;
		align-items: center;
		gap: 8px;
	}
	.p20-track {
		position: relative;
		flex: 1;
		min-width: 90px;
		height: 8px;
		border-radius: 999px;
		background: linear-gradient(
			to right,
			color-mix(in srgb, var(--ok) 22%, var(--sunk)) 0 var(--lim),
			color-mix(in srgb, var(--err) 22%, var(--sunk)) 0
		);
	}
	.p20-tick,
	.p20-limit {
		position: absolute;
		top: -3px;
		bottom: -3px;
		width: 0;
		border-left: 1px dashed var(--muted);
	}
	.p20-limit {
		border-left: 2px solid var(--err);
	}
	.p20-dot {
		position: absolute;
		top: 50%;
		width: 12px;
		height: 12px;
		margin: -6px 0 0 -6px;
		border-radius: 50%;
		border: 2px solid var(--panel);
		background: var(--ok);
	}
	.p20-dot.err {
		background: var(--err);
	}
	.p20-dot.warn {
		background: var(--warn);
	}
	.p20-dot.muted {
		background: var(--faint);
	}
	.p20-agev {
		min-width: 44px;
		text-align: right;
	}
	.p20-agev.stale {
		color: var(--err);
		font-weight: 600;
	}
	.p20-behind {
		color: var(--warn);
		font-weight: 600;
	}
	tr.archived td {
		color: var(--faint);
	}
	.p20-stillvalid {
		color: var(--err);
		font-weight: 600;
	}
	.p20-rowacts {
		white-space: nowrap;
	}
	.p20-rowacts .btn + .btn {
		margin-left: 4px;
	}
	.p20-conseq {
		margin: 0;
		display: grid;
		gap: 4px;
		font-size: 13px;
	}
	.p20-conseq dt {
		font-weight: 600;
		margin-top: 6px;
	}
	.p20-conseq dt:first-child {
		margin-top: 0;
	}
	.p20-conseq dd {
		margin: 0;
		color: var(--muted);
	}
	.p20-conseq dd.p20-danger {
		color: var(--err);
	}
	.p20-bundle {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 10px;
		font-size: 13px;
	}
	.p20-app {
		font-weight: 600;
		margin-right: 6px;
	}
	.p20-ref {
		color: var(--faint);
		font-family: var(--mono);
		font-size: 11px;
	}
	.p20-note {
		margin: 12px 0 0;
		font-size: 13px;
	}
</style>
