<script lang="ts">
	// T4 Audit log (04 §9b K12/K44, 05 §4 K41, 12 §9). The tenant's ONE append-only record of who did what here:
	// its people (by sub), its AI agents (operation calls, policy denials), the system (provisioning, gate-2
	// refusals ≤1/h) — and the platform owner whenever they step in via tenant.switch. Decision on this page:
	// "is anything still broken that I must act on, and who changed this?" So: open failures first, then a
	// day-grouped timeline of plain sentences, each linked to the object it touched. Agent calls are a separate
	// view so routine traffic does not bury the changes. Read-only for every member.
	import { onMount, untrack } from 'svelte';
	import { page } from '$app/state';
	import { setQuery } from '$lib/query';
	import { duration } from '$lib/format';
	import { api, app } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import { debounce } from '$lib/debounce';
	import type { Paged } from '$lib/api/types/core';
	import type { ActorType, AuditEvent, AuditFamily, AuditStatus, AuditSummary } from '$lib/api/types/tenant';
	import { ACTOR_TYPES, STATUSES, statusLabel, statusVariant } from './labels';
	import {
		FAMILIES,
		byDay,
		familyOf,
		isCall,
		nextStep,
		objectLink,
		releasePartner,
		sentence,
	} from '$lib/tenant/auditT4';
	import { day, hhmm, when } from '$lib/tenant/ui';
	import PageHead from '$lib/ui/PageHead.svelte';
	import Pagination from '$lib/ui/Pagination.svelte';
	import Freshness from '$lib/ui/Freshness.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Seg from '$lib/ui/Seg.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';

	const PAGE = 50;
	const param = (k: string) => page.url.searchParams.get(k) ?? '';
	const q = $derived(param('q'));
	const actorType = $derived(param('actor_type'));
	const actor = $derived(param('actor'));
	const action = $derived(param('action'));
	const status = $derived(param('status'));
	const job = $derived(param('job'));
	const family = $derived(param('family'));
	const before = $derived(param('before'));
	const narrowed = $derived(!!(q || actorType || actor || action || status || job || family));
	/** Default lens: changes only. Any explicit filter searches everything unless a view was picked. */
	const view = $derived(param('view') || (narrowed ? 'all' : 'changes'));
	let text = $state(untrack(() => q));
	$effect(() => {
		text = q;
	});
	const pushText = debounce((v: string) => void setQuery({ q: v }), 250);

	const res = new Resource<Paged<AuditEvent>>(
		() =>
			api().tenantAudit({
				q: q || undefined,
				actor_type: (actorType || undefined) as ActorType | undefined,
				actor: actor || undefined,
				action: action || undefined,
				status: (status || undefined) as AuditStatus | undefined,
				job: job || undefined,
				family: (family || undefined) as AuditFamily | undefined,
				view: view === 'all' ? undefined : (view as 'changes' | 'calls'),
				before: before || null,
				limit: PAGE,
			}),
		(p) => p.items.length === 0,
	);
	const sum = new Resource<AuditSummary>(() => api().tenantAuditSummary());
	onMount(() => void sum.refresh());
	const cur = $derived(res.value);
	$effect(() => {
		void [q, actorType, actor, action, status, job, family, view, before];
		res.value = { state: 'loading' };
		void res.refresh();
	});

	const rows = $derived(cur.state === 'ok' ? cur.data.items : []);
	const famLabel = (k: string) =>
		k === 'calls' ? 'Agent call' : (FAMILIES.find((f) => f.key === k)?.label ?? 'Other');
	const actorName = (e: AuditEvent) => e.actor_name || e.actor_id;
	const actorKind = (e: AuditEvent) =>
		e.platform_actor
			? 'platform owner'
			: e.actor_type === 'human'
				? 'person'
				: e.actor_type === 'ai_agent'
					? 'AI agent'
					: 'system';
	const set = (k: string) => (e: Event) =>
		void setQuery({ [k]: (e.currentTarget as HTMLSelectElement).value });
	const CLEAR = { q: '', actor_type: '', actor: '', action: '', status: '', job: '', family: '' };
	function clear() {
		text = '';
		void setQuery({ ...CLEAR, view: '' });
	}
	const pickView = (v: string) => void setQuery({ ...CLEAR, view: v === 'changes' ? '' : v });
	const pickFamily = (k: string) => void setQuery({ family: family === k ? '' : k, view: '' });
	const pickActor = (id: string) => {
		text = '';
		void setQuery({ ...CLEAR, actor: actor === id ? '' : id, view: 'all' });
	};
	/** "1 min earlier" / "2 h later": when `b` happened relative to `a`. */
	function gap(a: string, b: string): string {
		const d = (Date.parse(b) - Date.parse(a)) / 60_000;
		const m = Math.round(Math.abs(d));
		if (m < 1) return 'in the same call';
		return `${duration(m)} ${d < 0 ? 'earlier' : 'later'}`;
	}
	/** Seg state mirrors the URL lens; a click on the Seg writes the URL. */
	let lens = $state(untrack(() => view));
	$effect(() => {
		lens = view;
	});
	$effect(() => {
		const l = lens;
		untrack(() => {
			if (l !== view) pickView(l);
		});
	});
	const VIEWS: [string, string][] = [
		['changes', 'Changes'],
		['calls', 'Agent calls'],
		['all', 'Everything'],
	];
	const dotKind = (e: AuditEvent) =>
		e.platform_actor
			? 'plat'
			: e.actor_type === 'ai_agent'
				? 'ai'
				: e.actor_type === 'system'
					? 'sys'
					: 'human';
</script>

<PageHead
	id="T4"
	sub="Everything done in this tenant: by your people, your AI agents, the system — and the platform owner when they step in. Append-only; ids and codes, never message text or secrets."
>
	{#snippet actions()}<Freshness resource={res} />{/snippet}
</PageHead>

{#if sum.value.state === 'ok'}
	{@const s = sum.value.data}
	<!-- Who writes here: the four kinds of actor, each a filter. -->
	<section class="t4-who" aria-label="Who acts in this tenant" data-testid="audit-actors">
		{#each [['human', 'People', 'Members, by their user id'], ['platform', 'Platform owner', 'Acting in your tenant — always shown to you'], ['ai_agent', 'AI agents', 'Operation calls your agents made'], ['system', 'System', 'Set-up, sync and licence checks']] as [k, label, blurb] (k)}
			{@const list = s.actors.filter((a) =>
				k === 'platform'
					? a.platform
					: k === 'human'
						? a.actor_type === 'human' && !a.platform
						: a.actor_type === k,
			)}
			<div class="t4-who-c t4-k-{k}">
				<div class="t4-who-h">
					<span
						class="t4-dot t4-d-{k === 'platform'
							? 'plat'
							: k === 'ai_agent'
								? 'ai'
								: k === 'system'
									? 'sys'
									: 'human'}"
					></span>
					<b>{label}</b>
					<span class="t4-n">{list.reduce((n, a) => n + a.count, 0)}</span>
				</div>
				<span class="t4-faint">{blurb}</span>
				<div class="t4-names">
					{#each list.slice(0, 4) as a (a.actor_id)}
						<button
							type="button"
							class="t4-name"
							class:on={actor === a.actor_id}
							aria-pressed={actor === a.actor_id}
							onclick={() => pickActor(a.actor_id)}>{a.actor_name || a.actor_id} · {a.count}</button
						>
					{:else}
						<span class="t4-faint">none</span>
					{/each}
				</div>
			</div>
		{/each}
	</section>

	<!-- The decision: is anything still broken that I must act on? -->
	<Card
		title={s.open.length
			? `Needs you: ${s.open.length} open failure${s.open.length === 1 ? '' : 's'}`
			: 'Nothing open'}
		sub={s.open.length
			? 'Failed rows that no later success on the same object has resolved.'
			: 'Every failure was followed by a success on the same object.'}
	>
		<div data-testid="audit-open">
			{#if s.open.length}
				<ul class="t4-open">
					{#each s.open as e (e.id)}
						{@const step = nextStep(e)}
						<li>
							<div class="t4-line">
								<Chip text={e.action} variant="err" />
								<span class="mono t4-faint">{when(e.at)} UTC</span>
								{#if e.job}<span class="mono t4-faint">{e.job}</span>{/if}
							</div>
							<p class="t4-sent"><b>{actorName(e)}</b> {sentence(e)}</p>
							<p class="t4-step">
								Next: {step.text} <a href={step.href}>{step.link} →</a>
							</p>
						</li>
					{/each}
				</ul>
			{:else}
				<p class="t4-faint">No action needed.</p>
			{/if}
		</div>
	</Card>
{:else if sum.value.state === 'unavailable'}
	<Unavailable reason={sum.value.reason} retry={sum.retry} />
{/if}

<div class="t4-bar">
	<Seg label="Which events" items={VIEWS} bind:active={lens} />
	{#if sum.value.state === 'ok' && view !== 'calls'}
		<div class="t4-fams" role="group" aria-label="Kind of change">
			{#each sum.value.data.families as f (f.family)}
				{@const def = FAMILIES.find((x) => x.key === f.family)}
				<button
					type="button"
					class="t4-fam"
					class:on={family === f.family}
					aria-pressed={family === f.family}
					title={def?.blurb}
					onclick={() => pickFamily(f.family)}
					>{famLabel(f.family)} <span class="t4-faint">{f.count}</span>{#if f.open}<span class="t4-bad">
							· {f.open} open</span
						>{/if}</button
				>
			{/each}
		</div>
	{:else if sum.value.state === 'ok'}
		{@const c = sum.value.data.calls}
		<p class="t4-calls" data-testid="audit-calls-mix">
			{c.total} operation calls: <b>{c.success}</b> ok · <b>{c.denied}</b> not allowed by policy ·
			<b>{c.error}</b> failed · <b>{c.fallback}</b> fell back. Denials are the tool scope working; failures
			are the system answering badly.
			<a href={`/admin/jobs/${app.jobs[0]?.key ?? ''}/operation-calls`}>Operation calls →</a>
		</p>
	{/if}
</div>

<Card cls="flush">
	<div class="t4-filters">
		<input
			class="input t4-grow"
			type="search"
			value={text}
			oninput={(e) => {
				text = e.currentTarget.value;
				pushText(text);
			}}
			placeholder="Search a name, action, object or case id"
			aria-label="Search audit log"
		/>
		<select class="input t4-auto" value={actorType} onchange={set('actor_type')} aria-label="Actor type">
			<option value="">All actors</option>
			{#each ACTOR_TYPES as [k, l] (k)}<option value={k}>{l}</option>{/each}
		</select>
		<select class="input t4-auto" value={status} onchange={set('status')} aria-label="Status">
			<option value="">Any status</option>
			{#each STATUSES as [k, l] (k)}<option value={k}>{l}</option>{/each}
		</select>
		<select class="input t4-auto" value={job} onchange={set('job')} aria-label="Job">
			<option value="">All jobs</option>
			{#each app.jobs as j (j.key)}<option value={j.key}>{j.label}</option>{/each}
		</select>
		{#if narrowed || view !== 'changes'}<button class="btn sm" type="button" onclick={clear}>Clear</button
			>{/if}
	</div>

	{#if cur.state === 'loading'}
		<p class="t4-faint t4-pad">Loading…</p>
	{:else if cur.state === 'unavailable'}
		<div class="t4-pad"><Unavailable reason={cur.reason} retry={res.retry} /></div>
	{:else if cur.state === 'empty'}
		<div class="empty t4-pad">
			{narrowed ? 'No audit events match these filters.' : 'No audit events yet.'}
		</div>
	{:else}
		<div class="t4-tl" data-testid="audit-rows">
			{#each byDay(rows) as g (g.day)}
				<section aria-label={g.day}>
					<h3 class="t4-day">{day(g.day)} {g.day.slice(0, 4)}</h3>
					<ol>
						{#each g.events as e (e.id)}
							{@const o = objectLink(e)}
							{@const pair = releasePartner(e, rows)}
							<li
								class="t4-ev"
								class:call={isCall(e) && e.status === 'success'}
								class:bad={e.status === 'failure' || e.status === 'error'}
								data-testid="audit-row"
								data-action={e.action}
							>
								<span class="t4-time mono">{hhmm(e.at)}</span>
								<span class="t4-dot t4-d-{dotKind(e)}" aria-hidden="true"></span>
								<div class="t4-body">
									<p class="t4-sent">
										<b>{actorName(e)}</b>
										<span class="t4-kind" class:plat={e.platform_actor}>{actorKind(e)}</span>
										{sentence(e)}
									</p>
									<div class="t4-meta">
										<span class="mono t4-code">{e.action}</span>
										{#if e.status !== 'success'}<Chip
												text={statusLabel(e.status)}
												variant={statusVariant(e.status)}
											/>{/if}
										{#if e.outcome?.state === 'open'}
											<Chip text="open" variant="err" />
										{:else if e.outcome?.state === 'resolved'}
											<Chip
												text={`resolved by ${e.outcome.by_action} ${hhmm(e.outcome.by_at)}`}
												variant="ok"
											/>
										{/if}
										{#if !isCall(e)}<span class="t4-faint">{famLabel(familyOf(e.action))}</span>{/if}
										{#if e.job && view !== 'calls'}<span class="mono t4-faint">{e.job}</span>{/if}
										<a href={o.href} data-testid="audit-object">{o.label} →</a>
									</div>
									{#if pair}
										<p class="t4-pair">
											{e.action === 'release.apply' ? 'Cut' : 'Applied'} by
											<b>{actorName(pair)}</b>
											{gap(e.at, pair.at)} ({pair.action}, {statusLabel(pair.status).toLowerCase()}) — every
											release writes both rows.
										</p>
									{/if}
									{#if Object.keys(e.details).length}
										<details class="t4-det">
											<summary>Details</summary>
											<div class="t4-chips">
												<Chip text={`event=${e.id}`} />
												<Chip text={`actor_id=${e.actor_id}`} />
												{#each Object.entries(e.details) as [k, v] (k)}<Chip
														text={`${k}=${Array.isArray(v) ? v.join(', ') : v}`}
													/>{/each}
											</div>
										</details>
									{/if}
								</div>
							</li>
						{/each}
					</ol>
				</section>
			{/each}
		</div>
	{/if}
</Card>

{#if cur.state === 'ok' || (cur.state === 'empty' && cur.data.next)}<Pagination
		next={cur.data.next}
		count={cur.data.items.length}
		busy={res.refreshing}
	/>
{:else if before && cur.state !== 'loading'}<Pagination escape next={null} count={0} />{/if}

<p class="note">
	Releases always write two rows: <span class="mono">release.approve</span> (the cut, frozen) and
	<span class="mono">release.apply</span> (goes live). Platform-level events with no tenant to live in — signups,
	failed creates — are in the platform owner's own log, not here.
</p>

<style>
	/* who acts */
	.t4-who {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(190px, 1fr));
		gap: 10px;
		margin: 0 0 16px;
	}
	.t4-who-c {
		border: 1px solid var(--line);
		border-radius: var(--r);
		background: var(--panel);
		padding: 10px 12px;
		display: flex;
		flex-direction: column;
		gap: 4px;
	}
	.t4-k-platform {
		border-color: var(--warn);
	}
	.t4-who-h {
		display: flex;
		align-items: center;
		gap: 8px;
	}
	.t4-n {
		margin-left: auto;
		font-size: 20px;
		font-weight: 600;
	}
	.t4-names {
		display: flex;
		flex-wrap: wrap;
		gap: 4px;
		margin-top: 2px;
	}
	.t4-name,
	.t4-fam {
		border: 1px solid var(--line);
		background: var(--panel);
		color: var(--fg);
		border-radius: 999px;
		padding: 2px 9px;
		font: inherit;
		font-size: 12.5px;
		cursor: pointer;
	}
	.t4-name:hover,
	.t4-fam:hover,
	.t4-name.on,
	.t4-fam.on {
		border-color: var(--accent);
	}
	.t4-name.on,
	.t4-fam.on {
		box-shadow: inset 0 0 0 1px var(--accent);
	}
	/* dots: one colour per kind of actor */
	.t4-dot {
		width: 10px;
		height: 10px;
		border-radius: 50%;
		flex: none;
		background: var(--accent);
	}
	.t4-d-ai {
		background: var(--work);
		border-radius: 2px;
	}
	.t4-d-sys {
		background: transparent;
		border: 2px solid var(--tool);
	}
	.t4-d-plat {
		background: var(--warn);
	}
	/* open failures */
	.t4-open {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 12px;
	}
	.t4-open li {
		border-left: 3px solid var(--err);
		padding-left: 10px;
	}
	.t4-step {
		margin: 2px 0 0;
		font-size: 13px;
	}
	.t4-line,
	.t4-meta {
		display: flex;
		gap: 8px;
		align-items: center;
		flex-wrap: wrap;
	}
	.t4-sent {
		margin: 2px 0 0;
		overflow-wrap: anywhere;
	}
	.t4-faint {
		color: var(--muted);
		font-size: 12.5px;
	}
	.t4-bad {
		color: var(--err);
	}
	/* lens bar */
	.t4-bar {
		display: flex;
		flex-wrap: wrap;
		gap: 12px;
		align-items: center;
		margin: 16px 0 12px;
	}
	.t4-fams {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
	}
	.t4-calls {
		margin: 0;
		font-size: 13px;
		flex: 1 1 320px;
	}
	.t4-filters {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
		padding: 12px 16px;
		align-items: center;
	}
	.t4-grow {
		flex: 1;
		min-width: 180px;
	}
	.t4-auto {
		width: auto;
	}
	.t4-pad {
		padding: 0 16px 16px;
	}
	/* timeline */
	.t4-tl {
		padding: 0 16px 8px;
	}
	.t4-tl ol {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.t4-day {
		font-size: 12.5px;
		color: var(--muted);
		margin: 14px 0 4px;
		font-weight: 600;
	}
	.t4-ev {
		display: grid;
		grid-template-columns: 44px 14px 1fr;
		gap: 8px;
		padding: 8px 0;
		border-top: 1px solid var(--line-2);
	}
	.t4-ev .t4-dot {
		margin-top: 6px;
	}
	.t4-ev.bad .t4-dot {
		background: var(--err);
		border-color: var(--err);
	}
	.t4-ev.call {
		padding: 4px 0;
		font-size: 13px;
	}
	.t4-ev.call .t4-meta {
		display: none;
	}
	.t4-ev.call .t4-det {
		display: none;
	}
	.t4-time {
		font-size: 12.5px;
		color: var(--muted);
		padding-top: 3px;
	}
	.t4-body {
		min-width: 0;
	}
	.t4-kind {
		font-size: 11.5px;
		color: var(--muted);
		border: 1px solid var(--line);
		border-radius: 4px;
		padding: 0 5px;
		margin-right: 2px;
	}
	.t4-kind.plat {
		color: var(--warn);
		border-color: var(--warn);
	}
	.t4-meta {
		font-size: 12.5px;
		margin-top: 4px;
	}
	.t4-code {
		color: var(--muted);
	}
	.t4-pair {
		margin: 4px 0 0;
		font-size: 12.5px;
		color: var(--muted);
	}
	.t4-det {
		margin-top: 4px;
		font-size: 12.5px;
	}
	.t4-det summary {
		cursor: pointer;
		color: var(--muted);
	}
	.t4-chips {
		display: flex;
		flex-wrap: wrap;
		gap: 4px;
		margin-top: 4px;
		overflow-wrap: anywhere;
		min-width: 0;
	}
	/* long key=value chips wrap inside the column instead of pushing past the edge */
	.t4-chips :global(.chip) {
		white-space: normal;
		overflow-wrap: anywhere;
		max-width: 100%;
		line-height: 1.3;
	}
	@media (max-width: 760px) {
		.t4-tl {
			padding: 0 10px 8px;
		}
		.t4-ev {
			grid-template-columns: 38px 10px 1fr;
			gap: 6px;
		}
		.t4-filters {
			padding: 10px;
		}
		.t4-auto {
			flex: 1 1 120px;
		}
	}
</style>
