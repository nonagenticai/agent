<script lang="ts">
	// B12 channel bindings of one job. Customer side: where customers reach this job (a web chat widget, a Telegram
	// bot). Worker side: where staff are told about a handover and reply (Frappe Helpdesk). A binding may carry its own
	// credential (bring your own bot or app): it is sealed by the API and never shown again — only that one is set
	// and its fingerprint. Writes need the Integrations manager role (D88).
	// 409 `binding_unavailable` = the account is bound already.
	// B13: a web chat binding's panel carries its embed snippet (the API's `snippet`), the widget's display (title,
	// greeting, colour) and rate limits — saved into the binding's settings — and "Rotate key", which ends open chats.
	// B14: the panel also sets the Turnstile bot check (off / the platform's widget / the tenant's own, whose secret goes
	// into the binding credential as `turnstile_secret`, write-only).
	// B7 T15b: a Telegram binding's State shows the webhook health agent-edge observed (`webhook`: ok / degraded after 3
	// failed sends in a row until a send succeeds / quiet — neutral, not an error — after 24 h without an update, the
	// last update and the last error code); no report yet is "unknown" — never guessed (this console does not ask
	// Telegram).
	import { onMount } from 'svelte';
	import { api, app, guard } from '$lib/app.svelte';
	import { ApiError } from '$lib/api/errors';
	import { Resource } from '$lib/load.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { canEditChannels, CHANNELS_HINT, writeAttrs } from '$lib/auth/roles';
	import { sizeLabel } from '$lib/support/attachments';
	import {
		BINDING_KIND_LABEL,
		BINDING_KINDS,
		attachmentSwitches,
		type AttachmentQuota,
		BINDING_UNAVAILABLE,
		CREDENTIAL_FIELDS,
		credentialRequired,
		externalIdLabel,
		sidesOf,
		webChatSnippet,
		displayError,
		limitsError,
		WEBCHAT_COLOUR_RE,
		WEBCHAT_GREETING_MAX,
		WEBCHAT_LIMIT_DEFAULTS,
		WEBCHAT_LIMIT_MAX,
		WEBCHAT_LIMIT_MIN,
		WEBCHAT_LIMITS,
		WEBCHAT_TITLE_MAX,
		IDENTITY_DEFAULT_HOURS,
		IDENTITY_MODES,
		identityError,
		identityOf,
		TURNSTILE_MODES,
		TURNSTILE_SECRET_KEY,
		turnstileError,
		type WebChatIdentityMode,
		type IdentitySecret,
		type TurnstileMode,
		type WebChatDisplay,
		type WebChatLimits,
		type WebChatTurnstile,
		type BindingKind,
		type BindingSettings,
		type BindingSide,
		type BindingWebhook,
		type ChannelBinding,
		type WebhookDownReason,
		type WebhookHealthStatus,
		type CreateChannelBinding,
		type UpdateChannelBinding,
	} from '$lib/api/types/conversations';
	import { webChatDemoUrl } from '$lib/tenant/channelsT13';
	import { toast } from '$lib/ui/toaster.svelte';
	import { when } from '$lib/runtime/labels';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LiveNote from '$lib/release/LiveNote.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Card from '$lib/ui/Card.svelte';

	const res = new Resource(() => api().channelBindings());
	onMount(() => res.refresh());
	/** The tenant's attachment storage, read-only (a platform super_admin sets it). A failed read just hides the line. */
	let quota = $state<AttachmentQuota | null>(null);
	onMount(async () => {
		try {
			quota = (await api().attachmentQuota?.()) ?? null;
		} catch {
			quota = null;
		}
	});
	const job = $derived(`/admin/jobs/${app.job}`);
	const may = $derived(canEditChannels(session));
	const hint = $derived(may ? undefined : CHANNELS_HINT);
	/** D130: publishing a customer channel is a release_manager's call (a tenant_admin holds it, D127). */
	const pub = $derived(writeAttrs(session, 'release_manager'));

	// ---- generic edit (D126.2): label + settings of every kind but web chat, and credential replacement ----
	let gEditing = $state<string | null>(null);
	let gLabel = $state('');
	let gF = $state<Record<string, string>>({});
	let gCred = $state<Record<string, string>>({});
	let gError = $state<{ code: string; message: string } | null>(null);
	let gBusy = $state(false);
	/** Photos and PDFs on a channel (SW-9): the two switches of the open edit form / widget panel. */
	let gAttCustomer = $state(true);
	let gAttStaff = $state(true);
	/** The settings fields an edit form shows for a binding (kind + side); empty = label and credential only. */
	function settingFields(
		b: ChannelBinding,
	): { key: string; label: string; placeholder?: string; area?: boolean }[] {
		if (b.kind === 'telegram')
			return [
				{ key: 'allowed_user_ids', label: 'Allowed Telegram user ids (one per line)', area: true },
				{ key: 'test_chat_id', label: 'Test chat id (empty = the first allowed user)' },
			];
		if (b.side !== 'worker') return [];
		if (b.kind === 'helpdesk')
			return [
				{ key: 'base_url', label: 'Helpdesk URL' },
				{ key: 'service_user', label: 'Service user (an email address)' },
			];
		return [];
	}
	function openGeneric(b: ChannelBinding) {
		if (gEditing === b.id) {
			gEditing = null;
			return;
		}
		gEditing = b.id;
		gLabel = b.label;
		gCred = {};
		gError = null;
		const s = b.settings ?? {};
		const att = attachmentSwitches(b.kind, b.settings);
		gAttCustomer = att.customer;
		gAttStaff = att.staff;
		gF = {
			allowed_user_ids: (s.allowed_user_ids ?? []).join('\n'),
			test_chat_id: s.test_chat_id === undefined ? '' : String(s.test_chat_id),
			base_url: s.base_url ?? '',
			service_user: s.service_user ?? '',
		};
	}
	async function saveGeneric(b: ChannelBinding) {
		gBusy = true;
		gError = null;
		try {
			const settings: BindingSettings = { ...(b.settings ?? {}) };
			for (const f of settingFields(b)) {
				const v = (gF[f.key] ?? '').trim();
				if (f.key === 'allowed_user_ids') settings.allowed_user_ids = lines(v).map(Number);
				else if (f.key === 'test_chat_id') {
					if (v) settings.test_chat_id = Number(v);
					else delete settings.test_chat_id;
				} else (settings as Record<string, unknown>)[f.key] = v;
			}
			if (b.kind === 'telegram') settings.attachments = { customer: gAttCustomer, staff: gAttStaff };
			const patch: UpdateChannelBinding = { label: gLabel.trim(), settings };
			const c: Record<string, string> = {};
			for (const f of CREDENTIAL_FIELDS[b.kind]) if (gCred[f.key]?.trim()) c[f.key] = gCred[f.key]!.trim();
			if (Object.keys(c).length) patch.credential = c;
			await api().updateChannelBinding(b.id, patch);
			toast(`“${gLabel.trim()}” saved.`);
			gEditing = null;
			await res.refresh();
		} catch (e) {
			// the server's 422 (invalid_settings, invalid_credential, …) stays next to the form
			gError =
				e instanceof ApiError
					? { code: e.code, message: e.message }
					: { code: 'error', message: e instanceof Error ? e.message : 'Not saved' };
		} finally {
			gBusy = false;
			gCred = {};
		}
	}

	// ---- Telegram test message (D126.2): one fixed text to the binding's test chat ----
	let testing = $state<string | null>(null);
	let testResult = $state<{ id: string; text: string; delivered: boolean } | null>(null);
	let testError = $state<{ id: string; code: string; message: string } | null>(null);
	async function sendTest(b: ChannelBinding) {
		testing = b.id;
		testError = null;
		try {
			const r = await api().sendChannelTestMessage(b.id, app.job);
			testResult = { id: b.id, text: r.text, delivered: r.delivered };
		} catch (e) {
			testResult = null;
			testError =
				e instanceof ApiError
					? { id: b.id, code: e.code, message: e.message }
					: { id: b.id, code: 'error', message: 'Not sent' };
		} finally {
			testing = null;
		}
	}

	// ---- web chat "Try as a customer" (a TEST channel in HS256 mode): the admin names a customer of the tenant's own
	// system; core signs the sign-in proof server-side and the hosted test page opens as that known customer ----
	let tryRef = $state('');
	let tryBusy = $state<string | null>(null);
	let tryError = $state<{ id: string; message: string } | null>(null);
	async function tryAsCustomer(b: ChannelBinding) {
		const ref = tryRef.trim();
		if (!ref) return;
		tryBusy = b.id;
		tryError = null;
		// opened inside the click so the browser allows it; pointed at the test page once core has signed the proof
		const tab = window.open('about:blank', '_blank');
		if (tab) tab.opener = null;
		try {
			const r = await api().createChannelTestSession(b.id, ref);
			if (tab) tab.location.href = r.url;
			else window.open(r.url, '_blank', 'noopener');
		} catch (e) {
			tab?.close();
			tryError = {
				id: b.id,
				message: e instanceof ApiError ? e.message : 'The test could not be started.',
			};
		} finally {
			tryBusy = null;
		}
	}

	// ---- add form ----
	let kind = $state<BindingKind>('web_chat');
	let side = $state<BindingSide>('customer');
	let externalId = $state('');
	let label = $state('');
	let origins = $state('');
	let hdBaseUrl = $state('');
	let hdServiceUser = $state('');
	let cred = $state<Record<string, string>>({});
	let saving = $state(false);
	/** The last add was refused with 409 `binding_unavailable`. */
	let unavailable = $state(false);

	/** Web chat only faces customers; Frappe Helpdesk is a worker target only. */
	const sides = $derived<BindingSide[]>(sidesOf(kind));
	$effect(() => {
		if (!sides.includes(side)) side = sides[0]!;
	});
	const idLabel = $derived(externalIdLabel(kind, side));

	const lines = (s: string) =>
		s
			.split(/[\n,]/)
			.map((x) => x.trim())
			.filter(Boolean);

	function settingsOf(): BindingSettings {
		if (kind === 'web_chat') return { allowed_origins: lines(origins) };
		if (side !== 'worker') return {};
		if (kind === 'helpdesk') return { base_url: hdBaseUrl.trim(), service_user: hdServiceUser.trim() };
		return {};
	}
	function credentialOf(): Record<string, string> | undefined {
		const out: Record<string, string> = {};
		for (const f of CREDENTIAL_FIELDS[kind]) if (cred[f.key]?.trim()) out[f.key] = cred[f.key]!.trim();
		return Object.keys(out).length ? out : undefined;
	}
	const missing = $derived(
		!label.trim()
			? 'Give the binding a label.'
			: idLabel && !externalId.trim()
				? `Fill in the ${idLabel.toLowerCase()}.`
				: credentialRequired(kind) && CREDENTIAL_FIELDS[kind].some((f) => !cred[f.key]?.trim())
					? 'Fill in every credential field.'
					: null,
	);

	function reset() {
		externalId = '';
		label = '';
		origins = '';
		hdBaseUrl = '';
		hdServiceUser = '';
		cred = {};
	}

	async function add() {
		if (missing) return;
		const input: CreateChannelBinding = { kind, side, label: label.trim(), settings: settingsOf() };
		if (kind !== 'web_chat') input.external_id = externalId.trim();
		const c = credentialOf();
		if (c) input.credential = c;
		saving = true;
		unavailable = false;
		let made: ChannelBinding | null = null;
		try {
			if (
				await guard(
					async () => {
						try {
							made = await api().createChannelBinding(input);
						} catch (e) {
							if (e instanceof ApiError && e.code === 'binding_unavailable') unavailable = true;
							throw e;
						}
					},
					() => `${BINDING_KIND_LABEL[kind]} binding “${input.label}” added.`,
					'Not added',
				)
			) {
				reset();
				await res.refresh();
				if (made && (made as ChannelBinding).kind === 'web_chat') openPanel(made as ChannelBinding);
			}
		} finally {
			saving = false;
			cred = {};
		}
	}

	// ---- row actions ----
	let busy = $state<string | null>(null);
	let confirming = $state<string | null>(null);
	/** The web chat binding whose embed snippet is expanded. */
	let shown = $state<string | null>(null);

	async function toggle(b: ChannelBinding) {
		busy = b.id;
		try {
			if (
				await guard(
					() => api().updateChannelBinding(b.id, { enabled: !b.enabled }),
					b.enabled ? `“${b.label}” disabled.` : `“${b.label}” enabled.`,
				)
			)
				await res.refresh();
		} finally {
			busy = null;
		}
	}
	async function remove(b: ChannelBinding) {
		busy = b.id;
		try {
			if (await guard(() => api().deleteChannelBinding(b.id), `“${b.label}” deleted.`, 'Not deleted')) {
				confirming = null;
				await res.refresh();
			}
		} finally {
			busy = null;
		}
	}
	// ---- web chat panel: display, limits, key rotation ----
	let display = $state<Required<WebChatDisplay>>({ title: '', greeting: '', colour: '' });
	/** Limits as typed (empty = the default). */
	let limits = $state<Record<keyof WebChatLimits, string>>({
		sessions_per_ip_hour: '',
		msgs_per_ip_10m: '',
		msgs_per_session_hour: '',
		msgs_per_minute: '',
	});
	let rotating = $state<string | null>(null);
	let csat = $state(true);
	let wOrigins = $state('');
	/** Turnstile as edited; the secret is write-only (never shown, empty = keep the sealed one). */
	let tsMode = $state<TurnstileMode>('off');
	let tsSiteKey = $state('');
	let tsSecret = $state('');
	/** The open panel's binding has a sealed credential (an own widget's secret was given before). */
	let tsHasSecret = $state(false);

	/** Identity (known customers) as edited; the HS256 secret is never here, only the one just made (shown once). */
	let idMode = $state<WebChatIdentityMode>('off');
	let idIssuer = $state('');
	let idSignIn = $state('');
	let idHours = $state('');
	let idJwks = $state('');
	let idRevoke = $state(false);
	/** The secret just made for binding `id`: shown once, gone when the panel closes or the person says done. */
	let idSecret = $state<{ id: string; kid: string; secret: string } | null>(null);

	function openPanel(b: ChannelBinding) {
		shown = b.id;
		rotating = null;
		idSecret = null;
		idRevoke = false;
		const i = b.settings?.identity;
		idMode = i?.mode ?? 'off';
		idIssuer = i?.issuer ?? '';
		idSignIn = i?.sign_in_url ?? '';
		idHours = i?.session_hours === undefined ? '' : String(i.session_hours);
		idJwks = i?.jwks?.length ? JSON.stringify(i.jwks, null, 2) : '';
		const d = b.settings?.display ?? {};
		display = { title: d.title ?? '', greeting: d.greeting ?? '', colour: d.colour ?? '' };
		csat = b.settings?.csat_enabled ?? true;
		wOrigins = (b.settings?.allowed_origins ?? []).join('\n');
		const att = attachmentSwitches(b.kind, b.settings);
		gAttCustomer = att.customer;
		gAttStaff = att.staff;
		tsMode = b.settings?.turnstile?.mode ?? 'off';
		tsSiteKey = b.settings?.turnstile?.site_key ?? '';
		tsSecret = '';
		tsHasSecret = b.has_credential;
		const l = b.settings?.limits ?? {};
		for (const { key } of WEBCHAT_LIMITS) limits[key] = l[key] === undefined ? '' : String(l[key]);
	}
	function togglePanel(b: ChannelBinding) {
		if (shown === b.id) shown = null;
		else openPanel(b);
	}
	function displayOf(): WebChatDisplay {
		const out: WebChatDisplay = {};
		for (const k of ['title', 'greeting', 'colour'] as const)
			if (display[k].trim()) out[k] = display[k].trim();
		return out;
	}
	function limitsOf(): WebChatLimits {
		const out: WebChatLimits = {};
		for (const { key } of WEBCHAT_LIMITS) if (limits[key].trim() !== '') out[key] = Number(limits[key]);
		return out;
	}
	function turnstileOf(): WebChatTurnstile {
		return tsMode === 'own' ? { mode: tsMode, site_key: tsSiteKey.trim() } : { mode: tsMode };
	}
	function identityForm(b: ChannelBinding) {
		return {
			mode: idMode,
			issuer: idIssuer,
			signInUrl: idSignIn,
			hours: idHours,
			jwks: idJwks,
		};
	}
	const panelError = $derived(
		displayError(displayOf()) ??
			limitsError(limitsOf()) ??
			turnstileError(turnstileOf(), tsSecret, tsHasSecret) ??
			(shown
				? identityError({ mode: idMode, issuer: idIssuer, signInUrl: idSignIn, hours: idHours, jwks: idJwks })
				: null),
	);

	async function savePanel(b: ChannelBinding) {
		if (panelError) return;
		busy = b.id;
		try {
			const settings = {
				...(b.settings ?? {}),
				allowed_origins: lines(wOrigins),
				attachments: { customer: gAttCustomer, staff: gAttStaff },
				display: displayOf(),
				limits: limitsOf(),
				csat_enabled: csat,
				turnstile: turnstileOf(),
				identity: identityOf(identityForm(b)),
			};
			const secret = tsMode === 'own' ? tsSecret.trim() : '';
			const patch: UpdateChannelBinding = { settings };
			if (secret) patch.credential = { [TURNSTILE_SECRET_KEY]: secret };
			if (
				await guard(() => api().updateChannelBinding(b.id, patch), `“${b.label}” widget saved.`, 'Not saved')
			) {
				if (secret) tsHasSecret = true;
				await res.refresh();
			}
		} finally {
			busy = null;
			tsSecret = '';
		}
	}
	/** Make (or replace) the HS256 secret: the answer is shown once and kept nowhere else. */
	async function makeSecret(b: ChannelBinding) {
		busy = b.id;
		try {
			const got: { v?: IdentitySecret } = {};
			if (
				await guard(
					async () => {
						got.v = await api().rotateIdentitySecret(b.id, { revokeSessions: idRevoke });
					},
					idRevoke
						? 'New secret made. Open identified chats are signed out.'
						: 'New secret made. Copy it now.',
					'Secret not made',
				)
			) {
				if (got.v) idSecret = { id: b.id, ...got.v };
				idRevoke = false;
				await res.refresh();
			}
		} finally {
			busy = null;
		}
	}
	/** D130: take a customer channel live (`prod`) or back to the test env; 409 `no_release` is shown by `guard`. */
	async function setLive(b: ChannelBinding, live: boolean) {
		busy = b.id;
		try {
			if (
				await guard(
					() => (live ? api().publishBinding(b.id) : api().unpublishBinding(b.id)),
					live
						? `“${b.label}” is live: customers now get the released job.`
						: `“${b.label}” is back in test: it answers with the draft.`,
					live ? 'Not published' : 'Not unpublished',
				)
			)
				await res.refresh();
		} finally {
			busy = null;
		}
	}
	async function rotate(b: ChannelBinding) {
		busy = b.id;
		try {
			if (
				await guard(
					() => api().rotateWebChatKey(b.id),
					`“${b.label}” has a new key. Replace the snippet on your site.`,
					'Key not rotated',
				)
			) {
				rotating = null;
				await res.refresh();
			}
		} finally {
			busy = null;
		}
	}

	async function copy(text: string, what = 'Snippet') {
		try {
			await navigator.clipboard.writeText(text);
			toast(`${what} copied.`);
		} catch {
			toast('Copy failed: select the snippet and copy it by hand.', 'err');
		}
	}

	// `quiet` (nobody wrote to the bot for 24 h) is informational: neutral, never red. `down` is.
	const WEBHOOK_CHIP: Record<WebhookHealthStatus, string> = {
		ok: 'ok',
		degraded: 'warn',
		quiet: 'muted',
		down: 'err',
	};

	const DOWN_REASON: Record<WebhookDownReason, string> = {
		url_unset: 'no webhook URL registered',
		url_mismatch: 'webhook URL points elsewhere',
		webhook_error: 'Telegram cannot deliver to the webhook',
	};

	/** The webhook health line of a Telegram binding (B7 T15b). */
	function webhookDetail(w: BindingWebhook | null): string {
		if (!w) return 'No report from the channel edge yet.';
		const parts: string[] = [];
		if (w.down_reason) parts.push(DOWN_REASON[w.down_reason] ?? w.down_reason);
		parts.push(w.last_update_at ? `last update ${when(w.last_update_at)}` : 'no update seen');
		if (w.pending_update_count != null) parts.push(`${w.pending_update_count} pending`);
		if (w.last_error) parts.push(`last error ${w.last_error} (${when(w.last_error_at)})`);
		parts.push(`reported ${when(w.observed_at)}`);
		return parts.join(' · ');
	}

	function settingsText(b: ChannelBinding): string {
		const s = b.settings ?? {};
		if (b.kind === 'web_chat') {
			// The edge refuses every outside origin while the list is empty: only its hosted test page gets in.
			const o = s.allowed_origins?.length
				? `origins: ${s.allowed_origins.join(', ')}`
				: 'no sites yet — only the hosted test page';
			const t = s.turnstile?.mode;
			return t && t !== 'off' ? `${o} · Turnstile: ${t === 'own' ? 'own widget' : 'platform'}` : o;
		}
		if (b.side !== 'worker') return '';
		if (b.kind === 'helpdesk') return `tickets on ${s.base_url ?? '—'} · requester ${s.service_user ?? '—'}`;
		return '';
	}
</script>

<PageHead
	id="J21"
	title="Channel bindings"
	sub="Where customers reach this job, and where your staff are told about handovers and answer them in thread."
>
	{#snippet actions()}
		<a class="btn" href="{job}/conversations">Conversations</a>
		<a class="btn" href="{job}/handover">Handover queue</a>
	{/snippet}
</PageHead>

<LiveNote
	what="Bindings act on live traffic: a new binding routes that channel's messages to this job at once."
/>

{#if quota}
	<p class="muted small" data-testid="attachment-quota">
		Photo and PDF storage: {sizeLabel(quota.used_bytes)} used of {sizeLabel(quota.quota_bytes)}{quota.custom
			? ''
			: ' (the platform default)'}. Your platform administrator sets this limit.
	</p>
{/if}

<LoadState resource={res} empty="No channels bound yet. Add one below.">
	{#snippet children(page)}
		<Card cls="flush">
			<div class="tbl-wrap">
				<table class="tbl" data-testid="bindings">
					<thead>
						<tr
							><th>Channel</th><th>Side</th><th>Bound to</th><th>State</th><th aria-label="Binding actions"
							></th></tr
						>
					</thead>
					<tbody>
						{#each page.items as b (b.id)}
							<tr data-testid="binding-row" data-id={b.id} data-kind={b.kind} data-enabled={b.enabled}>
								<td><b>{b.label}</b><span class="cell-sub">{BINDING_KIND_LABEL[b.kind] ?? b.kind}</span></td>
								<td data-label="Side"
									><span class="chip {b.side === 'customer' ? 'cust' : 'work'}"
										>{b.side === 'customer' ? 'customers' : 'staff'}</span
									></td
								>
								<td data-label="Bound to"
									><span class="mono cb-id">{b.external_id}</span><span class="cell-sub"
										>{settingsText(b)}</span
									></td
								>
								<td data-label="State">
									<span class="chip {b.enabled ? 'ok' : 'muted'}">{b.enabled ? 'enabled' : 'disabled'}</span>
									{#if b.side === 'customer'}<span
											class="chip {b.env === 'prod' ? 'ok' : 'warn'}"
											data-testid="binding-env"
											data-env={b.env}
											title={b.env === 'prod'
												? 'Live: customers get the released job.'
												: 'Test: answers with the draft design until published.'}
											>{b.env === 'prod' ? 'Live' : 'Test'}</span
										>{/if}
									{#if !b.verified}<span class="chip warn" data-testid="binding-unverified">not verified</span
										>{/if}
									{#if b.kind === 'telegram'}
										{@const w = b.webhook ?? null}
										<span
											class="chip {w ? WEBHOOK_CHIP[w.status] : 'muted'}"
											data-testid="binding-webhook"
											data-status={w?.status ?? 'unknown'}>webhook {w?.status ?? 'unknown'}</span
										>
										<span class="cell-sub" data-testid="binding-webhook-detail">{webhookDetail(w)}</span>
									{/if}
									<span class="cell-sub" data-testid="binding-credential"
										>{b.has_credential
											? `own credential ${b.credential_fpr ?? ''}`.trim()
											: 'platform credential'}</span
									>
								</td>
								<td class="r cb-act">
									{#if b.kind === 'web_chat'}
										<button
											class="btn sm"
											type="button"
											data-testid="binding-snippet-toggle"
											aria-expanded={shown === b.id}
											onclick={() => togglePanel(b)}>Widget</button
										>
									{:else}
										<button
											class="btn sm"
											type="button"
											data-testid="binding-edit"
											aria-expanded={gEditing === b.id}
											onclick={() => openGeneric(b)}>Edit</button
										>
									{/if}
									{#if b.kind === 'telegram'}
										<button
											class="btn sm"
											type="button"
											data-testid="binding-test-message"
											disabled={!may || testing === b.id}
											title={hint}
											onclick={() => sendTest(b)}
											>{testing === b.id ? 'Sending…' : 'Send test message'}</button
										>
									{/if}
									{#if b.side === 'customer'}
										<button
											class="btn sm"
											type="button"
											data-testid={b.env === 'prod' ? 'binding-unpublish' : 'binding-publish'}
											disabled={pub.disabled || busy === b.id}
											title={pub.title}
											onclick={() => setLive(b, b.env !== 'prod')}
											>{b.env === 'prod' ? 'Unpublish' : 'Publish'}</button
										>
									{/if}
									<button
										class="btn sm"
										type="button"
										data-testid="binding-toggle"
										disabled={!may || busy === b.id}
										title={hint}
										onclick={() => toggle(b)}>{b.enabled ? 'Disable' : 'Enable'}</button
									>
									{#if confirming === b.id}
										<span class="cb-confirm" role="group" aria-label={`Delete “${b.label}”?`}>
											<button
												class="btn sm danger"
												type="button"
												data-testid="binding-delete-confirm"
												disabled={!may || busy === b.id}
												onclick={() => remove(b)}>Delete binding</button
											>
											<button class="btn sm" type="button" onclick={() => (confirming = null)}>Cancel</button>
										</span>
									{:else}
										<button
											class="btn sm danger"
											type="button"
											data-testid="binding-delete"
											disabled={!may || busy === b.id}
											title={hint}
											onclick={() => (confirming = b.id)}>Delete…</button
										>
									{/if}
								</td>
							</tr>
							{#if testResult?.id === b.id || testError?.id === b.id}
								<tr class="cb-verify-row">
									<td colspan="5" data-testid="binding-test-result">
										{#if testError?.id === b.id}
											<span role="alert"
												><span class="mono">{testError.code}</span> — {testError.message}</span
											>
										{:else if testResult}
											Test message {testResult.delivered ? 'delivered' : 'not delivered'}: “{testResult.text}”
										{/if}
									</td>
								</tr>
							{/if}
							{#if gEditing === b.id}
								<tr class="cb-snippet-row">
									<td colspan="5" data-testid="generic-panel">
										<form
											class="cb-widget"
											data-testid="generic-edit-form"
											onsubmit={(e) => {
												e.preventDefault();
												void saveGeneric(b);
											}}
										>
											<label class="field"
												>Label
												<input
													class="input"
													bind:value={gLabel}
													maxlength="120"
													data-testid="edit-label"
													disabled={!may}
												/>
											</label>
											{#if b.kind === 'telegram'}
												<label class="cb-check"
													><input
														type="checkbox"
														bind:checked={gAttCustomer}
														data-testid="edit-attach-customer"
														disabled={!may}
													/> Customers can send photos and PDFs</label
												>
												<label class="cb-check"
													><input
														type="checkbox"
														bind:checked={gAttStaff}
														data-testid="edit-attach-staff"
														disabled={!may}
													/> Staff can send photos and PDFs</label
												>
											{/if}
											{#each settingFields(b) as f (f.key)}
												<label class="field"
													>{f.label}
													{#if f.area}
														<textarea
															class="input mono"
															rows="2"
															bind:value={gF[f.key]}
															data-testid="edit-{f.key}"
															disabled={!may}></textarea>
													{:else}
														<input
															class="input mono"
															placeholder={f.placeholder}
															bind:value={gF[f.key]}
															data-testid="edit-{f.key}"
															disabled={!may}
														/>
													{/if}
												</label>
											{/each}
											{#if CREDENTIAL_FIELDS[b.kind].length}
												<fieldset class="cb-cred" data-testid="edit-credential">
													<legend
														>Replace credential{b.has_credential
															? ' — one is set; fill in the fields to replace it, leave empty to keep it'
															: ' — none set'}</legend
													>
													{#each CREDENTIAL_FIELDS[b.kind] as f (f.key)}
														<label class="field"
															>{f.label}
															<input
																class="input"
																type="password"
																autocomplete="new-password"
																value={gCred[f.key] ?? ''}
																oninput={(e) => (gCred = { ...gCred, [f.key]: e.currentTarget.value })}
																data-testid="edit-cred-{f.key}"
																disabled={!may}
															/>
														</label>
													{/each}
												</fieldset>
											{/if}
											{#if gError}
												<p class="cb-unavailable small" role="alert" data-testid="edit-error">
													<span class="mono">{gError.code}</span> — {gError.message}
												</p>
											{/if}
											<div class="row">
												<button
													class="btn primary sm"
													type="submit"
													data-testid="edit-save"
													disabled={!may || gBusy || !gLabel.trim()}
													title={hint}>{gBusy ? 'Saving…' : 'Save'}</button
												>
												<button class="btn sm" type="button" onclick={() => (gEditing = null)}>Cancel</button>
											</div>
										</form>
									</td>
								</tr>
							{/if}
							{#if b.kind === 'web_chat' && shown === b.id}
								<tr class="cb-snippet-row">
									<td colspan="5" data-testid="binding-panel">
										<p class="muted small">
											{#if b.settings?.allowed_origins?.length}
												Paste this before <code>&lt;/body&gt;</code> on {b.settings.allowed_origins.join(
													', ',
												)}.
											{:else}
												No sites yet — only the hosted test page can open this chat. Add your website's origin
												to embed it, then paste this before <code>&lt;/body&gt;</code>.
											{/if}
										</p>
										<pre class="cb-snippet" data-testid="binding-snippet">{webChatSnippet(b)}</pre>
										<button class="btn sm" type="button" onclick={() => copy(webChatSnippet(b))}>Copy</button>
										{#if webChatDemoUrl(b)}
											<a
												class="btn sm"
												href={webChatDemoUrl(b)}
												target="_blank"
												rel="noopener"
												data-testid="webchat-try">Try it</a
											>
											<span class="muted small">Opens the hosted test page with this widget.</span>
										{:else}
											<span class="muted small" data-testid="webchat-try-off"
												>Enable this binding to try it on the hosted test page.</span
											>
										{/if}
										{#if b.env === 'test'}
											{#if b.settings?.identity?.mode === 'hs256' && b.has_credential}
												<form
													class="row"
													data-testid="webchat-try-as-form"
													onsubmit={(e) => {
														e.preventDefault();
														void tryAsCustomer(b);
													}}
												>
													<label class="field"
														>Customer reference from your system
														<input
															class="input"
															bind:value={tryRef}
															maxlength="200"
															autocomplete="off"
															data-testid="webchat-try-customer"
															disabled={!may}
														/>
													</label>
													<button
														class="btn sm"
														type="submit"
														data-testid="webchat-try-as"
														disabled={!may || !tryRef.trim() || tryBusy === b.id}>Try as this customer</button
													>
													<span class="muted small"
														>Opens the test page signed in as that customer, as your site would.</span
													>
													{#if tryError?.id === b.id}
														<span class="small" role="alert" data-testid="webchat-try-as-error"
															>{tryError.message}</span
														>
													{/if}
												</form>
											{:else}
												<p class="muted small" data-testid="webchat-try-anon-note">
													The test page opens as an anonymous visitor. To try this channel as one of your
													customers, set customer identity to HS256 and make its secret first.
												</p>
											{/if}
										{/if}

										<form
											class="cb-widget"
											data-testid="widget-form"
											onsubmit={(e) => {
												e.preventDefault();
												void savePanel(b);
											}}
										>
											<fieldset class="cb-cred">
												<legend>What the widget shows</legend>
												<label class="field"
													>Title ({display.title.length}/{WEBCHAT_TITLE_MAX})
													<input
														class="input"
														bind:value={display.title}
														maxlength={WEBCHAT_TITLE_MAX}
														placeholder="Chat with us"
														data-testid="widget-title"
														disabled={!may}
													/>
												</label>
												<label class="field"
													>Greeting ({display.greeting.length}/{WEBCHAT_GREETING_MAX})
													<textarea
														class="input"
														rows="2"
														bind:value={display.greeting}
														maxlength={WEBCHAT_GREETING_MAX}
														placeholder="Hi! How can we help?"
														data-testid="widget-greeting"
														disabled={!may}></textarea>
												</label>
												<label class="field"
													>Colour (#rrggbb)
													<span class="row">
														<input
															class="input mono cb-colour"
															bind:value={display.colour}
															placeholder="#1f6feb"
															data-testid="widget-colour"
															disabled={!may}
														/>
														<input
															type="color"
															aria-label="Pick the colour"
															value={WEBCHAT_COLOUR_RE.test(display.colour) ? display.colour : '#1f6feb'}
															oninput={(e) => (display.colour = e.currentTarget.value)}
															disabled={!may}
														/>
													</span>
												</label>
											</fieldset>
											<label class="field"
												>Websites that may show this chat (one origin per line)
												<textarea
													class="input mono"
													rows="2"
													bind:value={wOrigins}
													placeholder="https://shop.example.com"
													data-testid="widget-origins"
													disabled={!may}></textarea>
											</label>
											<label class="cb-check"
												><input
													type="checkbox"
													bind:checked={gAttCustomer}
													data-testid="widget-attach-customer"
													disabled={!may}
												/> Customers can send photos and PDFs</label
											>
											<label class="cb-check"
												><input
													type="checkbox"
													bind:checked={gAttStaff}
													data-testid="widget-attach-staff"
													disabled={!may}
												/> Staff can send photos and PDFs</label
											>
											<label class="cb-check"
												><input
													type="checkbox"
													bind:checked={csat}
													data-testid="widget-csat"
													disabled={!may}
												/> Ask the customer for a rating (1–5) when a case closes</label
											>
											<fieldset class="cb-cred" data-testid="widget-turnstile">
												<legend>Bot check before a chat starts (Cloudflare Turnstile)</legend>
												<label class="field"
													>Turnstile
													<select
														class="input"
														bind:value={tsMode}
														data-testid="widget-turnstile-mode"
														disabled={!may}
													>
														{#each TURNSTILE_MODES as m (m.mode)}<option value={m.mode}>{m.label}</option
															>{/each}
													</select>
												</label>
												{#if tsMode === 'platform'}
													<p class="muted small">
														The platform's widget answers only on nonagentic.ai pages; on another site chats
														cannot start — use your own widget there.
													</p>
												{:else if tsMode === 'own'}
													<label class="field"
														>Site key
														<input
															class="input mono"
															bind:value={tsSiteKey}
															placeholder="0x4AAAAAAA…"
															data-testid="widget-turnstile-site-key"
															disabled={!may}
														/>
													</label>
													<label class="field"
														>Secret key {tsHasSecret ? '(set — leave empty to keep it)' : ''}
														<input
															class="input"
															type="password"
															autocomplete="new-password"
															bind:value={tsSecret}
															data-testid="widget-turnstile-secret"
															disabled={!may}
														/>
													</label>
												{/if}
											</fieldset>
											<fieldset class="cb-cred" data-testid="widget-identity">
												<legend>Known customers (sign-in proof from your site)</legend>
												<p class="muted small">
													When your site already knows who is chatting, it can hand the chat a signed proof.
													The chat then greets the customer by name and never asks who they are.
												</p>
												<label class="field"
													>Identity
													<select
														class="input"
														bind:value={idMode}
														data-testid="widget-identity-mode"
														disabled={!may}
													>
														{#each IDENTITY_MODES as m (m.mode)}<option value={m.mode}>{m.label}</option
															>{/each}
													</select>
												</label>
												{#if idMode !== 'off'}
													<label class="field"
														>Issuer
														<input
															class="input"
															bind:value={idIssuer}
															placeholder="https://shop.example"
															data-testid="widget-identity-issuer"
															disabled={!may}
														/>
													</label>
													<label class="field"
														>Sign-in URL (where a visitor who is not signed in goes; optional)
														<input
															class="input"
															bind:value={idSignIn}
															placeholder="https://shop.example/login"
															data-testid="widget-identity-sign-in"
															disabled={!may}
														/>
													</label>
													<label class="field"
														>Session hours (1–24; empty = {IDENTITY_DEFAULT_HOURS})
														<input
															class="input cb-num"
															type="number"
															min="1"
															max="24"
															step="1"
															bind:value={idHours}
															placeholder={String(IDENTITY_DEFAULT_HOURS)}
															data-testid="widget-identity-hours"
															disabled={!may}
														/>
													</label>
												{/if}
												{#if idMode === 'es256'}
													<label class="field"
														>Public keys (JWK, one or two, each with a "kid")
														<textarea
															class="input mono"
															rows="6"
															bind:value={idJwks}
															placeholder={'{"kty":"EC","crv":"P-256","kid":"2026-1","x":"…","y":"…"}'}
															data-testid="widget-identity-jwks"
															disabled={!may}></textarea>
													</label>
												{/if}
												{#if idMode === 'hs256'}
													<div class="cb-secret" data-testid="widget-identity-secret-box">
														{#if b.settings?.identity?.mode !== 'hs256'}
															<p class="muted small">
																Save the widget with HS256 first, then make the secret.
															</p>
														{/if}
														<label class="cb-check"
															><input
																type="checkbox"
																bind:checked={idRevoke}
																data-testid="widget-identity-revoke"
																disabled={!may}
															/> Also sign out open identified chats</label
														>
														<button
															class="btn sm"
															type="button"
															data-testid="widget-identity-secret-make"
															disabled={!may || busy === b.id || b.settings?.identity?.mode !== 'hs256'}
															title={hint}
															onclick={() => makeSecret(b)}
															>{b.has_credential || idSecret?.id === b.id
																? 'Rotate secret'
																: 'Generate secret'}</button
														>
														<span class="muted small"
															>The old secret keeps working for 24 hours, so you can switch your server over.</span
														>
														{#if idSecret && idSecret.id === b.id}
															<div class="cb-once" role="alert" data-testid="widget-identity-secret-once">
																<p class="small">
																	<strong>Copy this secret now.</strong> It is shown only once and cannot be read
																	again. Put it in your server, never in a web page.
																</p>
																<code class="mono" data-testid="widget-identity-secret-value"
																	>{idSecret.secret}</code
																>
																<span class="muted small">Key id {idSecret.kid}</span>
																<span class="row">
																	<button
																		class="btn sm"
																		type="button"
																		data-testid="widget-identity-secret-copy"
																		onclick={() => idSecret && copy(idSecret.secret, 'Secret')}>Copy</button
																	>
																	<button
																		class="btn sm"
																		type="button"
																		data-testid="widget-identity-secret-done"
																		onclick={() => (idSecret = null)}>I have saved it</button
																	>
																</span>
															</div>
														{/if}
													</div>
												{/if}
											</fieldset>
											<fieldset class="cb-cred">
												<legend>Limits ({WEBCHAT_LIMIT_MIN}–{WEBCHAT_LIMIT_MAX}; empty = the default)</legend>
												{#each WEBCHAT_LIMITS as l (l.key)}
													<label class="field"
														>{l.label}
														<input
															class="input cb-num"
															type="number"
															min={WEBCHAT_LIMIT_MIN}
															max={WEBCHAT_LIMIT_MAX}
															step="1"
															placeholder={String(WEBCHAT_LIMIT_DEFAULTS[l.key])}
															value={limits[l.key]}
															oninput={(e) => (limits[l.key] = e.currentTarget.value)}
															data-testid="widget-limit-{l.key}"
															disabled={!may}
														/>
													</label>
												{/each}
											</fieldset>
											<div class="row">
												<button
													class="btn primary sm"
													type="submit"
													data-testid="widget-save"
													disabled={!may || busy === b.id || !!panelError}
													title={hint ?? panelError ?? undefined}>Save widget</button
												>
												{#if may && panelError}<span class="muted small" role="alert">{panelError}</span>{/if}
											</div>
										</form>

										<div class="cb-rotate" data-testid="widget-rotate">
											{#if rotating === b.id}
												<p class="small" role="alert">
													A new key ends every open chat on this widget, and the snippet on your site stops
													working until you replace it with the new one.
												</p>
												<span class="cb-confirm" role="group" aria-label={`Rotate the key of “${b.label}”?`}>
													<button
														class="btn sm danger"
														type="button"
														data-testid="widget-rotate-confirm"
														disabled={!may || busy === b.id}
														onclick={() => rotate(b)}>Rotate key and end open chats</button
													>
													<button class="btn sm" type="button" onclick={() => (rotating = null)}
														>Cancel</button
													>
												</span>
											{:else}
												<button
													class="btn sm"
													type="button"
													data-testid="widget-rotate-start"
													disabled={!may || busy === b.id}
													title={hint}
													onclick={() => (rotating = b.id)}>Rotate key…</button
												>
												<span class="muted small">Use it when the key leaked or the widget is abused.</span>
											{/if}
										</div>
									</td>
								</tr>
							{/if}
						{/each}
					</tbody>
				</table>
			</div>
		</Card>
	{/snippet}
</LoadState>

<section class="cb-add" data-testid="binding-add" aria-label="Add a channel binding">
	<h2>Add a binding</h2>
	{#if !may}<p class="muted small" data-testid="bindings-role-note">
			{CHANNELS_HINT}
		</p>{/if}
	<form
		class="cb-form"
		onsubmit={(e) => {
			e.preventDefault();
			void add();
		}}
	>
		<label class="field"
			>Channel
			<select class="input" bind:value={kind} data-testid="binding-kind" disabled={!may}>
				{#each BINDING_KINDS as k (k)}<option value={k}>{BINDING_KIND_LABEL[k]}</option>{/each}
			</select>
		</label>
		<label class="field"
			>Side
			<select class="input" bind:value={side} data-testid="binding-side" disabled={!may}>
				{#each sides as s (s)}<option value={s}
						>{s === 'customer' ? 'Customers reach the job here' : 'Staff get handovers here'}</option
					>{/each}
			</select>
		</label>
		<label class="field"
			>Label
			<input class="input" bind:value={label} data-testid="binding-label" maxlength="120" disabled={!may} />
		</label>
		{#if idLabel}
			<label class="field"
				>{idLabel}
				<input class="input mono" bind:value={externalId} data-testid="binding-external-id" disabled={!may} />
			</label>
		{:else}
			<p class="muted small">The web chat key (<code>wk_…</code>) is generated when you add the binding.</p>
		{/if}

		{#if kind === 'web_chat'}
			<label class="field"
				>Allowed origins (one per line; empty = any)
				<textarea
					class="input mono"
					rows="2"
					placeholder="https://shop.example.com"
					bind:value={origins}
					data-testid="binding-origins"
					disabled={!may}></textarea>
			</label>
		{:else if side === 'worker' && kind === 'helpdesk'}
			<label class="field"
				>Helpdesk URL
				<input
					class="input mono"
					placeholder="http://frappe.example.com:8000"
					bind:value={hdBaseUrl}
					data-testid="binding-hd-base-url"
					disabled={!may}
				/>
			</label>
			<label class="field"
				>Service user (ticket requester when the customer has no email)
				<input
					class="input mono"
					placeholder="agent-bot@helpdesk.example.com"
					bind:value={hdServiceUser}
					data-testid="binding-hd-service-user"
					disabled={!may}
				/>
			</label>
		{/if}

		{#if CREDENTIAL_FIELDS[kind].length}{:else if CREDENTIAL_FIELDS[kind].length}
			<fieldset class="cb-cred">
				<legend
					>{credentialRequired(kind)
						? 'Credential (required)'
						: "Own credential (optional — leave empty to use the platform's)"}</legend
				>
				{#each CREDENTIAL_FIELDS[kind] as f (f.key)}
					<label class="field"
						>{f.label}
						<input
							class="input"
							type="password"
							autocomplete="new-password"
							value={cred[f.key] ?? ''}
							oninput={(e) => (cred = { ...cred, [f.key]: e.currentTarget.value })}
							data-testid="binding-cred-{f.key}"
							disabled={!may}
						/>
					</label>
				{/each}
			</fieldset>
		{/if}

		<div class="row">
			<button
				class="btn primary"
				type="submit"
				data-testid="binding-create"
				disabled={!may || saving || !!missing}
				title={hint ?? missing ?? undefined}>Add binding</button
			>
			{#if may && missing}<span class="muted small">{missing}</span>{/if}
		</div>
		{#if unavailable}<p class="cb-unavailable small" role="alert" data-testid="binding-unavailable">
				{BINDING_UNAVAILABLE}
			</p>{/if}
	</form>
</section>

<style>
	.cb-act {
		white-space: nowrap;
	}
	.cb-act :global(.btn) {
		margin-left: 4px;
	}
	.cb-confirm {
		display: inline-flex;
		gap: 4px;
	}
	.cb-id {
		overflow-wrap: anywhere;
	}
	.cb-snippet {
		white-space: pre-wrap;
		overflow-wrap: anywhere;
		padding: 8px 10px;
		border-radius: 6px;
		background: color-mix(in srgb, var(--muted, #888) 10%, transparent);
		font-size: 0.85em;
	}
	.cb-widget {
		display: grid;
		gap: 10px;
		max-width: 640px;
		margin-top: 12px;
	}
	.cb-check {
		display: flex;
		gap: 6px;
		align-items: center;
	}
	.cb-colour {
		max-width: 10ch;
	}
	.cb-num {
		max-width: 12ch;
	}
	.cb-secret {
		display: grid;
		gap: 6px;
		justify-items: start;
	}
	.cb-once {
		display: grid;
		gap: 6px;
		justify-items: start;
		padding: 8px 10px;
		border: 1px solid var(--warn, #c80);
		border-radius: 6px;
		background: color-mix(in srgb, var(--warn, #c80) 8%, transparent);
	}
	.cb-once code {
		word-break: break-all;
		user-select: all;
	}
	.cb-rotate {
		margin-top: 12px;
		display: grid;
		gap: 6px;
		justify-items: start;
	}
	.cb-verify-row td {
		font-size: 0.9em;
		background: color-mix(in srgb, var(--warn, #c80) 8%, transparent);
	}
	.cb-unavailable {
		color: var(--err, #b00);
	}
	.cb-add {
		margin-top: 18px;
	}
	.cb-form {
		display: grid;
		gap: 10px;
		max-width: 640px;
	}
	.cb-cred {
		display: grid;
		gap: 8px;
		border: 1px solid var(--line, #ddd);
		border-radius: 8px;
		padding: 8px 12px;
	}
	@media (max-width: 640px) {
		.cb-act {
			white-space: normal;
		}
	}
</style>
