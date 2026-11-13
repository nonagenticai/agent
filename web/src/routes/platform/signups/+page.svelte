<script lang="ts">
	// P3 Signups — the early-access queue, on the REAL agent-api (`/api/signup-approvals*`, super_admin; based on
	// strata_configurator web/src/routes/signups). A request arrives from the website or the signup chat; approving
	// it creates the requester's workspace with them invited as its tenant admin (Keycloak user, licence, email in
	// their language), and each provisioning step is journalled and shown here. Rejecting sends nothing. A Telegram
	// deep link `?signup=<id>&action=approve|reject` opens that decision prefilled — nothing is posted until confirmed.
	import { onMount } from 'svelte';
	import { page } from '$app/state';
	import { signupsApi } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { superAdminAttrs } from '$lib/auth/roles';
	import { ApiError } from '$lib/api/errors';
	import type { Residency } from '$lib/api/types/core';
	import {
		EXISTING_IDENTITY_INVITE_PENDING,
		SIGNUP_LOCALES,
		stepsOf,
		type SignupLocale,
		type ProvisioningStep,
		type SignupApproval,
		type SignupApprovalStatus,
	} from '$lib/api/types/signup-approvals';
	import PageHead from '$lib/ui/PageHead.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';
	import { toast } from '$lib/ui/toaster.svelte';
	import { maskEmail, utc } from '$lib/platform/ui';
	import { orderedSignals, SIGNAL_MARK } from '$lib/platform/signupsP3';

	const LOCALE_NAMES: Record<string, string> = {
		ar: 'Arabic',
		de: 'German',
		en: 'English',
		es: 'Spanish',
		fr: 'French',
		he: 'Hebrew',
		nl: 'Dutch',
		pl: 'Polish',
	};
	const STEP_VARIANT = { ok: 'ok', failed: 'err', pending: 'warn', skipped: 'muted' } as const;
	/** B1b F-03: a signal's colour follows its level (and its glyph + word, so colour is never alone). */
	const SIGNAL_VARIANT = { ok: 'ok', info: 'accent', risk: 'err' } as const;

	/** Only a platform super_admin reviews signups. */
	const allowed = $derived(session.superAdmin);

	let view = $state<'pending' | 'decided'>('pending');
	let rows = $state<SignupApproval[]>([]);
	let loading = $state(true);
	let off = $state(false);
	let loadError = $state('');

	type Decision = { signup: SignupApproval; action: 'approve' | 'reject' };
	let decision = $state<Decision | null>(null);
	let open = $state(false);
	let tenantName = $state('');
	/** D66: required on approve, immutable after; no default so it is always a choice. */
	let residency = $state<Residency | ''>('');
	let deciding = $state(false);
	/** The signup whose failed steps are being retried. */
	let retrying = $state<string | null>(null);
	/** The refusal, and (a gating step failed: 502 `<step code>`) the journal so far — approve again resumes. */
	let error = $state<{ code: string; message: string; steps?: ProvisioningStep[] } | null>(null);
	let linkError = $state('');
	let notice = $state<{ text: string; tenant: string | null; steps: ProvisioningStep[] } | null>(null);

	/** Every row of one status (follows `next` cursors). */
	async function all(status: SignupApprovalStatus): Promise<SignupApproval[]> {
		const out: SignupApproval[] = [];
		let before: string | null = null;
		do {
			const p = await signupsApi().list(status, before);
			out.push(...p.items);
			before = p.next;
		} while (before);
		return out;
	}

	async function load(): Promise<SignupApproval[]> {
		loading = true;
		loadError = '';
		try {
			const found =
				view === 'pending' ? await all('pending') : [...(await all('approved')), ...(await all('rejected'))];
			rows = found.sort((a, b) => b.created_at.localeCompare(a.created_at));
			off = false;
		} catch (e) {
			rows = [];
			// The feature ships dark: switched off, its routes do not exist.
			if (e instanceof ApiError && e.status === 404) off = true;
			else loadError = e instanceof Error ? e.message : String(e);
		} finally {
			loading = false;
		}
		return rows;
	}

	function decide(signup: SignupApproval, action: Decision['action']) {
		decision = { signup, action };
		tenantName = signup.suggested_tenant_name;
		residency = '';
		error = null;
		notice = null;
		open = true;
	}

	async function confirm(e: SubmitEvent) {
		e.preventDefault();
		if (!decision) return;
		const { signup, action } = decision;
		deciding = true;
		error = null;
		try {
			if (action === 'approve') {
				if (!residency) {
					error = { code: 'invalid_residency', message: 'Choose where this workspace keeps its data.' };
					return;
				}
				const done = await signupsApi().approve(signup.id, tenantName, residency);
				notice = {
					text:
						done.seated === false
							? `Approved ${signup.company ?? signup.name}: workspace “${done.tenant.name}” created. ${maskEmail(signup.email)} already had an account, so it was not seated: a pending member invite holds the tenant-admin seat until they accept.`
							: `Approved ${signup.company ?? signup.name}: workspace “${done.tenant.name}” created and ${maskEmail(signup.email)} invited as its admin${signup.locale ? ` (email in ${LOCALE_NAMES[signup.locale] ?? signup.locale})` : ''}.`,
					tenant: done.tenant.id,
					steps: stepsOf(done.steps),
				};
			} else {
				const r = (await signupsApi().reject(signup.id)) as { offboarded_tenant_id?: string | null } | null;
				notice = {
					text: r?.offboarded_tenant_id
						? `Rejected ${signup.company ?? signup.name}. The workspace an interrupted approve had started was offboarded; nothing was sent.`
						: `Rejected ${signup.company ?? signup.name}. Nothing was created or sent.`,
					tenant: null,
					steps: [],
				};
			}
			toast(action === 'approve' ? 'Signup approved.' : 'Signup rejected.');
			open = false;
			await load();
		} catch (err) {
			error =
				err instanceof ApiError
					? {
							code: err.code,
							message: err.message,
							steps: stepsOf(err.details.steps as SignupApproval['steps']),
						}
					: { code: 'error', message: err instanceof Error ? err.message : String(err) };
		} finally {
			deciding = false;
		}
	}

	// ---- pre-approved invite (super_admin; CP inviteEarlyAccess) ----------------------------------------------------
	let inviteOpen = $state(false);
	let inv = $state({ email: '', name: '', company: '', tenantName: '', locale: '' as SignupLocale | '' });
	let invResidency = $state<Residency | ''>('');
	let inviting = $state(false);
	let inviteError = $state<{ code: string; message: string; steps?: ProvisioningStep[] } | null>(null);

	function openInvite() {
		inv = { email: '', name: '', company: '', tenantName: '', locale: '' };
		invResidency = '';
		inviteError = null;
		notice = null;
		inviteOpen = true;
	}

	async function sendInvite(e: SubmitEvent) {
		e.preventDefault();
		if (!invResidency) {
			inviteError = { code: 'invalid_residency', message: 'Choose where this workspace keeps its data.' };
			return;
		}
		inviting = true;
		inviteError = null;
		try {
			const r = await signupsApi().invite({
				email: inv.email,
				name: inv.name,
				company: inv.company,
				tenantName: inv.tenantName,
				locale: inv.locale || undefined,
				residency: invResidency,
			});
			notice = {
				text: r.seated
					? `Invited ${inv.name.trim()}: workspace “${r.tenant.name}” created and ${maskEmail(inv.email.trim())} invited as its admin${inv.locale ? ` (email in ${LOCALE_NAMES[inv.locale]})` : ''}.${r.reused_pending ? ' Their earlier pending request was approved instead of a new one.' : ''}`
					: `Invited ${inv.name.trim()}: workspace “${r.tenant.name}” created. ${maskEmail(inv.email.trim())} already had an account, so it was not seated: a pending member invite holds the tenant-admin seat until they accept.`,
				tenant: r.tenant.id,
				steps: stepsOf(r.steps),
			};
			toast('Invite sent.');
			inviteOpen = false;
			view = 'decided';
			await load();
		} catch (err) {
			inviteError =
				err instanceof ApiError
					? {
							code: err.code,
							message: err.message,
							steps: stepsOf(err.details.steps as SignupApproval['steps']),
						}
					: { code: 'error', message: err instanceof Error ? err.message : String(err) };
		} finally {
			inviting = false;
		}
	}

	const hasFailedStep = (s: SignupApproval) => stepsOf(s.steps).some((st) => st.status === 'failed');

	/** B1 T14 Retry: every failed provisioning step of an approved signup back to pending, run again. */
	async function retry(s: SignupApproval) {
		retrying = s.id;
		try {
			const r = await signupsApi().retry(s.id);
			const after = stepsOf(r.steps);
			const still = after.filter((st) => st.status === 'failed').length;
			toast(
				still ? `Retried; ${still} step${still === 1 ? '' : 's'} still failing.` : 'Failed steps retried.',
			);
			rows = rows.map((x) => (x.id === s.id ? { ...x, steps: r.steps } : x));
		} catch (err) {
			toast(`Retry failed: ${err instanceof Error ? err.message : String(err)}`, 'err');
		} finally {
			retrying = null;
		}
	}

	function show(v: 'pending' | 'decided') {
		view = v;
		void load();
	}

	onMount(() => {
		if (!allowed) return;
		const target = page.url.searchParams.get('signup');
		const action = page.url.searchParams.get('action');
		void load().then((found) => {
			if (!target) return;
			const s = found.find((x) => x.id === target);
			if (s && (action === 'approve' || action === 'reject')) decide(s, action);
			else if (!off && !loadError)
				linkError = 'That signup is no longer pending (it was approved or rejected already).';
		});
	});
</script>

{#snippet steps(list: ProvisioningStep[])}
	{#if list.length}
		<ol class="p3-journal" aria-label="Provisioning steps" data-testid="provisioning-steps">
			{#each list as st (st.step)}
				<li data-testid="provisioning-step">
					<Chip text={st.status} variant={STEP_VARIANT[st.status] ?? 'muted'} />
					<b>{st.step}</b>
					{#if st.code === EXISTING_IDENTITY_INVITE_PENDING}<span
							class="cell-sub p3-invite-pending"
							data-testid="step-invite-pending"
							>Existing account — not seated or licensed at approval; a pending member invite holds the
							tenant-admin seat until they accept.</span
						>{:else if st.code || st.message}<span class="cell-sub"
							>{st.code ?? ''}{st.code && st.message ? ' — ' : ''}{st.message ?? ''}</span
						>{/if}
					{#if st.attempts && st.attempts > 1}<span class="cell-sub">{st.attempts} attempts</span>{/if}
				</li>
			{/each}
		</ol>
	{/if}
{/snippet}

<PageHead
	id="P3"
	sub="Who gets a workspace without you creating it by hand. Approve a request and the workspace is created with the requester invited as its admin; reject it and nothing happens."
/>

{#if !allowed}
	<Callout kind="err" title="Super-admin only"
		>Only a platform super admin reviews early-access signups.</Callout
	>
{:else}
	{#if linkError}<Callout kind="err" title="Link expired">{linkError}</Callout>{/if}
	{#if notice}
		<Callout kind="info" title="Done">
			{notice.text}
			{@render steps(notice.steps)}
		</Callout>
	{/if}

	{#if off}
		<Card>
			<div class="empty" role="status" data-testid="signups-off">
				<h2>Early access is off</h2>
				<p>
					Self-signup is not enabled on this deployment, so its routes do not exist (404) and nobody can
					request a workspace.
				</p>
			</div>
		</Card>
	{:else}
		<div class="split p3-bar">
			<div class="seg" role="group" aria-label="Signup view">
				<button
					type="button"
					class="tab"
					class:on={view === 'pending'}
					aria-pressed={view === 'pending'}
					onclick={() => show('pending')}>Pending</button
				>
				<button
					type="button"
					class="tab"
					class:on={view === 'decided'}
					aria-pressed={view === 'decided'}
					onclick={() => show('decided')}>Decided</button
				>
			</div>
			<span class="row">
				<span class="muted" data-testid="signup-count">{loading ? '…' : `${rows.length} ${view}`}</span>
				<button
					class="btn sm"
					type="button"
					data-testid="invite-open"
					{...superAdminAttrs(session)}
					onclick={() => openInvite()}>Invite…</button
				>
			</span>
		</div>

		{#if loadError}
			<Unavailable reason={loadError} retry={() => void load()} />
		{:else if loading}
			<p class="muted" role="status">Loading signups…</p>
		{:else if rows.length}
			<div class="stack">
				{#each rows as s (s.id)}
					<article class="card p3-req" data-testid="signup-row">
						<header class="p3-req-head">
							<div class="p3-who">
								<span class="cell-title">{s.name}</span>
								<span class="cell-sub mono" title="Email is PII; shown masked">{maskEmail(s.email)}</span>
							</div>
							<div class="p3-org">
								<b>{s.company ?? '—'}</b>
								<span class="cell-sub row">
									via <Chip text={s.source} />
									{#if s.invited_by}<Chip
											text={`invited by ${s.invited_by}`}
											title="Pre-approved invite"
										/>{/if}
									{#if s.approving}<Chip
											text="approving…"
											variant="warn"
											title="An approve is running; a second one is refused until it ends"
										/>{/if}
									<span data-testid="signup-locale"
										><Chip
											text={s.locale
												? `${LOCALE_NAMES[s.locale] ?? s.locale} (${s.locale})`
												: 'English (not told)'}
											variant="accent"
										/></span
									>
									{#if s.trust !== undefined}<Chip
											text={s.trust ?? 'untrusted'}
											variant={s.trust ? 'ok' : 'warn'}
											title="How intake admitted the request"
										/>{/if}
								</span>
							</div>
							<div class="p3-when"><span class="mono nowrap">{utc(s.created_at)}</span></div>
						</header>

						{#if s.signals?.length}
							<ul class="p3-signals" aria-label="What intake noticed" data-testid="signup-signals">
								{#each orderedSignals(s) as g (g.key)}
									{@const text = `${SIGNAL_MARK[g.level].glyph} ${g.label}`}
									<li data-testid="signup-signal" data-key={g.key} data-level={g.level}>
										{#if g.audit_action}<a
												class="p3-sig-link"
												href={`/platform/audit?action=${encodeURIComponent(g.audit_action)}`}
												title={`${SIGNAL_MARK[g.level].word}: ${g.detail} Opens the platform audit for ${g.audit_action}.`}
												data-testid="signal-audit"><Chip {text} variant={SIGNAL_VARIANT[g.level]} /></a
											>{:else}<Chip
												{text}
												variant={SIGNAL_VARIANT[g.level]}
												title={`${SIGNAL_MARK[g.level].word}: ${g.detail}`}
											/>{/if}
									</li>
								{/each}
							</ul>
						{/if}

						{#if s.message}<blockquote class="p3-msg">{s.message}</blockquote>{/if}

						<footer class="p3-req-foot">
							{#if s.status === 'pending'}
								<p class="note">
									Approving creates workspace “{s.suggested_tenant_name}” with {s.name} as its admin.
								</p>
								<div class="actions">
									<button
										class="btn sm primary"
										{...superAdminAttrs(session)}
										onclick={() => decide(s, 'approve')}>Approve…</button
									>
									<button class="btn sm" {...superAdminAttrs(session)} onclick={() => decide(s, 'reject')}
										>Reject…</button
									>
								</div>
							{:else}
								<div class="p3-outcome">
									<Chip text={s.status} variant={s.status === 'approved' ? 'ok' : 'err'} />
									{#if s.decided_at}<span class="cell-sub"
											>{utc(s.decided_at)}{s.decided_by ? ` · ${s.decided_by}` : ''}</span
										>{/if}
									{#if s.status === 'approved' && hasFailedStep(s)}
										<button
											class="btn sm"
											data-testid="signup-retry"
											{...superAdminAttrs(session)}
											disabled={!allowed || !session.superAdmin || retrying === s.id}
											onclick={() => retry(s)}
											>{retrying === s.id ? 'Retrying…' : 'Retry failed steps'}</button
										>
									{/if}
								</div>
							{/if}
						</footer>
						{@render steps(stepsOf(s.steps))}
					</article>
				{/each}
			</div>
		{:else}
			<Card>
				<p class="empty">
					{view === 'pending'
						? 'No pending signups. New requests land here and in the ops Telegram chat.'
						: 'Nothing decided yet.'}
				</p>
			</Card>
		{/if}
	{/if}
{/if}

<Drawer
	bind:open
	title={decision
		? `${decision.action === 'approve' ? 'Approve' : 'Reject'} ${decision.signup.company ?? decision.signup.name}`
		: 'Signup'}
>
	{#snippet sub()}{#if decision}<span class="mono">{maskEmail(decision.signup.email)}</span>{/if}{/snippet}
	{#if decision}
		<form class="stack" onsubmit={confirm}>
			{#if decision.action === 'approve'}
				<div class="field">
					<label for="tenant-name">Workspace name</label>
					<input
						id="tenant-name"
						class="input"
						bind:value={tenantName}
						minlength="2"
						maxlength="120"
						required
					/>
				</div>
				<div class="field">
					<label for="signup-residency">Data residency</label>
					<select id="signup-residency" class="input" bind:value={residency} required>
						<option value="" disabled>Choose…</option>
						<option value="eu">EU</option>
						<option value="us">US</option>
					</select>
					<span class="cell-sub">Where its knowledge, models and logs live. Cannot be changed later.</span>
				</div>
				<div>
					<span class="label">When you confirm</span>
					<ol class="p3-steps">
						<li><b>Workspace “{tenantName || '…'}” created</b> with its licence.</li>
						<li>
							<b>{decision.signup.name} invited</b> as its tenant admin — the set-password email in
							{decision.signup.locale
								? (LOCALE_NAMES[decision.signup.locale] ?? decision.signup.locale)
								: 'English'}.
						</li>
						<li>Each step is journalled and shown on this request.</li>
					</ol>
				</div>
			{:else}
				<p class="note">
					No workspace is created and nothing is sent to {decision.signup.name}; the request moves to Decided.
				</p>
			{/if}
			{#if error}<Callout kind="err" title={error.code}>{error.message}</Callout>
				{#if error.steps?.length}{@render steps(error.steps)}{/if}{/if}
			<div class="actions">
				<button class="btn primary" type="submit" disabled={deciding || !allowed}
					>{decision.action === 'approve' ? 'Approve and create workspace' : 'Reject signup'}</button
				>
			</div>
		</form>
	{/if}
</Drawer>

<Drawer bind:open={inviteOpen} title="Invite to early access">
	{#snippet sub()}A pre-approved request: the workspace is created and its admin invited at once.{/snippet}
	<form class="stack" onsubmit={sendInvite} data-testid="invite-form">
		<div class="field">
			<label for="inv-email">E-mail</label>
			<input id="inv-email" class="input" type="email" bind:value={inv.email} required />
		</div>
		<div class="field">
			<label for="inv-name">Name</label>
			<input id="inv-name" class="input" bind:value={inv.name} required />
		</div>
		<div class="field">
			<label for="inv-company">Company (optional)</label>
			<input id="inv-company" class="input" bind:value={inv.company} />
		</div>
		<div class="field">
			<label for="inv-tenant">Workspace name (optional)</label>
			<input
				id="inv-tenant"
				class="input"
				bind:value={inv.tenantName}
				placeholder={inv.company || inv.name || 'From the company or name'}
			/>
		</div>
		<div class="field">
			<label for="inv-locale">Invite language</label>
			<select id="inv-locale" class="input" bind:value={inv.locale}>
				<option value="">English (not told)</option>
				{#each SIGNUP_LOCALES as l (l)}<option value={l}>{LOCALE_NAMES[l] ?? l} ({l})</option>{/each}
			</select>
		</div>
		<div class="field">
			<label for="inv-residency">Data residency</label>
			<select id="inv-residency" class="input" bind:value={invResidency} required>
				<option value="" disabled>Choose…</option>
				<option value="eu">EU</option>
				<option value="us">US</option>
			</select>
			<span class="cell-sub">Where its knowledge, models and logs live. Cannot be changed later.</span>
		</div>
		<p class="note">
			An address that already has a portal account is not seated at once: a pending member invite holds the
			tenant-admin seat until that person accepts.
		</p>
		{#if inviteError}<Callout kind="err" title={inviteError.code}>{inviteError.message}</Callout>
			{#if inviteError.steps?.length}{@render steps(inviteError.steps)}{/if}{/if}
		<div class="actions">
			<button class="btn primary" type="submit" disabled={inviting || !allowed || !session.superAdmin}
				>{inviting ? 'Sending…' : 'Create workspace and send invite'}</button
			>
		</div>
	</form>
</Drawer>

<style>
	.p3-signals {
		list-style: none;
		margin: 8px 0 0;
		padding: 0;
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
	}
	.p3-sig-link {
		text-decoration: none;
	}
	.p3-sig-link:hover :global(.chip),
	.p3-sig-link:focus-visible :global(.chip) {
		text-decoration: underline;
	}
	.p3-bar {
		margin-bottom: 10px;
	}
	.p3-req {
		display: grid;
		gap: 10px;
	}
	.p3-req-head {
		display: grid;
		grid-template-columns: minmax(0, 1.2fr) minmax(0, 1.4fr) auto;
		gap: 12px;
		align-items: start;
	}
	.p3-who,
	.p3-org,
	.p3-when {
		display: grid;
		gap: 3px;
		min-width: 0;
	}
	.p3-when {
		text-align: right;
	}
	.p3-org .row {
		flex-wrap: wrap;
		gap: 6px;
	}
	.p3-msg {
		margin: 0;
		padding: 8px 12px;
		border-left: 3px solid var(--line);
		color: var(--muted);
		white-space: pre-wrap;
	}
	.p3-req-foot {
		display: flex;
		justify-content: space-between;
		align-items: center;
		gap: 12px;
		flex-wrap: wrap;
	}
	.p3-req-foot .note {
		margin: 0;
	}
	.p3-outcome {
		display: flex;
		gap: 8px;
		align-items: center;
	}
	.actions {
		display: flex;
		gap: 8px;
	}
	.p3-journal {
		margin: 0;
		padding: 0;
		list-style: none;
		display: grid;
		gap: 4px;
		font-size: 13px;
	}
	.p3-journal li {
		display: flex;
		gap: 8px;
		align-items: baseline;
		flex-wrap: wrap;
	}
	.p3-steps {
		margin: 6px 0 0;
		padding-left: 20px;
		display: grid;
		gap: 4px;
	}
	@media (max-width: 860px) {
		.p3-req-head {
			grid-template-columns: minmax(0, 1fr);
		}
		.p3-when {
			text-align: left;
		}
	}
</style>
