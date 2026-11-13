<script lang="ts">
	// P16 Connector templates — the platform catalogue (D13, K49, doc 10 §4.2–§4.5, doc 12 §4). A template maps one
	// vendor API onto canonical operations, object statuses and error codes; the platform versions it, tenants pin a
	// version as a system. The page leads with that model and the decisions it raises (drift, drafts to publish,
	// tenants on old versions, status maps the object model does not know), then shows coverage against the
	// catalogue and each job, then versions × tenant pins.
	import '$lib/phone-stack.css';
	import { onMount } from 'svelte';
	import { api, guard } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Status from '$lib/ui/Status.svelte';
	import Kpi from '$lib/ui/Kpi.svelte';
	import Tabs from '$lib/ui/Tabs.svelte';
	import Seg from '$lib/ui/Seg.svelte';
	import MatrixTable from '$lib/ui/MatrixTable.svelte';
	import TierChip from '$lib/connectors/TierChip.svelte';
	import { rateLimitText } from '$lib/connectors/T11-limits';
	import TemplateModelP16 from './TemplateModelP16.svelte';
	import CreatePlatformTemplate from '$lib/connectors/CreatePlatformTemplate.svelte';
	import {
		attention,
		jobForWave,
		latestPerKey,
		requiredCoverage,
		servedBy,
		serves,
		templateHref as href,
		tplId,
		versionsByKey,
		groupAttention,
		jobServed,
		type Attention,
		type AttentionKind,
	} from '$lib/connectors/coverage-P16';
	import type {
		ConnectorTemplateOverview,
		ConnectorTemplateSummary,
		TemplateCoverage,
		TemplateWave,
	} from '$lib/api/types/connectors';

	const res = new Resource<ConnectorTemplateOverview>(() => api().connectorTemplateOverview());
	onMount(async () => {
		await res.refresh();
		// Coverage is measured against the first job archetype until the viewer picks another.
		if (!jobView && res.value.state === 'ok') jobView = res.value.data.jobs[0]?.key ?? 'catalogue';
	});

	let view = $state('templates');
	let filter = $state('all');
	let jobView = $state('');
	let busy = $state('');

	const WAVES: [TemplateWave, string, string][] = [
		[
			'v1',
			'v1 — retail customer care (K49)',
			'Platform connectors, built-ins and the vendor templates a retail tenant can go live with.',
		],
		[
			'next',
			'Next — same shapes, low cost',
			'Drafts: WooCommerce, Freshdesk, AfterShip. Owner may swap Shopify → WooCommerce or Stripe → Mollie (K49).',
		],
		[
			'itsd',
			'ITSD wave (D26)',
			'Drafts for the IT service desk job: ServiceNow, Entra ID + Graph, Okta. Twilio and CXone/Genesys follow.',
		],
	];
	const AUTH: Record<string, string> = {
		none: 'no auth',
		api_key_header: 'API key header',
		basic: 'basic',
		bearer_static: 'bearer (static)',
		oauth2_client_credentials: 'OAuth2 client credentials',
		bundle_token: 'licence bundle token',
		mtls: 'mutual TLS (client certificate)',
	};
	const KIND: Record<AttentionKind, [string, 'err' | 'warn' | 'accent' | 'muted']> = {
		drift: ['drift', 'err'],
		draft_in_use: ['draft in use', 'warn'],
		blocked: ['blocked', 'err'],
		publishable: ['publish', 'accent'],
		old_pin: ['old pin', 'warn'],
		status_map: ['status map', 'warn'],
	};

	const GROUP: Record<AttentionKind, (n: number) => string> = {
		drift: (n) => `${n} versions drift from their vendor API`,
		draft_in_use: (n) =>
			`${n} drafts are already pinned by tenants — they serve test-env routes only until published`,
		blocked: (n) => `${n} drafts cannot be published yet`,
		old_pin: (n) => `${n} older versions are still pinned by tenants`,
		publishable: (n) => `${n} drafts have no validation blockers — run T2, then publish`,
		status_map: (n) =>
			`${n} versions map system statuses onto states the object model does not define — rename the target or add the state`,
	};
	const shown = (rows: ConnectorTemplateSummary[], wave: TemplateWave) =>
		rows.filter((t) => t.wave === wave && (filter === 'all' || t.status === filter));
	const covOf = (o: ConnectorTemplateOverview, t: { key: string; version: string }) =>
		o.coverage.find((c) => c.key === t.key && c.version === t.version);
	const exampleOf = (o: ConnectorTemplateOverview) =>
		o.templates.find((t) => t.key === 'shopify_admin' && t.status === 'active' && !t.drift) ??
		o.templates.find((t) => !t.platform_connector && t.status === 'active') ??
		null;
	const mapClass = (m: TemplateCoverage['status_maps'][number]) =>
		m.unknown.length ? 'warn' : m.mapped.length === m.canonical.length ? 'ok' : 'muted';

	async function runT2(t: { key: string; version: string }) {
		busy = `t2:${tplId(t)}`;
		await guard(
			() => api().runTemplateSandbox(t.key, t.version),
			`T2 sandbox run finished for ${tplId(t)}.`,
			'T2 run refused',
		);
		busy = '';
		await res.refresh();
	}
	async function publish(t: { key: string; version: string }) {
		busy = `pub:${tplId(t)}`;
		await guard(
			() => api().publishConnectorTemplate(t.key, t.version),
			`${tplId(t)} published — immutable from now on.`,
			'Not published',
		);
		busy = '';
		await res.refresh();
	}
</script>

<PageHead
	id="P16"
	sub="A connector template maps one system’s API onto the canonical operations, object statuses and error codes. The platform versions it; each tenant pins a version as one of its systems. Decide here which versions are fit to offer, which vendor drift needs a new version, and who still runs an old one."
>
	{#snippet actions()}<CreatePlatformTemplate oncreated={() => res.refresh()} />{/snippet}
</PageHead>

<LoadState resource={res} empty="No connector templates yet.">
	{#snippet children(o)}
		{@const todo = attention(o)}
		{@const active = o.templates.filter((t) => t.status === 'active')}
		{@const drafts = o.templates.filter((t) => t.status === 'draft')}
		{@const oldPins = todo.filter((a) => a.kind === 'old_pin')}
		{@const careJob = jobForWave(o.jobs, 'v1')}
		{@const careServed = careJob ? jobServed(o, careJob) : null}

		<TemplateModelP16 example={exampleOf(o)} />

		<div class="kpis p16-kpis">
			<Kpi
				label="Active versions"
				value={String(active.length)}
				note="{new Set(active.map((t) => t.key)).size} templates tenants can pin"
			/>
			<Kpi
				label="Drafts"
				value={String(drafts.length)}
				note="{todo.filter((a) => a.kind === 'blocked').length} blocked from publishing"
			/>
			<Kpi
				label="Tenants on an older version"
				value={String(oldPins.reduce((n, a) => n + (covOf(o, a)?.pinned_by.length ?? 0), 0))}
				note="pins kept until the tenant re-points"
			/>
			<Kpi
				label="Customer care, required ops"
				value={careServed ? `${careServed.served}/${careServed.total}` : '—'}
				note={!careServed
					? 'no job expectation'
					: careServed.unserved.length
						? `not served by an active template: ${careServed.unserved.join(', ')}`
						: 'every required pair served by an active template'}
			/>
		</div>

		<Card
			title="Needs a decision"
			sub="Most urgent first. Each line is one template version; open it for the per-operation detail."
			cls="flush"
		>
			{#if todo.length === 0}
				<div class="empty">Nothing waiting: no drift, no drafts, every tenant on the newest version.</div>
			{:else}
				<ul class="p16-todo" data-testid="p16-attention">
					{#each groupAttention(todo) as g (g.kind)}
						{@const many = g.items.length > 1}
						<li class="p16-todo-row">
							<Chip text={KIND[g.kind][0]} variant={KIND[g.kind][1]} />
							{#if many}
								<div class="p16-todo-main">
									<strong>{GROUP[g.kind](g.items.length)}</strong>
									<ul class="p16-todo-sub">
										{#each g.items as a (tplId(a))}
											<li>
												<span class="p16-todo-main">
													<a class="mono" href={href(a)}>{a.key}<span class="muted">@{a.version}</span></a>
													<span class="cell-sub">{a.detail}</span>
												</span>
												{@render act(a)}
											</li>
										{/each}
									</ul>
								</div>
							{:else}
								{#each g.items as a (tplId(a))}
									<div class="p16-todo-main">
										{#if a.kind === 'drift'}
											<strong>{a.title}</strong>
										{:else}
											<strong
												><a class="mono" href={href(a)}>{a.key}<span class="muted">@{a.version}</span></a> —
												{a.title}</strong
											>
										{/if}
										<span class="cell-sub">{a.detail}</span>
									</div>
									{@render act(a)}
								{/each}
							{/if}
						</li>
					{/each}
				</ul>
				{#snippet act(a: Attention)}
					<span class="row nowrap p16-todo-act">
						{#if a.kind === 'publishable'}
							{@const t = o.templates.find((x) => x.key === a.key && x.version === a.version)}
							{#if t && t.t2.status !== 'green'}
								<button class="btn sm" type="button" disabled={busy !== ''} onclick={() => runT2(a)}
									>{busy === `t2:${tplId(a)}` ? 'Running…' : 'Run T2'}</button
								>
							{:else}
								<button class="btn sm primary" type="button" disabled={busy !== ''} onclick={() => publish(a)}
									>{busy === `pub:${tplId(a)}` ? 'Publishing…' : 'Publish'}</button
								>
							{/if}
						{:else}
							<a class="btn sm" href={href(a)}>Open</a>
						{/if}
					</span>
				{/snippet}
			{/if}
		</Card>

		<div class="split p16-views">
			<Tabs
				items={[
					['templates', 'Templates', o.templates.length],
					['coverage', 'Coverage'],
					['versions', 'Versions & tenants'],
				]}
				bind:active={view}
				label="Template views"
			/>
			{#if view === 'templates'}
				<Seg
					items={[
						['all', 'All'],
						['active', 'Active'],
						['draft', 'Draft'],
						['retired', 'Retired'],
					]}
					bind:active={filter}
					label="Status"
				/>
			{:else if view === 'coverage'}
				<Seg
					items={[
						...o.jobs.map((j) => [j.key, j.label] as [string, string]),
						['catalogue', 'Whole catalogue'],
					]}
					bind:active={jobView}
					label="Measure against"
				/>
			{/if}
		</div>

		{#if view === 'templates'}
			{#each WAVES as [wave, title, sub] (wave)}
				{@const rows = shown(o.templates, wave)}
				{@const job = jobForWave(o.jobs, wave)}
				<Card {title} {sub} cls="flush">
					{#if rows.length === 0}
						<div class="empty">No {filter === 'all' ? '' : filter} templates in this wave.</div>
					{:else}
						<div class="tbl-wrap ph-stack">
							<table class="tbl" data-testid="templates-{wave}">
								<thead>
									<tr>
										<th>Template</th>
										<th title="Required (object, verb) pairs of {job?.label ?? 'the job'} this version serves"
											>Covers {job ? `(${job.label})` : ''}</th
										>
										<th>Status mapping</th>
										<th>Tests</th>
										<th>Pinned by</th>
										<th>Status</th>
										<th></th>
									</tr>
								</thead>
								<tbody>
									{#each rows as t (tplId(t))}
										{@const c = covOf(o, t)}
										{@const rc = c && job ? requiredCoverage(c, job) : null}
										<tr>
											<td class="p16-tpl">
												<a class="cell-title mono" href={href(t)}>{t.key}@{t.version}</a>
												<span class="cell-sub"
													>{t.label} · {t.system_class}{#if t.platform_connector}
														· <Chip text="platform" variant="accent" />{/if}</span
												>
												<span class="cell-sub p16-meta">
													{t.transport}{t.alt_transport ? ` + ${t.alt_transport}` : ''} · {AUTH[t.auth_kind]}
													· {rateLimitText(t.rate_limit_model).toLowerCase()} · {t.vendor_api_pin}
													{#if t.webhooks.length}
														<span title={t.webhooks.map((w) => `${w.topic} — ${w.signature}`).join('\n')}
															>· {t.webhooks.length} webhook{t.webhooks.length > 1 ? 's' : ''}
															<Chip text="inert" variant="muted" /></span
														>
													{/if}
												</span>
											</td>
											<td class="p16-cov" data-label="Covers">
												{#if c?.any}
													<span class="mono">any</span>
													<span class="cell-sub"
														>base of {c.bases_of.length} tenant template{c.bases_of.length === 1
															? ''
															: 's'}</span
													>
												{:else if rc}
													<span class="p16-bar" aria-hidden="true"
														><i style="width:{(rc.covered / rc.total) * 100}%"></i><i
															class="p"
															style="width:{(rc.partial / rc.total) * 100}%"
														></i></span
													>
													<span class="mono">{rc.covered}/{rc.total}</span>
													<span class="cell-sub"
														>required{rc.partial ? ` · ${rc.partial} partial` : ''} · {t.implements.length}
														op{t.implements.length === 1 ? '' : 's'} total</span
													>
												{/if}
											</td>
											<td data-label="Status map">
												{#if c && c.status_maps.length}
													<span class="chips">
														{#each c.status_maps as m (m.object)}
															<Chip
																text="{m.object} {m.mapped.length}/{m.canonical.length}{m.unknown.length
																	? ` +${m.unknown.length}?`
																	: ''}"
																variant={mapClass(m)}
																title="Canonical: {m.canonical.join(', ')}&#10;Mapped: {m.mapped.join(', ') ||
																	'none'}{m.unknown.length ? `\nNot canonical: ${m.unknown.join(', ')}` : ''}"
															/>
														{/each}
													</span>
												{:else}
													<span class="muted">—</span>
													<span class="cell-sub">no status-bearing reads</span>
												{/if}
											</td>
											<td data-label="Tests">
												<span class="chips"
													><TierChip tier="T1" result={t.t1} /><TierChip tier="T2" result={t.t2} /></span
												>
											</td>
											<td class="p16-pins" data-label="Pinned by">
												{#if c && c.pinned_by.length}
													{#each [...new Set(c.pinned_by.map((p) => p.tenant))] as tn (tn)}
														<span class="cell-sub"
															>{tn}
															<span class="mono"
																>{c.pinned_by
																	.filter((p) => p.tenant === tn)
																	.map((p) => p.env)
																	.join(' · ')}</span
															></span
														>
													{/each}
												{:else}
													<span class="muted">none</span>
												{/if}
											</td>
											<td data-label="Status"><Status word={t.status} /></td>
											<td>
												<span class="row nowrap">
													{#if !t.platform_connector && t.status !== 'retired'}
														<button
															class="btn sm"
															type="button"
															disabled={busy !== ''}
															onclick={() => runT2(t)}
															>{busy === `t2:${tplId(t)}` ? 'Running…' : 'Run T2'}</button
														>
													{/if}
													{#if t.status === 'draft'}
														<button
															class="btn sm primary"
															type="button"
															disabled={busy !== ''}
															title={c?.publish_blockers.length
																? `Will be refused: ${c.publish_blockers.join('; ')}`
																: 'Publish — immutable from then on'}
															onclick={() => publish(t)}
															>{busy === `pub:${tplId(t)}` ? 'Publishing…' : 'Publish'}</button
														>
													{/if}
												</span>
											</td>
										</tr>
									{/each}
								</tbody>
							</table>
						</div>
					{/if}
				</Card>
			{/each}
		{:else if view === 'coverage'}
			{@const jk = jobView || o.jobs[0]?.key || 'catalogue'}
			{@const job = o.jobs.find((j) => j.key === jk) ?? null}
			{@const rows = job
				? [...job.ops].sort((a, b) => Number(b.required) - Number(a.required))
				: o.catalogue.map((c) => ({ key: c.key, required: false }))}
			{@const cols = latestPerKey(o.coverage).filter((c) => c.any || rows.some((r) => serves(c, r.key)))}
			{@const tplOf = (c: TemplateCoverage) =>
				o.templates.find((t) => t.key === c.key && t.version === c.version)}
			{@const gaps = rows.filter((r) => servedBy(o, r.key).level !== 'active')}
			<Card
				title={job
					? `What ${job.label} expects — and which template serves it`
					: 'The canonical operation catalogue'}
				sub={job
					? `${rows.filter((r) => r.required).length} required and ${rows.filter((r) => !r.required).length} optional operations. Columns are the newest version of each template that serves at least one of them; a variant is served by its unqualified operation (“via *”).`
					: `${rows.length} canonical operations (doc 11 §4.3.5). A template implements a subset; generic REST can be declared to serve any of them.`}
			>
				<p class="p16-cov-sum" data-testid="p16-coverage-summary">
					{#if gaps.length === 0}
						<span class="s-ok">●</span> Every operation here is served by an active template.
					{:else}
						<span class="s-warn">●</span>
						{gaps.filter((g) => servedBy(o, g.key).level === 'none').length} with no template ·
						{gaps.filter((g) => servedBy(o, g.key).level === 'draft').length} served by a draft only ·
						{rows.length - gaps.length} served by an active template.
					{/if}
				</p>
				<MatrixTable
					{rows}
					columns={cols}
					rowKey={(r) => r.key}
					colKey={(c) => c.key}
					caption="Which template serves which canonical operation"
					corner="Operation"
				>
					{#snippet rowHeader(r)}
						{@const s = servedBy(o, r.key)}
						<span class="p16-op">
							<span class="mono">{r.key}</span>
							{#if job}<Chip text={r.required ? 'R' : 'O'} variant={r.required ? 'work' : 'muted'} />{/if}
							{#if s.level === 'none'}<Chip text="gap" variant="err" />{:else if s.level === 'draft'}<Chip
									text="draft only"
									variant="warn"
								/>{/if}
						</span>
					{/snippet}
					{#snippet colHeader(c)}
						{@const t = tplOf(c)}
						<a class="mono" href={href(c)} title="{t?.label} · {t?.status}">{c.key}</a>
						{#if t?.status === 'draft'}<span class="cell-sub">draft</span>{/if}
					{/snippet}
					{#snippet cell(r, c)}
						{#if c.any}<span class="muted" title="declared per tenant template">any</span>
						{:else if c.implements.includes(r.key)}<span class="s-ok" aria-label="implements">●</span>
						{:else if serves(c, r.key)}<span class="s-accent" title="served by the unqualified operation"
								>via *</span
							>
						{:else}<span class="s-muted" aria-label="not implemented">·</span>{/if}
					{/snippet}
				</MatrixTable>
			</Card>
		{:else}
			<div class="p16-versions">
				{#each versionsByKey(o.templates) as [key, versions] (key)}
					{@const head = versions[0]!}
					<Card title={head.label} sub="{key} · {head.system_class}">
						<ol class="p16-vlist">
							{#each versions as v (v.version)}
								{@const c = covOf(o, v)}
								{@const newerActive = versions.some(
									(x) =>
										x.status === 'active' && x.version.localeCompare(v.version, 'en', { numeric: true }) > 0,
								)}
								<li class="p16-v">
									<div class="split">
										<a class="mono cell-title" href={href(v)}>@{v.version}</a>
										<span class="row">
											<TierChip tier="T2" result={v.t2} />
											<Status word={v.status} />
										</span>
									</div>
									<span class="cell-sub"
										>{v.vendor_api_pin}{v.published_at
											? ` · published ${v.published_at.slice(0, 10)}`
											: ' · not published'}</span
									>
									{#if c && c.pinned_by.length}
										<span class="chips p16-pinchips">
											{#each c.pinned_by as p (p.tenant_id + p.instance_id)}
												<Chip
													text="{p.tenant} · {p.env}"
													variant={newerActive ? 'warn' : 'cust'}
													title="system {p.instance_id}"
												/>
											{/each}
										</span>
										{#if newerActive}
											<span class="cell-sub s-warn"
												>a newer active version exists — tenants re-point on their system page</span
											>
										{/if}
									{:else}
										<span class="cell-sub">no tenant pins this version</span>
									{/if}
									{#if c && c.bases_of.length}
										<span class="cell-sub"
											>Base of {c.bases_of
												.map((b) => `${b.key}@${b.version} (${b.tenant}, ${b.status})`)
												.join(', ')}</span
										>
									{/if}
								</li>
							{/each}
						</ol>
					</Card>
				{/each}
			</div>
		{/if}
	{/snippet}
</LoadState>

<style>
	.p16-kpis {
		margin-bottom: 16px;
	}
	.p16-views {
		margin: 18px 0 10px;
	}
	.p16-todo {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.p16-todo-row {
		display: grid;
		grid-template-columns: 96px 1fr auto;
		gap: 12px;
		align-items: start;
		padding: 10px 16px;
		border-top: 1px solid var(--line);
	}
	.p16-todo-row:first-child {
		border-top: 0;
	}
	.p16-todo-main {
		min-width: 0;
		font-size: 13px;
	}
	.p16-todo-main strong {
		font-weight: 500;
	}
	.p16-todo-sub {
		list-style: none;
		margin: 6px 0 0;
		padding: 0;
		display: grid;
		gap: 6px;
	}
	.p16-todo-sub li {
		display: flex;
		justify-content: space-between;
		gap: 10px;
		align-items: start;
		padding-left: 10px;
		border-left: 2px solid var(--line);
	}
	.nowrap {
		flex-wrap: nowrap;
	}
	.p16-tpl {
		min-width: 260px;
	}
	.p16-meta {
		font-size: 11.5px;
		max-width: 420px;
	}
	.p16-cov {
		min-width: 130px;
	}
	.p16-bar {
		display: flex;
		height: 6px;
		width: 90px;
		border-radius: 3px;
		background: var(--sunk);
		border: 1px solid var(--line);
		overflow: hidden;
		margin-bottom: 4px;
	}
	.p16-bar i {
		display: block;
		background: var(--ok);
	}
	.p16-bar i.p {
		background: color-mix(in srgb, var(--warn) 70%, transparent);
	}
	.p16-pins {
		min-width: 120px;
	}
	.p16-cov-sum {
		margin: 0 0 10px;
		font-size: 13px;
	}
	.p16-op {
		display: inline-flex;
		gap: 6px;
		align-items: center;
		flex-wrap: wrap;
	}
	.p16-versions {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(300px, 1fr));
		gap: 16px;
	}
	.p16-vlist {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 10px;
	}
	.p16-v {
		display: grid;
		gap: 3px;
		padding-left: 10px;
		border-left: 2px solid var(--line);
	}
	.p16-pinchips {
		margin-top: 3px;
	}
	@media (max-width: 640px) {
		.p16-todo-row {
			grid-template-columns: 1fr;
			gap: 6px;
		}
	}
</style>
