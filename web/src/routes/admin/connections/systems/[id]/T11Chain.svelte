<script lang="ts">
	// T11 hero: the translation chain one connected system implements. Jobs ask for canonical operations; the pinned
	// template (plus this tenant's overrides and env IDs) translates each into a vendor call made with the sealed
	// credential, and maps fields, statuses and faults back. Each link carries its own health pip.
	import Chip from '$lib/ui/Chip.svelte';
	import { shortFp } from '$lib/connectors/ops';
	import { rateLimitKind } from '$lib/connectors/T11-limits';
	import { ageDays, type OpRow } from '$lib/connectors/T11-health';
	import type { SystemDetail, SystemInsight } from '$lib/api/types/connectors';

	let { d, rows, insight }: { d: SystemDetail; rows: OpRow[]; insight: SystemInsight | null } = $props();

	type Tone = 'ok' | 'warn' | 'err' | 'muted';
	const i = $derived(d.instance);
	const host = $derived.by(() => {
		try {
			return new URL(i.base_url).host;
		} catch {
			return i.base_url || '—';
		}
	});
	const routedRows = $derived(rows.filter((r) => r.readiness !== 'unrouted'));
	const objects = $derived([...new Set(rows.map((r) => r.object))]);
	const jobs = $derived(
		[...new Set(d.used_by.map((u) => u.job))].map((job) => ({
			job,
			envs: d.used_by.filter((u) => u.job === job).map((u) => `${u.env} · ${u.ops.length}`),
		})),
	);
	const credAge = $derived(ageDays(i.credential.updated_at));
	const nIds = $derived(Object.keys(i.env_ids).length);

	const tones = $derived.by((): Record<'jobs' | 'ops' | 'tpl' | 'cred' | 'sys', Tone> => {
		const blocked = routedRows.some((r) => r.readiness === 'blocked');
		const pending = routedRows.some((r) => r.readiness !== 'ready');
		return {
			jobs: d.used_by.length ? 'ok' : 'muted',
			ops: blocked ? 'err' : pending ? 'warn' : routedRows.length ? 'ok' : 'muted',
			tpl: d.template.status === 'draft' ? 'warn' : insight?.upgrade ? 'warn' : 'ok',
			cred: i.platform
				? 'ok'
				: i.status === 'auth_failed' || (!i.stub && i.credential_required !== false && !i.credential.set)
					? 'err'
					: 'ok',
			sys: i.status === 'invalid_target' || i.status === 'auth_failed' ? 'err' : i.stub ? 'muted' : 'ok',
		};
	});
	const TONE_TEXT: Record<Tone, string> = { ok: 'healthy', warn: 'attention', err: 'broken', muted: 'idle' };
</script>

<section class="t11-chain" aria-label="How your agent reaches this system">
	<ol class="t11-links">
		<li class="t11-node">
			<span class="t11-pip t11-{tones.jobs}" title={TONE_TEXT[tones.jobs]}></span>
			<span class="label">1 · Your jobs</span>
			{#if jobs.length}
				{#each jobs as j (j.job)}
					<a class="t11-strong" href="/admin/jobs/{j.job}/operations">{j.job}</a>
					<span class="t11-sub">{j.envs.join(' · ')} ops routed here</span>
				{/each}
			{:else}
				<span class="t11-strong">No routes yet</span>
				<span class="t11-sub">Route an operation on a job to put this system to work.</span>
			{/if}
		</li>
		<li class="t11-node">
			<span class="t11-pip t11-{tones.ops}" title={TONE_TEXT[tones.ops]}></span>
			<span class="label">2 · Canonical operations</span>
			<span class="t11-strong">{routedRows.length} of {rows.length} routed</span>
			<span class="t11-sub">{objects.join(', ')}</span>
			<span class="t11-sub">What the agent asks for — never vendor calls.</span>
		</li>
		<li class="t11-node t11-core">
			<span class="t11-pip t11-{tones.tpl}" title={TONE_TEXT[tones.tpl]}></span>
			<span class="label">3 · Translation</span>
			<span class="t11-strong mono">{i.template_key}@{i.template_version}</span>
			<span class="t11-sub"
				>{i.template_scope === 'platform' ? 'platform template' : 'your template'} · pinned · {d.template
					.transport}</span
			>
			<span class="t11-sub">
				+ {i.overrides.length} field override{i.overrides.length === 1 ? '' : 's'} · {nIds} env ID{nIds === 1
					? ''
					: 's'}
			</span>
			{#if insight?.upgrade}<Chip text="@{insight.upgrade.to} available" variant="accent" />{/if}
			{#if d.template.status === 'draft'}<Chip text="draft — test only" variant="warn" />{/if}
		</li>
		<li class="t11-node">
			<span class="t11-pip t11-{tones.cred}" title={TONE_TEXT[tones.cred]}></span>
			<span class="label">4 · Credential</span>
			{#if i.platform}
				<span class="t11-strong" data-testid="fingerprint">minted per call</span>
				<span class="t11-sub">nothing stored</span>
			{:else if i.stub}
				<span class="t11-strong" data-testid="fingerprint">none — stub</span>
				<span class="t11-sub">recorded answers only</span>
			{:else}
				<span class="t11-strong">
					{i.credential_required === false ? 'minted per call' : i.credential.set ? 'sealed' : 'not set'} ·
					<span class="mono" data-testid="fingerprint">{shortFp(i.credential.fingerprint)}</span>
				</span>
				<span class="t11-sub">{d.template.auth_kind}{credAge != null ? ` · set ${credAge} d ago` : ''}</span>
			{/if}
		</li>
		<li class="t11-node">
			<span class="t11-pip t11-{tones.sys}" title={TONE_TEXT[tones.sys]}></span>
			<span class="label">5 · {d.template.vendor || 'System'}</span>
			<span class="t11-strong mono t11-host">{i.platform ? 'in-app' : host}</span>
			<span class="t11-sub"
				>{i.env === 'all' ? 'prod + test' : i.env === 'prod' ? 'production' : 'test'}{i.stub
					? ' · stub'
					: ''}{rateLimitKind(insight?.rate_limit) ? ` · ${rateLimitKind(insight?.rate_limit)}` : ''}</span
			>
		</li>
	</ol>
	<p class="t11-caption">
		{#if i.platform}
			Your agent only speaks canonical operations. This platform connector answers them in-app; Agent
			provisions, versions and tests it for you — there is nothing to pin, store or rotate here.
		{:else}
			Your agent only speaks canonical operations. The pinned template turns each one into a call to {d
				.template.vendor || 'your system'} and maps the answer — fields, statuses, faults — back. The platform
			owns the template; you own the pin, the credential, the overrides and the tests.
		{/if}
	</p>
</section>

<style>
	.t11-chain {
		background: var(--panel);
		border: 1px solid var(--line);
		border-radius: var(--r);
		padding: 16px;
	}
	.t11-links {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		grid-template-columns: repeat(5, minmax(0, 1fr));
		gap: 22px;
	}
	.t11-node {
		position: relative;
		display: flex;
		flex-direction: column;
		gap: 4px;
		align-items: flex-start;
		padding: 10px 12px 12px;
		border: 1px solid var(--line-2);
		border-radius: var(--r);
		background: var(--sunk);
		min-width: 0;
	}
	.t11-core {
		border-color: var(--accent);
	}
	/* the arrow between links */
	.t11-node + .t11-node::before {
		content: '→';
		position: absolute;
		left: -18px;
		top: 50%;
		transform: translateY(-50%);
		color: var(--faint);
		font-size: 14px;
	}
	.t11-pip {
		position: absolute;
		top: 12px;
		right: 12px;
		width: 9px;
		height: 9px;
		border-radius: 50%;
		background: var(--faint);
	}
	.t11-ok {
		background: var(--ok);
	}
	.t11-warn {
		background: var(--warn);
	}
	.t11-err {
		background: var(--err);
	}
	.t11-muted {
		background: var(--faint);
	}
	.t11-strong {
		font-weight: 600;
		font-size: 13.5px;
		overflow-wrap: anywhere;
	}
	.t11-sub {
		color: var(--muted);
		font-size: 12px;
		overflow-wrap: anywhere;
	}
	.t11-host {
		font-size: 12.5px;
	}
	.t11-caption {
		margin: 12px 0 0;
		font-size: 12.5px;
		color: var(--muted);
		max-width: 900px;
	}
	@media (max-width: 1100px) {
		.t11-links {
			grid-template-columns: minmax(0, 1fr);
			gap: 18px;
		}
		.t11-node + .t11-node::before {
			content: '↓';
			left: 16px;
			top: -16px;
			transform: none;
		}
	}
</style>
