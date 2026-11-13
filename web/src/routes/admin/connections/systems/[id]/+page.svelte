<script lang="ts">
	// T11 System detail (doc 10 §4.2/§4.5 T3, doc 11 §6.1, doc 12 §7–§9). One connected system is a translation chain:
	// jobs route canonical operations here; the pinned template + this tenant's overrides and env IDs translate each into
	// a vendor call made with the sealed credential. The page answers "can my agent rely on this system, and what do I
	// fix?": the chain with health per link, a readiness verdict + what needs you, then per-op health (T3 + 30-day
	// outcomes mapped to canonical errors), the upgrade decision, mapping, and the connection itself.
	// D69: the system itself (connection, credential, pin) is a per-environment resource and saves live; the field
	// mapping overrides are design — saved to the draft of every job, live when each job publishes.
	import '$lib/phone-stack.css';
	import { page } from '$app/state';
	import { api, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { canWrite, writeAttrs, writeHint } from '$lib/auth/roles';
	import { Resource } from '$lib/load.svelte';
	import { toast } from '$lib/ui/toaster.svelte';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import JsonView from '$lib/ui/JsonView.svelte';
	import Tabs from '$lib/ui/Tabs.svelte';
	import LiveNote from '$lib/release/LiveNote.svelte';
	import TenantDraftBar from '$lib/release/TenantDraftBar.svelte';
	import OutcomeChip from '$lib/connectors/OutcomeChip.svelte';
	import {
		NO_RATE_LIMIT,
		hasRateLimit,
		mergeProbeIds,
		missingProbeKeys,
		probeKeys,
		probeLabel,
	} from '$lib/connectors/T11-limits';
	import { checkTarget, jsonPath, shortFp } from '$lib/connectors/ops';
	import { t3Summary } from '$lib/connectors/instances';
	import {
		byObject,
		opRows,
		READINESS,
		readinessMix,
		todosOf,
		verdictOf,
		type T11Tab,
		type Todo,
	} from '$lib/connectors/T11-health';
	import T11Chain from './T11Chain.svelte';
	import T11Upgrade from './T11Upgrade.svelte';
	import T11Maps from './T11Maps.svelte';
	import type { ContractResult, SystemDetail, SystemInsight } from '$lib/api/types/connectors';
	import type { OperationCall, OperationStat } from '$lib/api/types/runtime';

	const id = $derived(page.params.id ?? '');
	const res = new Resource<SystemDetail>(
		() => api().connectorInstance(id),
		() => false,
	);
	// Secondary reads: the page still renders (with "—") when one of them is unavailable.
	let insight = $state<SystemInsight | null>(null);
	let stats = $state<OperationStat[]>([]);
	let calls = $state<OperationCall[]>([]);
	let tab = $state<T11Tab>('ops');
	let tabFor = '';

	$effect(() => {
		void id;
		void load();
	});
	const writable = $derived(canWrite(session, 'integrations_manager'));

	// Local form state, re-seeded on every successful read.
	let cfg = $state({ label: '', base_url: '', env_ids: '', max_rps: '' });
	let cfgErr = $state('');
	let cred = $state<Record<string, string>>({});
	let credErr = $state('');
	let okFor = $state('');
	let okChecked = $state(false);
	let running = $state('');
	let mapOp = $state('');
	let paths = $state<Record<string, string>>({});
	let mapErr = $state<Record<string, string>>({});
	let mapTick = $state(0);
	let repinning = $state(false);
	let probeVals = $state<Record<string, string>>({});
	let probeSaved = $state(false);

	async function load() {
		// Secondary reads run alongside; the page renders as soon as the system itself is read.
		const secondary = Promise.all([
			api()
				.systemInsight(id)
				.catch(() => null),
			api()
				.operationStats()
				.then((p) => p.items)
				.catch(() => [] as OperationStat[]),
			api()
				.operationCalls()
				.then((p) => p.items)
				.catch(() => [] as OperationCall[]),
		]).then(([ins, st, cl]) => {
			insight = ins;
			stats = st;
			calls = cl;
		});
		await res.refresh();
		const v = res.value;
		if (v.state !== 'ok') return;
		const i = v.data.instance;
		// Open on the connection when the system cannot be called at all; otherwise on its operations.
		if (tabFor !== id) {
			tabFor = id;
			const asked = page.url?.searchParams.get('tab') as T11Tab | null;
			tab =
				asked && TABS.includes(asked)
					? asked
					: i.status === 'auth_failed' ||
						  i.status === 'invalid_target' ||
						  (!i.platform && !i.stub && i.credential_required !== false && !i.credential.set)
						? 'connection'
						: 'ops';
		}
		cfg = {
			label: i.label,
			base_url: i.base_url,
			env_ids: Object.entries(i.env_ids)
				.map(([k, x]) => `${k} = ${x}`)
				.join('\n'),
			max_rps: i.max_rps == null ? '' : String(i.max_rps),
		};
		probeVals = Object.fromEntries(Object.entries(i.env_ids).filter(([k]) => k.startsWith('probe.')));
		cred = Object.fromEntries(
			v.data.template.credential_schema.map((f) => [f.name, f.secret ? '' : (i.env_ids[f.name] ?? '')]),
		);
		if (!mapOp || !v.data.template.ops.some((o) => o.op === mapOp))
			mapOp = i.sample?.op ?? v.data.template.ops.find((o) => o.field_map.length)?.op ?? '';
		seedPaths(v.data);
		await secondary;
	}
	function seedPaths(d: SystemDetail) {
		const op = d.template.ops.find((o) => o.op === mapOp);
		paths = Object.fromEntries(
			(op?.field_map ?? []).map((f) => [
				f.field,
				d.instance.overrides.find((x) => x.op === mapOp && x.field === f.field)?.path ?? f.path,
			]),
		);
		mapErr = {};
	}

	function parseIds(text: string): Record<string, string> {
		const out: Record<string, string> = {};
		for (const line of text.split('\n')) {
			const m = /^\s*([A-Za-z0-9_.-]+)\s*=\s*(.+?)\s*$/.exec(line);
			if (m) out[m[1]!] = m[2]!;
		}
		return out;
	}
	const urlProblem = $derived(
		res.value.state === 'ok' && !res.value.data.instance.stub && !res.value.data.instance.platform
			? checkTarget(cfg.base_url)
			: null,
	);

	async function saveProbes(saved: Record<string, string>) {
		const ok = await saveCfg(mergeProbeIds(saved, $state.snapshot(probeVals)));
		if (ok) probeSaved = true;
	}
	function focusProbe(k: string) {
		const el = document.getElementById(`probe-${k}`) as HTMLInputElement | null;
		el?.scrollIntoView?.({ block: 'center' });
		el?.focus();
	}
	async function saveCfg(envIds: Record<string, string> = parseIds(cfg.env_ids)): Promise<boolean> {
		cfgErr = '';
		try {
			await api().updateConnectorInstance(id, {
				label: cfg.label,
				base_url: cfg.base_url,
				env_ids: envIds,
				max_rps: cfg.max_rps.trim() ? Number(cfg.max_rps) : null,
			});
			toast('Configuration saved.');
			await load();
			return true;
		} catch (e) {
			const x = e as { code?: string; message: string };
			cfgErr = `${x.code ?? 'error'} — ${x.message}`;
			return false;
		}
	}
	async function saveCred() {
		credErr = '';
		try {
			const s = await api().setConnectorCredential(id, $state.snapshot(cred));
			toast(`Credential stored · fingerprint ${shortFp(s.fingerprint)}. The value is never shown again.`);
			await load();
		} catch (e) {
			const x = e as { code?: string; message: string };
			credErr = `${x.code ?? 'error'} — ${x.message}`;
		}
	}
	async function run(r: Pick<ContractResult, 'op'>, prodOk = false) {
		running = r.op;
		await guard(
			() => api().runContractTest(id, r.op, { prod_write_ok: prodOk }),
			() => `Contract test ran for ${r.op}.`,
			'Test refused',
		);
		running = '';
		okFor = '';
		okChecked = false;
		await load();
	}
	async function runReads(d: SystemDetail) {
		running = '*';
		for (const o of d.template.ops.filter((x) => x.side_effect === 'R')) {
			try {
				await api().runContractTest(id, o.op);
			} catch (e) {
				toast(`Stopped: ${(e as Error).message}`, 'err');
				break;
			}
		}
		running = '';
		toast('Read probes finished.');
		await load();
	}
	async function repin(version: string) {
		repinning = true;
		await guard(
			() => api().repinConnectorInstance(id, version),
			`Re-pinned to @${version}; contract tests re-ran.`,
			'Not re-pinned',
		);
		repinning = false;
		await load();
	}
	async function fetchSample() {
		await guard(
			() => api().fetchSampleResponse(id, mapOp),
			'Sample response fetched from the system.',
			'No sample',
		);
		await load();
	}
	function tryPath(d: SystemDetail, field: string): string {
		if (!d.instance.sample || d.instance.sample.op !== mapOp) return '';
		const r = jsonPath(d.instance.sample.body, paths[field] ?? '');
		return r.ok ? JSON.stringify(r.value) : `✗ ${r.error}`;
	}
	async function saveOverride(field: string) {
		mapErr = { ...mapErr, [field]: '' };
		try {
			await api().saveMappingOverride(id, { op: mapOp, field, path: paths[field] ?? '' });
			toast(`Saved to draft: ${mapOp} → ${field} override. Test it, then publish.`);
			mapTick++;
			await load();
		} catch (e) {
			const x = e as { code?: string; message: string };
			mapErr = { ...mapErr, [field]: `${x.code ?? 'error'} — ${x.message}` };
		}
	}
	async function revert(field: string) {
		await guard(
			() => api().revertMappingOverride(id, mapOp, field),
			`Saved to draft: ${field} back to the template default. Test it, then publish.`,
			'Not reverted',
		);
		mapTick++;
		await load();
	}
	function act(t: Todo, d: SystemDetail) {
		const a = t.action;
		if (!a || a.kind === 'link') return;
		if (a.kind === 'tab') tab = a.tab;
		else if (a.op === '*') void runReads(d);
		else void run({ op: a.op });
	}
	const handlingOf = (code: string) =>
		insight?.taxonomy.find((t) => t.code === code.split(':')[0])?.handling ?? null;
	const fmtAt = (s: string | null) => (s ? s.slice(0, 16).replace('T', ' ') : '—');
	const pct = (n: number, of: number) => (of ? `${((n / of) * 100).toFixed(1)} %` : '—');
	const TABS: T11Tab[] = ['ops', 'upgrade', 'mapping', 'connection'];
	const ENV_NOTE = { prod: 'production', test: 'test', all: 'prod + test' } as const;
	const TONE_KIND = { err: 'err', warn: 'gap', info: 'info', ok: 'info' } as const;
</script>

<PageHead
	id="T11"
	title={res.value.state === 'ok' ? res.value.data.instance.label : id}
	sub={res.value.state === 'ok'
		? `${id} · ${ENV_NOTE[res.value.data.instance.env]} system · ${res.value.data.template.label}${res.value.data.template.vendor ? ` (${res.value.data.template.vendor})` : ''}`
		: id}
>
	{#snippet actions()}
		<a class="btn" href="/admin/connections/systems">All systems</a>
	{/snippet}
</PageHead>

<LoadState resource={res}>
	{#snippet children(d)}
		{@const i = d.instance}
		{@const t3 = t3Summary(i)}
		{@const rows = opRows(d, insight, stats)}
		{@const verdict = verdictOf(d, rows)}
		{@const todos = todosOf(d, rows, insight)}
		{@const mine = calls.filter((c) => c.instance === i.id)}
		{@const callTotal = rows.reduce((n, r) => n + (r.calls ?? 0), 0)}
		{@const escalated = rows.reduce((n, r) => n + r.escalated, 0)}
		{@const replied = rows.reduce((n, r) => n + r.replied, 0)}
		{@const callsJob = d.used_by[0]?.job ?? null}

		{#if d.template.status === 'draft'}
			<Callout kind="gap" title="Draft template — test routes only">
				{i.template_key}@{i.template_version} is not published yet. This system can be tested and can serve test
				routes, but a prod route to it is refused until the platform publishes the template.
			</Callout>
		{/if}
		{#if i.stub}
			<Callout kind="info" title="Stub instance"
				>Recorded fixtures with fault injection. Allowed on test routes only and never counts for readiness.</Callout
			>
		{/if}
		{#if i.platform}
			<Callout kind="info" title="Platform connector">
				Provisioned by Agent for this tenant. {i.template_key === 'strata_rag'
					? 'Each call uses a licence bundle token minted in memory (TTL ≤ 60 min); nothing is stored here.'
					: 'Runs in-app under row-level security; no credential exists.'}
			</Callout>
		{/if}

		<T11Chain {d} {rows} {insight} />

		<div class="grid g-main t11-decide">
			<Card title="Can your agent rely on it?">
				<p class="t11-verdict t11-v-{verdict.tone}" data-testid="t11-verdict">
					<span class="t11-vmark" aria-hidden="true"
						>{verdict.tone === 'ok' ? '✓' : verdict.tone === 'err' ? '✕' : '!'}</span
					>
					{verdict.headline}
				</p>
				<dl class="t11-kpis">
					<div>
						<dt>Routed ops ready</dt>
						<dd>{verdict.ready}<span class="t11-of"> / {verdict.routed}</span></dd>
					</div>
					<div>
						<dt>Contract tests</dt>
						<dd class="mono t11-t3" data-testid="t3-summary">T3 {t3.green}/{t3.run} green</dd>
						{#if t3.unverified}<dd class="t11-kn">{t3.unverified} write unverified</dd>{/if}
					</div>
					<div>
						<dt>Calls · 30 d</dt>
						<dd>{stats.length || callTotal ? callTotal.toLocaleString('en') : '—'}</dd>
					</div>
					<div>
						<dt>Escalated · 30 d</dt>
						<dd class={escalated ? 's-err' : ''}>{callTotal ? pct(escalated, callTotal) : '—'}</dd>
						{#if callTotal}<dd class="t11-kn">{escalated} system errors · {replied} answered</dd>{/if}
					</div>
				</dl>
				{@const mix = readinessMix(rows)}
				{#if mix.length}
					<div class="t11-ribbon" data-testid="t11-ribbon">
						<span class="label">Routed operations by readiness</span>
						<span
							class="t11-rbar"
							role="img"
							aria-label={mix.map((m) => `${m.count} ${READINESS[m.kind].label}`).join(', ')}
						>
							{#each mix as m (m.kind)}
								<span
									class="t11-r t11-r-{READINESS[m.kind].variant}"
									style:flex-grow={m.count}
									title={m.ops.join(', ')}
								></span>
							{/each}
						</span>
						<ul class="t11-rlegend">
							{#each mix as m (m.kind)}
								<li>
									<span class="t11-rdot t11-r-{READINESS[m.kind].variant}"></span>
									<strong>{m.count}</strong>
									{READINESS[m.kind].label}
									<span class="t11-rops">{m.ops.join(', ')}</span>
								</li>
							{/each}
						</ul>
					</div>
				{/if}
			</Card>
			<Card title="What needs you" sub={todos.length ? undefined : 'Nothing — this system is in order.'}>
				{#if todos.length}
					<ol class="t11-todos" data-testid="t11-todos">
						{#each todos as t (t.key)}
							<li class="t11-todo t11-t-{t.tone}">
								<div class="t11-todo-text">
									<strong>{t.title}</strong>
									<span>{t.body}</span>
								</div>
								{#if t.action?.kind === 'link'}
									<a class="btn sm" href={t.action.href}>{t.action.label}</a>
								{:else if t.action?.kind === 'tab'}
									<button class="btn sm" type="button" onclick={() => act(t, d)}>{t.action.label}</button>
								{:else if t.action}
									<button
										class="btn sm"
										type="button"
										{...writeAttrs(session, 'integrations_manager')}
										disabled={!writable || running !== ''}
										onclick={() => act(t, d)}>{t.action.label}</button
									>
								{/if}
							</li>
						{/each}
					</ol>
				{/if}
			</Card>
		</div>

		<Tabs
			label="System detail"
			bind:active={tab}
			items={[
				['ops', 'Operations', rows.length],
				['upgrade', insight?.upgrade ? `Upgrade · @${insight.upgrade.to}` : 'Version'],
				['mapping', 'Mapping', i.overrides.length || undefined],
				['connection', 'Connection & credential'],
			]}
		>
			<div class="t11-panel">
				{#if tab === 'ops'}
					{@const need = probeKeys(
						rows.flatMap((r) => missingProbeKeys(r.result.code, r.result.note)),
						Object.fromEntries(Object.entries(i.env_ids).filter(([k]) => k.startsWith('probe.'))),
					)}
					{#if need.length}
						<Card
							title="Test records"
							sub="A real record in this system the contract test may read (read-only tests)."
						>
							<div class="stack" data-testid="test-records">
								{#each need as k (k)}
									<div class="field">
										<label for="probe-{k}">{probeLabel(k)}</label>
										<input
											id="probe-{k}"
											class="input"
											bind:value={probeVals[k]}
											disabled={!writable || i.platform}
										/>
									</div>
								{/each}
								{#if !i.platform}
									<div class="actions">
										<button
											class="btn primary"
											type="button"
											{...writeAttrs(session, 'integrations_manager')}
											onclick={() => saveProbes(i.env_ids)}>Save test records</button
										>
										{#if probeSaved}
											<button
												class="btn"
												type="button"
												disabled={!writable || running !== ''}
												onclick={() => runReads(d)}>Run all reads</button
											>
										{/if}
									</div>
								{/if}
							</div>
						</Card>
					{/if}
					<Card
						title="Per-operation health — {ENV_NOTE[i.env]}"
						sub="T3 proves each op against your system; the last 30 days show how real calls ended. Failed calls split into answers the agent gave (user outcomes) and hand-overs (system errors)."
						cls="flush"
					>
						{#snippet actions()}
							<button
								class="btn sm"
								type="button"
								{...writeAttrs(session, 'integrations_manager')}
								disabled={!writable || running !== ''}
								onclick={() => runReads(d)}>{running === '*' ? 'Running…' : 'Run all reads'}</button
							>
						{/snippet}
						<div class="tbl-wrap ph-stack">
							<table class="tbl" data-testid="t3">
								<thead>
									<tr>
										<th>Operation</th>
										<th>Effect</th>
										<th>Readiness</th>
										<th>Contract test (T3)</th>
										<th>Calls · 30 d</th>
										<th>How calls ended</th>
										<th>p95</th>
										<th><span class="t11-sr">Run</span></th>
									</tr>
								</thead>
								<tbody>
									{#each byObject(rows) as g (g.object)}
										<tr class="t11-grp"><th colspan="8" scope="colgroup">{g.object}</th></tr>
										{#each g.rows as r (r.op)}
											{@const prodWrite = i.env === 'prod' && r.side_effect !== 'R'}
											{@const xp = prodWrite && (r.side_effect === 'X' || r.side_effect === 'P')}
											{@const ok = r.calls != null ? r.calls - r.replied - r.escalated : 0}
											<tr>
												<td class="mono">{r.op}</td>
												<td data-label="Effect"
													><span class="t11-eff" title="side effect">{r.side_effect}</span></td
												>
												<td data-label="Readiness">
													<Chip
														text={READINESS[r.readiness].label}
														variant={READINESS[r.readiness].variant}
													/>
													{#if r.routed.length}<span class="cell-sub">routed {r.routed.join(' + ')}</span
														>{/if}
												</td>
												<td class="t11-t3cell" data-label="Contract test">
													<OutcomeChip outcome={r.result.outcome} code={r.result.code} />
													{#if r.result.note}<span class="cell-sub">{r.result.note}</span>{/if}
													{#each missingProbeKeys(r.result.code, r.result.note) as k (k)}
														<button
															type="button"
															class="btn btn-sm"
															data-testid="add-probe-{k}"
															disabled={!writable || i.platform}
															onclick={() => focusProbe(k)}>Set test record</button
														>
													{/each}
													{#if xp && r.insight?.verify_read}<span class="cell-sub"
															>verify_read: {r.insight.verify_read}</span
														>{/if}
													{#if r.result.at}<span class="cell-sub mono">{fmtAt(r.result.at)}</span>{/if}
												</td>
												<td class="mono" data-label="Calls · 30 d"
													>{r.calls == null ? '—' : r.calls.toLocaleString('en')}</td
												>
												<td class="t11-mixcell" data-label="How ended">
													{#if r.calls}
														<span
															class="t11-mix"
															role="img"
															aria-label="{ok} ok, {r.replied} answered, {r.escalated} escalated"
														>
															{#if ok > 0}<span class="t11-m-ok" style:flex-grow={ok}></span>{/if}
															{#if r.replied}<span class="t11-m-reply" style:flex-grow={r.replied}
																></span>{/if}
															{#if r.escalated}<span class="t11-m-esc" style:flex-grow={r.escalated}
																></span>{/if}
														</span>
														{#if r.errors.length}
															<span class="cell-sub">
																{#each r.errors as e, k (e.code)}{k ? ' · ' : ''}<span
																		class={e.handling === 'reply' ? '' : 's-err'}>{e.count} {e.code}</span
																	>{/each}
															</span>
														{:else}
															<span class="cell-sub">no failures</span>
														{/if}
													{:else}
														<span class="muted">—</span>
													{/if}
												</td>
												<td class="mono t11-nw" data-label="p95"
													>{r.p95_ms == null ? '—' : `${r.p95_ms} ms`}</td
												>
												<td>
													{#if xp}
														<span class="muted t11-small">test env only</span>
													{:else if prodWrite}
														{#if okFor === r.op}
															<div class="t11-okbox" role="group" aria-label="Allow a prod write">
																<label class="row"
																	><input type="checkbox" bind:checked={okChecked} /> I allow one tagged test
																	write on {i.base_url}; it is closed afterwards.</label
																>
																<span class="row">
																	<button
																		class="btn sm primary"
																		type="button"
																		disabled={!okChecked || running !== ''}
																		onclick={() => run(r.result, true)}>Run with OK</button
																	>
																	<button class="btn sm" type="button" onclick={() => (okFor = '')}
																		>Cancel</button
																	>
																</span>
															</div>
														{:else}
															<button
																class="btn sm"
																type="button"
																{...writeAttrs(session, 'integrations_manager')}
																disabled={!writable || running !== ''}
																onclick={() => ((okFor = r.op), (okChecked = false))}>Run… (needs OK)</button
															>
														{/if}
													{:else}
														<button
															class="btn sm"
															type="button"
															{...writeAttrs(session, 'integrations_manager')}
															disabled={!writable || running !== ''}
															onclick={() => run(r.result)}>{running === r.op ? 'Running…' : 'Run'}</button
														>
													{/if}
												</td>
											</tr>
										{/each}
									{/each}
								</tbody>
							</table>
						</div>
						<p class="note t11-pad">
							<span class="t11-key t11-m-ok"></span> ok <span class="t11-key t11-m-reply"></span> answered
							(user outcome) <span class="t11-key t11-m-esc"></span> escalated (system error). Reads probe with
							your test identifiers; writes are tagged agent-contract-test and closed; a prod write needs your OK;
							X/P ops run only against a test-env instance. A stub never makes a route ready.
						</p>
					</Card>

					<Card
						title="Recent calls through this system"
						sub="operation_call rows — ids and codes only, never payloads."
					>
						{#snippet actions()}
							{#if callsJob}<a class="btn sm" href="/admin/jobs/{callsJob}/operation-calls">All calls</a>{/if}
						{/snippet}
						{#if mine.length === 0}
							<p class="muted">No calls through this system in the current job yet.</p>
						{:else}
							<ul class="list-plain">
								{#each mine.slice(0, 8) as c (c.call_id)}
									{@const h = c.state === 'ok' ? null : handlingOf(c.outcome)}
									<li>
										<span class="row">
											<span class="mono t11-small">{fmtAt(c.at)}</span>
											<span class="mono">{c.op}</span>
											{#if c.state === 'ok'}
												<Chip text="ok" variant="ok" />
											{:else}
												<Chip
													text={c.outcome}
													variant={c.state === 'denied' ? 'muted' : h === 'reply' ? 'warn' : 'err'}
												/>
												<span class="muted t11-small"
													>{c.state === 'denied'
														? 'denied before the call'
														: h === 'reply'
															? 'agent answered'
															: 'escalated'}</span
												>
											{/if}
										</span>
										<span class="row t11-small">
											{#if c.latency_ms != null}<span class="mono">{c.latency_ms} ms</span>{/if}
											{#if callsJob}<a href="/admin/jobs/{callsJob}/cases/{c.case_id}">{c.case_id}</a
												>{:else}<span>{c.case_id}</span>{/if}
										</span>
									</li>
								{/each}
							</ul>
						{/if}
					</Card>
				{:else if tab === 'upgrade'}
					<Card
						title="Template version"
						sub="This system stays on its pin until you re-pin; re-pinning re-runs the contract tests (doc 11 §6.1)."
					>
						<T11Upgrade {d} {insight} {writable} {repinning} onrepin={repin} />
					</Card>
				{:else if tab === 'mapping'}
					{@const mapped = d.template.ops.filter((o) => o.field_map.length)}
					{#if mapped.length}
						{@const op = d.template.ops.find((o) => o.op === mapOp)}
						<Card
							title="Field mapping — your overrides"
							sub="The template maps each canonical field from a JSONPath. Override one for this system only, tried against a sample response from it. Saved to the draft of every job: test it, then publish."
						>
							<TenantDraftBar
								kinds={['mapping_override']}
								tick={mapTick}
								only={(r) => r.ref.instance === i.id}
								label={(r) => `${r.ref.op} → ${r.ref.field}`}
							/>
							<div class="split">
								<div class="field t11-oppick">
									<label for="map-op">Operation</label>
									<select id="map-op" class="input mono" bind:value={mapOp} onchange={() => seedPaths(d)}>
										{#each mapped as o (o.op)}<option value={o.op}
												>{o.op}{i.overrides.some((x) => x.op === o.op) ? ' · overridden' : ''}</option
											>{/each}
									</select>
								</div>
								<button
									class="btn"
									type="button"
									{...writeAttrs(session, 'integrations_manager')}
									disabled={!writable || !mapOp}
									onclick={fetchSample}>Fetch sample from system</button
								>
							</div>
							<div class="grid g-main t11-mapgrid">
								<div class="tbl-wrap">
									<table class="tbl dense">
										<thead
											><tr><th>Canonical field</th><th>JSONPath</th><th>Value in sample</th><th></th></tr
											></thead
										>
										<tbody>
											{#each op?.field_map ?? [] as f (f.field)}
												{@const ov = i.overrides.find((x) => x.op === mapOp && x.field === f.field)}
												{@const val = tryPath(d, f.field)}
												<tr>
													<td class="mono"
														>{f.field}{#if ov}<Chip text="override" variant="accent" />{/if}</td
													>
													<td>
														<input
															class="input mono t11-path"
															aria-label="JSONPath for {f.field}"
															bind:value={paths[f.field]}
															disabled={!writable}
														/>
														{#if paths[f.field] !== f.path}<span class="cell-sub">default {f.path}</span>{/if}
														{#if mapErr[f.field]}<span class="s-err">{mapErr[f.field]}</span>{/if}
													</td>
													<td class="mono t11-val {val.startsWith('✗') ? 's-err' : ''}">{val || '—'}</td>
													<td>
														<span class="row t11-nowrap">
															<button
																class="btn sm"
																type="button"
																{...writeAttrs(session, 'integrations_manager')}
																disabled={!writable || paths[f.field] === (ov?.path ?? f.path)}
																onclick={() => saveOverride(f.field)}>Save</button
															>
															{#if ov}<button
																	class="btn sm"
																	type="button"
																	{...writeAttrs(session, 'integrations_manager')}
																	onclick={() => revert(f.field)}>Revert</button
																>{/if}
														</span>
													</td>
												</tr>
											{/each}
										</tbody>
									</table>
								</div>
								<div>
									<h3 class="label">Sample response</h3>
									{#if i.sample && i.sample.op === mapOp}
										<p class="muted t11-small">{i.sample.op} · fetched {fmtAt(i.sample.fetched_at)} UTC</p>
										<JsonView value={i.sample.body} />
									{:else}
										<p class="muted">No sample for {mapOp} yet. Fetch one to check paths before saving.</p>
									{/if}
								</div>
							</div>
							{#if i.overrides.length}
								<p class="note">
									{i.overrides.length} override{i.overrides.length === 1 ? '' : 's'} on this system:
									{i.overrides.map((o) => `${o.op} → ${o.field}`).join(', ')}. An override survives a re-pin
									only if the new version still maps that field.
								</p>
							{/if}
						</Card>
					{/if}
					{#if insight}
						<Card title="Statuses and faults, as this pin translates them">
							<T11Maps {insight} vendor={d.template.vendor} />
						</Card>
					{/if}
				{:else}
					<LiveNote
						of="environment"
						what="Connection, credential and pin apply to this system at once; the routes that use it ride the release."
					/>
					<div class="grid g2">
						<Card
							title="Connection"
							sub="Only the base URL host is ever called. Every resolved address passes the SSRF guard."
						>
							<div class="stack">
								<div class="field">
									<label for="c-label">Label</label><input
										id="c-label"
										class="input"
										bind:value={cfg.label}
										disabled={!writable || i.platform}
									/>
								</div>
								<div class="field">
									<label for="c-url">Base URL</label>
									<input
										id="c-url"
										class="input mono"
										bind:value={cfg.base_url}
										disabled={!writable || i.platform || i.stub}
										aria-invalid={!!urlProblem}
									/>
									{#if urlProblem}<span class="s-err">{urlProblem.code}: {urlProblem.message}</span>{/if}
								</div>
								<details class="field">
									<summary>Advanced: raw env ids</summary>
									<label for="c-ids">Env IDs (key = value)</label>
									<span class="cell-sub" data-testid="probe-hint"
										>Contract tests read sample inputs from env ids named <span class="mono"
											>probe.&lt;field&gt;</span
										>.</span
									>
									<textarea
										id="c-ids"
										class="input"
										rows="3"
										bind:value={cfg.env_ids}
										disabled={!writable || i.platform}
									></textarea>
								</details>
								<div class="field">
									<label for="c-rps">max_rps</label><input
										id="c-rps"
										class="input"
										inputmode="numeric"
										bind:value={cfg.max_rps}
										placeholder="template default"
										disabled={!writable || i.platform}
									/>
								</div>
								{#if cfgErr}<p class="s-err" role="alert">{cfgErr}</p>{/if}
								{#if !i.platform}
									<div class="actions">
										<button
											class="btn primary"
											type="button"
											{...writeAttrs(session, 'integrations_manager')}
											onclick={() => saveCfg()}>Save connection</button
										>
										{#if !writable}<span class="muted">{writeHint('integrations_manager')}</span>{/if}
									</div>
								{/if}
							</div>
						</Card>

						<Card
							title="Credential"
							sub="Write-only and sealed per tenant: never echoed, logged, put on the bus or in a release. Rotate by replacing it."
						>
							{#if i.status === 'auth_failed'}
								<Callout kind="err" title="connector_auth_failed"
									>{i.last_error?.message} (at {fmtAt(i.last_error?.at ?? null)} UTC)</Callout
								>
							{:else if i.status === 'invalid_target'}
								<Callout kind="err" title="invalid_target"
									>{i.last_error?.message ?? 'The base URL resolves to a non-public address.'}</Callout
								>
							{/if}
							<dl class="kvs">
								<dt>State</dt>
								<dd>
									{#if i.platform}<Chip
											text="minted per call"
											variant="muted"
										/>{:else if i.credential_required === false}<Chip
											text="minted per call"
											variant="ok"
										/>{:else if i.credential.set}<Chip text="sealed" variant="ok" />{:else}<Chip
											text="not set"
											variant="err"
										/>{/if}
								</dd>
								<dt>Auth</dt>
								<dd class="mono">{d.template.auth_kind}</dd>
								<dt>Updated</dt>
								<dd>{fmtAt(i.credential.updated_at)}</dd>
								<dt>Fingerprint</dt>
								<dd class="mono">
									{i.platform || i.credential_required === false ? '—' : shortFp(i.credential.fingerprint)}
								</dd>
							</dl>
							{#if !i.platform && !i.stub && i.credential_required !== false}
								<form
									class="stack t11-cred"
									autocomplete="off"
									onsubmit={(e) => {
										e.preventDefault();
										void saveCred();
									}}
								>
									{#each d.template.credential_schema as f (f.name)}
										<div class="field">
											<label for="cr-{f.name}"
												>{f.label}
												{#if f.secret}<Chip text="x-secret" variant="warn" />{/if}</label
											>
											<input
												id="cr-{f.name}"
												class="input mono"
												type={f.secret ? 'password' : 'text'}
												autocomplete={f.secret ? 'new-password' : 'off'}
												placeholder={f.secret
													? i.credential.set
														? '•••••••• (set — type to replace)'
														: (f.hint ?? 'write-only · not set')
													: f.hint
														? `e.g. ${f.hint}`
														: '— not set'}
												bind:value={cred[f.name]}
												disabled={!writable}
											/>
										</div>
									{/each}
									{#if credErr}<p class="s-err" role="alert">{credErr}</p>{/if}
									<div class="actions">
										<button class="btn primary" type="submit" {...writeAttrs(session, 'integrations_manager')}
											>{i.credential.set ? 'Replace credential' : 'Set credential'}</button
										>
									</div>
									<p class="note">
										Derived access tokens live only in runner memory and are re-minted on a 401; they are
										never stored.
									</p>
								</form>
							{/if}
						</Card>
					</div>

					<Card title="Limits and webhooks">
						<dl class="kvs">
							<dt>Vendor API</dt>
							<dd>{insight?.vendor_api_pin ?? '—'}</dd>
							<dt>Rate limit</dt>
							<dd>
								{hasRateLimit(insight?.rate_limit)
									? `${insight.rate_limit.value ?? insight.rate_limit.kind}${insight.rate_limit.honour_retry_after ? ' · honours Retry-After' : ''}`
									: NO_RATE_LIMIT}
								<span class="cell-sub"
									>Enforced per system{i.max_rps != null ? `, capped at ${i.max_rps} rps here` : ''}; 429 →
									rate_limited (one backoff, then escalate).</span
								>
							</dd>
							<dt>Webhooks</dt>
							<dd>
								{#if insight?.webhooks.length}
									<span class="row">
										{#each insight.webhooks as w (w.topic)}<Chip
												text={w.topic}
												variant="muted"
												title={w.signature}
											/>{/each}
										<Chip text="inert in v1" variant="warn" />
									</span>
									<span class="cell-sub"
										>Declared by the template, never delivered: the agent pulls and confirms writes with
										verify_read.</span
									>
								{:else}
									<span class="muted">None declared — v1 is pull-only.</span>
								{/if}
							</dd>
							<dt>Created</dt>
							<dd>{d.names[i.created_by] ?? i.created_by} · {i.created_at.slice(0, 10)}</dd>
						</dl>
					</Card>
				{/if}
			</div>
		</Tabs>
	{/snippet}
</LoadState>

<style>
	.t11-decide {
		margin: 16px 0;
	}
	.t11-verdict {
		display: flex;
		gap: 10px;
		align-items: baseline;
		margin: 0 0 14px;
		font-size: 15px;
		font-weight: 600;
	}
	.t11-vmark {
		display: inline-grid;
		place-items: center;
		width: 22px;
		height: 22px;
		border-radius: 50%;
		font-size: 13px;
		flex: none;
		color: var(--panel);
	}
	.t11-v-ok .t11-vmark {
		background: var(--ok);
	}
	.t11-v-warn .t11-vmark {
		background: var(--warn);
	}
	.t11-v-err .t11-vmark {
		background: var(--err);
	}
	.t11-kpis {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(130px, 1fr));
		gap: 12px;
		margin: 0;
	}
	.t11-kpis dt {
		font: 600 11px/1.2 var(--cond);
		letter-spacing: 0.08em;
		text-transform: uppercase;
		color: var(--muted);
	}
	.t11-kpis dd {
		margin: 4px 0 0;
		font-size: 20px;
		font-weight: 600;
	}
	.t11-kpis dd.t11-t3 {
		font-size: 15px;
	}
	.t11-kpis dd.t11-kn {
		font-size: 12px;
		font-weight: 400;
		color: var(--muted);
		margin-top: 2px;
	}
	.t11-of {
		color: var(--muted);
		font-weight: 400;
		font-size: 15px;
	}
	.t11-nw {
		white-space: nowrap;
	}
	.t11-ribbon {
		display: grid;
		gap: 8px;
		margin-top: 18px;
		padding-top: 14px;
		border-top: 1px solid var(--line-2);
	}
	.t11-rbar {
		display: flex;
		gap: 2px;
		height: 12px;
		border-radius: 4px;
		overflow: hidden;
	}
	.t11-r {
		flex-basis: 0;
		min-width: 6px;
	}
	.t11-r-ok {
		background: var(--ok);
	}
	.t11-r-err {
		background: var(--err);
	}
	.t11-r-accent {
		background: var(--accent);
	}
	.t11-r-warn {
		background: var(--warn);
	}
	.t11-r-muted {
		background: var(--faint);
	}
	.t11-rlegend {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 4px;
		font-size: 12.5px;
	}
	.t11-rdot {
		display: inline-block;
		width: 9px;
		height: 9px;
		border-radius: 2px;
		margin-right: 4px;
	}
	.t11-rops {
		display: block;
		margin-left: 13px;
		color: var(--muted);
		font-family: var(--mono);
		font-size: 11.5px;
		overflow-wrap: anywhere;
	}
	.t11-todos {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 8px;
	}
	.t11-todo {
		display: flex;
		gap: 10px;
		align-items: center;
		justify-content: space-between;
		padding: 8px 10px;
		border: 1px solid var(--line-2);
		border-left: 3px solid var(--accent);
		border-radius: var(--r);
		flex-wrap: wrap;
	}
	.t11-t-err {
		border-left-color: var(--err);
	}
	.t11-t-warn {
		border-left-color: var(--warn);
	}
	.t11-todo-text {
		display: grid;
		gap: 2px;
		min-width: 0;
		flex: 1 1 220px;
		font-size: 12.5px;
	}
	.t11-todo-text span {
		color: var(--muted);
		overflow-wrap: anywhere;
	}
	.t11-grp th {
		background: var(--sunk);
		font: 600 11px/1.2 var(--cond);
		letter-spacing: 0.08em;
		text-transform: uppercase;
		color: var(--muted);
		text-align: left;
	}
	.t11-eff {
		font-family: var(--mono);
		font-size: 12px;
	}
	.t11-t3cell {
		max-width: 300px;
		font-size: 12.5px;
	}
	.t11-mixcell {
		min-width: 170px;
		max-width: 240px;
	}
	.t11-mix {
		display: flex;
		height: 8px;
		border-radius: 4px;
		overflow: hidden;
		background: var(--line-2);
		gap: 1px;
	}
	.t11-mix span {
		flex-basis: 0;
		min-width: 0;
	}
	.t11-m-ok {
		background: var(--ok);
		opacity: 0.55;
	}
	.t11-m-reply {
		background: var(--warn);
	}
	.t11-m-esc {
		background: var(--err);
	}
	.t11-mix .t11-m-reply,
	.t11-mix .t11-m-esc {
		min-width: 3px;
	}
	.t11-key {
		display: inline-block;
		width: 10px;
		height: 8px;
		border-radius: 2px;
		margin: 0 2px 0 8px;
		vertical-align: middle;
	}
	.t11-key:first-child {
		margin-left: 0;
	}
	.t11-small {
		font-size: 12px;
	}
	.t11-okbox {
		display: grid;
		gap: 6px;
		max-width: 280px;
		font-size: 12px;
	}
	.t11-pad {
		padding: 0 16px 12px;
	}
	.t11-oppick {
		min-width: 240px;
	}
	.t11-mapgrid {
		margin-top: 12px;
	}
	.t11-val {
		white-space: nowrap;
	}
	.t11-path {
		min-width: 240px;
		width: 100%;
	}
	.t11-nowrap {
		flex-wrap: nowrap;
	}
	.t11-cred {
		margin-top: 12px;
	}
	.t11-panel {
		display: grid;
		gap: 16px;
		margin-top: 12px;
	}
	.t11-sr {
		position: absolute;
		width: 1px;
		height: 1px;
		overflow: hidden;
		clip: rect(0 0 0 0);
		white-space: nowrap;
	}
</style>
