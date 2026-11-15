<script lang="ts">
	// T10 Systems — leads with "can my agents do their job with the systems I've connected?": the current job's
	// required (object, verb) pairs per env resolved against its routes and the health of the system each lands on
	// (doc 12 §3.3/§3.5, K48), then each connected system (a platform template pinned at a version + a sealed
	// credential, D16/D32, doc 10 §5.2 steps 3–4) with what it serves for the job, and the catalogue with the gaps a
	// new system would fill.
	import '$lib/phone-stack.css';
	import { onMount } from 'svelte';
	import { goto } from '$app/navigation';
	import { api, app } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { canWrite, writeAttrs, writeHint } from '$lib/auth/roles';
	import { Resource } from '$lib/load.svelte';
	import { toast } from '$lib/ui/toaster.svelte';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LiveNote from '$lib/release/LiveNote.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import Meter from '$lib/ui/Meter.svelte';
	import { checkTarget, shortFp, targetLabel } from '$lib/connectors/ops';
	import { STATUS_TEXT, t3Summary } from '$lib/connectors/instances';
	import {
		blockers,
		FIT_TEXT,
		FIT_VARIANT,
		fillsGaps,
		jobFit,
		servesFor,
		upgradeFor,
		type EnvFit,
		type Fit,
		type PairFit,
	} from '$lib/connectors/T10-fit';
	import type { ConnectorInstance, Env, JobOperations, SystemCatalogueRow } from '$lib/api/types/connectors';
	import type { Paged } from '$lib/api/types/core';

	const systems = new Resource<Paged<ConnectorInstance>>(() => api().connectorInstances());
	const catalogue = new Resource<Paged<SystemCatalogueRow>>(() => api().systemCatalogue());
	const ops = new Resource<JobOperations>(() => api().jobOperations());
	onMount(() => {
		void systems.refresh();
		void catalogue.refresh();
		void ops.refresh();
	});
	const writable = $derived(canWrite(session, 'integrations_manager'));
	const dataOf = <T,>(r: Resource<T>): T | null =>
		r.value.state === 'ok' || r.value.state === 'empty' ? r.value.data : null;
	const opsData = $derived(dataOf(ops));
	const instances = $derived(dataOf(systems)?.items ?? []);
	const catalogueRows = $derived(dataOf(catalogue)?.items ?? []);
	const jobLabel = $derived(app.jobs.find((j) => j.key === app.job)?.label ?? app.job);
	const routesHref = $derived(`/admin/jobs/${encodeURIComponent(app.job)}/operations`);
	const sysHref = (id: string) => `/admin/connections/systems/${encodeURIComponent(id)}`;

	const fits = $derived<EnvFit[]>(opsData ? [jobFit(opsData, 'prod'), jobFit(opsData, 'test')] : []);
	/** Required pairs no system serves on some env — what a new system could fill. */
	const gaps = $derived<PairFit[]>(
		fits
			.flatMap((f) =>
				f.pairs.filter((p) => p.required && p.rows.some((r) => r.fit === 'missing' && !r.suggested)),
			)
			.filter((p, i, all) => all.findIndex((x) => x.pair === p.pair) === i),
	);
	const labelOf = (id: string) =>
		instances.find((i) => i.id === id)?.label ?? (id === 'agent_cases' ? 'Agent case queue' : id);
	const ENV_TEXT: Record<Env, [string, string]> = {
		prod: ['Production', 'What your customers reach.'],
		test: ['Test', 'Staff test chat and contract tests.'],
	};
	const SEGMENTS: Fit[] = ['ok', 'untested', 'failing', 'stub', 'missing'];

	function why(p: PairFit): string {
		const bad = p.rows.find((r) => r.fit === p.fit)!;
		const where = bad.target ? targetLabel(bad.target, labelOf) : null;
		switch (p.fit) {
			case 'missing':
				return bad.suggested
					? `No route for ${bad.key}. Suggested: ${targetLabel(bad.suggested, labelOf)}.`
					: `No connected system implements ${bad.key}.`;
			case 'failing':
				return `Routed to ${where}, whose last call failed.`;
			case 'stub':
				return `Routed to the stub ${where} — stubs never count as ready.`;
			case 'untested':
				return `Routed to ${where}, but ${bad.key} has no green contract test there yet.`;
			default:
				return '';
		}
	}

	// ---- add instance ----
	let adding = $state(false);
	let pick = $state<SystemCatalogueRow | null>(null);
	let form = $state({
		id: '',
		label: '',
		env: 'test' as Env,
		base_url: 'https://',
		env_ids: '',
		max_rps: '',
		stub: false,
	});
	let err = $state<{ code: string; message: string } | null>(null);
	let saving = $state(false);
	const urlProblem = $derived(
		form.stub || form.base_url.trim() === 'https://' ? null : checkTarget(form.base_url),
	);
	const dirty = $derived(adding && (form.id !== '' || form.base_url !== 'https://'));

	function startAdd(row: SystemCatalogueRow) {
		pick = row;
		const vendor = row.key.split('_')[0];
		form = {
			id: `${vendor}-${row.status === 'draft' ? 'test' : 'prod'}`.replace(/_/g, '-'),
			label: `${row.label}`,
			env: row.status === 'draft' ? 'test' : 'prod',
			base_url: 'https://',
			env_ids: '',
			max_rps: '',
			stub: false,
		};
		err = null;
		adding = true;
	}
	function parseIds(text: string): Record<string, string> {
		const out: Record<string, string> = {};
		for (const line of text.split('\n')) {
			const m = /^\s*([A-Za-z0-9_.-]+)\s*=\s*(.+?)\s*$/.exec(line);
			if (m) out[m[1]!] = m[2]!;
		}
		return out;
	}
	async function submit() {
		if (!pick) return;
		saving = true;
		err = null;
		try {
			const inst = await api().createConnectorInstance({
				id: form.id.trim(),
				label: form.label,
				template_key: pick.key,
				template_version: pick.version,
				env: form.env,
				base_url: form.base_url,
				env_ids: parseIds(form.env_ids),
				max_rps: form.max_rps.trim() ? Number(form.max_rps) : null,
				stub: form.stub,
			});
			adding = false;
			toast(`${inst.id} added. Set its credential, then run the contract tests.`);
			await systems.refresh();
			await goto(`/admin/connections/systems/${encodeURIComponent(inst.id)}`);
		} catch (e) {
			const x = e as { code?: string; message: string };
			err = { code: x.code ?? 'error', message: x.message };
		}
		saving = false;
	}

	const groups: [string, string, string, (i: ConnectorInstance) => boolean][] = [
		[
			'Production',
			'production',
			'Prod routes resolve only to these. Stubs and draft templates are refused here.',
			(i) => !i.platform && i.env === 'prod',
		],
		[
			'Test',
			'test',
			'Where you try a system out first. Stubs allowed; they never count for readiness.',
			(i) => !i.platform && i.env === 'test',
		],
		[
			'Platform connectors',
			'platform',
			'Provisioned by Agent for your tenant (provision-down): knowledge search and the case queue. Nothing to manage.',
			(i) => i.platform,
		],
	];
	const statusVariant = (s: ConnectorInstance['status']) =>
		s === 'ok' ? 'ok' : s === 'untested' || s === 'draft_template' ? 'warn' : 'err';
	const connectedKeys = $derived(new Set(instances.map((i) => `${i.template_key}@${i.template_version}`)));
</script>

<PageHead
	id="T10"
	sub="The systems of record your agents act on — your shop, payments, helpdesk. Each one is a platform connector template pinned at a version, pointed at your account with a sealed credential, per environment."
>
	{#snippet actions()}
		<a class="btn" href="/admin/connections/templates">Your templates</a>
	{/snippet}
</PageHead>

<LiveNote
	of="environment"
	what="Adding a system or storing its credential applies at once (test and production systems are separate); the routes that use a system ride the release."
/>

<ol class="t10-model" aria-label="How a system serves a job">
	<li>
		<b>Connector template</b>
		<span>published by the platform, pinned at a version</span>
	</li>
	<li>
		<b>Your system</b>
		<span>base URL + sealed credential, per env</span>
	</li>
	<li>
		<b>Routes</b>
		<span>each operation of the job → one system</span>
	</li>
	<li>
		<b>{jobLabel}</b>
		<span>can act when every required operation is ready</span>
	</li>
</ol>

<Card
	title="Can {jobLabel} do its job on your systems?"
	sub="Every operation the job requires, resolved per environment to the system it is routed to. Ready = routed to a healthy, non-stub system with a green contract test (or served by the agent itself)."
>
	{#snippet actions()}
		<a class="btn sm" href={routesHref}>Change routes</a>
	{/snippet}
	<LoadState resource={ops} empty="This job expects no operations.">
		{#snippet children(_d)}
			<div class="t10-lanes" data-testid="job-fit">
				{#each fits as f (f.env)}
					{@const bl = blockers(f)}
					{@const ready = f.required.ok === f.required.total}
					<section class="t10-lane" data-testid="fit-{f.env}" aria-label="{ENV_TEXT[f.env][0]} readiness">
						<header>
							<div>
								<h3>{ENV_TEXT[f.env][0]}</h3>
								<span class="muted">{ENV_TEXT[f.env][1]}</span>
							</div>
							<Chip text={ready ? 'job can act' : 'blocked'} variant={ready ? 'ok' : 'err'} />
						</header>
						<p class="t10-big">
							<b>{f.required.ok}</b><span>/ {f.required.total}</span>
							<em>required operations ready</em>
						</p>
						<div class="t10-bar" role="img" aria-label="{f.required.ok} of {f.required.total} ready">
							{#each SEGMENTS as s (s)}
								{#if f.required[s]}<i class="seg-{s}" style:flex-grow={f.required[s]}></i>{/if}
							{/each}
						</div>
						<ul class="t10-legend">
							{#each SEGMENTS as s (s)}
								{#if f.required[s]}<li><i class="seg-{s}"></i>{f.required[s]} {FIT_TEXT[s]}</li>{/if}
							{/each}
							<li class="muted">· optional {f.optional.served}/{f.optional.total} served</li>
						</ul>
						{#if bl.length === 0}
							<p class="t10-clear">Nothing blocks the job here.</p>
						{:else}
							<ul class="t10-blockers">
								{#each bl.slice(0, 6) as p (p.pair)}
									<li>
										<span class="mono">{p.pair}</span>
										<Chip
											text={p.fit === 'missing' && p.rows.every((r) => r.fit !== 'missing' || r.suggested)
												? 'not routed'
												: p.fit === 'missing'
													? 'no system'
													: FIT_TEXT[p.fit]}
											variant={FIT_VARIANT[p.fit]}
										/>
										<span class="t10-why">{why(p)}</span>
									</li>
								{/each}
								{#if bl.length > 6}<li class="muted">
										+ {bl.length - 6} more on Operations &amp; routes
									</li>{/if}
							</ul>
						{/if}
					</section>
				{/each}
			</div>
		{/snippet}
	</LoadState>
</Card>

<LoadState resource={systems} empty="No systems yet. Connect one from the catalogue below.">
	{#snippet children(page)}
		{@const rows = page.items}
		{#each rows.filter((i) => i.status === 'auth_failed' || i.status === 'invalid_target') as f (f.id)}
			<Callout kind="err" title="{f.id}: {f.last_error?.code ?? f.status}">
				{f.last_error?.message ?? 'The last call failed.'}
				<a href={sysHref(f.id)}>Open {f.id}</a>
			</Callout>
		{/each}
		{#each groups as [title, slug, sub, test] (slug)}
			{@const list = rows.filter(test)}
			<Card {title} {sub}>
				{#if list.length === 0}
					<p class="muted">
						No {title.toLowerCase()} systems yet.{#if slug === 'production'}
							Your agents can only act for customers once a system is connected on prod.{/if}
					</p>
				{:else}
					<div class="t10-grid" data-testid="systems-{slug}">
						{#each list as i (i.id)}
							{@const t3 = t3Summary(i)}
							{@const up = upgradeFor(i, catalogueRows)}
							{@const served = servesFor(fits, i.id)}
							{@const req = served.filter((o) => o.required)}
							{@const opt = served.filter((o) => !o.required)}
							{@const bad = i.status === 'auth_failed' || i.status === 'invalid_target'}
							<article class="t10-sys" class:bad data-testid="system-{i.id}">
								<header>
									<div class="t10-name">
										<a href={sysHref(i.id)}>{i.label}</a>
										<span class="mono muted">{i.id}</span>
									</div>
									<Chip text={STATUS_TEXT[i.status]} variant={statusVariant(i.status)} />
								</header>
								<dl class="t10-facts">
									<dt>Template</dt>
									<dd>
										<span class="mono">{i.template_key}@{i.template_version}</span>
										{#if i.template_scope === 'tenant'}<Chip text="your template" variant="accent" />{/if}
										{#if i.stub}<Chip text="stub" variant="warn" />{/if}
										{#if up}
											<a
												class="t10-up"
												href={sysHref(i.id)}
												title="Re-pin on the system page; re-runs the contract tests">@{up} available</a
											>
										{/if}
									</dd>
									<dt>Credential</dt>
									<dd>
										{#if i.platform}<span class="muted">managed by Agent</span>
										{:else if i.stub}<span class="muted">not needed (recorded fixtures)</span>
										{:else if i.credential_required === false}<span class="muted">minted per call</span>
										{:else if i.credential.set}
											<Chip text="set · {shortFp(i.credential.fingerprint)}" variant="ok" />
											<span class="muted">sealed · {i.credential.updated_at?.slice(0, 10)}</span>
										{:else}<Chip text="not set" variant="err" />{/if}
									</dd>
									<dt>Contract tests</dt>
									<dd>
										{#if t3.run === 0}<span class="muted">{i.platform ? 'platform-tested' : 'not run'}</span
											>{:else}
											<span class="mono">{t3.green}/{t3.run} green</span>
											{#if t3.unverified}<span class="muted">· {t3.unverified} write unverified</span>{/if}
										{/if}
									</dd>
									<dt>Endpoint</dt>
									<dd class="mono t10-url" title={i.base_url}>{i.base_url}</dd>
								</dl>
								{#if bad && i.last_error}
									<p class="t10-err"><b class="mono">{i.last_error.code}</b> {i.last_error.message}</p>
								{/if}
								<div class="t10-serves">
									<span class="t10-serves-h"
										>Serves for {jobLabel}{#if served.length}: <b>{req.length}</b> required ·
											{opt.length} optional{/if}</span
									>
									{#if served.length === 0}
										<span class="muted">No operation of this job is routed here.</span>
									{:else}
										<span class="t10-ops">
											{#each req as o (o.key)}
												<Chip
													text={o.key}
													variant={FIT_VARIANT[o.fit]}
													title="required · {FIT_TEXT[o.fit]}"
												/>
											{/each}
											{#each opt as o (o.key)}
												<Chip text={o.key} variant="muted" title="optional · {FIT_TEXT[o.fit]}" />
											{/each}
										</span>
									{/if}
								</div>
							</article>
						{/each}
					</div>
				{/if}
			</Card>
		{/each}
	{/snippet}
</LoadState>

<Card
	title="Connect another system"
	sub="Templates you can connect, ranked by how many of the required operations of {jobLabel} they implement. Drafts may serve test routes only."
	cls="flush"
>
	<LoadState resource={catalogue} empty="No templates available.">
		{#snippet children(page)}
			{#if gaps.length}
				<p class="t10-gapnote">
					No connected system serves
					{#each gaps as g, k (g.pair)}<span class="mono">{g.pair}</span>{k < gaps.length - 1
							? ', '
							: ''}{/each}
					yet — look for “fills” below.
				</p>
			{/if}
			<div class="tbl-wrap ph-stack">
				<table class="tbl" data-testid="catalogue">
					<thead>
						<tr><th>System</th><th>Covers your required ops</th><th>Auth</th><th>Status</th><th></th></tr>
					</thead>
					<tbody>
						{#each page.items as row (row.scope + row.key + row.version)}
							{@const fills = row.key === 'generic_rest' ? [] : fillsGaps(row, gaps)}
							<tr>
								<td>
									<span class="cell-title">{row.label}</span>
									<span class="cell-sub mono">{row.key}@{row.version} · {row.vendor}</span>
									{#if connectedKeys.has(`${row.key}@${row.version}`)}<Chip
											text="connected"
											variant="ok"
											title="You already run a system on this version"
										/>{/if}
								</td>
								<td data-label="Covers">
									{#if row.key === 'generic_rest'}
										<span class="muted">any — via your own templates</span>
									{:else}
										<span class="row">
											<Meter
												value={row.required_total ? row.implements_required / row.required_total : 0}
												cls="s-ok"
											/>
											<span
												>implements <b>{row.implements_required}</b> of your {row.required_total} required ops</span
											>
										</span>
										{#if fills.length}
											<span class="t10-fills"
												>fills {#each fills as g (g)}<Chip text={g} variant="accent" />{/each}</span
											>
										{/if}
										<span class="cell-sub"
											>{row.implements_total} operation{row.implements_total === 1 ? '' : 's'} in total</span
										>
									{/if}
								</td>
								<td class="mono" data-label="Auth">{row.auth_kind}</td>
								<td data-label="Status">
									<Chip
										text={row.status === 'draft' ? 'draft · test only' : row.status}
										variant={row.status === 'active' ? 'ok' : 'warn'}
									/>
								</td>
								<td>
									{#if row.key !== 'generic_rest'}
										<button
											class="btn sm"
											type="button"
											{...writeAttrs(session, 'integrations_manager')}
											onclick={() => startAdd(row)}>Add instance</button
										>
									{:else}
										<a class="btn sm" href="/admin/connections/templates">Build a template</a>
									{/if}
								</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
			{#if !writable}<p class="note">{writeHint('integrations_manager')}</p>{/if}
		{/snippet}
	</LoadState>
</Card>

<Drawer bind:open={adding} title="Add {pick?.label ?? 'system'}" {dirty}>
	{#snippet sub()}<span class="mono">{pick?.key}@{pick?.version}</span>{/snippet}
	{#if pick}
		<form
			class="stack"
			onsubmit={(e) => {
				e.preventDefault();
				void submit();
			}}
		>
			<div class="form-grid">
				<div class="field">
					<label for="sys-id">Id</label>
					<input id="sys-id" class="input mono" bind:value={form.id} required />
				</div>
				<div class="field">
					<label for="sys-env">Environment</label>
					<select id="sys-env" class="input" bind:value={form.env}>
						<option value="prod">prod</option>
						<option value="test">test</option>
					</select>
				</div>
			</div>
			<div class="field">
				<label for="sys-label">Label</label>
				<input id="sys-label" class="input" bind:value={form.label} />
			</div>
			{#if pick.stub_capable}
				<label class="row">
					<input type="checkbox" bind:checked={form.stub} />
					Stub instance (recorded fixtures, fault injection) — test only, never counts for readiness
				</label>
				{#if form.stub && form.env === 'prod'}<p class="s-warn">Stub instances are refused on prod.</p>{/if}
			{/if}
			{#if !form.stub}
				<div class="field">
					<label for="sys-url">Base URL</label>
					<input
						id="sys-url"
						class="input mono"
						bind:value={form.base_url}
						aria-invalid={!!urlProblem}
						aria-describedby="sys-url-hint"
					/>
					<span id="sys-url-hint" class={urlProblem ? 's-err' : 'muted'}
						>{urlProblem
							? `${urlProblem.code}: ${urlProblem.message}`
							: 'The only host this instance will ever call. Public https hosts only.'}</span
					>
				</div>
			{/if}
			<div class="field">
				<label for="sys-ids">Env IDs (one per line, key = value)</label>
				<textarea
					id="sys-ids"
					class="input"
					rows="3"
					placeholder={'location_id = gid://shopify/Location/…\nreturn_reason_default = OTHER'}
					bind:value={form.env_ids}></textarea>
			</div>
			<div class="field">
				<label for="sys-rps">max_rps (empty = template default)</label>
				<input id="sys-rps" class="input" inputmode="numeric" bind:value={form.max_rps} />
			</div>
			{#if pick.status === 'draft'}
				<Callout kind="gap" title="Draft template"
					>This system can serve test routes only until the template is published.</Callout
				>
			{/if}
			<p class="muted">
				Credential fields ({pick.credential_schema.map((f) => f.label).join(', ') || 'none'}) are set on the
				system page after it is created — they are write-only.
			</p>
			{#if err}<p class="s-err" role="alert"><b class="mono">{err.code}</b> — {err.message}</p>{/if}
			<div class="actions">
				<button class="btn primary" type="submit" disabled={saving}
					>{saving ? 'Adding…' : 'Add system'}</button
				>
				<button class="btn" type="button" onclick={() => (adding = false)}>Cancel</button>
			</div>
		</form>
	{/if}
</Drawer>

<style>
	.t10-model {
		list-style: none;
		margin: 0 0 16px;
		padding: 0;
		display: grid;
		grid-template-columns: repeat(4, minmax(0, 1fr));
		gap: 22px;
		counter-reset: t10;
	}
	.t10-model li {
		position: relative;
		display: flex;
		flex-direction: column;
		gap: 2px;
		padding: 8px 12px;
		border: 1px solid var(--line);
		border-radius: var(--r);
		background: var(--panel);
		font-size: 12.5px;
	}
	.t10-model li:last-child {
		border-color: color-mix(in srgb, var(--accent) 45%, var(--line));
	}
	.t10-model li span {
		color: var(--muted);
	}
	.t10-model li + li::before {
		content: '→';
		position: absolute;
		left: -17px;
		top: 50%;
		transform: translateY(-50%);
		color: var(--faint);
	}
	.t10-lanes {
		display: grid;
		grid-template-columns: repeat(2, minmax(0, 1fr));
		gap: 16px;
	}
	.t10-lane {
		border: 1px solid var(--line-2);
		border-radius: var(--r);
		padding: 14px;
		background: var(--sunk);
		display: flex;
		flex-direction: column;
		gap: 8px;
		min-width: 0;
	}
	.t10-lane header,
	.t10-sys header {
		display: flex;
		justify-content: space-between;
		align-items: flex-start;
		gap: 8px;
	}
	.t10-lane h3 {
		margin: 0;
		font-size: 15px;
	}
	.t10-lane header .muted {
		font-size: 12.5px;
	}
	.t10-big {
		margin: 0;
		display: flex;
		align-items: baseline;
		gap: 6px;
		flex-wrap: wrap;
	}
	.t10-big b {
		font: 600 30px/1 var(--cond);
	}
	.t10-big span {
		color: var(--muted);
		font-size: 16px;
	}
	.t10-big em {
		font-style: normal;
		color: var(--muted);
	}
	.t10-bar {
		display: flex;
		height: 10px;
		border-radius: 999px;
		overflow: hidden;
		gap: 2px;
		background: var(--line-2);
	}
	.t10-bar i,
	.t10-legend i {
		display: block;
	}
	.t10-legend {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-wrap: wrap;
		gap: 4px 12px;
		font-size: 12.5px;
	}
	.t10-legend li {
		display: inline-flex;
		align-items: center;
		gap: 5px;
	}
	.t10-legend i {
		width: 9px;
		height: 9px;
		border-radius: 2px;
	}
	.seg-ok {
		background: var(--ok);
	}
	.seg-untested,
	.seg-stub {
		background: var(--warn);
	}
	.seg-stub {
		opacity: 0.55;
	}
	.seg-failing {
		background: var(--err);
	}
	.seg-missing {
		background: repeating-linear-gradient(45deg, var(--err) 0 3px, transparent 3px 6px);
		outline: 1px solid var(--err);
		outline-offset: -1px;
	}
	.t10-clear {
		margin: 0;
		color: var(--ok);
		font-size: 13px;
	}
	.t10-blockers {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 6px;
		border-top: 1px solid var(--line-2);
		padding-top: 8px;
	}
	.t10-blockers li {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 4px 8px;
		font-size: 13px;
	}
	.t10-why {
		flex-basis: 100%;
		color: var(--muted);
		font-size: 12.5px;
	}
	.t10-grid {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(320px, 1fr));
		gap: 12px;
	}
	.t10-sys {
		border: 1px solid var(--line);
		border-radius: var(--r);
		padding: 12px 14px;
		display: flex;
		flex-direction: column;
		gap: 10px;
		min-width: 0;
		background: var(--panel);
	}
	.t10-sys.bad {
		border-color: color-mix(in srgb, var(--err) 55%, var(--line));
		box-shadow: inset 3px 0 0 var(--err);
	}
	.t10-name {
		display: flex;
		flex-direction: column;
		gap: 2px;
		min-width: 0;
	}
	.t10-name a {
		font-weight: 600;
	}
	.t10-facts {
		margin: 0;
		display: grid;
		grid-template-columns: max-content minmax(0, 1fr);
		gap: 5px 12px;
		font-size: 13px;
	}
	.t10-facts dt {
		color: var(--muted);
	}
	.t10-facts dd {
		margin: 0;
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 4px 6px;
		min-width: 0;
	}
	.t10-url {
		display: block;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.t10-up {
		font: 500 11.5px/1 var(--mono);
		padding: 4px 7px;
		border-radius: 999px;
		border: 1px dashed var(--accent);
		color: var(--accent);
		text-decoration: none;
	}
	.t10-err {
		margin: 0;
		font-size: 12.5px;
		color: var(--err);
	}
	.t10-serves {
		border-top: 1px solid var(--line-2);
		padding-top: 8px;
		display: flex;
		flex-direction: column;
		gap: 6px;
		font-size: 12.5px;
	}
	.t10-serves-h {
		color: var(--muted);
	}
	.t10-serves-h b {
		color: var(--fg);
	}
	.t10-ops,
	.t10-fills {
		display: flex;
		flex-wrap: wrap;
		gap: 4px;
		align-items: center;
	}
	.t10-fills {
		margin-top: 4px;
		font-size: 12.5px;
		color: var(--accent);
	}
	.t10-gapnote {
		margin: 0;
		padding: 10px 16px;
		font-size: 13px;
		border-bottom: 1px solid var(--line-2);
	}
	@media (max-width: 900px) {
		.t10-model {
			grid-template-columns: repeat(2, minmax(0, 1fr));
		}
		.t10-model li:nth-child(3)::before {
			display: none;
		}
		.t10-lanes {
			grid-template-columns: minmax(0, 1fr);
		}
	}
	@media (max-width: 520px) {
		.t10-grid {
			grid-template-columns: minmax(0, 1fr);
		}
	}
</style>
