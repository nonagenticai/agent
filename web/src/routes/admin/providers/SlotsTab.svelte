<script lang="ts">
	// Knowledge (strata_rag) or Ingest (strata_ingest) slots: one provider per slot, live-tested on save, with the
	// slot's integer version as expectedVersion. A version moved by another app is drift: shown, never overwritten
	// (06 §4.2, §4.6). D69: the slot configuration is design — saved to the draft of every job and written into the
	// slot when a job publishes; adopting a drifted version is a draft change too.
	// D70: every slot is a GPU capability bound to an external provider — provider, model, URL and token (write-only).
	// Only nonagentic's own tenant binds the internal kinds — mlx_api, pgx vLLM for chat (fixed endpoint, no tenant token).
	// F-08 (B3 T4b): "Use default" resets a slot to the deployment default (confirm first: jobs using the current model
	// get a model change). F-09 (B3 T4c): "Test" probes the drawer's values through the slot's service, saving nothing.
	// Review I4: an embed model a push withheld (a store a job reads still holds content on the old model) is shown
	// with "Push again" (`pushProviderSlot`), which queues what strata has not confirmed.
	import { onMount } from 'svelte';
	import { api, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { canWrite as canWriteFor, writeAttrs } from '$lib/auth/roles';
	import { Resource } from '$lib/load.svelte';
	import { ApiError } from '$lib/api/errors';
	import { toast } from '$lib/ui/toaster.svelte';
	import {
		isInternalKind,
		kindBaseUrl,
		kindModels,
		slotDeploymentDefault,
		type GpuHosting,
		type ProviderSlot,
		type SlotTestResult,
	} from '$lib/api/types/tenant';
	import { secretText, when } from '$lib/tenant/ui';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import TenantDraftBar from '$lib/release/TenantDraftBar.svelte';

	import { FUNDING, INTERNAL_COPY, kindLabel, slotFunding, slotRegion } from './logic';

	let { service, onchange = () => {} }: { service: 'strata_rag' | 'strata_ingest'; onchange?: () => void } =
		$props();
	const INTRO = {
		strata_rag: {
			title: 'Knowledge slots · strata_rag',
			sub: 'strata_rag answers knowledge questions for this tenant. Agent provisions its provider slots down: per slot a provider, model, URL and token, written into strata_rag for your tenant only.',
		},
		strata_ingest: {
			title: 'Ingest slots · strata_ingest',
			sub: 'strata_ingest reads uploaded files into knowledge. Agent provisions its provider slots down: per slot a provider, model, URL and token, written into strata_ingest for your tenant only.',
		},
	} as const;
	// The tenant's hosting (D70) comes from the same view the LLM tab reads: the internal tenant's slots are bound by
	// the platform (mlx_api, pgx vLLM) and the API refuses every save, so the drawer is read-only there.
	const res = new Resource<{ slots: ProviderSlot[]; hosting: GpuHosting }>(
		async () => {
			const [slots, llm] = await Promise.all([api().providerSlots(), api().llmProviders()]);
			return { slots, hosting: llm.hosting };
		},
		() => false,
	);
	onMount(() => res.refresh());
	const hosting = $derived(res.value.state === 'ok' ? res.value.data.hosting : 'external');
	const platformManaged = $derived(hosting === 'internal');
	const canWrite = $derived(canWriteFor(session, 'integrations_manager') && !platformManaged);
	const mine = (all: ProviderSlot[]) => all.filter((s) => s.service === service);

	let open = $state(false);
	let cur = $state<ProviderSlot | null>(null);
	let kind = $state('');
	let baseUrl = $state('');
	let model = $state('');
	/** The model select's value: a listed model, or OTHER (then `model` is typed). */
	let modelPick = $state('');
	let secret = $state('');
	const OTHER = '__other__';
	const internal = $derived(isInternalKind(kind));
	const offered = $derived(cur ? kindModels(kind, cur.slot) : []);
	let busy = $state(false);
	let tick = $state(0);
	let failure = $state<{ code: string; message: string } | null>(null);
	/** The "Use default" confirmation is open. */
	let confirmReset = $state(false);
	let testing = $state(false);
	/** The last "Test" answer for the drawer's values. */
	let probe = $state<SlotTestResult | null>(null);
	const resettable = (s: ProviderSlot) =>
		s.configured && s.source !== 'default' && !!slotDeploymentDefault(s.slot);
	const dirty = $derived(
		open &&
			!!cur &&
			(kind !== (cur.kind ?? cur.kinds[0] ?? '') ||
				baseUrl !== (cur.base_url ?? '') ||
				model !== (cur.model ?? '') ||
				!!secret),
	);

	function edit(s: ProviderSlot) {
		cur = s;
		kind = s.kind ?? s.kinds[0] ?? '';
		baseUrl = s.base_url ?? kindBaseUrl(kind, s.slot);
		model = s.model ?? kindModels(kind, s.slot)[0] ?? '';
		modelPick = kindModels(kind, s.slot).includes(model) ? model : OTHER;
		secret = '';
		failure = null;
		confirmReset = false;
		probe = null;
		open = true;
	}
	/** Row action: open the slot with the "Use default" confirmation showing. */
	function askReset(s: ProviderSlot) {
		edit(s);
		confirmReset = true;
	}
	const problem = (e: unknown) =>
		e instanceof ApiError ? { code: e.code, message: e.message } : { code: 'error', message: String(e) };
	async function resetSlot() {
		if (!cur) return;
		busy = true;
		failure = null;
		try {
			const s = await api().resetProviderSlot(cur.slot, cur.version);
			toast(
				`${s.label} slot uses the deployment default${s.model ? ` (${s.model})` : ''}, v${s.version}.` +
					(s.model_change_id ? ' Jobs using the old model got a model change.' : ''),
			);
			tick++;
			confirmReset = false;
			open = false;
			await res.refresh();
			onchange();
		} catch (e) {
			failure = problem(e);
		} finally {
			busy = false;
		}
	}
	async function testDraft() {
		if (!cur) return;
		testing = true;
		failure = null;
		probe = null;
		try {
			probe = await api().testProviderSlotDraft(cur.slot, {
				kind,
				base_url: baseUrl,
				model,
				secret: secret || undefined,
			});
		} catch (e) {
			failure = problem(e);
		} finally {
			testing = false;
		}
	}
	/** Picking another provider fills in its endpoint and first model (the URL stays editable, the token is asked). */
	function pickKind(next: string) {
		if (!cur) return;
		kind = next;
		baseUrl = next === cur.kind ? (cur.base_url ?? kindBaseUrl(next, cur.slot)) : kindBaseUrl(next, cur.slot);
		const ms = kindModels(next, cur.slot);
		model = next === cur.kind && cur.model ? cur.model : (ms[0] ?? '');
		modelPick = ms.includes(model) ? model : OTHER;
	}
	function pickModel(v: string) {
		modelPick = v;
		if (v !== OTHER) model = v;
		else if (offered.includes(model)) model = '';
	}
	async function save() {
		if (!cur) return;
		busy = true;
		failure = null;
		try {
			const s = await api().saveProviderSlot(cur.slot, {
				kind,
				base_url: internal ? '' : baseUrl,
				model,
				secret: secret || undefined,
				expected_version: cur.version,
			});
			// D93 (U-07): a slot save updates the tenant's catalogue; every job pinning the old model gets a model change.
			toast(
				`Saved: ${s.label} slot, live-tested (v${s.version}). Jobs using the old model get a model change — see Model changes.`,
			);
			tick++;
			open = false;
			await res.refresh();
			onchange();
		} catch (e) {
			failure = problem(e);
		} finally {
			busy = false;
		}
	}
	let pushing = $state<string | null>(null);
	/** "Push again": queue the slot's unconfirmed published entries (a withheld embed model) to strata. */
	async function pushAgain(s: ProviderSlot) {
		pushing = s.slot;
		try {
			const ok = await guard(
				() => api().pushProviderSlot(s.slot),
				`Queued: ${s.label} slot pushed again. It stays withheld while a job still reads a store on the old model.`,
				'Not pushed',
			);
			if (ok) {
				await res.refresh();
				if (open && cur?.slot === s.slot && res.value.state === 'ok') {
					const fresh = res.value.data.slots.find((x) => x.slot === s.slot);
					if (fresh) cur = fresh;
				}
				onchange();
			}
		} finally {
			pushing = null;
		}
	}
	async function adopt() {
		if (!cur) return;
		const key = cur.slot;
		const ok = await guard(
			() => api().acceptSlotDrift(key),
			'Saved to draft: the version the other app wrote. Test it, then publish.',
			'Not loaded',
		);
		tick++;
		await res.refresh();
		onchange();
		if (ok && res.value.state === 'ok') {
			const fresh = res.value.data.slots.find((s) => s.slot === key);
			if (fresh) edit(fresh);
		}
	}
</script>

<LoadState resource={res}>
	{#snippet children(view)}
		{@const slots = mine(view.slots)}
		<TenantDraftBar
			kinds={['provider_slot']}
			{tick}
			only={(r) => slots.some((s) => s.slot === r.ref.slot)}
			label={(r) => `${slots.find((s) => s.slot === r.ref.slot)?.label ?? r.ref.slot} slot`}
		/>
		{#if slots.some((s) => !s.configured)}
			<Callout kind="gap" title="Empty slots"
				><span
					>{slots
						.filter((s) => !s.configured)
						.map((s) => s.label)
						.join(', ')} not configured: work that needs {slots.filter((s) => !s.configured).length === 1
						? 'it'
						: 'them'} fails (and the release readiness lists it as a missing W5 slot).</span
				></Callout
			>
		{/if}
		<Card cls="flush" title={INTRO[service].title} sub={platformManaged ? INTERNAL_COPY : INTRO[service].sub}>
			<div class="tbl-wrap">
				<table class="tbl clickable" data-testid={`slots-${service}`}>
					<thead>
						<tr
							><th>Slot · used by</th><th>→ Provider · model</th><th>Funding · region</th><th class="r"
								>Version</th
							><th>Token</th><th>Updated</th></tr
						>
					</thead>
					<tbody>
						{#each slots as s (s.slot)}
							<tr data-slot={s.slot} onclick={() => edit(s)}>
								<td>
									<button class="table-action" type="button" onclick={(e) => (e.stopPropagation(), edit(s))}
										>{s.label}</button
									>
									<span class="cell-sub mono">{s.service}.{s.slot}</span>
									<span class="cell-sub">{s.used_by.join(', ')}</span>
									{#if s.drift}<span class="cell-sub"
											><Chip text={`drift: v${s.drift.remote_version} by another app`} variant="warn" /></span
										>{/if}
									{#if s.withheld}<span class="cell-sub" data-testid={`slot-withheld-${s.slot}`}
											><Chip
												text={`withheld: ${s.withheld.model ?? 'deployment default'} not live`}
												variant="warn"
												title={s.withheld.roots.length
													? `Still on the old model: ${s.withheld.roots.join(', ')}`
													: 'Knowledge on the deployment default still holds content'}
											/>{#if canWrite}
												<button
													class="table-action"
													type="button"
													data-testid={`slot-push-${s.slot}`}
													disabled={pushing === s.slot}
													onclick={(e) => (e.stopPropagation(), pushAgain(s))}
													>{pushing === s.slot ? 'Pushing…' : 'Push again'}</button
												>{/if}</span
										>{/if}
								</td>
								<td>
									{#if s.source === 'default'}<Chip text="deployment default" variant="ok" /><span
											class="cell-sub mono">{s.model}</span
										>{:else if s.configured}<span>{kindLabel(s.kind ?? '')}</span><span class="cell-sub mono"
											>{s.model}</span
										>{:else}<Chip text="empty" variant="err" />{/if}
									{#if canWrite && resettable(s)}<span class="cell-sub"
											><button
												class="table-action"
												type="button"
												data-testid={`slot-use-default-${s.slot}`}
												onclick={(e) => (e.stopPropagation(), askReset(s))}>Use default</button
											></span
										>{/if}
								</td>
								<td>
									{#if s.configured}
										{@const f = slotFunding(s)}
										{#if f}<Chip
												text={FUNDING[f].label}
												variant={FUNDING[f].variant}
												title={FUNDING[f].hint}
											/>{/if}
										<span class="cell-sub">{slotRegion(s) ?? 'region not declared'}</span>
										{#if s.base_url}<span class="cell-sub mono url">{s.base_url}</span>{/if}
									{:else}<span class="cell-sub">no fallback: work that needs it fails</span>{/if}
								</td>
								<td class="r mono">v{s.version}</td>
								<td>{isInternalKind(s.kind) ? 'platform service key' : secretText(s.secret)}</td>
								<td>{when(s.updated_at)}</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		</Card>
		<p class="note">
			Saving a slot tests it live and creates a model change for every job that uses the old model: each job
			is rebuilt (re-embed, copy or re-ingest) and accepted into its draft on <a
				href="/admin/knowledge/model-changes">Model changes</a
			>, so no job changes model until you publish it. The version you edited must still be the current one,
			so a change made elsewhere is never silently overwritten.
		</p>
	{/snippet}
</LoadState>

<Drawer bind:open title={cur ? `${cur.label} · ${cur.service}` : ''} {dirty}>
	{#if cur}
		<div class="stack">
			<p class="muted">{cur.purpose} Used by: {cur.used_by.join(', ')}.</p>
			{#if platformManaged}
				<Callout kind="info" title="Managed by the platform"
					><span data-testid="slot-platform-managed"
						>nonagentic’s own tenant runs this slot on {kindLabel(cur.kind ?? '')} (D70); the platform binds its
						provider, model and endpoint. Nothing here is editable.</span
					></Callout
				>
			{/if}
			{#if cur.source === 'default' && !platformManaged}
				<Callout kind="info" title="Deployment default"
					><span data-testid="slot-default"
						>This slot uses the deployment default{cur.model ? ` (${cur.model})` : ''}; no URL or token of
						yours is stored. Save a provider below to use your own.</span
					></Callout
				>
			{/if}
			{#if confirmReset && !platformManaged}
				<Callout kind="gap" title="Use the deployment default?"
					><span data-testid="slot-reset-confirm"
						>The {cur.label} slot drops its provider, URL and token and uses the deployment default{slotDeploymentDefault(
							cur.slot,
						)
							? ` (${slotDeploymentDefault(cur.slot)})`
							: ''}. Jobs using the current model get a model change.</span
					></Callout
				>
				<div class="actions">
					<button
						class="btn primary"
						type="button"
						{...writeAttrs(session, 'integrations_manager')}
						disabled={!canWrite || busy}
						data-testid="slot-reset-go"
						onclick={resetSlot}>{busy ? 'Resetting…' : 'Use default'}</button
					>
					<button class="btn" type="button" onclick={() => (confirmReset = false)}>Keep</button>
				</div>
			{/if}
			{#if cur.withheld && !platformManaged}
				<Callout kind="gap" title="The new embedding model is not live yet"
					><span data-testid="slot-withheld"
						>The published embedding model {cur.withheld.model ?? 'the deployment default'} (v{cur.withheld
							.version}) was withheld at {when(cur.withheld.at)}: {cur.withheld.roots.length
							? cur.withheld.roots.join(', ')
							: 'knowledge on the deployment default'} still holds content a job reads on the old model. Rebuild
						it through its <a href="/admin/knowledge/model-changes">model change</a> and publish; then push again.</span
					></Callout
				>
				<div class="actions">
					<button
						class="btn"
						type="button"
						{...writeAttrs(session, 'integrations_manager')}
						disabled={!canWrite || pushing === cur.slot}
						data-testid="slot-push-go"
						onclick={() => cur && pushAgain(cur)}>{pushing === cur.slot ? 'Pushing…' : 'Push again'}</button
					>
				</div>
			{/if}
			{#if cur.drift && !platformManaged}
				<Callout kind="gap" title="Changed by another app"
					><span
						>{cur.drift.changed_by} wrote v{cur.drift.remote_version} at {when(cur.drift.at)}; you have v{cur.version}.
						Agent will not overwrite it. Load the current version, then edit again.</span
					></Callout
				>
				<div class="actions">
					<button class="btn" type="button" {...writeAttrs(session, 'integrations_manager')} onclick={adopt}
						>Load the current version</button
					>
				</div>
			{/if}
			<fieldset class="fs" disabled={!canWrite || busy}>
				<label class="field"
					><span class="label">Provider</span>
					<select
						class="input"
						aria-label="Provider"
						value={kind}
						onchange={(e) => pickKind(e.currentTarget.value)}
					>
						{#each cur.kinds as k (k)}<option value={k}>{kindLabel(k)}</option>{/each}
					</select></label
				>
				<label class="field"
					><span class="label">Model</span>
					<select
						class="input mono"
						aria-label="Model"
						value={modelPick}
						onchange={(e) => pickModel(e.currentTarget.value)}
					>
						{#each offered as m (m)}<option value={m}>{m}</option>{/each}
						{#if !platformManaged}<option value={OTHER}>Other model…</option>{/if}
					</select></label
				>
				{#if modelPick === OTHER}
					<label class="field"
						><span class="label">Model name</span><input
							class="input mono"
							aria-label="Model name"
							bind:value={model}
							placeholder="The model id the provider expects"
						/></label
					>
				{/if}
				{#if internal}
					<p class="cell-sub" data-testid="slot-internal">
						{kindLabel(kind)}, nonagentic's own tenant:
						<span class="mono">{kindBaseUrl(kind, cur.slot)}</span>. The platform holds its service key; no
						token is stored for this tenant.
					</p>
				{:else}
					<label class="field"
						><span class="label">URL</span><input
							class="input mono"
							aria-label="URL"
							bind:value={baseUrl}
							placeholder="https://api.example.eu/v1"
							autocomplete="off"
							spellcheck="false"
						/></label
					>
					<label class="field"
						><span class="label"
							>Token {#if cur.secret.set && kind === cur.kind}<Chip
									text={secretText(cur.secret)}
									variant="ok"
								/>{/if}</span
						><input
							class="input mono"
							type="password"
							aria-label="Token"
							autocomplete="new-password"
							bind:value={secret}
							placeholder={cur.secret.set && kind === cur.kind
								? 'Leave empty to keep the stored token (same provider and URL only)'
								: 'Paste the provider API key'}
						/></label
					>
					<p class="cell-sub">Write-only: stored sealed, never shown again, only its fingerprint.</p>
				{/if}
				{#if !platformManaged}
					<p class="cell-sub">
						Expected version: v{cur.version}. Tokens are write-only, passed through to {cur.service}.
					</p>
				{/if}
			</fieldset>
			{#if probe}
				<Callout kind={probe.ok ? 'info' : 'err'} title={probe.ok ? 'Test passed' : 'Test failed'}
					><span data-testid="slot-test-result"
						>{probe.detail}{probe.dims ? ` · ${probe.dims} dimensions` : ''} · nothing was saved.</span
					></Callout
				>
			{/if}
			{#if failure}
				<Callout
					kind="err"
					title={failure.code === 'VERSION_CONFLICT'
						? '409 VERSION_CONFLICT — nothing overwritten'
						: 'Not saved'}
					><span>{failure.message}{failure.code === 'VERSION_CONFLICT' ? '' : ` (${failure.code})`}</span
					></Callout
				>
			{/if}
			{#if !platformManaged}
				<div class="actions">
					<button
						class="btn primary"
						type="button"
						{...writeAttrs(session, 'integrations_manager')}
						disabled={!canWrite || busy}
						onclick={save}>{busy ? 'Testing and saving…' : 'Save'}</button
					>
					{#if !internal}
						<button
							class="btn"
							type="button"
							{...writeAttrs(session, 'integrations_manager')}
							disabled={!canWrite || busy || testing}
							onclick={testDraft}>{testing ? 'Testing…' : 'Test'}</button
						>
					{/if}
					{#if resettable(cur) && !confirmReset}
						<button
							class="btn"
							type="button"
							{...writeAttrs(session, 'integrations_manager')}
							disabled={!canWrite || busy}
							onclick={() => (confirmReset = true)}>Use default</button
						>
					{/if}
				</div>
			{/if}
		</div>
	{/if}
</Drawer>

<style>
	.url {
		overflow-wrap: anywhere;
	}
	.fs {
		border: 0;
		padding: 0;
		margin: 0;
		min-width: 0;
		display: grid;
		gap: 12px;
	}
</style>
