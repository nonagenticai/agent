<script lang="ts">
	// LLM providers (06 §7, D38): every provider is base URL + model(s) + token. D70: every GPU service is external —
	// a customer tenant picks a provider, enters its URL and its own token (write-only, masked after save); there is
	// no licence-minted fallback. Only nonagentic's own tenant runs internally: the pgx vLLM for its LLM, mlx_api for the rest (read-only here).
	// Ported from care-bus-agents #27/#28: a provider has a kind (OpenAI-compatible or Laya), the drawer tests its
	// unsaved values (the typed key is sent once, never stored) and says which uses it serves; only Laya serves gates.
	// D69: the provider records (endpoint, key, kind) are tenant environment resources and save live. Which provider
	// serves which use is design: a binding is saved to the draft of every job, tried in the test chat, and reaches a
	// job's Production when that job publishes (TenantDraftBar shows draft vs live per job).
	import { onMount } from 'svelte';
	import { api, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { canWrite as canWriteFor, writeHint, writeAttrs } from '$lib/auth/roles';
	import { Resource } from '$lib/load.svelte';
	import { ApiError } from '$lib/api/errors';
	import { toast } from '$lib/ui/toaster.svelte';
	import {
		LLM_KIND_LABEL,
		LLM_PRESETS,
		LLM_USE_RULES,
		LLM_USES,
		llmUseProblem,
		validVoice,
		type LlmKind,
		type LlmProvider,
		type LlmProviders,
		type LlmProviderTestResult,
		type LlmUse,
	} from '$lib/api/types/tenant';
	import { secretText, when } from '$lib/tenant/ui';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import LiveNote from '$lib/release/LiveNote.svelte';
	import TenantDraftBar from '$lib/release/TenantDraftBar.svelte';
	import {
		FUNDING,
		INTERNAL_COPY,
		providerFunding,
		providerRegion,
		unboundUses,
		useForInitial,
		useForPlan,
		type UseForChoice,
	} from './logic';

	let { onchange = () => {} }: { onchange?: () => void } = $props();

	let tick = $state(0);
	const res = new Resource<LlmProviders>(
		() => api().llmProviders(),
		() => false,
	);
	onMount(() => res.refresh());
	const canWrite = $derived(canWriteFor(session, 'integrations_manager'));

	let open = $state(false);
	let editing = $state<LlmProvider | null>(null);
	let name = $state('');
	let preset = $state('custom');
	/** A new provider's values as the drawer opened (a preset fills some in): dirty = changed since. */
	let start = $state({ name: '', baseUrl: '', models: '' });
	let kind = $state<LlmKind>('openai');
	let baseUrl = $state('');
	let models = $state('');
	let secret = $state('');
	let clearKey = $state(false);
	let failure = $state<{ code: string; message: string } | null>(null);
	let busy = $state(false);
	let testing = $state('');
	let confirmDelete = $state(false);
	let useFor = $state<Record<LlmUse, UseForChoice>>(useForInitial(null, null));
	let draftTest = $state<LlmProviderTestResult | null>(null);
	let draftTestError = $state('');
	let draftTesting = $state(false);
	const modelList = $derived(
		models
			.split(',')
			.map((m) => m.trim())
			.filter(Boolean),
	);
	/** The edited provider is one of the internal tenant's (pgx vLLM): managed by the platform, shown read-only. */
	const managed = $derived(editing?.token_mode === 'internal');
	/** Nothing in the drawer can change: no test-before-save, no Save, no "applied on Save" wording. */
	const readOnly = $derived(managed || !canWrite);
	const dirty = $derived(
		open &&
			(editing
				? name !== editing.name ||
					kind !== editing.kind ||
					baseUrl !== editing.base_url ||
					models !== editing.models.join(', ') ||
					!!secret ||
					clearKey
				: name !== start.name || baseUrl !== start.baseUrl || models !== start.models || !!secret),
	);
	// A test result describes the values it ran with; any edit to them makes it stale.
	$effect(() => {
		void [kind, baseUrl, secret, clearKey];
		draftTest = null;
		draftTestError = '';
	});

	function edit(p: LlmProvider | null, want: LlmKind = 'openai') {
		editing = p;
		// "Add provider" starts blank on the Provider select; "Add a Laya provider" starts from the Laya preset.
		preset = !p && want === 'laya' ? 'laya' : 'custom';
		name = p?.name ?? '';
		kind = p?.kind ?? want;
		baseUrl = p?.base_url ?? '';
		models = p?.models.join(', ') ?? '';
		secret = '';
		clearKey = false;
		failure = null;
		confirmDelete = false;
		useFor = useForInitial(res.value.state === 'ok' ? res.value.data : null, p?.id ?? null);
		if (!p) {
			for (const u of LLM_USES) useFor[u].on = !llmUseProblem(u, want) && want === 'laya';
			applyPreset(preset);
		}
		start = { name, baseUrl, models };
		open = true;
	}
	/** A new provider starts from a preset: its dialect, endpoint and models are filled in, the token is typed. */
	function applyPreset(key: string) {
		preset = key;
		const p = LLM_PRESETS.find((x) => x.key === key);
		if (!p || key === 'custom') return;
		kind = p.kind;
		baseUrl = p.base_url;
		models = p.models.join(', ');
		if (!name.trim() || LLM_PRESETS.some((x) => x.label === name)) name = p.label;
	}
	/** `POST /api/providers/test` with the drawer's values: nothing is saved, the typed key is sent once. */
	async function testDraft() {
		draftTesting = true;
		draftTest = null;
		draftTestError = '';
		try {
			draftTest = await api().testLlmProviderDraft({
				kind,
				base_url: baseUrl,
				secret: clearKey ? null : secret || undefined,
				provider_id: editing && !clearKey && !secret ? editing.id : undefined,
			});
		} catch (e) {
			draftTestError = e instanceof ApiError ? `${e.message} (${e.code})` : String(e);
		} finally {
			draftTesting = false;
		}
	}
	function useTestedModels() {
		if (draftTest?.models.length) models = draftTest.models.join(', ');
	}
	async function save() {
		busy = true;
		failure = null;
		const input = {
			name,
			kind,
			base_url: baseUrl,
			models: models
				.split(',')
				.map((m) => m.trim())
				.filter(Boolean),
			secret: clearKey ? null : secret || undefined,
		};
		const before = res.value.state === 'ok' ? res.value.data : null;
		try {
			const saved = editing
				? await api().updateLlmProvider(editing.id, input)
				: await api().createLlmProvider(input);
			// "Use for": bind or unbind each use this save changes, after the provider exists.
			let boundNow = false;
			for (const use of LLM_USES) {
				const plan = useForPlan(before, editing?.id ?? null, use, useFor[use], saved);
				if (plan) {
					await api().bindLlmUse(use, plan.providerId, plan.model);
					boundNow = true;
				}
			}
			open = false;
			toast(
				`${editing ? `Saved ${name}` : `Added ${name}`}.${boundNow ? ' Saved to draft: the use binding — test it, then publish.' : ''}`,
			);
			tick++;
			await res.refresh();
			onchange();
		} catch (e) {
			failure =
				e instanceof ApiError ? { code: e.code, message: e.message } : { code: 'error', message: String(e) };
		} finally {
			busy = false;
		}
	}
	async function remove() {
		if (!editing) return;
		const p = editing;
		const ok = await guard(() => api().deleteLlmProvider(p.id), `Removed ${p.name}.`, 'Not removed');
		if (ok) open = false;
		confirmDelete = false;
		await res.refresh();
		onchange();
	}
	async function test(p: LlmProvider) {
		testing = p.id;
		await guard(
			async () => {
				const r = await api().testLlmProvider(p.id);
				if (!r.ok) throw new Error(r.detail);
			},
			`${p.name}: test passed.`,
			'Test failed',
		);
		testing = '';
		await res.refresh();
		onchange();
	}
	/** The uses table's unsaved picks, per use. */
	let sel = $state<Record<LlmUse, { provider: string; model: string; voice: string }>>(
		Object.fromEntries(LLM_USES.map((u) => [u, { provider: '', model: '', voice: '' }])) as Record<
			LlmUse,
			{ provider: string; model: string; voice: string }
		>,
	);
	$effect(() => {
		if (res.value.state !== 'ok') return;
		for (const u of res.value.data.uses)
			sel[u.use] = { provider: u.provider_id ?? '', model: u.model ?? '', voice: u.voice ?? '' };
	});
	const modelsOf = (v: LlmProviders, id: string): string[] =>
		v.providers.find((p) => p.id === id)?.models ?? [];
	/** Why the picked provider cannot serve `use` (shown under the select), or null. */
	const pickProblem = (v: LlmProviders, use: LlmUse): string | null => {
		const p = v.providers.find((x) => x.id === sel[use].provider);
		return p ? llmUseProblem(use, p.kind) : null;
	};
	async function bindUse(
		use: LlmUse,
		providerId: string | null,
		model: string | null,
		voice: string | null = null,
	) {
		const label = LLM_USE_RULES[use].label;
		await guard(
			() =>
				use === 'tts' && providerId && voice
					? api().bindLlmUse(use, providerId, model, voice)
					: api().bindLlmUse(use, providerId, providerId ? model : null),
			providerId
				? `Saved to draft: ${label} uses this provider. Test it, then publish.`
				: `Saved to draft: ${label} unbound (${LLM_USE_RULES[use].fallback}). Test it, then publish.`,
			'Not bound',
		);
		tick++;
		await res.refresh();
		onchange();
	}
	function bind(v: LlmProviders, use: LlmUse) {
		const id = sel[use].provider;
		const ms = modelsOf(v, id);
		return bindUse(
			use,
			id || null,
			id ? (ms.includes(sel[use].model) ? sel[use].model : (ms[0] ?? null)) : null,
			use === 'tts' ? sel[use].voice.trim() || null : null,
		);
	}
	const bound = (v: LlmProviders, id: string): string =>
		v.uses
			.filter((u) => u.provider_id === id)
			.map((u) => u.label.toLowerCase())
			.join(', ');
	/** The drawer's "Use for" reason a use is off-limits for the kind being edited. */
	const useForProblem = (use: LlmUse): string | null => llmUseProblem(use, kind);
	/** Why a typed tts voice would be refused (422 `invalid_voice`), or null. */
	const voiceProblem = (v: string): string | null =>
		!v.trim() || validVoice(v.trim()) ? null : 'A voice is 1–64 letters, digits, - or _.';
	const kindsText = (use: LlmUse) => LLM_USE_RULES[use].kinds.map((k) => LLM_KIND_LABEL[k]).join(' or ');
	const tokenChip = (p: LlmProvider): { text: string; variant: 'ok' | 'err' | 'accent' } =>
		p.token_mode === 'internal'
			? { text: 'platform service key', variant: 'accent' }
			: p.secret.set
				? { text: `token ${p.secret.fingerprint}`, variant: 'ok' }
				: { text: 'token required', variant: 'err' };
</script>

<LoadState resource={res}>
	{#snippet children(v)}
		{#if v.warning?.code === 'key_required'}
			<Callout kind="gap" title="Key required"
				><span>{v.warning.message} The release readiness lists this too (not a block).</span></Callout
			>
		{/if}
		{#if v.live_problems?.length}
			<Callout kind="err" title="Live bindings that cannot serve"
				><span data-testid="llm-live-problems"
					>{#each v.live_problems as p, i (`${p.job}:${p.use}`)}{#if i}{'; '}{/if}<b>{p.job}</b> · {LLM_USE_RULES[
							p.use
						]?.label ?? p.use}: {p.message} ({p.code}){/each}. Bind a provider that can serve, then publish
					the job.</span
				></Callout
			>
		{/if}
		{#if v.hosting === 'internal'}
			<Callout kind="info" title="nonagentic's own tenant"
				><span data-testid="llm-internal"
					>The LLM runs on the pgx vLLM, every other GPU capability on mlx_api, both managed by the platform.
					Customer tenants bind their own external providers with a URL and a token.</span
				></Callout
			>
		{/if}
		{#each unboundUses(v) as u (u.use)}
			<div class="callout gap bind-callout" data-testid="bind-callout-{u.use}">
				<strong>{u.label}: not bound</strong>
				<span>{u.text}</span>
				<span class="bind-act">
					{#if u.candidate}
						<button
							class="btn sm primary"
							type="button"
							{...writeAttrs(session, 'integrations_manager')}
							onclick={() => bindUse(u.use, u.candidate!.id, u.candidate!.model)}
							>Bind {u.candidate.name}{u.candidate.model ? ` · ${u.candidate.model}` : ''}</button
						>
					{:else if v.hosting === 'internal'}
						<span class="muted">The internal providers serve no {LLM_KIND_LABEL[u.kind]} endpoint yet.</span>
					{:else}
						<span class="muted"
							>No {LLM_KIND_LABEL[u.kind]} provider yet{u.use === 'gates'
								? ': only a Laya provider can serve gates'
								: ''}.</span
						>
						<button
							class="btn sm"
							type="button"
							{...writeAttrs(session, 'integrations_manager')}
							onclick={() => edit(null, u.kind)}>Add a {LLM_KIND_LABEL[u.kind]} provider</button
						>
					{/if}
				</span>
			</div>
		{/each}
		<LiveNote
			of="environment"
			what={v.hosting === 'internal'
				? 'The platform-managed providers are live for every job; which use a provider serves is saved to the draft (Agent uses).'
				: 'Provider endpoint, models and token are live for every job; which use a provider serves is saved to the draft (Agent uses).'}
		/>
		<Card
			cls="flush"
			title="Providers"
			sub={v.hosting === 'internal'
				? INTERNAL_COPY
				: 'A provider is a URL + the models it serves + your token. Every provider is external: the token is stored sealed and never shown again, only its fingerprint.'}
		>
			{#snippet actions()}
				{#if v.hosting !== 'internal'}
					<button
						class="btn primary sm"
						type="button"
						{...writeAttrs(session, 'integrations_manager')}
						onclick={() => edit(null)}>Add provider</button
					>
				{/if}
			{/snippet}
			{#if v.providers.length === 0}
				<div class="empty pad">
					{v.hosting === 'internal'
						? 'No platform-managed provider is registered for this tenant yet.'
						: 'No LLM provider yet. Add one: pick the provider, then enter its URL, models and your token.'}
				</div>
			{:else}
				<div class="tbl-wrap">
					<table class="tbl" data-testid="llm-providers">
						<thead>
							<tr
								><th>Provider</th><th>Endpoint · region</th><th>Funding · token</th><th>Models</th><th
									>Health</th
								><th aria-label="Provider actions"></th></tr
							>
						</thead>
						<tbody>
							{#each v.providers as p (p.id)}
								{@const tc = tokenChip(p)}
								{@const f = providerFunding(p)}
								<tr data-provider={p.id}>
									<td
										><b>{p.name}</b>
										<Chip text={LLM_KIND_LABEL[p.kind]} /><span class="cell-sub"
											>{bound(v, p.id) ? `serves ${bound(v, p.id)}` : 'not bound to a use'} · by {p.updated_by}</span
										></td
									>
									<td
										><span class="mono url">{p.base_url}</span><span class="cell-sub"
											>{providerRegion(p) ?? 'region not declared'}</span
										></td
									>
									<td
										><span class="chips"
											><Chip
												text={FUNDING[f].label}
												variant={FUNDING[f].variant}
												title={FUNDING[f].hint}
											/><Chip text={tc.text} variant={tc.variant} /></span
										></td
									>
									<td class="mono">{p.models.join(', ')}</td>
									<td>
										{#if p.last_test}
											<Chip text={p.last_test.ok ? 'ok' : 'failed'} variant={p.last_test.ok ? 'ok' : 'err'} />
											<span class="cell-sub">{p.last_test.detail}</span>
											<span class="cell-sub">{when(p.last_test.at)}</span>
										{:else}<span class="muted">never tested</span>{/if}
									</td>
									<td class="r">
										<span class="row-actions">
											<button
												class="btn sm"
												type="button"
												{...writeAttrs(session, 'integrations_manager')}
												disabled={!canWrite || testing === p.id}
												onclick={() => test(p)}>{testing === p.id ? 'Testing…' : 'Test'}</button
											>
											<button class="btn sm" type="button" onclick={() => edit(p)}
												>{canWrite && p.token_mode !== 'internal' ? 'Edit' : 'View'}</button
											>
										</span>
									</td>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
			{/if}
		</Card>

		<!-- B8: the generator UI links here (`/admin/providers#uses`) -->
		<div id="uses" data-testid="llm-uses-section">
			<Card
				title="Agent uses"
				sub="Which provider and model Agent calls for each step of a turn. Qualification and the generator need an OpenAI-compatible provider, gates a Laya one, speech (asr / tts) a w2v or OpenAI-compatible one. A binding is saved to the draft of every job: test it, then publish."
			>
				<TenantDraftBar
					kinds={['llm_use']}
					{tick}
					label={(r) => LLM_USE_RULES[r.ref.use as LlmUse]?.label ?? r.key}
				/>
				<div class="tbl-wrap">
					<table class="tbl uses" data-testid="llm-uses">
						<thead><tr><th>Use</th><th>→ Provider</th><th>→ Model</th><th>If unbound</th><th></th></tr></thead
						>
						<tbody>
							{#each v.uses as u (u.use)}
								{@const rule = LLM_USE_RULES[u.use]}
								{@const problem = pickProblem(v, u.use)}
								<tr data-use={u.use}>
									<td><b>{u.label}</b><span class="cell-sub mono">{u.use} · {kindsText(u.use)} only</span></td
									>
									<td>
										<select
											class="input"
											aria-label="Provider for {u.use}"
											bind:value={sel[u.use].provider}
											disabled={!canWrite}
											title={canWrite ? undefined : writeHint('integrations_manager')}
										>
											<option value="">None — {rule.fallback.split(' (')[0]!.toLowerCase()}</option>
											{#each v.providers as p (p.id)}<option
													value={p.id}
													disabled={!!llmUseProblem(u.use, p.kind)}
													>{p.name}{llmUseProblem(u.use, p.kind)
														? ` (${LLM_KIND_LABEL[p.kind]}: cannot serve)`
														: ''}</option
												>{/each}
										</select>
										{#if problem}<span class="cell-sub err-text" role="alert">{problem}</span>{/if}
									</td>
									<td>
										<select
											class="input"
											aria-label="Model for {u.use}"
											bind:value={sel[u.use].model}
											disabled={!canWrite || !sel[u.use].provider}
										>
											{#each modelsOf(v, sel[u.use].provider) as m (m)}<option value={m}>{m}</option>{/each}
										</select>
										{#if u.use === 'tts'}
											<input
												class="input mono"
												aria-label="Voice for tts"
												placeholder="Voice (provider default)"
												bind:value={sel.tts.voice}
												disabled={!canWrite || !sel.tts.provider}
											/>
											{#if voiceProblem(sel.tts.voice)}<span class="cell-sub err-text" role="alert"
													>{voiceProblem(sel.tts.voice)}</span
												>{/if}
										{/if}
									</td>
									<td class="muted">{rule.fallback}</td>
									<td class="r"
										><button
											class="btn sm"
											type="button"
											aria-label="Bind {u.use}"
											{...writeAttrs(session, 'integrations_manager')}
											disabled={!canWrite || !!problem || (u.use === 'tts' && !!voiceProblem(sel.tts.voice))}
											onclick={() => bind(v, u.use)}>Bind</button
										></td
									>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
				<p class="note">
					Knowledge and ingest models are set per slot in the next two tabs; the routing map above shows all
					of them together.
				</p>
			</Card>
		</div>
	{/snippet}
</LoadState>

<Drawer bind:open title={editing ? editing.name : 'Add LLM provider'} {dirty}>
	<div class="stack">
		{#if !canWrite}
			<Callout kind="info" title="Read only"
				><span>Changing providers needs the tenant_admin role.</span></Callout
			>
		{/if}
		{#if managed}
			<Callout kind="info" title="Managed by the platform"
				><span data-testid="llm-managed">{INTERNAL_COPY}</span></Callout
			>
		{/if}
		<fieldset class="fs" disabled={!canWrite || busy || managed}>
			{#if !editing}
				<label class="field"
					><span class="label">Provider</span><select
						class="input"
						aria-label="Provider preset"
						value={preset}
						onchange={(e) => applyPreset(e.currentTarget.value)}
					>
						{#each LLM_PRESETS as p (p.key)}<option value={p.key}>{p.label}</option>{/each}
					</select></label
				>
			{/if}
			<label class="field"
				><span class="label">Name</span><input
					class="input"
					bind:value={name}
					placeholder="Mistral EU"
				/></label
			>
			<label class="field"
				><span class="label">Kind</span><select class="input" bind:value={kind} aria-label="Kind">
					<option value="openai">{LLM_KIND_LABEL.openai}</option>
					<option value="laya">{LLM_KIND_LABEL.laya} (gate answers)</option>
					<option value="w2v">{LLM_KIND_LABEL.w2v} (asr / tts only)</option>
				</select></label
			>
			<label class="field"
				><span class="label">URL</span><input
					class="input mono"
					bind:value={baseUrl}
					placeholder="https://api.mistral.ai/v1"
					autocomplete="off"
					spellcheck="false"
				/></label
			>
			<label class="field"
				><span class="label">Models (comma separated)</span><input
					class="input mono"
					bind:value={models}
					placeholder="mistral-large-latest"
				/></label
			>
			{#if managed}
				<p class="cell-sub">Token: the platform's internal service credential (never stored per tenant).</p>
			{:else}
				<label class="field"
					><span class="label"
						>Token {#if editing?.secret.set && !clearKey}<Chip
								text={secretText(editing.secret)}
								variant="ok"
							/>{/if}</span
					><input
						class="input mono"
						type="password"
						aria-label="Token"
						autocomplete="new-password"
						bind:value={secret}
						placeholder={editing?.secret.set
							? 'Leave empty to keep the stored token'
							: 'Paste the provider API key'}
					/></label
				>
				{#if editing?.secret.set}
					<label class="check"
						><input type="checkbox" bind:checked={clearKey} /> Remove the stored token</label
					>
				{/if}
				<p class="cell-sub">
					Write-only: stored encrypted per tenant, never returned, logged or audited; only its fingerprint
					shows.
				</p>
			{/if}
			{#if !readOnly}<div class="draft-test" data-testid="draft-test">
					<button
						class="btn sm"
						type="button"
						disabled={!canWrite || draftTesting || !baseUrl.trim()}
						onclick={testDraft}>{draftTesting ? 'Testing…' : 'Test before saving'}</button
					>
					<span class="cell-sub">Probes these values now. Nothing is saved; a typed token is sent once.</span>
					{#if draftTest}
						<p class="test-line" role="status">
							<Chip text={draftTest.ok ? 'ok' : 'failed'} variant={draftTest.ok ? 'ok' : 'err'} />
							{draftTest.detail}
						</p>
						{#if draftTest.models.length}
							<p class="cell-sub">
								Listed: <span class="mono">{draftTest.models.join(', ')}</span>
								<button class="btn sm" type="button" onclick={useTestedModels}>Use these models</button>
							</p>
						{/if}
					{:else if draftTestError}
						<p class="test-line err-text" role="alert">Test refused: {draftTestError}</p>
					{/if}
				</div>{/if}
			<fieldset class="fs use-for" data-testid="use-for">
				<legend class="label">Use for</legend>
				{#each LLM_USES as use (use)}
					{@const problem = useForProblem(use)}
					<div class="use-row">
						<label class="check"
							><input
								type="checkbox"
								aria-label="Use for {use}"
								checked={useFor[use].on && !problem}
								disabled={!!problem}
								onchange={(e) => (useFor[use].on = e.currentTarget.checked)}
							/>
							{LLM_USE_RULES[use].label}</label
						>
						{#if problem}
							<span class="cell-sub" data-testid="use-for-reason-{use}">{problem}</span>
						{:else if useFor[use].on}
							<select class="input" aria-label="Model for {use} (drawer)" bind:value={useFor[use].model}>
								<option value="">First listed model</option>
								{#each modelList as m (m)}<option value={m}>{m}</option>{/each}
							</select>
						{:else}
							<span class="cell-sub">{LLM_USE_RULES[use].purpose}</span>
						{/if}
					</div>
				{/each}
				<span class="cell-sub"
					>{readOnly
						? 'Which use this provider serves is set per job in Agent uses; live when each job publishes (D69).'
						: 'Applied on Save to the draft of every job; live when each job publishes (D69).'}</span
				>
			</fieldset>
		</fieldset>
		{#if failure}
			<Callout kind="err" title="Not saved"><span>{failure.message} ({failure.code})</span></Callout>
		{/if}
		{#if confirmDelete && editing}
			<div class="callout err" role="alertdialog" aria-label="Remove provider">
				<strong>Remove {editing.name}?</strong>
				<span>Its token is deleted. A provider bound to a use cannot be removed.</span>
				<span
					><button class="btn sm danger" type="button" onclick={remove}>Remove</button>
					<button class="btn sm" type="button" onclick={() => (confirmDelete = false)}>Keep</button></span
				>
			</div>
		{/if}
		<div class="actions" hidden={readOnly}>
			{#if editing}
				<button
					class="btn"
					type="button"
					{...writeAttrs(session, 'integrations_manager')}
					onclick={() => (confirmDelete = true)}>Remove</button
				>
			{/if}
			<button
				class="btn primary"
				type="button"
				{...writeAttrs(session, 'integrations_manager')}
				disabled={!canWrite || busy}
				onclick={save}>{busy ? 'Saving…' : 'Save'}</button
			>
		</div>
	</div>
</Drawer>

<style>
	/* Bind button sits on the selects' centre line, not the cell top. */
	.uses td {
		vertical-align: middle;
	}
	.fs {
		border: 0;
		padding: 0;
		margin: 0;
		min-width: 0;
		display: grid;
		gap: 12px;
	}
	.pad {
		padding: 0 16px 16px;
	}
	.row-actions {
		display: inline-flex;
		gap: 6px;
	}
	.chips {
		display: flex;
		flex-wrap: wrap;
		gap: 4px;
	}
	.url {
		overflow-wrap: anywhere;
	}
	.check {
		display: flex;
		gap: 6px;
		align-items: center;
	}
	.bind-callout .bind-act {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
		align-items: center;
		margin-top: 6px;
	}
	.draft-test {
		display: grid;
		gap: 6px;
		justify-items: start;
	}
	.test-line {
		margin: 0;
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		align-items: center;
		overflow-wrap: anywhere;
	}
	.use-for legend {
		padding: 0;
		margin-bottom: 4px;
	}
	.use-row {
		display: grid;
		gap: 4px;
		min-width: 0;
	}
	.use-row select {
		max-width: 100%;
	}
	.err-text {
		color: var(--err);
	}
</style>
