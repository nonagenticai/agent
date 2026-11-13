<script lang="ts">
	// J6 Operations & routes (D13, D28, doc 11 §4.3.5/§5.3, doc 12 §3.3/§3.5, doc 10 §4.5/§5.1). Leads with "which
	// required operations can't run in prod yet, and how do I route them?": the job's templates fix WHAT it must be
	// able to do (expected ops, R/O — read-only here); the tenant decides WHO does it per env (a connected system whose
	// template implements it, or a fallback). A route runs once its system is healthy, not a stub and the op's T3 is
	// green (prod writes after an explicit OK). Route writes are saved to the job's draft (D54; D63: they ride the
	// release, no separate routing revision): the test env runs the draft at once, Production gets prod routes when a release is published; apply refuses while a required op has no PROD route (409 release_not_ready, 12 §3.5) — a missing test route blocks testing the draft, never publishing. Required ops can never be not
	// offered (422 required_operation); a variant without its own route uses the `*` route.
	import { onMount } from 'svelte';
	import { api, app } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { canWrite, isSuperAdmin, writeAttrs, writeHint } from '$lib/auth/roles';
	import { Resource } from '$lib/load.svelte';
	import { toast } from '$lib/ui/toaster.svelte';
	import DraftBar from '$lib/release/DraftBar.svelte';
	import { fmt } from '$lib/format';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import Seg from '$lib/ui/Seg.svelte';
	import MatrixTable from '$lib/ui/MatrixTable.svelte';
	import { opCounts, opKey, sameOp, targetLabel } from '$lib/connectors/ops';
	import { unroutedRequired } from '$lib/connectors/routing';
	import {
		blockersOn,
		byCause,
		CELL,
		cellOf,
		gate,
		heldBack,
		needsAttention,
		tally,
		type Cell,
		type CellState,
		addableOwnOps,
		availableKey,
		IDENTITY_LEVELS,
		levelLabel,
	} from '$lib/connectors/J6-routes';
	import type {
		Env,
		ExpectedOperation,
		JobOperations,
		JobOp,
		OwnOperationsView,
		RouteTarget,
		SpecFormat,
		SpecOf,
	} from '$lib/api/types/connectors';

	type Col = Env | 'stats';
	const COLS: Col[] = ['prod', 'test', 'stats'];
	const ENVS: Env[] = ['prod', 'test'];

	const res = new Resource<JobOperations>(
		() => api().jobOperations(),
		(d) => d.ops.length === 0,
	);
	const own = new Resource<OwnOperationsView>(
		() => api().jobOwnOperations(),
		() => false,
	);
	onMount(() => {
		res.refresh();
		own.refresh();
	});
	const writable = $derived(canWrite(session, 'integrations_manager'));
	const designer = $derived(canWrite(session, 'job_designer'));
	let ownLevel = $state<Record<string, JobOp['identity_level']>>({});
	let ownBusy = $state('');
	let ownErr = $state('');
	const levelOf = (k: string): JobOp['identity_level'] => ownLevel[k] ?? '2';
	/** Add / remove a job's own operation; then reload both views so the op shows up (or goes) as routable. */
	async function ownWrite(op: string, run: () => Promise<unknown>, done: string) {
		ownBusy = op;
		ownErr = '';
		try {
			await run();
			toast(done);
			await Promise.all([own.refresh(), res.refresh()]);
		} catch (e) {
			const x = e as { code?: string; message: string };
			ownErr = `${op}: ${x.code ?? 'error'} — ${x.message}`;
		} finally {
			ownBusy = '';
		}
	}
	const sa = $derived(isSuperAdmin(session));
	const jobPath = (p: string) => `/admin/jobs/${encodeURIComponent(app.job)}${p}`;
	const sysHref = (id: string) => `/admin/connections/systems/${encodeURIComponent(id)}`;

	let filter = $state('all');
	let bump = $state<{ from: number; to: number; op: string } | null>(null);
	let rowErr = $state<Record<string, string>>({});
	let bulk = $state(false);

	// ---- route editor ----
	let editing = $state(false);
	let edOp = $state<ExpectedOperation | null>(null);
	let edEnv = $state<Env>('prod');
	let kind = $state<RouteTarget['kind']>('instance');
	let instanceId = $state('');
	let detail = $state('');
	let url = $state('https://');
	let edErr = $state<{ code: string; message: string } | null>(null);
	let saving = $state(false);

	const shown = (d: JobOperations) =>
		d.ops.filter((o) =>
			filter === 'required'
				? o.required
				: filter === 'optional'
					? !o.required
					: filter === 'attention'
						? ENVS.some((e) => needsAttention(cellOf(d, o, e)))
						: true,
		);
	const statsOf = (d: JobOperations, o: ExpectedOperation) =>
		d.stats.find((s) => s.op === `${o.object}.${o.verb}`);
	const SIDE: Record<string, string> = {
		R: 'reads only',
		W: 'writes (reversible)',
		X: 'irreversible write',
		P: 'payment',
	};
	const ENV_TEXT: Record<Env, [string, string]> = {
		prod: ['Production', 'What your customers reach.'],
		test: ['Test', 'Staff test chat and contract tests.'],
	};
	const BAR: CellState[] = [
		'ready',
		'fallback',
		'unverified',
		'needs_ok',
		'untested',
		'stub',
		'red',
		'failing',
		'unrouted',
		'not_offered',
	];

	function openEditor(d: JobOperations, o: ExpectedOperation, env: Env) {
		edOp = o;
		edEnv = env;
		const c = cellOf(d, o, env);
		const t = (c.inherited ? null : c.target) ?? c.suggested ?? { kind: 'instance', instance_id: '' };
		kind = t.kind;
		instanceId = t.kind === 'instance' ? t.instance_id : '';
		detail = t.kind === 'instance' ? (t.detail ?? '') : '';
		url = t.kind === 'deep_link' ? t.url : 'https://';
		edErr = null;
		editing = true;
	}
	function target(): RouteTarget {
		switch (kind) {
			case 'instance':
				return { kind, instance_id: instanceId, ...(detail.trim() ? { detail: detail.trim() } : {}) };
			case 'deep_link':
				return { kind, url: url.trim() };
			default:
				return { kind } as RouteTarget;
		}
	}
	async function write(o: ExpectedOperation, env: Env, t: RouteTarget, refresh = true): Promise<boolean> {
		try {
			const r = await api().setOperationRoute({
				object: o.object,
				verb: o.verb,
				variant: o.variant,
				env,
				target: t,
			});
			bump = {
				from: bump && !refresh ? bump.from : r.previous_rev,
				to: r.draft_rev,
				op: `${opKey(o)} @ ${env}`,
			};
			if (refresh) {
				toast(`${opKey(o)} on ${env} → ${targetLabel(t)}. Saved to draft (draft rev ${r.draft_rev}).`);
				await res.refresh();
			}
			return true;
		} catch (e) {
			const x = e as { code?: string; message: string };
			if (editing) edErr = { code: x.code ?? 'error', message: x.message };
			else rowErr = { ...rowErr, [`${opKey(o)}|${env}`]: `${x.code ?? 'error'} — ${x.message}` };
			if (x.code === 'forbidden') toast(`${writeHint('integrations_manager')}.`, 'err');
			return false;
		}
	}
	async function save() {
		if (!edOp) return;
		saving = true;
		if (await write(edOp, edEnv, target())) editing = false;
		saving = false;
	}
	async function clear() {
		if (!edOp) return;
		saving = true;
		try {
			const r = await api().clearOperationRoute({
				object: edOp.object,
				verb: edOp.verb,
				variant: edOp.variant,
				env: edEnv,
			});
			bump = { from: r.previous_rev, to: r.draft_rev, op: `${opKey(edOp)} @ ${edEnv}` };
			toast(`${opKey(edOp)} on ${edEnv}: route cleared. Saved to draft.`);
			editing = false;
			await res.refresh();
		} catch (e) {
			edErr = { code: (e as { code?: string }).code ?? 'error', message: (e as Error).message };
		}
		saving = false;
	}
	async function accept(d: JobOperations, o: ExpectedOperation, env: Env) {
		const s = cellOf(d, o, env).suggested;
		if (s) await write(o, env, s);
	}
	async function acceptAll(cells: Cell[]) {
		bulk = true;
		bump = null;
		let n = 0;
		for (const c of cells) if (c.suggested && (await write(c.op, c.env, c.suggested, false))) n++;
		const last = bump as { from: number; to: number; op: string } | null;
		if (last) bump = { ...last, op: `${n} suggested route${n === 1 ? '' : 's'}` };
		toast(`${n} suggested route${n === 1 ? '' : 's'} accepted. Saved to draft.`);
		await res.refresh();
		bulk = false;
	}
	// ---- F-36 integration field-spec export (T13a): the client's IT gets one row per op × field as a file.
	let exporting = $state<string | null>(null);
	const EXPORTS: [SpecFormat, SpecOf, string][] = [
		['csv', 'draft', 'CSV — draft'],
		['md', 'draft', 'Markdown — draft'],
		['csv', 'live', 'CSV — live release'],
		['md', 'live', 'Markdown — live release'],
	];
	async function exportSpec(format: SpecFormat, of: SpecOf) {
		exporting = `${format}-${of}`;
		try {
			const f = await api().exportOperationSpec(format, of);
			const href = URL.createObjectURL(new Blob([f.body], { type: f.content_type || 'text/plain' }));
			const a = document.createElement('a');
			a.href = href;
			a.download = f.filename || `${app.job}-integration-spec-${of}.${format}`;
			a.click();
			setTimeout(() => URL.revokeObjectURL(href), 0);
			toast(`Exported ${a.download}.`);
		} catch (e) {
			const x = e as { code?: string; message?: string };
			toast(
				x.code === 'no_release'
					? 'This job has no published release yet — export the draft.'
					: `Export failed: ${x.code ?? 'error'} — ${x.message ?? ''}`,
				'err',
			);
		} finally {
			exporting = null;
		}
	}

	const KINDS: [RouteTarget['kind'], string, string][] = [
		['instance', 'A connected system', 'Its template must implement the operation.'],
		['builtin', 'Built-in', 'Runs inside the agent (identity checks, business rules).'],
		['agent_cases', 'Fallback: case queue', 'Creates a case in agent_cases; a person finishes it.'],
		['deep_link', 'Fallback: link to your site', 'The agent hands the customer a URL.'],
		['not_offered', 'Not offered', 'Optional operations only — the agent says it cannot help with this.'],
	];
</script>

<PageHead
	id="J6"
	sub="Your job's templates decide what the agent must be able to do. Here you decide who does it — per environment, a connected system or a fallback."
>
	{#snippet actions()}
		<a class="btn" href="/admin/connections/systems">Systems</a>
		<a class="btn" href={jobPath('/release')}>Release</a>
		<details class="j6-export">
			<summary class="btn">Export spec</summary>
			<div class="j6-export-menu" role="menu">
				{#each EXPORTS as [format, of, label] (`${format}-${of}`)}
					<button
						type="button"
						class="btn"
						role="menuitem"
						disabled={exporting !== null}
						onclick={() => exportSpec(format, of)}
						>{exporting === `${format}-${of}` ? 'Exporting…' : label}</button
					>
				{/each}
			</div>
		</details>
	{/snippet}
</PageHead>

<DraftBar page="J6" tick={res.value} />

<section class="stack" data-testid="j6-own-ops" aria-label="Your own operations">
	<h2 class="label">Your own operations</h2>
	<p class="muted">
		Operations defined by your own connector templates (MCP servers, ERP, CRM, any REST backend). Add one to
		this job and it appears below as a required operation you can route.
	</p>
	{#if ownErr}<p class="s-err" role="alert">{ownErr}</p>{/if}
	<LoadState resource={own}>
		{#snippet children(v)}
			{@const addable = addableOwnOps(v.available)}
			{#if v.ops.length}
				<div class="tbl-wrap">
					<table class="tbl dense">
						<thead
							><tr
								><th>Operation</th><th>Side effect</th><th>Identity level</th><th>From template</th><th
								></th></tr
							></thead
						>
						<tbody>
							{#each v.ops as o (o.op)}
								<tr>
									<td class="mono">{o.op}</td>
									<td
										><Chip
											text={o.value.side_effect}
											variant={o.value.side_effect === 'R' ? 'muted' : 'work'}
										/></td
									>
									<td>{levelLabel(o.value.identity_level)}</td>
									<td class="mono">{o.value.template}</td>
									<td
										><button
											class="btn"
											type="button"
											{...writeAttrs(session, 'job_designer')}
											disabled={!designer || ownBusy !== ''}
											onclick={() =>
												ownWrite(o.op, () => api().removeJobOwnOperation(o.op), `Removed ${o.op}.`)}
											>Remove</button
										></td
									>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
			{:else}
				<p class="muted">This job has no operations of its own yet.</p>
			{/if}
			<h3 class="label">Add from your connectors</h3>
			{#if addable.length}
				<div class="tbl-wrap">
					<table class="tbl dense">
						<thead
							><tr><th>Operation</th><th>Template</th><th>Side effect</th><th>Identity level</th><th></th></tr
							></thead
						>
						<tbody>
							{#each addable as a (availableKey(a))}
								{@const k = availableKey(a)}
								<tr>
									<td>
										<span class="mono">{a.op}</span>
										{#if a.own}<Chip text="your op" variant="accent" />{/if}
										{#if a.description}<div class="cell-sub">{a.description}</div>{/if}
									</td>
									<td class="mono">{a.template}@{a.version} <span class="muted">{a.status}</span></td>
									<td><Chip text={a.side_effect} variant={a.side_effect === 'R' ? 'muted' : 'work'} /></td>
									<td>
										<select
											class="input"
											aria-label="Identity level for {a.op}"
											value={levelOf(k)}
											onchange={(e) => (ownLevel[k] = e.currentTarget.value as JobOp['identity_level'])}
											disabled={!designer}
										>
											{#each IDENTITY_LEVELS as l (l.level)}
												<option value={l.level}>{l.level} · {l.label}</option>
											{/each}
										</select>
									</td>
									<td
										><button
											class="btn"
											type="button"
											{...writeAttrs(session, 'job_designer')}
											disabled={!designer || ownBusy !== '' || a.status === 'retired'}
											onclick={() =>
												ownWrite(
													a.op,
													() =>
														api().addJobOwnOperation(a.op, {
															template: a.template,
															version: a.version,
															identity_level: levelOf(k),
														}),
													`Added ${a.op}.`,
												)}>Add</button
										></td
									>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
			{:else}
				<p class="muted">
					No further operations to add. Build a template on <a href="/admin/connections/templates"
						>Templates</a
					> or import tools from an MCP server.
				</p>
			{/if}
		{/snippet}
	</LoadState>
</section>

<LoadState resource={res}>
	{#snippet empty()}
		<p>
			No expected operations for <span class="mono">{app.job}</span> yet — they are compiled from the job's template
			pins and enabled activities.
		</p>
	{/snippet}
	{#snippet children(d)}
		{@const c = opCounts(d.ops)}
		{@const unrouted = unroutedRequired(d)}
		{@const held = heldBack(d, 'prod')}
		<ol class="j6-chain" aria-label="How an operation reaches a system">
			<li>
				<span class="j6-step">1 · Expected</span>
				<b data-testid="op-counts">{c.ops} ops / {c.objects} objects / {c.required} required</b>
				<span
					>From the job's templates — read-only here. <b>R</b> must run, <b>O</b> you may switch off.{#if sa}{' '}<a
							href="/platform/operations">Open the operations catalogue</a
						>{/if}</span
				>
			</li>
			<li>
				<span class="j6-step">2 · Routed per environment</span>
				<b>prod and test, by you</b>
				<span
					>Saved to draft — the test env runs it at once, Production when you publish · draft rev
					<span class="mono">{d.draft_rev}</span>.</span
				>
			</li>
			<li>
				<span class="j6-step">3 · Run by a system</span>
				<b>template implements it</b>
				<span
					>Ready when the system is healthy and its contract test is green; prod writes after your OK.</span
				>
			</li>
			<li class="j6-alt">
				<span class="j6-step">… or a fallback</span>
				<b>built-in · case queue · link</b>
				<span>A person or your site finishes it. Optional ops can be <i>not offered</i>.</span>
			</li>
		</ol>

		{#if bump}
			<Callout kind="info" title="draft rev {bump.from} → {bump.to}">
				{bump.op} saved to draft: <a href="/chat">test it in the test chat</a> (test env), then
				<a href="/admin/jobs/{app.job}/release">review &amp; publish</a>. Once published, a write already
				planned on the old routes returns <span class="mono">route_changed</span> and re-plans — it is never sent
				to the old system.
			</Callout>
		{/if}

		<div class="j6-gate" data-testid="j6-gate">
			{#each ENVS as env (env)}
				{@const g = gate(d, env)}
				{@const bl = blockersOn(d, env)}
				{@const sug = bl.filter((x) => x.state === 'unrouted' && x.suggested)}
				{@const t = tally(d, env)}
				<section class="j6-env" class:j6-bad={bl.length > 0} data-testid="gate-{env}">
					<header>
						<div>
							<h2>{ENV_TEXT[env][0]}</h2>
							<p class="muted">
								{ENV_TEXT[env][1]}{env === 'prod'
									? ' Release apply checks these routes.'
									: ' Gaps here block testing, not publishing.'}
							</p>
						</div>
						<Chip
							text={bl.length ? `${g.total - g.runs} of ${g.total} blocked` : 'job can act'}
							variant={bl.length ? (env === 'prod' ? 'err' : 'warn') : 'ok'}
						/>
					</header>
					<p class="j6-big"><b>{g.runs} of {g.total}</b> <span>required operations can run</span></p>
					<p class="muted j6-small">{g.routed}/{g.total} routed · counted per object.verb</p>
					<div class="j6-bar" role="img" aria-label="Routes on {env} by state">
						{#each BAR as s (s)}
							{#if t[s]}<span class="j6-seg j6-{s}" style:flex-grow={t[s]} title="{t[s]} {CELL[s].label}"
								></span>{/if}
						{/each}
					</div>
					<p class="j6-legend">
						{#each BAR as s (s)}
							{#if t[s]}<span><i class="j6-dot j6-{s}"></i>{t[s]} {CELL[s].label}</span>{/if}
						{/each}
					</p>
					{#if bl.length}
						<div class="j6-blhead">
							<h3>Can't run on {env} yet</h3>
							{#if sug.length > 1}
								<button
									class="btn sm primary"
									type="button"
									{...writeAttrs(session, 'integrations_manager')}
									disabled={!writable || bulk}
									onclick={() => acceptAll(sug)}>Accept {sug.length} suggestions</button
								>
							{/if}
						</div>
						<ul class="j6-bl" data-testid="blockers-{env}">
							{#each byCause(bl) as g (g.id)}
								{@const one = g.cells.length === 1 ? g.cells[0] : null}
								<li>
									<div class="row">
										<Chip text={CELL[g.state].label} variant={CELL[g.state].variant} />
										{#each g.cells as b (b.key)}<span class="mono j6-opk">{b.key}</span>{/each}
										{#if g.state === 'unrouted'}<span class="{env === 'prod' ? 's-err' : 's-warn'} j6-small"
												>{env === 'prod' ? 'blocks release apply' : 'blocks testing, not publishing'}</span
											>{/if}
									</div>
									<span class="cell-sub">{g.why}</span>
									<span class="row">
										{#if one?.suggested}
											<button
												class="btn sm"
												type="button"
												{...writeAttrs(session, 'integrations_manager')}
												disabled={!writable || bulk}
												onclick={() => accept(d, one.op, env)}>Use {targetLabel(one.suggested)}</button
											>
										{/if}
										{#if g.instance}
											<a class="btn sm" href={sysHref(g.instance)}
												>Fix {g.instance}{g.cells.length > 1 ? ` (${g.cells.length} ops)` : ''}</a
											>
										{/if}
										{#if one}
											<button class="btn sm" type="button" onclick={() => openEditor(d, one.op, env)}
												>{g.state === 'unrouted' ? 'Route…' : 'Re-route…'}</button
											>
										{/if}
									</span>
									{#if one && g.state === 'unrouted' && !one.suggested}
										<span class="cell-sub"
											>No connected system implements it — <a href="/admin/connections/systems">connect one</a
											>.</span
										>
									{/if}
								</li>
							{/each}
						</ul>
					{:else}
						<p class="s-ok j6-clear">
							Every required operation runs on {env}{env === 'prod'
								? ' — routes will not hold up the next release.'
								: '.'}
						</p>
					{/if}
				</section>
			{/each}
		</div>

		{@const prodGaps = unrouted.filter((u) => u.env === 'prod')}
		{@const testGaps = unrouted.filter((u) => u.env === 'test')}
		{#if prodGaps.length}
			<p class="j6-note">
				<b>{prodGaps.length}</b> required prod route{prodGaps.length === 1 ? ' is' : 's are'} missing — release
				apply returns <span class="mono">409 release_not_ready</span> until they exist.
				<a href={jobPath('/release')}>Release</a>
			</p>
		{/if}
		{#if testGaps.length}
			<p class="j6-note" data-testid="j6-test-note">
				<b>{testGaps.length}</b> required test route{testGaps.length === 1 ? ' is' : 's are'} missing — the test
				chat and regression runs cannot try {testGaps.length === 1 ? 'it' : 'them'}. This does not block
				publishing: release apply checks prod routes only.
			</p>
		{/if}

		{#if held.length}
			<Card
				title="Optional operations held back in prod"
				sub="Routed, but the agent will not use them for customers until the test passes. Fix them on the system page."
			>
				<ul class="j6-held" data-testid="held-prod">
					{#each held as h (h.key)}
						<li>
							<span class="mono">{h.key}</span>
							<Chip text={CELL[h.state].label} variant={CELL[h.state].variant} />
							<span class="cell-sub">{h.why}</span>
							{#if h.instance}<a href={sysHref(h.instance)}
									>{h.state === 'needs_ok' ? `Give your OK on ${h.instance}` : `Open ${h.instance}`}</a
								>{/if}
						</li>
					{/each}
				</ul>
			</Card>
		{/if}

		<Card
			title="Route matrix"
			sub="Every expected operation, where it goes on each environment and whether it can run. Click a route to change it. A variant without its own route uses the * route."
			cls="flush"
		>
			{#snippet actions()}
				<Seg
					items={[
						['all', 'All'],
						['required', 'Required'],
						['optional', 'Optional'],
						['attention', 'Needs attention'],
					]}
					bind:active={filter}
					label="Filter operations"
				/>
			{/snippet}
			<MatrixTable
				rows={shown(d)}
				columns={COLS}
				rowKey={(o) => opKey(o)}
				colKey={(c) => c}
				caption="Operations × environments"
				corner="Operation"
				empty="No operations match this filter."
			>
				{#snippet colHeader(col)}{col === 'stats' ? 'Prod · last 7 days' : col}{/snippet}
				{#snippet rowHeader(o)}
					<div class="op-head">
						<span class="row j6-nowrap">
							<Chip
								text={o.required ? 'R' : 'O'}
								variant={o.required ? 'accent' : 'muted'}
								title={o.required
									? 'required — needs a route or a fallback'
									: 'optional — may be not offered'}
							/>
							<span class="mono">{opKey(o)}</span>
						</span>
						<span class="cell-sub"
							><span
								class:s-err={o.side_effect === 'X' || o.side_effect === 'P'}
								class:s-warn={o.side_effect === 'W'}>{SIDE[o.side_effect]}</span
							>
							· identity {o.identity_level} · {o.stages.length === 7
								? 'all stages'
								: o.stages.join(', ')}{o.note ? ` · ${o.note}` : ''}</span
						>
					</div>
				{/snippet}
				{#snippet cell(o, col)}
					{#if col === 'stats'}
						{@const s = statsOf(d, o)}
						{#if s}
							<span class="mono">{fmt.n(s.calls_7d)}</span> calls
							<span class="cell-sub" class:s-err={s.error_rate >= 0.05}
								>{fmt.pct(s.error_rate)} errors · p95 {fmt.ms(s.p95_ms)}</span
							>
						{:else}<span class="muted">no calls</span>{/if}
					{:else}
						{@const cl = cellOf(d, o, col)}
						{@const errKey = `${opKey(o)}|${col}`}
						<div class="route" data-testid="route-{opKey(o)}-{col}" title={cl.why}>
							{#if cl.target}
								<button class="table-action" type="button" onclick={() => openEditor(d, o, col)}
									>{targetLabel(cl.target)}</button
								>
								<span class="chips">
									<Chip text={CELL[cl.state].label} variant={CELL[cl.state].variant} />
									{#if cl.inherited}<Chip
											text="via *"
											variant="accent"
											title="no route of its own: uses the {o.object}.{o.verb} route"
										/>{/if}
								</span>
								{#if cl.target.kind === 'deep_link'}<span class="cell-sub mono url" title={cl.target.url}
										>{cl.target.url}</span
									>{/if}
							{:else}
								<Chip text={CELL.unrouted.label} variant="err" />
								{#if cl.suggested}
									<span class="cell-sub">suggested: {targetLabel(cl.suggested)}</span>
									<span class="row">
										<button
											class="btn sm"
											type="button"
											{...writeAttrs(session, 'integrations_manager')}
											onclick={() => accept(d, o, col)}>Accept</button
										>
										<button class="btn sm" type="button" onclick={() => openEditor(d, o, col)}>Choose…</button
										>
									</span>
								{:else}
									<button class="btn sm" type="button" onclick={() => openEditor(d, o, col)}>Route…</button>
								{/if}
							{/if}
							{#if rowErr[errKey]}<span class="s-err small" role="alert">{rowErr[errKey]}</span>{/if}
						</div>
					{/if}
				{/snippet}
			</MatrixTable>
		</Card>
		{#if !writable}<p class="note">
				{writeHint('integrations_manager')} — routes are read-only for you.
			</p>{/if}

		<Drawer bind:open={editing} title={edOp ? `${opKey(edOp)} — ${edEnv}` : 'Route'}>
			{#snippet sub()}
				{#if edOp}
					<Chip
						text={edOp.required ? 'required (R)' : 'optional (O)'}
						variant={edOp.required ? 'accent' : 'muted'}
					/>
					<span>{SIDE[edOp.side_effect]} · identity {edOp.identity_level}</span>
				{/if}
			{/snippet}
			{#if edOp}
				{@const op = edOp}
				{@const s = statsOf(d, op)}
				{@const now = cellOf(d, op, edEnv)}
				{@const others = d.ops.filter((x) => x.object === op.object && x.verb === op.verb && !sameOp(x, op))}
				<div class="stack">
					<p class="j6-now">
						<Chip text={CELL[now.state].label} variant={CELL[now.state].variant} />
						<span>{now.why}</span>
					</p>
					{#if op.note}<p class="muted">{op.note}</p>{/if}
					<fieldset class="editor-fields stack" disabled={!writable}>
						<legend class="label">Who does it on {edEnv}</legend>
						{#each KINDS as [k, text, hint] (k)}
							<label class="j6-kind">
								<input type="radio" name="kind" value={k} bind:group={kind} />
								<span
									><b>{text}</b>
									{#if k === 'not_offered' && op.required}<Chip
											text="required — refused"
											variant="err"
										/>{/if}
									<span class="cell-sub">{hint}</span></span
								>
							</label>
						{/each}
						{#if kind === 'instance'}
							<div class="field">
								<label for="rt-inst">System</label>
								<select id="rt-inst" class="input mono" bind:value={instanceId}>
									<option value="" disabled>Pick a system…</option>
									{#each d.instances.filter((i) => (i.env === edEnv || i.env === 'all') && i.template_key !== 'agent_cases') as i (i.id)}
										{@const impl = (d.implements[i.id] ?? []).some(
											(k) => k === opKey(op) || k === opKey({ ...op, variant: '*' }),
										)}
										<option value={i.id}>
											{i.id} — {i.template_key}@{i.template_version}{i.stub
												? ' · stub'
												: ''}{d.draft_instances.includes(i.id) ? ' · draft template' : ''}{impl
												? ''
												: ' · does not implement'}
										</option>
									{/each}
								</select>
							</div>
							<div class="field">
								<label for="rt-detail">Vendor operation note (optional)</label>
								<input id="rt-detail" class="input" bind:value={detail} placeholder="e.g. returnRequest" />
							</div>
						{:else if kind === 'deep_link'}
							<div class="field">
								<label for="rt-url">Deep link URL</label>
								<input id="rt-url" class="input mono" bind:value={url} />
							</div>
						{/if}
					</fieldset>
					{#if edErr}
						<p class="s-err" role="alert" data-testid="route-error">
							<b class="mono">{edErr.code}</b> — {edErr.message}
						</p>
					{/if}
					<div class="actions">
						<button
							class="btn primary"
							type="button"
							{...writeAttrs(session, 'integrations_manager')}
							disabled={!writable || saving}
							onclick={save}>Save route</button
						>
						{#if now.target && !now.inherited}
							<button
								class="btn"
								type="button"
								{...writeAttrs(session, 'integrations_manager')}
								disabled={!writable || saving}
								onclick={clear}>Clear route</button
							>
						{/if}
						{#if !writable}<span class="muted">{writeHint('integrations_manager')}</span>{/if}
					</div>
					<p class="muted j6-small">
						Saving goes to the draft (draft rev +1): the test env uses it at once; customers get it when you
						publish a release.
					</p>
					<div>
						<h3>Both environments</h3>
						<dl class="kvs">
							{#each ENVS as e (e)}
								{@const r = cellOf(d, op, e)}
								{@const own = d.routes.find((x) => x.env === e && sameOp(x, op))}
								<dt>{e}</dt>
								<dd>
									{r.target ? targetLabel(r.target) : 'no route'}{r.inherited ? ' (via *)' : ''} · {CELL[
										r.state
									].label}
									{#if own}<span class="cell-sub"
											>by {d.names[own.updated_by] ?? own.updated_by}, {own.updated_at
												.slice(0, 16)
												.replace('T', ' ')}</span
										>{/if}
								</dd>
							{/each}
						</dl>
					</div>
					<div>
						<h3>Prod · last 7 days ({op.object}.{op.verb})</h3>
						{#if s}
							<dl class="kvs">
								<dt>Calls</dt>
								<dd>{fmt.n(s.calls_7d)}</dd>
								<dt>Error rate</dt>
								<dd>{fmt.pct(s.error_rate)}</dd>
								<dt>p95 latency</dt>
								<dd>{fmt.ms(s.p95_ms)}</dd>
							</dl>
						{:else}<p class="muted">No calls yet.</p>{/if}
					</div>
					{#if others.length}
						<div>
							<h3>Other variants of {op.object}.{op.verb}</h3>
							<p class="mono small">{others.map(opKey).join(' · ')}</p>
						</div>
					{/if}
				</div>
			{/if}
		</Drawer>
	{/snippet}
</LoadState>

<style>
	.j6-export {
		position: relative;
	}
	.j6-export > summary {
		list-style: none;
	}
	.j6-export > summary::-webkit-details-marker {
		display: none;
	}
	.j6-export-menu {
		position: absolute;
		right: 0;
		top: calc(100% + 4px);
		z-index: 5;
		display: grid;
		gap: 4px;
		padding: 6px;
		min-width: 200px;
		background: var(--panel);
		border: 1px solid var(--line);
		border-radius: 6px;
	}
	.j6-export-menu .btn {
		justify-content: flex-start;
	}
	.j6-chain {
		list-style: none;
		margin: 0 0 16px;
		padding: 0;
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(200px, 1fr));
		gap: 8px;
	}
	.j6-chain li {
		position: relative;
		display: grid;
		gap: 2px;
		align-content: start;
		padding: 10px 12px;
		border: 1px solid var(--line);
		border-radius: var(--r);
		background: var(--panel);
		font-size: 12.5px;
		color: var(--muted);
	}
	.j6-chain li b {
		color: var(--fg);
		font-size: 13.5px;
	}
	.j6-chain li.j6-alt {
		border-style: dashed;
		background: var(--sunk);
	}
	.j6-step {
		font-family: var(--cond);
		text-transform: uppercase;
		letter-spacing: 0.05em;
		font-size: 11px;
		color: var(--accent);
	}
	.j6-gate {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(300px, 1fr));
		gap: 12px;
		margin-bottom: 10px;
	}
	.j6-env {
		border: 1px solid var(--line);
		border-radius: var(--r);
		background: var(--panel);
		padding: 14px 16px;
		display: grid;
		gap: 8px;
		align-content: start;
		min-width: 0;
	}
	.j6-env.j6-bad {
		border-color: color-mix(in srgb, var(--err) 45%, var(--line));
	}
	.j6-env header {
		display: flex;
		justify-content: space-between;
		gap: 8px;
		align-items: start;
	}
	.j6-env h2 {
		margin: 0;
		font-size: 15px;
	}
	.j6-env header p {
		margin: 2px 0 0;
		font-size: 12.5px;
	}
	.j6-big {
		margin: 0;
		display: flex;
		align-items: baseline;
		gap: 8px;
		flex-wrap: wrap;
	}
	.j6-big b {
		font-size: 26px;
		line-height: 1.1;
	}
	.j6-small {
		font-size: 12px;
		margin: 0;
	}
	.j6-bar {
		display: flex;
		height: 8px;
		border-radius: 4px;
		overflow: hidden;
		gap: 2px;
		background: var(--sunk);
	}
	.j6-seg {
		flex-basis: 0;
		min-width: 4px;
	}
	.j6-legend {
		margin: 0;
		display: flex;
		flex-wrap: wrap;
		gap: 4px 12px;
		font-size: 12px;
		color: var(--muted);
	}
	.j6-dot {
		display: inline-block;
		width: 8px;
		height: 8px;
		border-radius: 2px;
		margin-right: 4px;
	}
	.j6-ready,
	.j6-fallback {
		background: var(--ok);
	}
	.j6-fallback {
		opacity: 0.6;
	}
	.j6-unverified {
		background: var(--cust);
	}
	.j6-needs_ok {
		background: var(--accent);
	}
	.j6-untested,
	.j6-stub {
		background: var(--warn);
	}
	.j6-red,
	.j6-failing,
	.j6-unrouted {
		background: var(--err);
	}
	.j6-not_offered {
		background: var(--line);
	}
	.j6-blhead {
		display: flex;
		justify-content: space-between;
		align-items: center;
		gap: 8px;
		margin-top: 4px;
	}
	.j6-blhead h3 {
		margin: 0;
		font-size: 13px;
	}
	.j6-bl,
	.j6-held {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 0;
	}
	.j6-bl li {
		display: grid;
		gap: 4px;
		padding: 8px 0;
		border-top: 1px solid var(--line-2);
	}
	.j6-held li {
		display: flex;
		flex-wrap: wrap;
		gap: 4px 10px;
		align-items: baseline;
		padding: 7px 0;
		border-top: 1px solid var(--line-2);
	}
	.j6-held li:first-child {
		border-top: 0;
	}
	.j6-clear {
		margin: 4px 0 0;
		font-size: 13px;
	}
	.j6-note {
		margin: 0 0 14px;
		font-size: 13px;
	}
	.j6-opk {
		font-size: 12.5px;
	}
	.j6-nowrap {
		flex-wrap: nowrap;
	}
	.j6-now {
		display: flex;
		gap: 8px;
		align-items: baseline;
		margin: 0;
	}
	.j6-kind {
		display: flex;
		gap: 8px;
		align-items: start;
	}
	.j6-kind > span {
		display: grid;
		gap: 1px;
	}
	.op-head {
		display: grid;
		white-space: normal;
		gap: 3px;
		min-width: 220px;
		max-width: 340px;
		font-weight: 400;
		text-transform: none;
		letter-spacing: 0;
		font-size: 13px;
		color: var(--fg);
	}
	.route {
		display: grid;
		gap: 4px;
		min-width: 150px;
		max-width: 320px;
		justify-items: start;
	}
	.url {
		overflow-wrap: anywhere;
	}
	.small {
		font-size: 12px;
	}
</style>
