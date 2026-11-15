<script lang="ts">
	// T12 Tenant connector templates (D31, K50 OQ-3, doc 12 §4): tenant-private generic and custom templates built on
	// generic REST, for systems the platform catalogue does not cover. Declarative mapping only (closed transform
	// list), validated against operation_def, secrets only via typed {{secret.*}} placeholders, contract tests
	// mandatory before a route may use the template. The page leads with where a template sits (catalogue → systems
	// → routes), then each template family with its versions, who pins which version and the upgrade path, then the
	// selected version: its gate, what connecting it needs, and its operations (editable while draft).
	// D69: a tenant template is design — every create, edit, new version and status change is saved to the draft of
	// every job (validation and contract-test results are not), and goes live when each job publishes.
	import { onMount } from 'svelte';
	import { api, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { canWrite, writeAttrs, writeHint } from '$lib/auth/roles';
	import { Resource } from '$lib/load.svelte';
	import { toast } from '$lib/ui/toaster.svelte';
	import PageHead from '$lib/ui/PageHead.svelte';
	import TenantDraftBar from '$lib/release/TenantDraftBar.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Status from '$lib/ui/Status.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import StepFlow, { type Step } from '$lib/ui/StepFlow.svelte';
	import {
		opKey,
		parseOpKey,
		TRANSFORMS,
		transformAllowed,
		idempotencyLabel,
		validateTenantTemplate,
		type OpDefLite,
	} from '$lib/connectors/ops';
	import { AUTH_T12, diffT12, familiesT12, routedT12, type T12Family } from '$lib/connectors/T12-templates';
	import {
		canImportMcp,
		importedLine,
		importSummary,
		issueLine,
		pinnedSystems,
		transportLabel,
	} from '$lib/connectors/mcp-import';
	import T12Model from './T12Model.svelte';
	import type {
		Idempotency,
		TenantConnectorTemplate,
		TenantTemplateOp,
		TenantTemplatePage,
		TenantTemplateUsage,
		McpImportResult,
	} from '$lib/api/types/connectors';

	const list = new Resource<TenantTemplatePage>(() => api().tenantConnectorTemplates());
	let defs = $state<OpDefLite[]>([]);
	let usage = $state<TenantTemplateUsage | null>(null);
	async function loadUsage() {
		try {
			usage = await api().tenantTemplateUsage();
		} catch {
			usage = null;
		}
	}
	onMount(async () => {
		await Promise.all([
			list.refresh(),
			loadUsage(),
			api()
				.connectorOpDefs()
				.then((d) => (defs = d))
				.catch(() => (defs = [])),
		]);
	});
	async function reload() {
		await Promise.all([list.refresh(), loadUsage()]);
	}
	const writable = $derived(canWrite(session, 'integrations_manager'));

	let sel = $state(''); // `${key}@${version}`
	let draft = $state<TenantConnectorTemplate | null>(null);
	/** `task_tools` as typed (comma- or space-separated op keys); parsed into the save body. */
	let taskToolsText = $state('');
	let taskToolsOriginal = '';
	let original = '';
	let busy = $state('');
	let err = $state('');
	const dirty = $derived(
		!!draft && (JSON.stringify($state.snapshot(draft)) !== original || taskToolsText !== taskToolsOriginal),
	);

	/** Default selection: the newest draft (the thing being worked on), else the first in-use version. */
	function defaultOf(items: TenantConnectorTemplate[]): TenantConnectorTemplate | undefined {
		return items.find((t) => t.status === 'draft') ?? items[0];
	}
	function current(page: TenantTemplatePage): TenantConnectorTemplate | null {
		return page.items.find((t) => `${t.key}@${t.version}` === sel) ?? defaultOf(page.items) ?? null;
	}
	/** Load `t` into the editor — its draft copy, the task-tools text and the clean baseline. */
	function load(t: TenantConnectorTemplate) {
		draft = structuredClone($state.snapshot(t));
		taskToolsText = (t.task_tools ?? []).join(', ');
		taskToolsOriginal = taskToolsText;
		original = JSON.stringify(draft);
	}
	function select(t: TenantConnectorTemplate) {
		sel = `${t.key}@${t.version}`;
		load(t);
		err = '';
	}
	$effect(() => {
		const v = list.value;
		if (v.state !== 'ok') return;
		const t = v.data.items.find((x) => `${x.key}@${x.version}` === sel) ?? defaultOf(v.data.items);
		if (!t) return;
		const keyChanged = !draft || `${draft.key}@${draft.version}` !== `${t.key}@${t.version}`;
		if (keyChanged || (!dirty && JSON.stringify(t) !== original)) {
			sel = `${t.key}@${t.version}`;
			load(t);
		}
	});

	const editable = $derived(writable && draft?.status === 'draft');
	const defOf = (o: TenantTemplateOp) =>
		defs.find((d) => opKey(d) === opKey(o)) ??
		defs.find((d) => opKey(d) === opKey({ ...o, variant: '*' })) ??
		(o.def ? { object: o.object, verb: o.verb, variant: o.variant, ...o.def } : undefined);
	const csv = (s: string) =>
		s
			.split(',')
			.map((x) => x.trim())
			.filter(Boolean);
	/** `task_tools` text → op keys: comma- or space-separated, empties dropped. */
	const parseTaskTools = (s: string) =>
		s
			.split(/[\s,]+/)
			.map((x) => x.trim())
			.filter(Boolean);
	function setOwnDef(o: TenantTemplateOp, on: boolean) {
		if (!on) delete o.def;
		else o.def = { side_effect: 'R', input_fields: [], output_fields: [], description: '' };
	}
	/** Live pre-check of the draft as typed — the server re-validates on Validate. */
	const precheck = $derived(
		draft && draft.status === 'draft' && defs.length
			? validateTenantTemplate($state.snapshot(draft), defs)
			: [],
	);

	function lifecycle(t: TenantConnectorTemplate): Step[] {
		const valid = t.validation && t.validation.errors.length === 0;
		const green = t.contract?.status === 'green';
		return [
			{
				key: 'draft',
				label: 'Draft on generic REST',
				status: 'done',
				detail: `${t.ops.length} operation(s) · ${t.base}`,
			},
			{
				key: 'validate',
				label: 'Validate against operation_def',
				status: !t.validation ? (t.status === 'draft' ? 'current' : 'done') : valid ? 'done' : 'missing',
				detail: !t.validation
					? precheck.length && t.status === 'draft'
						? `not run · pre-check finds ${precheck.length} issue(s)`
						: 'not run'
					: valid
						? 'no errors'
						: `${t.validation.errors.length} error(s)`,
			},
			{
				key: 'contract',
				label: 'Contract tests (T1 fixtures)',
				status: green ? 'done' : valid ? 'current' : 'blocked',
				detail: t.contract ? `${t.contract.passed}/${t.contract.total} passed` : 'mandatory before routing',
			},
			{
				key: 'active',
				label: t.status === 'retired' ? 'Retired' : 'Active — systems may pin it',
				status: t.status === 'draft' ? 'todo' : 'done',
				detail: t.status === 'retired' ? 'no new pins' : t.status === 'active' ? 'immutable from now on' : '',
			},
		];
	}

	async function save(): Promise<boolean> {
		if (!draft) return false;
		err = '';
		const task_tools = parseTaskTools(taskToolsText);
		// a variable, not an inline literal: the Api type predates the route's `task_tools` body field
		const body = {
			label: draft.label,
			description: draft.description,
			credential_schema: $state.snapshot(draft.credential_schema),
			ops: $state.snapshot(draft.ops),
			task_tools,
		};
		try {
			await api().saveTenantConnectorTemplate(draft.key, draft.version, body);
			draft.task_tools = task_tools;
			taskToolsOriginal = taskToolsText;
			original = JSON.stringify($state.snapshot(draft));
			return true;
		} catch (e) {
			err = (e as Error).message;
			return false;
		}
	}
	async function saveDraft() {
		busy = 'save';
		if (await save())
			toast('Saved to draft. Validate it before running contract tests; test it, then publish.');
		busy = '';
		await reload();
	}
	async function validate() {
		if (!draft) return;
		busy = 'validate';
		if (dirty && !(await save())) {
			busy = '';
			return;
		}
		try {
			const errors = await api().validateTenantConnectorTemplate(draft.key, draft.version);
			toast(
				errors.length ? `${errors.length} validation error(s).` : 'Valid against the operation catalogue.',
				errors.length ? 'err' : 'ok',
			);
		} catch (e) {
			toast(`Not validated: ${(e as Error).message}`, 'err');
		}
		busy = '';
		await reload();
	}
	async function contract() {
		if (!draft) return;
		busy = 'contract';
		await guard(
			() => api().runTenantTemplateContractTests(draft!.key, draft!.version),
			'Contract tests green.',
			'Contract tests refused',
		);
		busy = '';
		await reload();
	}
	async function setStatus(status: 'active' | 'retired') {
		if (!draft) return;
		busy = status;
		await guard(
			() => api().setTenantTemplateStatus(draft!.key, draft!.version, status),
			status === 'active'
				? `Saved to draft: ${draft.key}@${draft.version} active — systems can pin it. Test it, then publish.`
				: `Saved to draft: ${draft.key}@${draft.version} retired. Test it, then publish.`,
			status === 'active' ? 'Not activated' : 'Not retired',
		);
		busy = '';
		await reload();
	}
	async function newVersion() {
		if (!draft) return;
		busy = 'version';
		try {
			const t = await api().draftTenantTemplateVersion(draft.key, draft.version);
			sel = `${t.key}@${t.version}`;
			draft = null;
			toast(`Saved to draft: ${t.key}@${t.version} started.`);
		} catch (e) {
			toast(`No new version: ${(e as Error).message}`, 'err');
		}
		busy = '';
		await reload();
	}

	function addOp() {
		draft?.ops.push({
			object: 'case',
			verb: 'read',
			variant: '*',
			method: 'GET',
			path: '/api/',
			request: '',
			field_map: [{ path: '$.', field: '', transform: 'none' }],
			status_map: [],
			idempotency: { kind: 'nat' },
		});
	}
	function setOpKey(o: TenantTemplateOp, key: string) {
		const r = parseOpKey(key);
		o.object = r.object;
		o.verb = r.verb;
		o.variant = r.variant;
	}
	function setIdem(o: TenantTemplateOp, kind: Idempotency['kind'], value = '') {
		o.idempotency =
			kind === 'header'
				? { kind, header: value || 'Idempotency-Key' }
				: kind === 'dup_search'
					? { kind, on: value || 'external_id' }
					: { kind };
	}
	const errFor = (t: TenantConnectorTemplate, i: number) =>
		(t.validation?.errors ?? []).filter((e) => e.path.startsWith(`ops[${i}] `) || e.path === `ops[${i}]`);
	const preFor = (i: number) => precheck.filter((e) => e.path.startsWith(`ops[${i}] `));
	/** `{{env.<id>}}` placeholders: the per-system IDs a tenant fills in when connecting. */
	function envIds(t: TenantConnectorTemplate): string[] {
		const s = new Set<string>();
		for (const o of t.ops)
			for (const m of `${o.path} ${o.request}`.matchAll(/\{\{env\.([a-zA-Z0-9_]+)\}\}/g)) s.add(m[1]!);
		return [...s];
	}
	const familyOf = (fams: T12Family[], key: string) => fams.find((f) => f.key === key) ?? null;

	// ---- import tools from MCP (draft mcp templates) ----
	let impSystem = $state('');
	let importing = $state(false);
	let impErr = $state('');
	let impResult = $state<McpImportResult | null>(null);
	async function importTools() {
		if (!draft || !impSystem) return;
		importing = true;
		impErr = '';
		impResult = null;
		try {
			const r = await api().importMcpTools(draft.key, draft.version, impSystem);
			impResult = r;
			toast(importSummary(r), r.ops.length ? 'ok' : 'err');
			// the import wrote the draft server-side: drop the local copy so the ops reload from the server
			const k = `${draft.key}@${draft.version}`;
			await list.refresh();
			await loadUsage();
			sel = k;
			draft = null;
		} catch (e) {
			const x = e as { code?: string; message: string };
			impErr = `${x.code ?? 'error'} — ${x.message}`;
		}
		importing = false;
	}

	// ---- create ----
	let creating = $state(false);
	let nf = $state({
		key: '',
		label: '',
		kind: 'custom' as 'generic' | 'custom',
		auth_kind: 'api_key_header' as TenantConnectorTemplate['auth_kind'],
		description: '',
		transport: 'rest' as 'rest' | 'mcp',
	});
	let createErr = $state('');
	function openCreate() {
		nf = {
			key: '',
			label: '',
			kind: 'custom',
			auth_kind: 'api_key_header',
			description: '',
			transport: 'rest',
		};
		createErr = '';
		creating = true;
	}
	async function create() {
		createErr = '';
		try {
			const t = await api().createTenantConnectorTemplate($state.snapshot(nf));
			creating = false;
			sel = `${t.key}@${t.version}`;
			draft = null;
			toast(`Saved to draft: ${t.key}@1 created on ${nf.transport === 'mcp' ? 'MCP' : 'generic REST'}.`);
			await reload();
		} catch (e) {
			const x = e as { code?: string; message: string };
			createErr = `${x.code ?? 'error'} — ${x.message}`;
		}
	}
</script>

<PageHead
	id="T12"
	sub="Your own connector templates, for in-house or bespoke systems the platform catalogue does not cover. Declarative only, built on generic REST — and no system may route to a version until it is validated and contract-tested."
>
	{#snippet actions()}
		<button
			class="btn primary"
			type="button"
			{...writeAttrs(session, 'integrations_manager')}
			onclick={openCreate}>New template</button
		>
	{/snippet}
</PageHead>

<TenantDraftBar
	kinds={['tenant_template']}
	tick={list.value}
	label={(r) => `${r.ref.key}@${r.ref.version}`}
/>

<LoadState resource={list}>
	{#snippet empty()}
		<T12Model {usage} families={0} live={0} />
		<div data-testid="tenant-templates" class="t12-empty">
			<p>
				No tenant templates yet. Every platform template is available on the
				<a href="/admin/connections/systems">Systems</a> page; build one here only for a system none of them covers.
			</p>
			<button class="btn" type="button" {...writeAttrs(session, 'integrations_manager')} onclick={openCreate}
				>New template</button
			>
		</div>
	{/snippet}
	{#snippet children(page)}
		{@const cur = current(page)}
		{@const fams = familiesT12(page.items, usage)}
		<T12Model {usage} families={fams.length} live={page.items.filter((t) => t.status === 'active').length} />

		<section class="t12-fams" data-testid="tenant-templates" aria-label="Your templates">
			<h2 class="t12-h">Your templates</h2>
			{#each fams as f (f.key)}
				<article class="t12-fam" class:on={cur?.key === f.key}>
					<header class="t12-fam-head">
						<div>
							<strong>{f.label}</strong>
							<span class="mono muted">{f.key}</span>
							<Chip
								text={f.kind}
								variant={f.kind === 'custom' ? 'accent' : 'tool'}
								title={f.kind === 'custom'
									? 'Custom: the definition of one in-house system'
									: 'Generic: a reusable REST shape'}
							/>
						</div>
						<p class="muted t12-desc">{f.description}</p>
					</header>

					<ol class="t12-chain" aria-label="Versions of {f.key}">
						{#each f.versions as v (v.version)}
							{@const pinned = f.pins.filter((p) => p.version === v.version)}
							<li>
								<button
									type="button"
									class="t12-ver {v.status}"
									class:sel={cur?.key === v.key && cur?.version === v.version}
									aria-pressed={cur?.key === v.key && cur?.version === v.version}
									aria-label="{v.key}@{v.version}"
									onclick={() => select(v)}
								>
									<span class="mono t12-ver-id">@{v.version}</span>
									<Status word={v.status} />
									<span class="t12-ver-sub"
										>{#if v.status === 'draft'}{v.validation
												? v.validation.errors.length
													? `${v.validation.errors.length} validation errors`
													: v.contract?.status === 'green'
														? 'tested — ready to activate'
														: 'valid — needs contract tests'
												: 'not validated'}{:else}{v.ops.length} ops · {v.contract
												? `${v.contract.passed}/${v.contract.total} tests`
												: 'untested'}{/if}</span
									>
									<span class="t12-ver-sub"
										>{pinned.length
											? `pinned by ${pinned.length} system${pinned.length === 1 ? '' : 's'}`
											: 'no system pins it'}</span
									>
								</button>
							</li>
						{/each}
					</ol>

					<div class="t12-use">
						{#if f.pins.length}
							<div>
								<span class="label">In use</span>
								<ul class="t12-pins">
									{#each f.pins as p (p.instance_id)}
										<li>
											<a class="mono" href="/admin/connections/systems/{encodeURIComponent(p.instance_id)}"
												>{p.instance_id}</a
											>
											<span class="muted">· {p.env} · pins @{p.version}</span>
											<Status word={p.status} />
										</li>
									{/each}
								</ul>
								<span class="cell-sub"
									>{f.routes.length} job route{f.routes.length === 1 ? '' : 's'} send operations here
									{#each [...new Map(f.routes.map((r) => [r.job, r.job_label]))] as [job, label] (job)}
										· <a href="/admin/jobs/{job}/operations">{label}</a>
									{/each}</span
								>
							</div>
						{:else}
							<span class="muted"
								>{f.versions.some((v) => v.status === 'active')
									? 'Active but no system pins it yet — connect it on Systems.'
									: f.versions.every((v) => v.status === 'retired')
										? 'Retired: no system pins it and none may.'
										: 'Not in use yet.'}</span
							>
						{/if}
						{#if f.upgrade}
							<div class="t12-up {f.upgrade.state}" data-testid="t12-upgrade-{f.key}">
								<span class="label">Upgrade path</span>
								<span class="t12-up-line">
									<span class="mono">@{f.upgrade.from}</span>
									<span aria-hidden="true">→</span>
									<span class="mono">@{f.upgrade.to}</span>
									<Chip
										text={f.upgrade.state === 'ready'
											? 're-pin available'
											: `@${f.upgrade.to} is ${f.upgrade.toStatus}`}
										variant={f.upgrade.state === 'ready' ? 'ok' : 'warn'}
									/>
								</span>
								<span class="cell-sub">{f.upgrade.next}</span>
							</div>
						{/if}
					</div>
				</article>
			{/each}
		</section>

		{#if cur && draft}
			{@const fam = familyOf(fams, cur.key)}
			{@const diff = fam ? diffT12(fam.versions, cur) : null}
			{@const routed = routedT12(fam?.routes ?? [])}
			{@const pinVer = (id: string) => fam?.pins.find((p) => p.instance_id === id)?.version ?? '?'}
			{@const ids = envIds(draft)}
			<Card title="{draft.key}@{draft.version} — {draft.label}" sub={draft.description}>
				{#snippet actions()}
					<span class="row">
						{#if draft?.status === 'draft'}
							<button
								class="btn"
								type="button"
								{...writeAttrs(session, 'integrations_manager')}
								disabled={!writable || busy !== '' || !dirty}
								onclick={saveDraft}>Save draft</button
							>
							<button
								class="btn"
								type="button"
								{...writeAttrs(session, 'integrations_manager')}
								disabled={!writable || busy !== ''}
								onclick={validate}>{busy === 'validate' ? 'Validating…' : 'Validate'}</button
							>
							<button
								class="btn"
								type="button"
								{...writeAttrs(session, 'integrations_manager')}
								disabled={!writable || busy !== '' || dirty}
								onclick={contract}>Run contract tests</button
							>
							<button
								class="btn primary"
								type="button"
								{...writeAttrs(session, 'integrations_manager')}
								disabled={!writable || busy !== '' || dirty}
								onclick={() => setStatus('active')}>Activate</button
							>
						{:else}
							<button
								class="btn"
								type="button"
								{...writeAttrs(session, 'integrations_manager')}
								disabled={!writable || busy !== ''}
								onclick={newVersion}>New draft version</button
							>
							{#if draft?.status === 'active'}
								<button
									class="btn danger"
									type="button"
									{...writeAttrs(session, 'integrations_manager')}
									disabled={!writable || busy !== ''}
									onclick={() => setStatus('retired')}>Retire</button
								>
							{/if}
						{/if}
					</span>
				{/snippet}
				<div class="stack">
					{#if !writable}<p class="muted">{writeHint('integrations_manager')}</p>{/if}
					<div class="t12-top">
						<div>
							<h3 class="label t12-sub">Gate — before any route may use this version</h3>
							<StepFlow steps={lifecycle(cur)} label="Template lifecycle" />
						</div>
						<div class="t12-needs" data-testid="t12-needs">
							<h3 class="label t12-sub">What connecting it needs</h3>
							<dl>
								<dt>Transport</dt>
								<dd data-testid="t12-transport">
									{transportLabel(draft.transport)} <span class="mono muted">{draft.transport}</span>
								</dd>
								<dt>Auth</dt>
								<dd>{AUTH_T12[draft.auth_kind]} <span class="mono muted">{draft.auth_kind}</span></dd>
								<dt>Secrets</dt>
								<dd>
									{#each draft.credential_schema.filter((c) => c.secret) as c (c.name)}<span
											class="mono t12-tag">{c.name}</span
										>{:else}<span class="muted">none</span>{/each}
									<span class="cell-sub">write-only, encrypted per tenant; never shown again</span>
								</dd>
								<dt>Config</dt>
								<dd>
									{#each draft.credential_schema.filter((c) => !c.secret) as c (c.name)}<span
											class="mono t12-tag">{c.name}</span
										>{:else}<span class="muted">none</span>{/each}
									<span class="cell-sub">stored on the system, visible to admins</span>
								</dd>
								<dt>Env IDs</dt>
								<dd>
									{#each ids as id (id)}<span class="mono t12-tag">{id}</span>{:else}<span class="muted"
											>none</span
										>{/each}
									<span class="cell-sub">per system — e.g. a team or queue name</span>
								</dd>
								<dt>Base URL</dt>
								<dd>
									<span class="cell-sub"
										>set per system; the only host a request may reach (SSRF-guarded)</span
									>
								</dd>
							</dl>
						</div>
					</div>

					{#if canImportMcp(draft)}
						{@const systems = pinnedSystems(usage?.pins ?? [], draft.key, draft.version)}
						<div class="t12-import" data-testid="t12-mcp-import">
							<h3 class="label t12-sub">Import tools from MCP</h3>
							<p class="muted">
								Reads the tools of the MCP server a system points at and writes one operation per tool to this
								draft. Pick a system pinned to this version.
							</p>
							{#if dirty}<p class="muted">Save the draft first — an import replaces its operations.</p>{/if}
							{#if systems.length}
								<div class="row">
									<select id="mcp-sys" class="input" aria-label="System" bind:value={impSystem}>
										<option value="">Choose a system…</option>
										{#each systems as s (s.instance_id)}
											<option value={s.instance_id}>{s.label} · {s.env} · {s.instance_id}</option>
										{/each}
									</select>
									<button
										class="btn"
										type="button"
										{...writeAttrs(session, 'integrations_manager')}
										disabled={!writable || busy !== '' || importing || dirty || !impSystem}
										onclick={importTools}>{importing ? 'Importing…' : 'Import tools from MCP'}</button
									>
								</div>
							{:else}
								<p class="muted">
									No system pins this version yet — add one on <a href="/admin/connections/systems">Systems</a
									>
									(its base URL is the MCP server).
								</p>
							{/if}
							{#if impErr}<p class="s-err" role="alert">{impErr}</p>{/if}
							{#if impResult}
								{@const r = impResult}
								<div class="stack" data-testid="t12-mcp-result">
									{#if r.ops.length}
										<p class="muted" data-testid="t12-mcp-imported">{importedLine(r)}</p>
										<ul class="errs">
											{#each r.ops as o (o.op)}
												<li>
													<Chip text={o.action} variant={o.action === 'created' ? 'ok' : 'warn'} />
													<span class="mono">{o.op}</span>
													{#if o.own}<Chip text="your op" variant="accent" />{/if}
													<span class="muted"
														>← tool <span class="mono">{o.tool}</span> · {o.side_effect} · {o.inputs.length}
														input{o.inputs.length === 1 ? '' : 's'}</span
													>
												</li>
											{/each}
										</ul>
									{:else}
										<p class="muted">No operations were written.</p>
									{/if}
									{#if r.invalid_operations.length}
										<Callout kind="err" title="Invalid operations — {r.invalid_operations.length}">
											<ul class="errs">
												{#each r.invalid_operations as m (m.key)}
													<li>
														<span class="mono">{m.key}</span>
														<ul>
															{#each m.issues as i, n (n)}<li>{issueLine(i)}</li>{/each}
														</ul>
													</li>
												{/each}
											</ul>
										</Callout>
									{/if}
									{#if r.skipped.length}
										<p class="muted">Skipped: {r.skipped.join('; ')}</p>
									{/if}
								</div>
							{/if}
						</div>
					{/if}

					{#if draft.status !== 'draft'}
						<Callout kind="info" title="{draft.status === 'active' ? 'Active' : 'Retired'} — read-only">
							A published version never changes. Start a new draft version to edit; pinned systems move to it
							by re-pinning on their system page.
						</Callout>
					{/if}
					{#if err}<p class="s-err" role="alert">{err}</p>{/if}
					{#if cur.validation?.errors.length}
						<Callout kind="err" title="Validation failed — {cur.validation.errors.length} error(s)">
							<ul class="errs">
								{#each cur.validation.errors as e, i (i)}
									<li>
										<span class="mono">{e.code}</span> <span class="muted mono">{e.path}</span> — {e.message}
									</li>
								{/each}
							</ul>
						</Callout>
					{/if}

					<div>
						<div class="t12-ops-head">
							<h3 class="label t12-sub">Operations it implements ({draft.ops.length})</h3>
							{#if diff?.base}
								<span class="row t12-diff" data-testid="t12-diff">
									<span class="muted">vs @{diff.base}:</span>
									{#if diff.added.length}<Chip text="+{diff.added.length} new" variant="ok" />{/if}
									{#if diff.changed.length}<Chip text="{diff.changed.length} changed" variant="warn" />{/if}
									{#if diff.removed.length}<Chip text="−{diff.removed.length} removed" variant="err" />{/if}
									<span class="muted">{diff.same} unchanged</span>
								</span>
							{/if}
						</div>
						<p class="muted t12-hint">
							Each maps one canonical operation onto a request and a response mapping. Typed placeholders only
							—
							<span class="mono"
								>{'{{canonical.*}} {{env.*}} {{config.*}} {{secret.*}} {{step.idempotency_key}}'}</span
							>; a closed transform list; no code.
						</p>
						{#if diff?.removed.length}
							<p class="s-err t12-hint">
								Removed vs @{diff.base}: <span class="mono">{diff.removed.join(', ')}</span>
							</p>
						{/if}
						<div class="t12-ops">
							{#each draft.ops as o, i (i)}
								{@const def = defOf(o)}
								{@const k = opKey(o)}
								{@const opErrs = errFor(cur, i)}
								{@const pre = preFor(i)}
								{@const isNew = !!diff?.base && diff.added.includes(k)}
								{@const isChanged = !!diff?.base && diff.changed.includes(k)}
								<details
									class="t12-op"
									class:bad={opErrs.length + pre.length > 0}
									open={editable && (isNew || pre.length > 0)}
								>
									<summary>
										<span class="t12-op-key">
											<span class="mono">{k}</span>
											{#if def}<Chip
													text={def.side_effect}
													variant={def.side_effect === 'R' ? 'muted' : 'work'}
													title={def.side_effect === 'R' ? 'Read' : 'Write / side effect'}
												/>{/if}
											{#if isNew}<Chip text="new" variant="ok" />{/if}
											{#if isChanged}<Chip text="changed" variant="warn" />{/if}
										</span>
										<span class="mono t12-op-call">{o.method} {o.path}</span>
										<span class="t12-op-meta">
											<span class="muted">{idempotencyLabel(o.idempotency)}</span>
											{#if routed.get(k)?.length}
												<span class="cell-sub"
													>routed: {routed
														.get(k)!
														.map(
															(r) =>
																`${r.job_label} · ${r.env}${pinVer(r.instance_id) === cur.version ? '' : ` (via @${pinVer(r.instance_id)})`}`,
														)
														.join(', ')}</span
												>
											{/if}
											{#if opErrs.length + pre.length}
												<Chip
													text="{opErrs.length || pre.length} issue{(opErrs.length || pre.length) === 1
														? ''
														: 's'}"
													variant="err"
												/>
											{/if}
										</span>
									</summary>
									<div class="t12-op-body">
										<div class="form-grid">
											<div class="field">
												<label for="op-{i}-key">Operation</label>
												<input
													id="op-{i}-key"
													class="input mono"
													list="opdefs"
													value={k}
													onchange={(e) => setOpKey(o, e.currentTarget.value)}
													disabled={!editable}
												/>
											</div>
											<div class="field">
												<label for="op-{i}-m">Method</label><input
													id="op-{i}-m"
													class="input mono"
													bind:value={o.method}
													disabled={!editable}
												/>
											</div>
											<div class="field">
												<label for="op-{i}-idem">Idempotency</label>
												<select
													id="op-{i}-idem"
													class="input"
													value={o.idempotency.kind}
													onchange={(e) => setIdem(o, e.currentTarget.value as Idempotency['kind'])}
													disabled={!editable}
												>
													<option value="nat">natural (reads)</option>
													<option value="header">header</option>
													<option value="dup_search">dup_search</option>
													<option value="none">none</option>
												</select>
											</div>
										</div>
										<div class="t12-own-def" data-testid="t12-own-def-{i}">
											<label class="check"
												><input
													type="checkbox"
													checked={!!o.def}
													onchange={(e) => setOwnDef(o, e.currentTarget.checked)}
													disabled={!editable}
												/> Own definition (an operation the platform catalogue does not define)</label
											>
											{#if o.def}
												<div class="form-grid">
													<div class="field">
														<label for="op-{i}-se">Side effect</label>
														<select
															id="op-{i}-se"
															class="input"
															bind:value={o.def.side_effect}
															disabled={!editable}
														>
															<option value="R">R · reads only</option>
															<option value="W">W · writes (reversible)</option>
															<option value="X">X · irreversible write</option>
															<option value="P">P · payment</option>
														</select>
													</div>
													<div class="field">
														<label for="op-{i}-in">Input fields (comma separated)</label>
														<input
															id="op-{i}-in"
															class="input mono"
															value={o.def.input_fields.join(', ')}
															onchange={(e) => o.def && (o.def.input_fields = csv(e.currentTarget.value))}
															disabled={!editable}
														/>
													</div>
													<div class="field">
														<label for="op-{i}-out">Output fields (comma separated)</label>
														<input
															id="op-{i}-out"
															class="input mono"
															value={o.def.output_fields.join(', ')}
															onchange={(e) => o.def && (o.def.output_fields = csv(e.currentTarget.value))}
															disabled={!editable}
														/>
													</div>
												</div>
												<div class="field">
													<label for="op-{i}-desc">Description</label>
													<input
														id="op-{i}-desc"
														class="input"
														bind:value={o.def.description}
														disabled={!editable}
													/>
												</div>
											{/if}
										</div>
										<div class="field">
											<label for="op-{i}-p">Path</label><input
												id="op-{i}-p"
												class="input mono"
												bind:value={o.path}
												disabled={!editable}
											/>
										</div>
										<div class="field">
											<label for="op-{i}-r"
												>Request template{def ? ` — inputs: ${def.input_fields.join(', ')}` : ''}</label
											>
											<textarea
												id="op-{i}-r"
												class="input mono"
												rows="3"
												bind:value={o.request}
												disabled={!editable}></textarea>
										</div>
										<div class="tbl-wrap">
											<table class="tbl dense">
												<thead
													><tr
														><th>Response JSONPath</th><th>Canonical field</th><th>Transform</th><th></th></tr
													></thead
												>
												<tbody>
													{#each o.field_map as f, j (j)}
														<tr>
															<td
																><input
																	class="input mono"
																	aria-label="Path {j + 1} of {k}"
																	bind:value={f.path}
																	disabled={!editable}
																/></td
															>
															<td>
																<select
																	class="input mono"
																	aria-label="Field {j + 1} of {k}"
																	bind:value={f.field}
																	disabled={!editable}
																>
																	{#if !def?.output_fields.includes(f.field)}<option value={f.field}
																			>{f.field || '—'} (unknown)</option
																		>{/if}
																	{#each def?.output_fields ?? [] as of (of)}<option value={of}>{of}</option
																		>{/each}
																</select>
															</td>
															<td>
																<select
																	class="input mono"
																	aria-label="Transform {j + 1} of {k}"
																	bind:value={f.transform}
																	disabled={!editable}
																>
																	{#if !transformAllowed(f.transform)}<option value={f.transform}
																			>{f.transform} (not allowed)</option
																		>{:else if !(TRANSFORMS as readonly string[]).includes(f.transform)}<option
																			value={f.transform}>{f.transform}</option
																		>{/if}
																	{#each TRANSFORMS as tr (tr)}<option value={tr}>{tr}</option>{/each}
																</select>
															</td>
															<td
																>{#if editable}<button
																		class="btn sm"
																		type="button"
																		onclick={() => o.field_map.splice(j, 1)}>Remove</button
																	>{/if}</td
															>
														</tr>
													{/each}
												</tbody>
											</table>
										</div>
										{#if editable}
											<span class="row">
												<button
													class="btn sm"
													type="button"
													onclick={() =>
														o.field_map.push({
															path: '$.',
															field: def?.output_fields[0] ?? '',
															transform: 'none',
														})}>Add mapping</button
												>
												<button class="btn sm danger" type="button" onclick={() => draft?.ops.splice(i, 1)}
													>Remove operation</button
												>
											</span>
										{/if}
										{#if opErrs.length}
											<ul class="errs s-err">
												{#each opErrs as e, n (n)}<li>{e.message}</li>{/each}
											</ul>
										{:else if pre.length}
											<ul class="errs s-err" aria-label="Pre-check issues">
												{#each pre as e, n (n)}<li>
														<span class="mono">{e.code}</span> — {e.message}
													</li>{/each}
											</ul>
										{/if}
									</div>
								</details>
							{/each}
						</div>
						{#if editable}<button class="btn t12-add" type="button" onclick={addOp}>Add operation</button
							>{/if}
						<datalist id="opdefs"
							>{#each defs as d (opKey(d))}<option value={opKey(d)}></option>{/each}</datalist
						>
					</div>

					<div class="field" data-testid="t12-task-tools">
						<label for="t12-task-tools">Task tools (comma or space separated op keys)</label>
						<input
							id="t12-task-tools"
							class="input mono"
							bind:value={taskToolsText}
							placeholder="projects.execute, tasks.answer"
							disabled={!editable}
						/>
						<span class="cell-sub"
							>Operations on this template that run as durable MCP tasks (up to 16); each must be one it
							implements.</span
						>
					</div>

					<details class="t12-op t12-cred" open={editable}>
						<summary>
							<span class="t12-op-key"
								><span class="label">Credential schema</span>
								<span class="muted">{draft.credential_schema.length} field(s) a system fills in</span></span
							>
						</summary>
						<div class="t12-op-body">
							<div class="tbl-wrap">
								<table class="tbl dense">
									<thead><tr><th>Field</th><th>Label</th><th>Secret</th><th></th></tr></thead>
									<tbody>
										{#each draft.credential_schema as f, i (i)}
											<tr>
												<td
													><input
														class="input mono"
														aria-label="Field name {i + 1}"
														bind:value={f.name}
														disabled={!editable}
													/></td
												>
												<td
													><input
														class="input"
														aria-label="Field label {i + 1}"
														bind:value={f.label}
														disabled={!editable}
													/></td
												>
												<td
													><label class="row"
														><input type="checkbox" bind:checked={f.secret} disabled={!editable} /> x-secret</label
													></td
												>
												<td
													>{#if editable}<button
															class="btn sm"
															type="button"
															onclick={() => draft?.credential_schema.splice(i, 1)}>Remove</button
														>{/if}</td
												>
											</tr>
										{/each}
									</tbody>
								</table>
							</div>
							{#if editable}<button
									class="btn sm"
									type="button"
									onclick={() => draft?.credential_schema.push({ name: '', label: '', secret: true })}
									>Add field</button
								>{/if}
						</div>
					</details>
				</div>
			</Card>
		{/if}
	{/snippet}
</LoadState>

<Drawer bind:open={creating} title="New tenant connector template">
	<form
		class="stack"
		onsubmit={(e) => {
			e.preventDefault();
			void create();
		}}
	>
		<p class="muted">
			Check the platform catalogue on <a href="/admin/connections/systems">Systems</a> first — a platform template
			is tested and maintained for you.
		</p>
		<div class="field">
			<label for="nt-key">Key</label><input
				id="nt-key"
				class="input mono"
				bind:value={nf.key}
				placeholder="lumen_erp"
				required
			/>
		</div>
		<div class="field">
			<label for="nt-label">Label</label><input id="nt-label" class="input" bind:value={nf.label} />
		</div>
		<div class="field">
			<label for="nt-transport">Transport</label>
			<select
				id="nt-transport"
				class="input"
				bind:value={nf.transport}
				onchange={() => {
					if (nf.transport === 'rest' && (nf.auth_kind === 'none' || nf.auth_kind === 'licence_bearer'))
						nf.auth_kind = 'api_key_header';
				}}
			>
				<option value="rest">REST — declarative calls on generic REST</option>
				<option value="mcp">MCP — an MCP server at the system's base URL</option>
			</select>
		</div>
		<div class="form-grid">
			<div class="field">
				<label for="nt-kind">Kind</label>
				<select id="nt-kind" class="input" bind:value={nf.kind}>
					<option value="custom">custom — the definition of one in-house system</option>
					<option value="generic">generic — a reusable REST shape</option>
				</select>
			</div>
			<div class="field">
				<label for="nt-auth">auth_kind</label>
				<select id="nt-auth" class="input" bind:value={nf.auth_kind}>
					<option value="api_key_header">api_key_header</option>
					<option value="basic">basic</option>
					<option value="bearer_static">bearer_static</option>
					<option value="oauth2_client_credentials">oauth2_client_credentials</option>
					<option value="mtls">mtls</option>
					{#if nf.transport === 'mcp'}<option value="none">none — the platform MCP server</option>{/if}
					{#if nf.transport === 'mcp'}<option value="licence_bearer"
							>licence_bearer — a platform licence, minted per call</option
						>{/if}
				</select>
			</div>
		</div>
		<div class="field">
			<label for="nt-desc">Description</label><textarea
				id="nt-desc"
				class="input"
				rows="2"
				bind:value={nf.description}></textarea>
		</div>
		<p class="muted">
			Starts as draft <span class="mono">@1</span> on
			<span class="mono">{nf.transport === 'mcp' ? 'an MCP server' : 'generic_rest@2'}</span>.
		</p>
		{#if createErr}<p class="s-err" role="alert">{createErr}</p>{/if}
		<div class="actions">
			<button class="btn primary" type="submit" {...writeAttrs(session, 'integrations_manager')}
				>Create draft</button
			>
			<button class="btn" type="button" onclick={() => (creating = false)}>Cancel</button>
		</div>
	</form>
</Drawer>

<style>
	.t12-h {
		font-size: 15px;
		margin: 0 0 8px;
	}
	.t12-empty {
		border: 1px dashed var(--line);
		border-radius: var(--r);
		padding: 16px;
		display: grid;
		gap: 10px;
		justify-items: start;
	}
	.t12-fams {
		display: grid;
		gap: 10px;
		margin-bottom: 16px;
	}
	.t12-fam {
		border: 1px solid var(--line);
		border-radius: var(--r);
		background: var(--panel);
		padding: 12px 14px;
		display: grid;
		grid-template-columns: minmax(0, 1.1fr) minmax(0, 1.3fr) minmax(0, 1.2fr);
		gap: 14px;
		align-items: start;
	}
	.t12-fam.on {
		border-color: color-mix(in srgb, var(--accent) 45%, var(--line));
	}
	.t12-fam-head > div {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		align-items: center;
	}
	.t12-desc {
		margin: 4px 0 0;
		font-size: 12.5px;
		line-height: 1.4;
	}
	.t12-chain {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-wrap: wrap;
		gap: 18px;
	}
	.t12-chain li {
		position: relative;
	}
	.t12-chain li + li::before {
		content: '→';
		position: absolute;
		left: -14px;
		top: 50%;
		transform: translateY(-50%);
		color: var(--faint);
		font-size: 12px;
	}
	.t12-ver {
		display: grid;
		gap: 2px;
		text-align: left;
		padding: 7px 10px;
		border-radius: 8px;
		border: 1px solid var(--line);
		background: var(--sunk);
		color: inherit;
		font: inherit;
		cursor: pointer;
		min-width: 128px;
	}
	.t12-ver.draft {
		border-style: dashed;
	}
	.t12-ver.retired {
		opacity: 0.75;
	}
	.t12-ver.sel {
		border-color: var(--accent);
		box-shadow: 0 0 0 1px var(--accent);
		background: color-mix(in srgb, var(--accent) 8%, var(--panel));
	}
	.t12-ver-id {
		font-weight: 600;
		font-size: 13px;
	}
	.t12-ver-sub {
		font-size: 11.5px;
		color: var(--muted);
	}
	.t12-use {
		display: grid;
		gap: 10px;
		font-size: 12.5px;
	}
	.t12-pins {
		list-style: none;
		margin: 4px 0;
		padding: 0;
		display: grid;
		gap: 3px;
	}
	.t12-pins li {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		align-items: center;
	}
	.t12-up {
		display: grid;
		gap: 3px;
		border-left: 3px solid var(--warn);
		padding-left: 8px;
	}
	.t12-up.ready {
		border-left-color: var(--ok);
	}
	.t12-up-line {
		display: flex;
		gap: 6px;
		align-items: center;
		flex-wrap: wrap;
	}
	.t12-top {
		display: grid;
		grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
		gap: 18px;
	}
	.t12-sub {
		margin: 0 0 8px;
	}
	.t12-needs dl {
		margin: 0;
		display: grid;
		grid-template-columns: 76px minmax(0, 1fr);
		gap: 8px 10px;
		font-size: 13px;
	}
	.t12-needs dt {
		color: var(--muted);
	}
	.t12-needs dd {
		margin: 0;
	}
	.t12-tag {
		display: inline-block;
		padding: 1px 6px;
		margin: 0 4px 2px 0;
		border-radius: 4px;
		background: var(--sunk);
		border: 1px solid var(--line-2);
	}
	.t12-ops-head {
		display: flex;
		flex-wrap: wrap;
		justify-content: space-between;
		gap: 8px;
		align-items: baseline;
	}
	.t12-diff {
		font-size: 12.5px;
		gap: 6px;
	}
	.t12-hint {
		margin: 0 0 8px;
		font-size: 12.5px;
	}
	.t12-ops {
		display: grid;
		gap: 6px;
	}
	.t12-op {
		border: 1px solid var(--line);
		border-radius: 8px;
		min-width: 0;
	}
	.t12-op.bad {
		border-color: color-mix(in srgb, var(--err) 50%, var(--line));
	}
	.t12-op summary {
		display: grid;
		grid-template-columns: minmax(0, 1.1fr) minmax(0, 1.6fr) minmax(0, 1.2fr);
		gap: 10px;
		align-items: center;
		padding: 8px 12px 8px 24px;
		cursor: pointer;
		list-style: none;
		position: relative;
	}
	.t12-op summary::-webkit-details-marker {
		display: none;
	}
	.t12-op summary::before {
		content: '▸';
		position: absolute;
		left: 9px;
		top: 50%;
		transform: translateY(-50%);
		color: var(--faint);
		font-size: 10px;
	}
	.t12-op[open] summary::before {
		content: '▾';
	}
	.t12-op[open] summary {
		border-bottom: 1px solid var(--line-2);
	}
	.t12-op-key {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		align-items: center;
		min-width: 0;
	}
	.t12-op-call {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		color: var(--muted);
	}
	.t12-op-meta {
		display: flex;
		flex-wrap: wrap;
		gap: 4px 8px;
		align-items: center;
		font-size: 12px;
	}
	.t12-op-meta .cell-sub {
		flex-basis: 100%;
	}
	.t12-op-body {
		padding: 10px 12px 12px;
		display: grid;
		gap: 10px;
	}
	.t12-cred summary {
		grid-template-columns: minmax(0, 1fr);
	}
	.t12-add {
		margin-top: 8px;
	}
	.errs {
		margin: 4px 0 0;
		padding-left: 18px;
		font-size: 12.5px;
	}
	@media (max-width: 1100px) {
		.t12-fam {
			grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
		}
		.t12-fam-head {
			grid-column: 1 / -1;
		}
	}
	@media (max-width: 760px) {
		.t12-fam,
		.t12-top,
		.t12-op summary {
			grid-template-columns: minmax(0, 1fr);
		}
		.t12-op-call {
			white-space: normal;
			overflow-wrap: anywhere;
		}
	}
</style>
