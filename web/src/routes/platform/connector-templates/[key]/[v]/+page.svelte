<script lang="ts">
	// P17 Connector template detail (doc 11 §5.3/§6.1, doc 10 §4.2, doc 12 §8.1). A connector template is one
	// versioned translation between Agent's canonical operations and one vendor API: per op the request, the field /
	// status / fault maps onto the canonical model and error taxonomy, idempotency and verify_read. The page leads
	// with the lineage (what changed vs the previous version, who runs which version, who can re-pin), then what a
	// tenant must supply, then the per-op translation. Drafts are edited and published here; published = immutable.
	import { page } from '$app/state';
	import { NO_RATE_LIMIT, hasRateLimit } from '$lib/connectors/T11-limits';
	import { goto } from '$app/navigation';
	import { api, guard } from '$lib/app.svelte';
	import { toast } from '$lib/ui/toaster.svelte';
	import { Resource } from '$lib/load.svelte';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Status from '$lib/ui/Status.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import Kpi from '$lib/ui/Kpi.svelte';
	import TierChip from '$lib/connectors/TierChip.svelte';
	import { needsKey, opKey } from '$lib/connectors/ops';
	import { diffTemplates, groupByObject } from '$lib/connectors/P17-diff';
	import P17Lineage from './P17Lineage.svelte';
	import P17Adoption from './P17Adoption.svelte';
	import P17OpDetail from './P17OpDetail.svelte';
	import type {
		ConnectorTemplateDetail,
		ConnectorTemplateOp,
		ConnectorTemplateUsage,
		Idempotency,
		SideEffect,
	} from '$lib/api/types/connectors';
	import type { ErrorStateDef, ObjectModelMap } from '$lib/api/types/model';

	const key = $derived(page.params.key ?? '');
	const v = $derived(page.params.v ?? '');
	const res = new Resource<ConnectorTemplateDetail>(
		() => api().connectorTemplate(key, v),
		() => false,
	);
	// Context the translation is read against: lineage + adoption (this area), the canonical error taxonomy and the
	// object state machines (model area). Each degrades on its own; the template itself never waits on them.
	const usage = new Resource<ConnectorTemplateUsage>(
		() => api().connectorTemplateUsage(key, v),
		() => false,
	);
	const taxonomy = new Resource<ErrorStateDef[]>(
		() => api().errorStateDefs(),
		() => false,
	);
	const objects = new Resource<ObjectModelMap>(
		() => api().objectModelMap(),
		() => false,
	);
	$effect(() => {
		void key;
		void v;
		void res.refresh();
		void usage.refresh();
	});
	$effect(() => {
		void taxonomy.refresh();
		void objects.refresh();
	});
	const use = $derived(usage.value.state === 'ok' ? usage.value.data : null);
	const taxo = $derived(taxonomy.value.state === 'ok' ? taxonomy.value.data : null);
	const statesOf = (object: string): string[] | null => {
		if (objects.value.state !== 'ok') return null;
		const n = objects.value.data.nodes.find((x) => x.object === object);
		return n ? n.states.map((s) => s.status) : null;
	};
	const pinCounts = $derived.by(() => {
		if (!use) return null;
		const out: Record<string, number> = {};
		for (const i of use.instances) out[i.pinned_version] = (out[i.pinned_version] ?? 0) + 1;
		return out;
	});
	function pickOp(op: string) {
		selected = op;
		document.getElementById('p17-ops')?.scrollIntoView({ behavior: 'smooth', block: 'start' });
	}

	let selected = $state('');
	let busy = $state(false);

	// ---- op editor (drafts only) ----
	let editing = $state(false);
	let draft = $state<ConnectorTemplateOp | null>(null);
	let original = '';
	let idemKind = $state<Idempotency['kind']>('nat');
	let idemValue = $state('');
	let editErr = $state('');
	let fixturesText = $state('[]');
	let originalFixtures = '';
	const dirty = $derived(
		draft !== null && (JSON.stringify(build()) !== original || fixturesText !== originalFixtures),
	);

	function blankOp(): ConnectorTemplateOp {
		return {
			object: '',
			verb: '',
			variant: '*',
			side_effect: 'R',
			op_kind: 'standard',
			method: 'GET',
			path: '',
			request: '',
			field_map: [{ path: '$.', field: '' }],
			status_map: [],
			fault_map: [
				{ system: 'HTTP 401', code: 'connector_auth_failed' },
				{ system: 'HTTP 404', code: 'not_found' },
				{ system: 'HTTP 5xx', code: 'upstream_unavailable' },
			],
			pagination: null,
			idempotency: { kind: 'nat' },
			verify_read: null,
			timeout_ms: 6000,
			fixtures: [],
		};
	}
	function openEditor(op: ConnectorTemplateOp | null) {
		draft = structuredClone($state.snapshot(op) ?? blankOp());
		idemKind = draft.idempotency.kind;
		idemValue =
			draft.idempotency.kind === 'header'
				? draft.idempotency.header
				: draft.idempotency.kind === 'dup_search'
					? draft.idempotency.on
					: '';
		editErr = '';
		fixturesText = JSON.stringify(draft.fixtures, null, 2);
		originalFixtures = fixturesText;
		original = JSON.stringify(build());
		editing = true;
	}
	function build(): ConnectorTemplateOp | null {
		if (!draft) return null;
		const idempotency: Idempotency =
			idemKind === 'header'
				? { kind: 'header', header: idemValue.trim() }
				: idemKind === 'dup_search'
					? { kind: 'dup_search', on: idemValue.trim() }
					: { kind: idemKind };
		return {
			...$state.snapshot(draft),
			variant: draft.variant.trim() || '*',
			idempotency,
			field_map: draft.field_map.filter((f) => f.path.trim() || f.field.trim()),
		};
	}
	const idemRefused = $derived(!!draft && needsKey(draft.side_effect) && idemKind === 'none');
	async function saveOp() {
		const op = build();
		if (!op) return;
		editErr = '';
		try {
			const fixtures: unknown = JSON.parse(fixturesText);
			if (!Array.isArray(fixtures)) throw new Error('Recorded fixtures must be a JSON array.');
			op.fixtures = fixtures;
		} catch (e) {
			editErr = `Invalid recorded fixtures: ${(e as Error).message}`;
			return;
		}
		busy = true;
		try {
			await api().saveConnectorTemplateOp(key, v, op);
			editing = false;
			selected = opKey(op);
			await res.refresh();
		} catch (e) {
			editErr = (e as Error).message;
		}
		busy = false;
	}

	async function publish() {
		busy = true;
		await guard(
			() => api().publishConnectorTemplate(key, v),
			`${key}@${v} published — immutable from now on.`,
			'Not published',
		);
		busy = false;
		await res.refresh();
		await usage.refresh();
	}
	async function setRetired(retire: boolean) {
		busy = true;
		await guard(
			() => (retire ? api().retireConnectorTemplate(key, v) : api().restoreConnectorTemplate(key, v)),
			retire
				? `${key}@${v} retired — hidden from new connections; existing ones keep working.`
				: `${key}@${v} restored.`,
			retire ? 'Not retired' : 'Not restored',
		);
		busy = false;
		await res.refresh();
	}
	async function newDraft() {
		busy = true;
		try {
			const d = await api().draftConnectorTemplateVersion(key, v);
			await goto(`/platform/connector-templates/${encodeURIComponent(key)}/${encodeURIComponent(d.version)}`);
		} catch (e) {
			toast(`No draft: ${(e as Error).message}`, 'err');
		}
		busy = false;
	}
	async function runT2() {
		busy = true;
		await guard(() => api().runTemplateSandbox(key, v), 'T2 sandbox run finished.', 'T2 run refused');
		busy = false;
		await res.refresh();
	}
	const EFFECTS: SideEffect[] = ['R', 'W', 'X', 'P'];
	const fmtAt = (s: string | null) => (s ? s.slice(0, 16).replace('T', ' ') + ' UTC' : '—');
	const EFFECT_VARIANT: Record<SideEffect, 'muted' | 'work' | 'err'> = {
		R: 'muted',
		W: 'work',
		X: 'err',
		P: 'err',
	};
	const AUTH_PLAIN: Record<string, string> = {
		none: 'No credential',
		api_key_header: 'API key in a header',
		basic: 'Username + password (basic)',
		bearer_static: 'Static bearer token',
		oauth2_client_credentials: 'OAuth2 client credentials — Agent mints short-lived tokens',
		bundle_token: 'Agent licence bundle token',
		mtls: 'Mutual TLS — client certificate + key, optional private CA',
	};
</script>

<PageHead
	id="P17"
	title="{key}@{v}"
	sub="One versioned translation between Agent’s canonical operations and a vendor’s API. Tenant systems pin a version; a published version never changes."
>
	{#snippet actions()}
		<a class="btn" href="/platform/connector-templates">All templates</a>
	{/snippet}
</PageHead>

<LoadState resource={res}>
	{#snippet children(d)}
		{@const t = d.template}
		{@const op = t.ops.find((o) => opKey(o) === selected) ?? t.ops[0]}
		{@const diff = use ? diffTemplates(use.previous, t) : null}
		{@const writes = t.ops.filter((o) => needsKey(o.side_effect))}
		{@const guarded = writes.filter((o) => o.idempotency.kind !== 'none').length}
		{@const effects = EFFECTS.map(
			(e) => [e, t.ops.filter((o) => o.side_effect === e).length] as const,
		).filter(([, n]) => n > 0)}

		{#if t.status === 'draft'}
			<Callout kind="gap" title="Draft — not offered to tenants">
				Edit operations freely. Publishing runs T1 (recorded fixtures) and refuses any W/X/P operation whose
				idempotency is
				<span class="mono">none</span>. Instances on a draft may serve <b>test</b> routes only.
			</Callout>
		{:else}
			<Callout kind="info" title="Immutable — {t.status}">
				Published {fmtAt(t.published_at)} by {t.published_by
					? (d.names[t.published_by] ?? t.published_by)
					: '—'}. A published version never changes: edits go into a new draft version, and an instance moves
				to it only when its tenant re-pins (which re-runs the contract tests).
			</Callout>
		{/if}
		{#if t.drift}
			<Callout kind="gap" title="Vendor version drift found by T2">
				{t.drift.vendor_version}: {t.drift.note} One re-record PR is open: {t.drift.pr}. Tenants keep this pin
				until they re-pin to the version that PR produces.
			</Callout>
		{/if}

		<Card
			title="Version lineage"
			sub="Every version of {t.key}, oldest → newest, and the tenant systems on each."
		>
			{#snippet actions()}
				<div class="p17-actions">
					{#if !t.platform_connector && t.status !== 'retired'}
						<button class="btn sm" type="button" disabled={busy} onclick={runT2}>Run T2 sandbox</button>
					{/if}
					{#if t.status === 'draft'}
						<button class="btn sm" type="button" disabled={busy} onclick={() => openEditor(null)}
							>Add operation</button
						>
						<button class="btn sm primary" type="button" disabled={busy} onclick={publish}
							>Publish @{v}</button
						>
					{:else}
						<button class="btn sm" type="button" disabled={busy} onclick={newDraft}
							>Start a new draft version</button
						>
						{#if t.status === 'active'}
							<button class="btn sm" type="button" disabled={busy} onclick={() => setRetired(true)}
								>Retire</button
							>
						{:else if t.status === 'retired'}
							<button class="btn sm" type="button" disabled={busy} onclick={() => setRetired(false)}
								>Restore</button
							>
						{/if}
					{/if}
				</div>
			{/snippet}
			<P17Lineage tplKey={key} current={v} versions={d.versions} {pinCounts} {diff} onpick={pickOp} />
		</Card>

		<div class="kpis p17-kpis">
			<Kpi
				label="Canonical operations"
				value={String(t.ops.length)}
				note={effects.length
					? effects.map(([e, n]) => `${n} ${e}`).join(' · ')
					: 'declared per tenant template'}
			/>
			<Kpi
				label="Writes with idempotency"
				value={writes.length ? `${guarded}/${writes.length}` : '—'}
				note={writes.length
					? guarded === writes.length
						? 'every W/X/P op is retry-safe'
						: `${writes.length - guarded} would be refused at publish`
					: 'no write operations'}
			/>
			<Kpi
				label="Status mappings"
				value={String(t.ops.reduce((n, o) => n + o.status_map.length, 0))}
				note="{t.ops.filter((o) => o.status_map.length).length} ops report a canonical status"
			/>
			<div class="p17-tiers">
				<span class="label">Contract tests</span>
				<div class="chips"><TierChip tier="T1" result={t.t1} /><TierChip tier="T2" result={t.t2} /></div>
				<span class="cell-sub">T1 replays recorded fixtures · T2 runs the vendor sandbox</span>
			</div>
		</div>

		<div class="grid g2 p17-pair">
			<Card
				title="Who runs this template"
				sub="Tenant systems on any version. A tenant moves only by re-pinning."
			>
				{#if usage.value.state === 'loading'}<p class="muted">Loading…</p>
				{:else if usage.value.state === 'unavailable'}
					<p class="s-err">Usage unavailable: {usage.value.reason}</p>
					<button class="btn sm" type="button" onclick={usage.retry}>Try again</button>
				{:else if use}
					<P17Adoption current={v} instances={use.instances} latest={use.latest_active} />
				{/if}
			</Card>
			<Card title="What a tenant must supply" sub={t.description}>
				<dl class="kvs">
					<dt>Auth</dt>
					<dd>
						{AUTH_PLAIN[t.auth_kind] ?? t.auth_kind} <span class="mono small">({t.auth_kind})</span>
						<span class="cell-sub">{t.auth_detail}</span>
					</dd>
					<dt>Credential</dt>
					<dd>
						{#if t.credential_schema.length === 0}
							<span class="muted"
								>None — {t.platform_connector ? 'minted or in-app by Agent' : 'none needed'}.</span
							>
						{:else}
							<ul class="p17-creds">
								{#each t.credential_schema as f (f.name)}
									<li>
										<span class="mono">{f.name}</span>
										<Chip
											text={f.secret ? 'x-secret' : 'config'}
											variant={f.secret ? 'warn' : 'muted'}
											title={f.secret
												? 'Encrypted, write-only: never shown again after it is saved'
												: 'Stored in instance config'}
										/>
										<span class="cell-sub">{f.label}{f.hint ? ` · ${f.hint}` : ''}</span>
									</li>
								{/each}
							</ul>
						{/if}
					</dd>
					<dt>Vendor · class</dt>
					<dd>{t.vendor} · {t.system_class}</dd>
					<dt>Transport</dt>
					<dd class="chips">
						<Chip text={t.transport} variant="tool" />{#if t.alt_transport}<Chip
								text={t.alt_transport}
								variant="tool"
							/>{/if}
					</dd>
					<dt>Vendor API pin</dt>
					<dd>{t.vendor_api_pin}</dd>
					<dt>Rate limit</dt>
					<dd>
						{#if hasRateLimit(t.rate_limit_model)}
							<span class="mono">{t.rate_limit_model.kind}</span> — {t.rate_limit_model.value ?? ''}{t
								.rate_limit_model.honour_retry_after
								? ' · honours Retry-After'
								: ''}
							<span class="cell-sub">Enforced per tenant system; a 429 becomes rate_limited.</span>
						{:else}
							{NO_RATE_LIMIT}
						{/if}
					</dd>
					<dt>Webhooks</dt>
					<dd>
						{#if t.webhooks.length === 0}<span class="muted">none</span>{:else}
							{#each t.webhooks as w (w.topic)}<div>
									<span class="mono">{w.topic}</span>
									<Chip text="inert" variant="muted" title="v1 is pull-only" />
								</div>{/each}
							<span class="cell-sub">Metadata only in v1 (pull-only).</span>
						{/if}
					</dd>
					<dt>Status</dt>
					<dd><Status word={t.status} /></dd>
				</dl>
			</Card>
		</div>

		<section id="p17-ops" class="p17-explorer" aria-label="Operations">
			<div class="p17-flow" aria-label="How one call is translated">
				<div class="p17-box">
					<span class="label">Agent</span><b>canonical operation</b><span class="cell-sub"
						>object.verb + canonical inputs</span
					>
				</div>
				<span class="p17-to" aria-hidden="true"></span>
				<div class="p17-box tpl">
					<span class="label">@{v}</span><b>request template</b><span class="cell-sub"
						>slots, secret, idempotency key</span
					>
				</div>
				<span class="p17-to" aria-hidden="true"></span>
				<div class="p17-box ven">
					<span class="label">{t.vendor}</span><b>{t.transport} API</b><span class="cell-sub"
						>{t.vendor_api_pin}</span
					>
				</div>
				<span class="p17-to" aria-hidden="true"></span>
				<div class="p17-box tpl">
					<span class="label">@{v}</span><b>field · status · fault maps</b><span class="cell-sub"
						>JSONPath, enum, error code</span
					>
				</div>
				<span class="p17-to" aria-hidden="true"></span>
				<div class="p17-box">
					<span class="label">Agent</span><b>canonical result</b><span class="cell-sub"
						>or a typed error from the taxonomy</span
					>
				</div>
			</div>

			{#if t.ops.length === 0}
				<Card title="Operations">
					<div class="empty">
						{t.key === 'generic_rest'
							? 'Generic REST declares no operations of its own: each tenant template (D31) declares its operations on top of it.'
							: 'No operations yet. Add one to start.'}
					</div>
				</Card>
			{:else if op}
				<div class="ops-layout">
					<Card title="Operations" sub="{t.ops.length} implemented, by canonical object" cls="flush">
						<ul class="op-list" role="listbox" aria-label="Operations">
							{#each groupByObject(t.ops) as g (g.object)}
								<li class="op-group" role="presentation"><span class="label">{g.object}</span></li>
								{#each g.ops as o (opKey(o))}
									{@const ch = diff?.ops.find((x) => x.op === opKey(o))}
									<li role="presentation">
										<button
											type="button"
											role="option"
											aria-selected={opKey(o) === opKey(op)}
											class:on={opKey(o) === opKey(op)}
											onclick={() => (selected = opKey(o))}
										>
											<span class="mono">{opKey(o)}</span>
											<span class="op-marks">
												{#if ch && ch.change !== 'same' && diff?.against}<span
														class="p17-dot {ch.change}"
														title="{ch.change} vs @{diff.against}"
													></span>{/if}
												<Chip text={o.side_effect} variant={EFFECT_VARIANT[o.side_effect]} />
											</span>
										</button>
									</li>
								{/each}
							{/each}
						</ul>
						{#if diff?.against && diff.counts.changed + diff.counts.added > 0}
							<p class="cell-sub op-legend">
								<span class="p17-dot changed"></span> changed / <span class="p17-dot added"></span> added vs @{diff.against}
							</p>
						{/if}
					</Card>
					<Card title={opKey(op)} sub="How this canonical operation reaches {t.vendor} and comes back.">
						{#snippet actions()}
							{#if t.status === 'draft'}
								<button class="btn sm" type="button" onclick={() => openEditor(op)}>Edit operation</button>
							{/if}
						{/snippet}
						<P17OpDetail
							{op}
							vendor={t.vendor}
							change={diff?.against ? (diff.ops.find((x) => x.op === opKey(op)) ?? null) : null}
							taxonomy={taxo}
							states={statesOf(op.object)}
						/>
					</Card>
				</div>
			{/if}
		</section>

		<Drawer bind:open={editing} title={draft?.object ? `Edit ${opKey(draft)}` : 'Add operation'} wide {dirty}>
			{#if draft}
				<div class="form-grid">
					<div class="field">
						<label for="op-object">Object</label><input
							id="op-object"
							class="input mono"
							bind:value={draft.object}
						/>
					</div>
					<div class="field">
						<label for="op-verb">Verb</label><input id="op-verb" class="input mono" bind:value={draft.verb} />
					</div>
					<div class="field">
						<label for="op-variant">Variant</label><input
							id="op-variant"
							class="input mono"
							bind:value={draft.variant}
						/>
					</div>
					<div class="field">
						<label for="op-effect">Side effect</label>
						<select id="op-effect" class="input" bind:value={draft.side_effect}>
							{#each EFFECTS as e (e)}<option value={e}>{e}</option>{/each}
						</select>
					</div>
					<div class="field">
						<label for="op-method">Method</label><input
							id="op-method"
							class="input mono"
							bind:value={draft.method}
						/>
					</div>
					<div class="field">
						<label for="op-timeout">Timeout (ms)</label><input
							id="op-timeout"
							class="input"
							type="number"
							bind:value={draft.timeout_ms}
						/>
					</div>
				</div>
				<div class="field">
					<label for="op-path">Path</label><input id="op-path" class="input mono" bind:value={draft.path} />
				</div>
				<div class="field">
					<label for="op-req"
						>Request template ({'{{canonical.field}}'}, {'{{env.id}}'}, {'{{step.idempotency_key}}'})</label
					>
					<textarea id="op-req" class="input" rows="5" bind:value={draft.request}></textarea>
				</div>
				<div class="form-grid">
					<div class="field">
						<label for="op-idem">Idempotency</label>
						<select id="op-idem" class="input" bind:value={idemKind} aria-invalid={idemRefused}>
							<option value="nat">natural (reads)</option>
							<option value="header">header</option>
							<option value="dup_search">dup_search</option>
							<option value="none">none</option>
						</select>
					</div>
					{#if idemKind === 'header' || idemKind === 'dup_search'}
						<div class="field">
							<label for="op-idem-v">{idemKind === 'header' ? 'Header name' : 'Search on'}</label>
							<input id="op-idem-v" class="input mono" bind:value={idemValue} />
						</div>
					{/if}
					<div class="field">
						<label for="op-verify">verify_read</label><input
							id="op-verify"
							class="input mono"
							value={draft.verify_read ?? ''}
							oninput={(e) => draft && (draft.verify_read = e.currentTarget.value.trim() || null)}
						/>
					</div>
					<div class="field">
						<label for="op-page">Pagination</label><input
							id="op-page"
							class="input mono"
							value={draft.pagination ?? ''}
							oninput={(e) => draft && (draft.pagination = e.currentTarget.value.trim() || null)}
						/>
					</div>
				</div>
				{#if idemRefused}
					<p class="s-err" role="alert">
						Idempotency “none” is refused for {draft.side_effect} operations (doc 10 §4.2).
					</p>
				{/if}
				<div>
					<h3>Field map</h3>
					{#each draft.field_map as f, i (i)}
						<div class="map-row">
							<input class="input mono" aria-label="System path {i + 1}" bind:value={f.path} />
							<input class="input mono" aria-label="Canonical field {i + 1}" bind:value={f.field} />
							<button class="btn sm" type="button" onclick={() => draft?.field_map.splice(i, 1)}
								>Remove</button
							>
						</div>
					{/each}
					<button
						class="btn sm"
						type="button"
						onclick={() => draft?.field_map.push({ path: '$.', field: '' })}>Add mapping</button
					>
				</div>
				<div class="field">
					<label for="op-fixtures">Recorded response fixtures (JSON array)</label>
					<textarea
						id="op-fixtures"
						class="input mono"
						rows="8"
						bind:value={fixturesText}
						aria-describedby="op-fixtures-hint"></textarea>
					<p id="op-fixtures-hint" class="muted">
						Use redacted vendor responses with name, status, outcome and body. Publishing replays these
						through the field and error mappings.
					</p>
				</div>
				{#if editErr}<p class="s-err" role="alert">{editErr}</p>{/if}
				<div class="actions">
					<button class="btn primary" type="button" disabled={busy || !dirty} onclick={saveOp}
						>Save operation</button
					>
					<button class="btn" type="button" onclick={() => (editing = false)}>Cancel</button>
				</div>
			{/if}
		</Drawer>
	{/snippet}
</LoadState>

<style>
	.p17-actions {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
	}
	.p17-kpis {
		margin: 16px 0;
	}
	.p17-tiers {
		display: grid;
		gap: 6px;
		align-content: start;
		padding: 12px 14px;
		border: 1px solid var(--line);
		border-radius: var(--r);
		background: var(--panel);
	}
	.p17-pair {
		margin-bottom: 16px;
	}
	.p17-creds {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 6px;
	}
	.p17-creds li {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		align-items: baseline;
	}
	.p17-creds .cell-sub {
		flex-basis: 100%;
	}
	.small {
		font-size: 12px;
	}
	.p17-explorer {
		scroll-margin-top: 16px;
	}
	.p17-flow {
		display: flex;
		align-items: stretch;
		gap: 0;
		margin-bottom: 16px;
	}
	/* boxes shrink and wrap their text rather than scrolling the strip; stacks vertically on narrow screens */
	.p17-box {
		flex: 1 1 0;
		min-width: 0;
		overflow-wrap: anywhere;
		display: grid;
		gap: 2px;
		align-content: start;
		padding: 10px 12px;
		border: 1px solid var(--line);
		border-radius: var(--r);
		background: var(--panel);
		font-size: 13px;
	}
	.p17-box.tpl {
		border-color: var(--accent);
		background: var(--sunk);
	}
	.p17-box.ven {
		border-style: dashed;
	}
	.p17-to {
		flex: 0 0 22px;
		align-self: center;
		height: 2px;
		background: var(--line);
		position: relative;
	}
	.p17-to::after {
		content: '';
		position: absolute;
		right: 0;
		top: -4px;
		border: 5px solid transparent;
		border-left-color: var(--muted);
		border-right: 0;
	}
	@media (max-width: 860px) {
		.p17-flow {
			flex-direction: column;
		}
		.p17-to {
			flex: 0 0 14px;
			width: 2px;
			height: 14px;
			align-self: center;
		}
		.p17-to::after {
			right: -4px;
			top: auto;
			bottom: -4px;
			border: 5px solid transparent;
			border-top-color: var(--muted);
			border-bottom: 0;
		}
	}
	.ops-layout {
		display: grid;
		grid-template-columns: minmax(220px, 300px) minmax(0, 1fr);
		gap: 16px;
		align-items: start;
	}
	@container (max-width: 760px) {
		.ops-layout {
			grid-template-columns: minmax(0, 1fr);
		}
	}
	@media (max-width: 760px) {
		.ops-layout {
			grid-template-columns: minmax(0, 1fr);
		}
	}
	.op-list {
		list-style: none;
		margin: 0;
		padding: 0 0 8px;
	}
	.op-group {
		padding: 10px 16px 2px;
	}
	.op-list button {
		display: flex;
		width: 100%;
		justify-content: space-between;
		align-items: center;
		gap: 8px;
		padding: 6px 16px 6px 24px;
		border: 0;
		background: none;
		color: var(--fg);
		cursor: pointer;
		text-align: left;
	}
	.op-list button:hover,
	.op-list button.on {
		background: var(--sunk);
	}
	.op-list button.on {
		box-shadow: inset 3px 0 0 var(--accent);
	}
	.op-marks {
		display: flex;
		gap: 6px;
		align-items: center;
	}
	.p17-dot {
		display: inline-block;
		width: 8px;
		height: 8px;
		border-radius: 50%;
		background: var(--work);
	}
	.p17-dot.added {
		background: var(--ok);
	}
	.p17-dot.removed {
		background: var(--err);
	}
	.op-legend {
		padding: 0 16px 10px;
	}
	.map-row {
		display: grid;
		grid-template-columns: minmax(0, 1.4fr) minmax(0, 1fr) auto;
		gap: 8px;
		margin-bottom: 6px;
	}
</style>
