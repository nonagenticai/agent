<script lang="ts">
	// P1: one live tenant as a portfolio card — who owns it, is it provisioned, licensed, serving, current, used?
	// The row actions (act in, suspend/resume, offboard) sit where that judgement is made.
	import type { PlatformTenantRow, TenantPortfolio } from '$lib/api/types/platform';
	import { superAdminAttrs } from '$lib/auth/roles';
	import { session } from '$lib/auth/session.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import { PROVISION_CHIP, STATUS_CHIP, TARGET_LABEL, utc } from '$lib/platform/ui';

	let {
		t,
		pf,
		busy,
		onactin,
		onstatus,
		onoffboard,
	}: {
		t: PlatformTenantRow;
		pf: TenantPortfolio | null;
		busy: boolean;
		onactin: () => void;
		onstatus: (s: 'active' | 'suspended') => void;
		onoffboard: () => void;
	} = $props();

	const provChip = {
		ok: ['ok', 'ready'],
		pending: ['warn', 'pending'],
		failed: ['err', 'failed'],
		dead: ['dead', 'dead'],
	} as const;

	const compact = (n: number) =>
		n >= 1_000_000
			? `${(n / 1_000_000).toFixed(1)}M`
			: n >= 10_000
				? `${Math.round(n / 1000)}k`
				: n.toLocaleString('en-GB');
	const detail = $derived(`/platform/tenants/${t.id}`);
	const worst = $derived(
		pf?.attention.some((a) => a.severity === 'err')
			? 'err'
			: pf?.attention.some((a) => a.severity === 'warn')
				? 'warn'
				: 'ok',
	);
</script>

<article class="tc tc-{worst}" data-testid="tenant-row" aria-label={t.name}>
	<header class="tc-head">
		<div class="tc-title">
			<a class="tc-name" href={detail} data-testid="tenant-name">{t.name}</a>
			{#if t.brand}<span class="muted">trades as {t.brand}</span>{/if}
		</div>
		<div class="chips">
			<Chip text={t.status} variant={STATUS_CHIP[t.status]} />
			{#if t.residency}<span title="Data residency (D59): pins stores, LLM providers and logs"
					><Chip text={`data in ${t.residency.toUpperCase()}`} variant="tool" /></span
				>{/if}
			{#if pf?.stage === 'provisioning'}<Chip text="provisioning" variant="warn" />{/if}
		</div>
	</header>
	<p class="tc-desc">
		{t.description}
		<span class="mono tc-id" title="Tenant id">{t.id}</span>
	</p>

	<dl class="tc-facts">
		<div>
			<dt class="label">Owner</dt>
			<dd>
				{#if pf && pf.owners.length}
					{pf.owners.map((o) => o.display_name).join(', ')}
				{:else if pf}
					<span class="tc-warn-text">No active tenant_admin</span>
				{:else}
					—
				{/if}
				<span class="tc-sub"
					>{t.users} member{t.users === 1 ? '' : 's'}{pf && pf.members.disabled
						? ` · ${pf.members.disabled} disabled`
						: ''}</span
				>
			</dd>
		</div>
		<div>
			<dt class="label">Licence</dt>
			<dd class="chips">
				<Chip
					text={t.licence.status}
					variant={t.licence.status === 'active' ? 'ok' : t.licence.status === 'inactive' ? 'warn' : 'err'}
				/>
				<Chip text={t.licence.plan} />
				{#if t.licence.kid}<Chip text={`kid ${t.licence.kid}`} title="Sealed with this key id" />{/if}
				{#if t.licence.platform_owned}<Chip text="platform-owned" variant="accent" />{/if}
			</dd>
		</div>
		<div>
			<dt class="label">Usage · 30 d</dt>
			<dd>
				{#if pf && pf.usage_30d.turns}
					<strong>{compact(pf.usage_30d.turns)}</strong> turns
					<span class="tc-sub">{compact(pf.usage_30d.tokens)} tokens · last {pf.usage_30d.last_day}</span>
				{:else if pf}
					<span class="muted">No traffic</span>
				{:else}
					—
				{/if}
			</dd>
		</div>
	</dl>

	<div class="tc-prov">
		<span class="label">Provision-down</span>
		<ol class="tc-targets">
			{#each pf?.provision ?? [] as p (p.target)}
				<li class="tc-target" title={p.error ?? `${TARGET_LABEL[p.target]}: ${p.status ?? 'not queued'}`}>
					<span class="tc-dot dot-{p.status ? PROVISION_CHIP[p.status] : 'muted'}" aria-hidden="true"></span>
					{TARGET_LABEL[p.target]}
					<span class="tc-sub">{p.status === 'succeeded' ? 'done' : (p.status ?? 'not queued')}</span>
				</li>
			{/each}
		</ol>
		<a href={detail} class="tc-provchip">
			<Chip
				text={t.provisioning === 'ok' ? 'ready' : `${t.provisioning_count} ${provChip[t.provisioning][1]}`}
				variant={provChip[t.provisioning][0]}
				title="Licences / Knowledge / Ingest outbox (06 §4.6)"
			/>
		</a>
	</div>

	{#if pf}
		<div class="tc-jobs">
			<span class="label">Jobs ({pf.jobs.length})</span>
			{#each pf.jobs as j (j.key)}
				<div class="tc-job">
					<span class="tc-dot dot-{j.on_bus ? 'ok' : 'muted'}" title={j.on_bus ? 'Serving' : 'Not serving'}
					></span>
					<span class="tc-job-name">{j.label}</span>
					<span class="chips">
						{#if j.release}
							<Chip text={j.release.id} title={`Current release · ${j.release.hash}`} />
						{:else}
							<Chip text="no release" variant="warn" />
						{/if}
						{#if j.behind.length}
							<Chip
								text={`${j.behind.length} pin${j.behind.length === 1 ? '' : 's'} behind`}
								variant="work"
								title={j.behind.map((b) => `${b.layer} v${b.pinned} → v${b.latest}`).join('\n')}
							/>
						{:else}
							<Chip text="on latest" variant="ok" />
						{/if}
						{#if j.generator && (j.generator.status === 'running' || j.generator.status === 'blocked')}
							<a href={`/platform/generator/${j.generator.run}`} class="tc-chiplink"
								><Chip
									text={`run #${j.generator.run} · ${j.generator.step}`}
									variant={j.generator.status === 'blocked' ? 'warn' : 'tool'}
								/></a
							>
						{/if}
					</span>
					{#if j.cases_7d !== null && j.cases_7d > 0}
						<span class="tc-sub tc-cases"
							>{j.cases_7d.toLocaleString('en-GB')} cases · 7 d{j.containment_7d !== null
								? ` · ${Math.round(j.containment_7d * 100)}% contained`
								: ''}</span
						>
					{/if}
				</div>
			{/each}
		</div>
	{/if}

	{#if pf && pf.attention.length}
		<ul class="tc-att" aria-label="Needs attention">
			{#each pf.attention as a, i (i)}
				<li class="att-{a.severity}"><a href={a.href}>{a.text}</a></li>
			{/each}
		</ul>
	{/if}

	<footer class="tc-foot">
		<span class="tc-sub"
			>Created {utc(t.created_at)}{t.suspended_at ? ` · suspended ${utc(t.suspended_at)}` : ''}</span
		>
		<div class="actions tc-actions">
			<button class="btn sm" disabled={busy} onclick={onactin}>Act in tenant</button>
			{#if t.status === 'active'}
				<button
					class="btn sm"
					{...superAdminAttrs(session)}
					disabled={busy || !session.superAdmin}
					onclick={() => onstatus('suspended')}>Suspend</button
				>
			{:else}
				<button
					class="btn sm"
					{...superAdminAttrs(session)}
					disabled={busy || !session.superAdmin}
					onclick={() => onstatus('active')}>Resume</button
				>
			{/if}
			<button
				class="btn sm danger"
				{...superAdminAttrs(session)}
				aria-label={`Offboard ${t.name}`}
				onclick={onoffboard}>Offboard</button
			>
		</div>
	</footer>
</article>

<style>
	.tc {
		background: var(--panel);
		border: 1px solid var(--line);
		border-left: 3px solid var(--tc-tone);
		border-radius: 8px;
		padding: 12px 14px;
		display: flex;
		flex-direction: column;
		gap: 10px;
		min-width: 0;
	}
	.tc-ok {
		--tc-tone: var(--ok);
	}
	.tc-warn {
		--tc-tone: var(--warn);
	}
	.tc-err {
		--tc-tone: var(--err);
	}
	.tc-head {
		display: flex;
		justify-content: space-between;
		gap: 8px;
		align-items: flex-start;
		flex-wrap: wrap;
	}
	.tc-title {
		display: flex;
		flex-direction: column;
		gap: 1px;
		min-width: 0;
		font-size: 12.5px;
	}
	.tc-name {
		font: 600 16px/1.25 var(--cond);
		color: var(--fg);
	}
	.tc-desc {
		margin: -4px 0 0;
		font-size: 12.5px;
		color: var(--muted);
		display: flex;
		flex-wrap: wrap;
		gap: 2px 8px;
	}
	.tc-id {
		font-size: 11px;
		color: var(--faint);
		overflow-wrap: anywhere;
	}
	.tc-facts {
		margin: 0;
		display: grid;
		grid-template-columns: repeat(3, minmax(0, 1fr));
		gap: 10px;
	}
	.tc-facts dd {
		margin: 3px 0 0;
		font-size: 13px;
		min-width: 0;
	}
	.tc-sub {
		display: block;
		font-size: 11.5px;
		color: var(--faint);
	}
	.tc-warn-text {
		color: var(--warn);
	}
	.tc-prov {
		display: grid;
		grid-template-columns: 1fr auto;
		align-items: center;
		gap: 4px 10px;
		padding: 8px 10px;
		background: var(--sunk);
		border-radius: var(--r);
	}
	.tc-prov > .label {
		grid-column: 1 / -1;
	}
	.tc-targets {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		gap: 14px;
		flex-wrap: wrap;
		flex: 1;
	}
	.tc-target {
		display: flex;
		align-items: center;
		gap: 6px;
		font-size: 12.5px;
	}
	.tc-target .tc-sub {
		display: inline;
	}
	.tc-provchip,
	.tc-chiplink {
		text-decoration: none;
	}
	.tc-dot {
		width: 8px;
		height: 8px;
		border-radius: 50%;
		flex: none;
		background: var(--dot);
	}
	.dot-ok {
		--dot: var(--ok);
	}
	.dot-warn {
		--dot: var(--warn);
	}
	.dot-err {
		--dot: var(--err);
	}
	.dot-dead {
		--dot: var(--dead);
	}
	.dot-muted {
		--dot: var(--line);
	}
	.tc-jobs {
		display: flex;
		flex-direction: column;
		gap: 6px;
	}
	.tc-job {
		display: grid;
		grid-template-columns: auto minmax(0, max-content) 1fr;
		align-items: center;
		gap: 4px 8px;
		font-size: 13px;
	}
	.tc-job-name {
		font-weight: 500;
	}
	.tc-cases {
		grid-column: 2 / -1;
	}
	.tc-att {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 4px;
		font-size: 12.5px;
	}
	.tc-att li {
		padding-left: 10px;
		border-left: 2px solid var(--att);
	}
	.tc-att a {
		color: var(--fg);
	}
	.att-err {
		--att: var(--err);
	}
	.att-warn {
		--att: var(--warn);
	}
	.att-info {
		--att: var(--accent);
	}
	.tc-foot {
		display: flex;
		justify-content: space-between;
		align-items: center;
		gap: 8px;
		flex-wrap: wrap;
		border-top: 1px solid var(--line-2);
		padding-top: 10px;
		margin-top: auto;
	}
	.tc-actions {
		justify-content: flex-end;
	}
	@media (max-width: 560px) {
		.tc-facts {
			grid-template-columns: 1fr 1fr;
		}
		.tc-job {
			grid-template-columns: auto 1fr;
		}
		.tc-job .chips {
			grid-column: 2 / -1;
		}
	}
</style>
