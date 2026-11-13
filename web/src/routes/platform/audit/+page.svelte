<script lang="ts">
	// P19 Platform audit (04 §9b K12/K44, 05 grants, K47). Who did what at platform level: every audited action is
	// written to exactly ONE log — the tenant's own audit when the tenant row exists and the event is not a failed
	// create, otherwise here. So this page holds signups, failed creates/imports, unresolved first-admin invites,
	// key reseals and platform catalogue writes. A super_admin's actions INSIDE a tenant (acting in it, suspend,
	// re-home, releases) are in that tenant's audit under their sub. Details hold ids and codes only.
	import { onMount, untrack } from 'svelte';
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { api } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import type { Paged } from '$lib/api/types/core';
	import type { PlatformAuditEvent, PlatformAuditSummary, PlatformTenantRow } from '$lib/api/types/platform';
	import PageHead from '$lib/ui/PageHead.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Pagination from '$lib/ui/Pagination.svelte';
	import Freshness from '$lib/ui/Freshness.svelte';
	import Seg from '$lib/ui/Seg.svelte';
	import { hhmm, utc } from '$lib/platform/ui';
	import {
		FAMILIES,
		byDay,
		familyOf,
		nextStep,
		objectLink,
		sentence,
		threadKey,
	} from '$lib/platform/auditP19';

	const qp = (k: string) => page.url.searchParams.get(k) ?? '';
	let action = $state(qp('action'));
	let family = $state(qp('family'));
	let actor = $state(qp('actor'));
	let status = $state(qp('status'));
	let q = $state(qp('q'));
	/** B1b F-04: the subject tenant and an inclusive UTC start date; both live in the URL query (survive reload). */
	let tenant = $state(qp('tenant'));
	let since = $state(qp('since'));

	const res = new Resource<Paged<PlatformAuditEvent>>(() =>
		api().platformAudit({
			action: action || undefined,
			family: family || undefined,
			actor: actor || undefined,
			status: (status || undefined) as 'ok' | 'failed' | undefined,
			q: q || undefined,
			tenant: tenant || undefined,
			since: since || undefined,
			before: page.url.searchParams.get('before'),
		}),
	);
	const sum = new Resource<PlatformAuditSummary>(() => api().platformAuditSummary());
	/** Every tenant, archived ones included (their events stay here). */
	let tenants = $state<PlatformTenantRow[]>([]);
	onMount(() => {
		void sum.refresh();
		void Promise.all([api().platformTenants(), api().platformTenants({ archived: true })])
			.then(([live, gone]) => {
				tenants = [...live.items, ...gone.items].sort((a, b) => a.name.localeCompare(b.name));
			})
			.catch(() => (tenants = []));
	});
	$effect(() => {
		void [action, family, actor, status, q, tenant, since];
		untrack(() => void res.refresh());
	});
	// The filters into the URL query (replace, no history entry), so a reload or a shared link keeps them.
	$effect(() => {
		const want: Record<string, string> = { action, family, actor, status, q, tenant, since };
		untrack(() => {
			const u = new URL(page.url.href);
			for (const [k, v] of Object.entries(want)) {
				if (v) u.searchParams.set(k, v);
				else u.searchParams.delete(k);
			}
			if (u.search !== page.url.search) void goto(u, { replace: true, reset: false });
		});
	});

	const filtered = $derived(!!(action || family || actor || status || q || tenant || since));
	const famLabel = (k: string) => FAMILIES.find((f) => f.key === k)?.label ?? 'Other';
	const actorLabel = (e: { actor_name?: string | null; actor_id: string }) => e.actor_name || e.actor_id;
	function clear() {
		action = family = actor = status = q = tenant = since = '';
	}
	function pickFamily(k: string) {
		family = family === k ? '' : k;
		action = '';
	}
	function sameSubject(e: PlatformAuditEvent) {
		clear();
		q = e.subject_label.replace(/\s*\(.*\)\s*$/, '');
	}
</script>

<PageHead
	id="P19"
	sub="Who did what at platform level — the events that have no tenant to live in. Newest first, ids and codes only."
>
	{#snippet actions()}<Freshness resource={res} />{/snippet}
</PageHead>

<!-- The routing rule: the whole concept of this page in one picture. -->
<section class="p19-route" data-testid="audit-routing" aria-label="Where an audited action is written">
	<div class="p19-src">
		<span class="p19-k">Every audited action</span>
		<span class="p19-s">actor = the caller's user id, or a system client</span>
	</div>
	<div class="p19-q" aria-hidden="true">
		<span>Tenant row exists, and not a failed create?</span>
	</div>
	<div class="p19-dests">
		<div class="p19-dest p19-there">
			<span class="p19-branch">yes</span>
			<span class="p19-k">That tenant's Audit log</span>
			<span class="p19-s"
				>tenant.create · suspend / resume / offboard · <b>tenant.switch</b> (you acting in a tenant) · user.* ·
				release.* — readable by its members</span
			>
		</div>
		<div class="p19-dest p19-here">
			<span class="p19-branch">no</span>
			<span class="p19-k">Platform audit — this page</span>
			<span class="p19-s"
				>signups · failed creates and imports · unresolved invites · key reseals · catalogue edits —
				super_admin only</span
			>
		</div>
	</div>
</section>

{#if sum.value.state === 'ok'}
	{@const s = sum.value.data}
	<!-- The decision: is there anything I still have to act on? -->
	<Card
		title={s.open.length
			? `Needs you: ${s.open.length} open failure${s.open.length === 1 ? '' : 's'}`
			: 'Nothing open'}
		sub={s.open.length
			? 'Failed attempts that no later success has resolved.'
			: 'Every failed attempt was followed by a success on the same subject.'}
	>
		<div data-testid="audit-open">
			{#if s.open.length}
				<ul class="p19-open">
					{#each s.open as e (e.id)}
						{@const step = nextStep(e)}
						<li>
							<div class="p19-open-h">
								<Chip text={e.action} variant="err" />
								<span class="mono p19-faint">{utc(e.at)}</span>
							</div>
							<p class="p19-sent">{sentence(e)}</p>
							<p class="p19-step">Next: {step.text} <a href={step.href}>{step.link} →</a></p>
						</li>
					{/each}
				</ul>
			{:else}
				<p class="p19-faint">No action needed.</p>
			{/if}
		</div>
	</Card>

	<div class="p19-fams" role="group" aria-label="Filter by kind of event">
		{#each s.families as f (f.family)}
			{@const def = FAMILIES.find((x) => x.key === f.family)}
			<button
				type="button"
				class="p19-fam"
				class:on={family === f.family}
				aria-pressed={family === f.family}
				data-testid="audit-family"
				onclick={() => pickFamily(f.family)}
			>
				<span class="p19-fam-n">{f.count}</span>
				<span class="p19-fam-l">{famLabel(f.family)}</span>
				{#if def}<span class="p19-fam-b">{def.blurb}</span>{/if}
				{#if f.failed}
					<span class="p19-fam-f">{f.failed} failed{f.open ? ` · ${f.open} open` : ''}</span>
				{/if}
			</button>
		{/each}
	</div>
{/if}

<div class="p19-filters">
	<div class="field">
		<label for="audit-action">Action</label>
		<select id="audit-action" class="input" bind:value={action}>
			<option value="">All actions</option>
			{#if sum.value.state === 'ok'}
				{#each sum.value.data.actions as a (a)}<option value={a}>{a}</option>{/each}
			{:else if action}
				<option value={action}>{action}</option>
			{/if}
		</select>
	</div>
	<div class="field">
		<label for="audit-actor">Actor</label>
		<select id="audit-actor" class="input" bind:value={actor}>
			<option value="">Anyone</option>
			{#if sum.value.state === 'ok'}
				{#each sum.value.data.actors as a (a.actor_id)}
					<option value={a.actor_id}
						>{a.actor_name || a.actor_id} ({a.actor_type === 'user' ? 'person' : 'system'}, {a.count})</option
					>
				{/each}
			{/if}
		</select>
	</div>
	<div class="field">
		<label for="audit-tenant">Tenant</label>
		<select id="audit-tenant" class="input" bind:value={tenant} data-testid="audit-tenant">
			<option value="">Any tenant</option>
			{#each tenants as t (t.id)}
				<option value={t.id}>{t.name}{t.status === 'archived' ? ' (archived)' : ''}</option>
			{/each}
			{#if tenant && !tenants.some((t) => t.id === tenant)}<option value={tenant}>{tenant}</option>{/if}
		</select>
	</div>
	<div class="field">
		<label for="audit-since">Since (UTC)</label>
		<input id="audit-since" class="input" type="date" bind:value={since} data-testid="audit-since" />
	</div>
	<div class="field">
		<label for="audit-q">Subject or id</label>
		<input id="audit-q" class="input" type="search" placeholder="Northwind, sg-93c0f2, k2…" bind:value={q} />
	</div>
	<div class="field">
		<span class="p19-lbl">Status</span>
		<Seg
			label="Status"
			items={[
				['', 'Any'],
				['ok', 'ok'],
				['failed', 'failed'],
			]}
			bind:active={status}
		/>
	</div>
	{#if filtered}<button type="button" class="btn p19-clear" onclick={clear}>Clear filters</button>{/if}
</div>

<Card cls="flush">
	<LoadState resource={res} loading="Loading platform audit…" empty="No platform events match these filters.">
		{#snippet children(p)}
			<div class="p19-tl">
				{#each byDay(p.items) as g (g.day)}
					<section class="p19-day" aria-label={g.day}>
						<h3 class="p19-day-h mono">{g.day}</h3>
						<ol>
							{#each g.events as e (e.id)}
								{@const obj = objectLink(e)}
								<li class="p19-ev" class:failed={e.status === 'failed'} data-testid="audit-row">
									<span class="p19-time mono">{hhmm(e.at)}</span>
									<span class="p19-dot" aria-hidden="true"></span>
									<div class="p19-body">
										<div class="p19-line">
											<Chip text={e.action} variant={e.status === 'failed' ? 'err' : 'accent'} />
											<span class="p19-fam-tag">{famLabel(familyOf(e.action))}</span>
											{#if e.outcome?.state === 'open'}
												<Chip text="failed · open" variant="err" />
											{:else if e.outcome?.state === 'resolved'}
												<Chip
													text={`failed · resolved by ${e.outcome.by_action} ${hhmm(e.outcome.by_at)}`}
													variant="ok"
												/>
											{/if}
										</div>
										<p class="p19-sent">{sentence(e)}</p>
										<div class="p19-meta">
											<span
												>by <b>{actorLabel(e)}</b>
												<span class="p19-faint"
													>{e.actor_type === 'user' ? 'person' : 'system client'}{e.actor_name
														? ` · ${e.actor_id}`
														: ''}</span
												></span
											>
											<a href={obj.href} data-testid="audit-object">{obj.label} →</a>
											{#if threadKey(e)}
												<button type="button" class="linkish" onclick={() => sameSubject(e)}
													>all for this subject</button
												>
											{/if}
										</div>
										<details class="p19-det">
											<summary>Details ({Object.keys(e.details).length})</summary>
											<div class="chips">
												{#each Object.entries(e.details) as [k, v] (k)}<Chip text={`${k}=${v}`} />{/each}
											</div>
										</details>
									</div>
								</li>
							{/each}
						</ol>
					</section>
				{/each}
			</div>
			<Pagination next={p.next} count={p.items.length} />
		{/snippet}
	</LoadState>
</Card>

<p class="note">
	Looking for what you did inside a tenant — acting in it, suspending it, re-homing a user, cutting a release?
	Those events are in that tenant's own Audit log under your user id: pick the tenant from <a
		href="/platform/tenants">Tenants</a
	>, then open Audit log.
</p>

<style>
	/* routing picture */
	.p19-route {
		display: grid;
		grid-template-columns: minmax(150px, 1fr) minmax(150px, 1fr) minmax(260px, 2.4fr);
		gap: 10px;
		align-items: stretch;
		margin: 0 0 16px;
	}
	.p19-src,
	.p19-q,
	.p19-dest {
		border: 1px solid var(--line);
		border-radius: var(--r);
		background: var(--panel);
		padding: 10px 12px;
		display: flex;
		flex-direction: column;
		gap: 4px;
		justify-content: center;
	}
	.p19-q {
		background: var(--sunk);
		border-style: dashed;
		font-size: 13px;
		text-align: center;
		position: relative;
	}
	.p19-q::before,
	.p19-q::after {
		content: '→';
		position: absolute;
		top: 50%;
		transform: translateY(-50%);
		color: var(--muted);
	}
	.p19-q::before {
		left: -10px;
	}
	.p19-q::after {
		right: -10px;
	}
	.p19-dests {
		display: grid;
		gap: 8px;
	}
	.p19-here {
		border-color: var(--accent);
		border-width: 2px;
	}
	.p19-k {
		font-weight: 600;
	}
	.p19-s {
		font-size: 12.5px;
		color: var(--muted);
	}
	.p19-branch {
		font-family: var(--mono);
		font-size: 11px;
		text-transform: uppercase;
		color: var(--muted);
	}
	.p19-here .p19-branch {
		color: var(--accent);
	}
	/* open failures */
	.p19-open {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 12px;
	}
	.p19-open li {
		border-left: 3px solid var(--err);
		padding-left: 10px;
	}
	.p19-open-h {
		display: flex;
		gap: 8px;
		align-items: center;
		flex-wrap: wrap;
	}
	.p19-step {
		margin: 2px 0 0;
		font-size: 13px;
	}
	.p19-sent {
		margin: 4px 0 0;
	}
	.p19-faint {
		color: var(--muted);
		font-size: 12.5px;
	}
	/* families */
	.p19-fams {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(170px, 1fr));
		gap: 10px;
		margin: 16px 0;
	}
	.p19-fam {
		text-align: left;
		border: 1px solid var(--line);
		border-radius: var(--r);
		background: var(--panel);
		color: var(--fg);
		padding: 10px 12px;
		display: flex;
		flex-direction: column;
		gap: 2px;
		cursor: pointer;
		font: inherit;
	}
	.p19-fam:hover {
		border-color: var(--accent);
	}
	.p19-fam.on {
		border-color: var(--accent);
		box-shadow: inset 0 0 0 1px var(--accent);
	}
	.p19-fam-n {
		font-size: 22px;
		font-weight: 600;
		line-height: 1.1;
	}
	.p19-fam-l {
		font-weight: 600;
	}
	.p19-fam-b {
		font-size: 12px;
		color: var(--muted);
	}
	.p19-fam-f {
		font-size: 12px;
		color: var(--err);
	}
	/* filters */
	.p19-filters {
		display: flex;
		flex-wrap: wrap;
		gap: 12px;
		align-items: flex-end;
		margin: 0 0 12px;
	}
	.p19-filters .field {
		min-width: 180px;
	}
	.p19-lbl {
		display: block;
		font-size: 12.5px;
		font-weight: 500;
		margin-bottom: 4px;
	}
	/* timeline */
	.p19-tl {
		padding: 4px 16px 8px;
	}
	.p19-day-h {
		font-size: 12px;
		color: var(--muted);
		margin: 14px 0 6px;
		font-weight: 600;
	}
	.p19-day ol {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.p19-ev {
		display: grid;
		grid-template-columns: 44px 14px 1fr;
		gap: 8px;
		padding: 8px 0;
		border-top: 1px solid var(--line-2);
	}
	.p19-time {
		font-size: 12.5px;
		color: var(--muted);
		padding-top: 3px;
	}
	.p19-dot {
		width: 10px;
		height: 10px;
		border-radius: 50%;
		background: var(--accent);
		margin-top: 6px;
	}
	.p19-ev.failed .p19-dot {
		background: var(--err);
	}
	.p19-body {
		min-width: 0;
	}
	.p19-line {
		display: flex;
		gap: 8px;
		align-items: center;
		flex-wrap: wrap;
	}
	.p19-fam-tag {
		font-size: 12px;
		color: var(--muted);
	}
	.p19-meta {
		display: flex;
		gap: 14px;
		flex-wrap: wrap;
		font-size: 13px;
		margin-top: 4px;
		align-items: baseline;
	}
	.linkish {
		background: none;
		border: 0;
		padding: 0;
		color: var(--accent);
		cursor: pointer;
		font: inherit;
	}
	.p19-det {
		margin-top: 4px;
		font-size: 12.5px;
	}
	.p19-det summary {
		cursor: pointer;
		color: var(--muted);
	}
	.p19-det .chips {
		margin-top: 4px;
		overflow-wrap: anywhere;
		min-width: 0;
	}
	/* long key=value chips (uuids) wrap inside the column instead of pushing past the edge */
	.p19-det .chips :global(.chip) {
		white-space: normal;
		overflow-wrap: anywhere;
		max-width: 100%;
		line-height: 1.3;
	}
	@media (max-width: 760px) {
		.p19-route {
			grid-template-columns: 1fr;
		}
		.p19-q::before,
		.p19-q::after {
			content: none;
		}
		.p19-filters .field {
			min-width: 0;
			flex: 1 1 140px;
		}
		.p19-tl {
			padding: 4px 10px 8px;
		}
		.p19-ev {
			grid-template-columns: 40px 10px 1fr;
		}
	}
</style>
