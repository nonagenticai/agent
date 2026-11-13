<script lang="ts">
	// J25 Backend systems — the systems this job's tools reach (doc 10 §5.1/§5.2 steps 3–4, doc 12 §4): per connected
	// system, whether the agent can use it (credential accepted, address allowed, contract tests green), which tools
	// land on it in Production and Test, and where data stays (D59/D66 residency). The same routes as Tools, read per
	// system. Systems and credentials are environment resources and save live on the system page (D69); the routes
	// that use them ride the release. Data: jobOperations + connectorInstances (connectors), tenantOverview (residency).
	import { onMount } from 'svelte';
	import { api, app } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import { jobPath } from '$lib/scope';
	import { when } from '$lib/runtime/labels';
	import type { ConnectorInstance, JobOperations } from '$lib/api/types/connectors';
	import { RESIDENCY_LABEL, type Residency } from '$lib/api/types/core';
	import { CELL } from '$lib/connectors/J6-routes';
	import {
		EFFECT,
		ENVS,
		jobSystems,
		systemHealth,
		toolKeys,
		type JobSystem,
	} from '$lib/connectors/J25-tools';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Freshness from '$lib/ui/Freshness.svelte';
	import Status from '$lib/ui/Status.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Callout from '$lib/ui/Callout.svelte';

	interface Data {
		ops: JobOperations;
		instances: ConnectorInstance[];
		/** null when the tenant record could not be read: residency then shows as unknown. */
		residency: Residency | null;
	}

	const res = new Resource<Data>(
		async () => {
			const [ops, instances, residency] = await Promise.all([
				api().jobOperations(),
				api()
					.connectorInstances()
					.then((p) => p.items),
				api()
					.tenantOverview()
					.then((o) => o.tenant.residency)
					.catch(() => null),
			]);
			return { ops, instances, residency };
		},
		(d) => d.instances.length === 0 && d.ops.ops.length === 0,
	);
	onMount(() => void res.refresh());

	const ENV_LABEL = { prod: 'Production', test: 'Test' } as const;
	const sysHref = (id: string) => `/admin/connections/systems/${encodeURIComponent(id)}`;

	function host(url: string): string {
		try {
			const u = new URL(url);
			return u.protocol === 'stub:' ? 'local fixtures, no customer data' : u.host;
		} catch {
			return url;
		}
	}
	function credentialText(i: ConnectorInstance): string {
		if (i.status === 'auth_failed')
			return `Rejected by the system${i.last_error ? ` ${when(i.last_error.at)}` : ''}`;
		if (i.stub) return 'Not needed — stub';
		if (i.credential_required === false) return 'Minted per call';
		if (!i.credential.set) return 'Not set';
		return `Set${i.credential.updated_at ? ` ${when(i.credential.updated_at)}` : ''} · write-only`;
	}
	const residencyText = (r: Residency | null) => (r ? RESIDENCY_LABEL[r] : 'not known');
</script>

<PageHead
	id="J25"
	title="Backend systems"
	sub="The systems this job's tools reach: whether the agent can use each one, which tools land on it per environment, and where data stays."
>
	{#snippet actions()}
		<Freshness resource={res} />
		<a class="btn" href={jobPath(app.job, '/tools')}>Tools</a>
		<a class="btn" href="/admin/connections/systems">All systems</a>
	{/snippet}
</PageHead>

<LoadState
	resource={res}
	empty="No systems yet. Connect one under Systems, then route this job's tools to it."
>
	{#snippet children(d)}
		{@const v = jobSystems(d.ops, d.instances)}
		{@const bad = v.systems.filter((s) => systemHealth(s).word === 'err').length}

		<section class="j25b-sum" aria-label="Systems at a glance" data-testid="backends-summary">
			<div>
				<b class="j25b-big">{v.systems.length}</b>
				<span>connected system{v.systems.length === 1 ? '' : 's'} this job uses</span>
			</div>
			<div class:bad={bad > 0}>
				<b class="j25b-big">{bad}</b>
				<span>need you before tools on them can run</span>
			</div>
			<div>
				<b class="j25b-big">{toolKeys(v.systems.flatMap((s) => s.uses)).length}</b>
				<span>tools answered by your systems</span>
			</div>
			<div>
				<b class="j25b-big" data-testid="residency">{residencyText(d.residency)}</b>
				<span>data residency — calls, cases and logs stay here</span>
			</div>
		</section>

		<Callout kind="info" title="What saves where">
			A system's address, environment IDs and credential save live on its system page — they name an
			environment's resources. Which tools reach a system is design: it goes to the job's draft from
			<a href={jobPath(app.job, '/tools')}>Tools</a> or
			<a href={jobPath(app.job, '/operations')}>Operations &amp; routes</a> and reaches customers with the next
			release.
		</Callout>

		<div class="j25b-list" data-testid="backends">
			{#each v.systems as s (s.instance.id)}
				{@render system(s, d)}
			{:else}
				<p class="muted">
					No tool of this job is routed to one of your systems yet. Route one in
					<a href={jobPath(app.job, '/tools')}>Tools</a>.
				</p>
			{/each}
		</div>

		{#if v.platform.length}
			<section class="j25b-plat" aria-labelledby="j25b-plat-h">
				<h2 id="j25b-plat-h">Answered without one of your systems</h2>
				<ul>
					{#each v.platform as p (p.kind)}
						<li>
							<b>{p.label}</b>
							<span class="mono j25b-tools">{toolKeys(p.uses).join(' · ')}</span>
						</li>
					{/each}
				</ul>
			</section>
		{/if}

		{#if v.unused.length}
			<section class="j25b-plat" aria-labelledby="j25b-unused-h" data-testid="backends-unused">
				<h2 id="j25b-unused-h">Your other systems</h2>
				<p class="muted">Connected for your tenant; no tool of this job reaches them.</p>
				<ul>
					{#each v.unused as i (i.id)}
						<li>
							<a href={sysHref(i.id)}>{i.label}</a>
							<span class="muted">{i.template_key}@{i.template_version} · {i.env}</span>
						</li>
					{/each}
				</ul>
			</section>
		{/if}
	{/snippet}
</LoadState>

{#snippet system(s: JobSystem, d: Data)}
	{@const i = s.instance}
	{@const h = systemHealth(s)}
	<article class="j25b-sys" class:err={h.word === 'err'} data-testid="system-{i.id}">
		<header>
			<div>
				<a class="j25b-name" href={sysHref(i.id)}>{i.label}</a>
				<span class="muted mono">{i.template_key}@{i.template_version}</span>
			</div>
			<span class="row">
				{#each s.envs as e (e)}<Chip text={ENV_LABEL[e]} variant="tool" />{/each}
				<Status word={h.word} label={h.label} />
			</span>
		</header>
		{#if i.last_error}
			<p class="s-err j25b-err">
				<span class="mono">{i.last_error.code}</span> — {i.last_error.message} · {when(i.last_error.at)}
			</p>
		{/if}
		<dl class="kvs j25b-kv">
			<dt>Credential</dt>
			<dd>{credentialText(i)}</dd>
			<dt>Contract tests</dt>
			<dd>
				{s.contract.run ? `${s.contract.green} of ${s.contract.run} green` : 'none run yet'}{s.contract
					.unverified
					? ` · ${s.contract.unverified} write unverified`
					: ''}
			</dd>
			<dt>Data residency</dt>
			<dd>
				The agent's call records stay in {residencyText(d.residency)}; the system keeps its own data at
				<span class="mono">{host(i.base_url)}</span>.
			</dd>
		</dl>
		<div class="j25b-uses">
			{#each ENVS as env (env)}
				{@const u = s.uses.filter((x) => x.env === env)}
				{#if u.length}
					<div>
						<span class="j25b-k">{ENV_LABEL[env]} · {u.length} tool{u.length === 1 ? '' : 's'}</span>
						<ul>
							{#each u as x (x.key)}
								<li>
									<span class="mono">{x.key}</span>
									<span class="muted">{EFFECT[x.side_effect].label}</span>
									<Chip text={CELL[x.cell.state].label} variant={CELL[x.cell.state].variant} />
								</li>
							{/each}
						</ul>
					</div>
				{/if}
			{/each}
		</div>
		<div class="row">
			<a class="btn sm" href={sysHref(i.id)}>{h.word === 'ok' ? 'Open system' : 'Fix on the system page'}</a>
			{#if s.blockedUses}<span class="muted"
					>{s.blockedUses} tool route{s.blockedUses === 1 ? '' : 's'} on it cannot run yet</span
				>{/if}
		</div>
	</article>
{/snippet}

<style>
	.j25b-sum {
		display: grid;
		grid-template-columns: repeat(4, minmax(0, 1fr));
		gap: 1px;
		background: var(--line);
		border: 1px solid var(--line);
		border-radius: var(--r);
		overflow: hidden;
		margin-bottom: 14px;
	}
	.j25b-sum > div {
		background: var(--panel);
		padding: 12px 14px;
		display: flex;
		flex-direction: column;
		gap: 2px;
		font-size: 13px;
		color: var(--muted);
	}
	.j25b-sum > div.bad .j25b-big {
		color: var(--err);
	}
	.j25b-big {
		font-size: 22px;
		color: var(--fg);
	}
	.j25b-list {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(min(100%, 420px), 1fr));
		gap: 12px;
		margin-top: 14px;
	}
	.j25b-sys {
		display: flex;
		flex-direction: column;
		gap: 10px;
		padding: 14px 16px;
		background: var(--panel);
		border: 1px solid var(--line);
		border-radius: var(--r);
		min-width: 0;
	}
	.j25b-sys.err {
		box-shadow: inset 3px 0 0 var(--err);
	}
	.j25b-sys header {
		display: flex;
		flex-wrap: wrap;
		justify-content: space-between;
		gap: 6px 12px;
	}
	.j25b-sys header > div {
		display: flex;
		flex-direction: column;
		gap: 2px;
		min-width: 0;
	}
	.j25b-name {
		font-weight: 600;
		overflow-wrap: anywhere;
	}
	.j25b-err {
		margin: 0;
		font-size: 12.5px;
		overflow-wrap: anywhere;
	}
	.j25b-kv {
		margin: 0;
	}
	.j25b-kv dd {
		overflow-wrap: anywhere;
	}
	.j25b-uses {
		display: grid;
		gap: 8px;
	}
	.j25b-uses ul,
	.j25b-plat ul {
		list-style: none;
		margin: 4px 0 0;
		padding: 0;
		display: grid;
		gap: 4px;
		font-size: 13px;
	}
	.j25b-uses li {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 2px 8px;
	}
	.j25b-k {
		font: 600 11px/1.3 var(--mono);
		text-transform: uppercase;
		letter-spacing: 0.04em;
		color: var(--faint);
	}
	.j25b-plat {
		margin-top: 18px;
	}
	.j25b-plat h2 {
		font-size: 14px;
		margin: 0 0 4px;
	}
	.j25b-plat li {
		display: flex;
		flex-wrap: wrap;
		gap: 2px 10px;
	}
	.j25b-tools {
		overflow-wrap: anywhere;
	}
	@media (max-width: 900px) {
		.j25b-sum {
			grid-template-columns: repeat(2, minmax(0, 1fr));
		}
	}
</style>
