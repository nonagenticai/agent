<script lang="ts">
	// J15 Release detail: ONE immutable release of this job (K41, 05 §4.1, D54). Leads with what changed for
	// customers against the release before it — customer-visible bundle changes, the tenant design changes it
	// published (draft → test → publish), template upgrades (D15) — then its lifecycle (cut/approved → applied → live
	// → superseded), readiness at apply, production outcomes since (J2), conflicts it raised (J16) and rollback.
	// The frozen content, selection (profile values inlined) and provenance follow as the record.
	import { onMount } from 'svelte';
	import { SELECTION_VALUE_KEYS, selectionValues } from '$lib/api/types/core';
	import { api, app, guardOutcome } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { writeAttrs } from '$lib/auth/roles';
	import { ApiError } from '$lib/api/errors';
	import { ReleaseError } from '$lib/api/areas/release';
	import { Resource } from '$lib/load.svelte';
	import { jobPath } from '$lib/scope';
	import { hrefOf, pageById, routesOf } from '$lib/pages';
	import type {
		DraftSummary,
		ReadinessItem,
		PublishedChange,
		ReadinessBoard,
		ReleaseCompare,
		ReleaseConflict,
		ReleaseDetail,
		ReleaseRow,
		RollbackDirection,
	} from '$lib/api/types/release';
	import type { AuditEvent } from '$lib/api/types/tenant';
	import type { ReleaseWindow } from '$lib/api/types/runtime';
	import {
		bundleDiff,
		containment,
		customerImpact,
		escalation,
		lifecycle,
		nextOf,
		pct,
		pinChanges,
		previousOf,
		profileValueChanges,
		ptDelta,
		rollbackPreviewTitle,
		tenantChanges,
		when,
	} from '$lib/release/J15-detail';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import JsonView from '$lib/ui/JsonView.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import PageHead from '$lib/ui/PageHead.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import RollbackPreviewJ14 from '$lib/release/RollbackPreviewJ14.svelte';
	import Tabs from '$lib/ui/Tabs.svelte';
	import ReleaseValidationTab from './ReleaseValidationTab.svelte';

	let { id }: { id: string } = $props();
	/** Overview (everything below) or the Validation tab: the grid run an approve queues (B8 T12a). */
	const TABS: [string, string][] = [
		['overview', 'Overview'],
		['validation', 'Validation'],
	];
	let tab = $state('overview');
	/** Rollback goes through the impact preview + confirm (05 §4.4): the target's whole design comes back. */
	let rbTarget = $state<string | null>(null);
	let rbOpen = $state(false);
	/** The open preview's direction: the drawer title says roll back, re-apply or apply (not always "roll back"). */
	let rbDirection = $state<RollbackDirection | null>(null);
	function previewRollback(target: string) {
		rbTarget = target;
		rbDirection = null;
		rbOpen = true;
	}

	/** D59/D60 labels for the selection keys. */
	const SEL_KEY: Record<string, string> = { area: 'Areas', channel: 'Channels', region: 'Region' };
	const detail = new Resource<ReleaseDetail>(
		() => api().releaseDetail(id),
		() => false,
	);
	const compare = new Resource<ReleaseCompare>(
		() => api().releaseCompare(id),
		() => false,
	);

	// Context around the release; each piece degrades on its own (a failed side read never hides the release).
	interface Around {
		rows: ReleaseRow[];
		prev: ReleaseDetail | null;
		published: PublishedChange[];
		conflicts: ReleaseConflict[] | null;
		events: AuditEvent[] | null;
		windows: ReleaseWindow[] | null;
		board: ReadinessBoard | null;
		draft: DraftSummary | null;
	}
	let around = $state<Around | null>(null);
	const soft = async <T,>(p: () => Promise<T>): Promise<T | null> => {
		try {
			return await p();
		} catch {
			return null;
		}
	};
	async function loadAround() {
		const [list, published, conflicts, audit, analytics, board, draft] = await Promise.all([
			soft(() => api().releaseList()),
			soft(() => api().publishedChanges()),
			soft(() => api().releaseConflicts('all')),
			soft(() => api().tenantAudit({ job: app.job, q: id, limit: 50 })),
			soft(() => api().jobAnalytics(30)),
			soft(() => api().readinessBoard()),
			soft(() => api().draftSummary()),
		]);
		const rows = list?.items ?? [];
		const p = previousOf(rows, id);
		const prev = p ? await soft(() => api().releaseDetail(p.id)) : null;
		around = {
			rows,
			prev,
			published: published ?? [],
			conflicts: conflicts ? conflicts.items.filter((c) => c.release_id === id) : null,
			events: audit ? audit.items.filter((e) => e.resource_id === id || e.details.release === id) : null,
			windows: analytics?.release_windows ?? null,
			board,
			draft,
		};
	}
	function refreshAll() {
		void detail.refresh();
		void loadAround();
		if (showCompare) void compare.refresh();
	}
	onMount(refreshAll);

	let showCompare = $state(false);
	function toggleCompare() {
		showCompare = !showCompare;
		if (showCompare) void compare.refresh();
	}

	const wa = $derived(writeAttrs(session, 'release_manager'));
	let busy = $state(false);
	let refused = $state<{ code: string; message: string; blocking: ReadinessItem[] } | null>(null);
	/** Apply `target` (this release, or the previous one for "roll back"). */
	async function apply(target: string, rollback: boolean) {
		if (busy) return;
		busy = true;
		refused = null;
		let applied = true;
		const out = await guardOutcome(
			async () => (applied = (await api().applyRelease(target)).applied),
			() =>
				!applied
					? `${target} is already current.`
					: rollback
						? `Rolled back to ${target}.`
						: `Applied ${target}; it is live in Production now.`,
			rollback ? 'Not rolled back' : 'Not applied',
		);
		busy = false;
		if (out.kind !== 'ok' && out.error instanceof ApiError && out.error.status !== 403)
			refused = {
				code: out.error.code,
				message: out.error.message,
				blocking: out.error instanceof ReleaseError ? out.error.blocking : [],
			};
		refreshAll();
	}

	const STATE = {
		current: ['Live in Production', 'ok'],
		approved: ['Approved · not applied', 'accent'],
		applied_earlier: ['Superseded', 'muted'],
	} as const;
	const who = (d: ReleaseDetail, sub: string | null) => (sub ? (d.names[sub] ?? sub) : '—');
	function pageHref(pid: string): string | null {
		const p = pageById(pid);
		const r = p ? routesOf(p)[0] : undefined;
		return p && r ? hrefOf(p, r, app.job) : null;
	}
	const pageTitle = (pid: string) => pageById(pid)?.title ?? pid;
	const relHref = (rid: string) => jobPath(app.job, `/release/${rid}`);
	const CONFLICT_KIND: Record<string, string> = {
		base_changed: 'Template changed under your version',
		base_removed: 'Template removed what you refined',
		base_added: 'Template now defines yours too',
		parent_missing: 'Parent gone',
		job_setting: 'Setting changed under your override',
	};
</script>

<PageHead
	id="J15"
	title="Release {id}"
	sub="One frozen version of this job: what it changed for customers, who cut and applied it, and how it did in Production."
>
	{#snippet actions()}
		<a class="btn" href={jobPath(app.job, '/release')}>All releases</a>
		{#if detail.value.state === 'ok' && detail.value.data.state !== 'current'}
			{@const rb = detail.value.data.state === 'applied_earlier'}
			<button
				class="btn primary"
				type="button"
				disabled={busy || (!rb && wa.disabled)}
				title={rb ? 'See the impact first, then confirm' : wa.title}
				onclick={() => (rb ? previewRollback(id) : apply(id, false))}
				>{rb ? 'Roll back to this…' : 'Apply'}</button
			>
		{/if}
		<button class="btn" type="button" onclick={toggleCompare} aria-pressed={showCompare}
			>{showCompare ? 'Hide comparison' : 'Compare with current'}</button
		>
	{/snippet}
</PageHead>

{#if refused}
	<div class="callout err" role="alert" data-testid="release-refusal" data-code={refused.code}>
		<strong>Refused: {refused.code}</strong>
		<div>
			<p>{refused.message}</p>
			{#if refused.blocking.length}
				<ul>
					{#each refused.blocking as b (b.subject)}<li>
							<span class="mono">{b.subject}</span> — {b.message}. <a href={b.href}>Fix</a>
						</li>{/each}
				</ul>
			{/if}
		</div>
	</div>
{/if}

<LoadState resource={detail}>
	{#snippet children(d)}
		{@const a = around}
		{@const prev = a?.prev ?? null}
		{@const succ = a ? nextOf(a.rows, d.id) : null}
		{@const win = a?.windows?.find((w) => w.release_id === d.id) ?? null}
		{@const prevWin = prev ? (a?.windows?.find((w) => w.release_id === prev.id) ?? null) : null}
		{@const steps = lifecycle(d, a?.events ?? [], succ, win)}
		{@const impact = customerImpact(d.bundle, prev?.bundle ?? null)}
		{@const mine = tenantChanges(d, a?.published ?? [])}
		{@const pins = pinChanges(d.provenance.pins, prev?.provenance.pins ?? null)}
		{@const profile = profileValueChanges(d.selection, prev?.selection ?? null)}
		{@const diff = bundleDiff(d.bundle, prev?.bundle ?? null)}
		{@const current = a?.rows.find((r) => r.state === 'current') ?? null}
		{@const shipped = d.state !== 'approved'}

		<!-- Identity + lifecycle -->
		<section class="j15-hero" data-state={d.state} aria-label="Release identity and lifecycle">
			<div class="j15-id">
				<div class="j15-id-top">
					<Chip text={STATE[d.state][0]} variant={STATE[d.state][1]} />
				</div>
				<p class="j15-hash">
					<span class="muted">bundle</span> <span class="mono">{d.hash}</span>
				</p>
				<p class="j15-immutable">
					<strong>Immutable</strong> — content, selection and provenance never change. Only who applied it, and
					when, is recorded later; rolling back re-applies this exact bundle.
				</p>
				<dl class="kvs j15-kvs" data-testid="release-summary">
					<dt>Release</dt>
					<dd class="mono">{d.id}</dd>
					<dt>Approved</dt>
					<dd>
						{d.approved_by ? `${who(d, d.approved_by)} · ${when(d.approved_at)}` : 'Not approved yet'}
					</dd>
					<dt>First applied</dt>
					<dd>{d.released_by ? `${who(d, d.released_by)} · ${when(d.released_at)}` : 'Never applied'}</dd>
					<dt>Last applied</dt>
					<dd>{d.applied_by ? `${who(d, d.applied_by)} · ${when(d.last_applied_at)}` : '—'}</dd>
				</dl>
			</div>
			<ol class="j15-life" aria-label="Lifecycle" data-testid="j15-lifecycle">
				{#each steps as s, i (s.key)}
					<li class="j15-step {s.status}" data-step={s.key} data-status={s.status}>
						<span class="j15-dot" aria-hidden="true">{s.status === 'done' ? '✓' : i + 1}</span>
						<div>
							<b>{s.label}</b>
							<span class="j15-step-st"
								>{s.status === 'done'
									? 'done'
									: s.status === 'current'
										? 'now'
										: s.status === 'blocked'
											? 'blocked'
											: 'not yet'}</span
							>
							<p class="muted">{s.detail}</p>
						</div>
					</li>
				{/each}
			</ol>
		</section>

		<Tabs items={TABS} bind:active={tab} label="Release sections" />

		{#if tab === 'overview'}
			<!-- What changed for customers -->
			<Card
				title={shipped ? 'What changed for customers' : 'What will change for customers'}
				sub={prev
					? `Against the release before it, ${prev.id}.`
					: a
						? 'First release of this job: everything in it is new.'
						: 'Loading the release before it…'}
			>
				<div class="j15-changes" data-testid="j15-changes">
					{#if prev}
						<p class="j15-vs">
							<a class="mono" href={relHref(prev.id)}>{prev.id}</a>
							<span aria-hidden="true">→</span>
							<b class="mono">{d.id}</b>
						</p>
					{/if}
					<section class="j15-block">
						<h3 class="j15-h">{shipped ? 'Customers noticed' : 'Customers will notice'}</h3>
						{#if !a}
							<p class="muted">Loading…</p>
						{:else if !prev}
							<p class="muted">Nothing to compare with: see the full content below.</p>
						{:else if impact.length}
							<ul class="j15-impact" data-testid="j15-impact">
								{#each impact as l (l.section + l.item)}
									<li class="j15-{l.tone}">
										<span class="j15-sign" aria-label={l.tone === 'add' ? 'added' : 'removed'}
											>{l.tone === 'add' ? '+' : '−'}</span
										>{l.text}
									</li>
								{/each}
							</ul>
						{:else}
							<p class="muted">
								Nothing customer-visible: same channels, regions, requests and topics as {prev.id}.
							</p>
						{/if}
					</section>

					<section class="j15-block" data-testid="j15-tenant-changes">
						<h3 class="j15-h">
							Your design changes {shipped ? 'it published' : 'it will publish'}
							<span class="muted small">({mine.length})</span>
						</h3>
						{#if mine.length}
							<ul class="j15-mine">
								{#each mine as c (c.key)}
									{@const href = pageHref(c.page)}
									{@const rb = (c as PublishedChange).rolled_back}
									<li data-key={c.key}>
										<div class="j15-mine-head">
											<span>{c.label}</span>
											{#if href}<a class="small" {href}>{pageTitle(c.page)}</a>{/if}
										</div>
										{#if c.live !== c.draft}
											<div class="j15-wasnow small">
												<span class="muted">was</span> <span class="mono">{c.live ?? '—'}</span>
												<span aria-hidden="true">→</span>
												<span class="muted">now</span> <span class="mono">{c.draft ?? '—'}</span>
											</div>
										{/if}
										<div class="muted small">saved by {c.by_name} · {when(c.at)}</div>
										{#if rb}
											<div class="small j15-rolled">
												<Chip text="Rolled back" variant="warn" /> on {when(rb.at)} by {rb.by_name} (Production
												went back to {rb.to}){#if (c as PublishedChange).restored_to_draft}
													· restored to the draft{/if}
											</div>
										{/if}
									</li>
								{/each}
							</ul>
						{:else if !shipped}
							<p class="muted">
								No tenant design changes were bundled at this cut — it carries the selection and template
								changes only.
								{#if a?.draft?.count}
									{a.draft.count} unpublished change{a.draft.count === 1 ? '' : 's'} saved after it wait for the
									next cut: <a href={jobPath(app.job, '/release')}>Review &amp; publish</a>.
								{/if}
							</p>
						{:else}
							<p class="muted">This release published no design changes of yours.</p>
						{/if}
					</section>

					<section class="j15-block" data-testid="j15-template">
						<h3 class="j15-h">Template upgrades and selection</h3>
						{#if pins.length || profile.length}
							<ul class="j15-items small">
								{#each pins as p (p.layer)}
									<li>
										<span class="mono">{p.layer}</span>
										{p.from == null ? 'added at' : `v${p.from} →`} <b>v{p.to}</b>
									</li>
								{/each}
								{#each profile as p (p.key)}
									<li>
										profile <span class="mono">{p.key}</span>: {p.from ?? '—'} → <b>{p.to ?? '—'}</b>
									</li>
								{/each}
							</ul>
						{:else if prev}
							<p class="muted">Same template versions and profile values as {prev.id}.</p>
						{:else}
							<p class="muted">—</p>
						{/if}
						{#if prev}
							{@const unchanged = diff.filter((x) => !x.added.length && !x.removed.length)}
							{#if unchanged.length}
								<p class="muted small">
									Unchanged: {unchanged.map((x) => x.label.toLowerCase()).join(', ')}.
								</p>
							{/if}
						{/if}
					</section>
				</div>
			</Card>

			<div class="grid g2">
				<!-- Readiness -->
				<Card
					title={shipped ? 'Readiness at apply' : 'Can it be applied?'}
					sub="The K25 check (J3 Readiness)."
				>
					<div data-testid="j15-readiness">
						{#if d.readiness}
							<p>
								<Chip
									text={d.readiness.ready ? 'Ready' : 'Not ready'}
									variant={d.readiness.ready ? 'ok' : 'err'}
								/>
								recorded {when(d.readiness.checked_at)}
							</p>
							{#if d.readiness.missing.length}
								<ul class="j15-items small">
									{#each d.readiness.missing as m (m.dependency + m.slot)}<li>
											<span class="mono">{m.dependency}.{m.slot}</span> — {m.detail}
										</li>{/each}
								</ul>
							{:else}
								<p class="muted small">Every slot and tested route it needed was in place.</p>
							{/if}
						{:else if d.state === 'approved' && a?.board?.release?.id === d.id}
							{@const b = a!.board!}
							{@const blocking = b.gaps.filter((g) => g.blocks === 'apply')}
							{@const golive = b.gaps.filter((g) => g.blocks === 'go_live')}
							<p>
								<Chip
									text={b.verdict === 'blocked'
										? 'Apply blocked'
										: b.verdict === 'go_live_gaps'
											? 'Applies, gaps remain'
											: 'Ready'}
									variant={b.verdict === 'blocked' ? 'err' : b.verdict === 'go_live_gaps' ? 'warn' : 'ok'}
								/>
								checked now
							</p>
							{#if blocking.length}
								<h4 class="j15-h4">Blocks apply ({blocking.length})</h4>
								<ul class="j15-gaps">
									{#each blocking as g (g.key)}<li>
											<b>{g.title}</b>{#if g.cause}<span class="muted small">
													· needed by {g.cause}</span
												>{/if}
											<div class="muted small">{g.why}</div>
											<a class="small" href={g.href}>{g.fix_label}</a>
										</li>{/each}
								</ul>
							{/if}
							{#if golive.length}
								<p class="muted small">
									{golive.length} more gap{golive.length === 1 ? '' : 's'} before go-live.
									<a href={jobPath(app.job, '/readiness')}>Open Readiness</a>
								</p>
							{/if}
						{:else}
							<p class="muted">Never applied, so never checked.</p>
						{/if}
						<details class="j15-needs">
							<summary>What it needs ({d.requires.slots.length + d.requires.routes.length})</summary>
							<ul class="list-plain">
								{#each d.requires.slots as s}<li>
										<span class="mono">{s}</span> <span class="muted small">W5 slot</span>
									</li>{/each}
								{#each d.requires.routes as r}<li>
										<span class="mono">{r}</span> <span class="muted small">tested route</span>
									</li>{/each}
							</ul>
						</details>
					</div>
				</Card>

				<!-- Production outcomes + rollback -->
				<Card title="In Production" sub="Production conversations only; test-chat traffic is kept apart.">
					<div data-testid="j15-outcomes">
						{#if !a}
							<p class="muted">Loading…</p>
						{:else if d.state === 'approved'}
							<p class="muted">
								No production traffic yet: Production runs
								{#if current}<a class="mono" href={relHref(current.id)}>{current.id}</a>{:else}no release{/if}
								until this one is applied. After that, its outcomes show here and on
								<a href={jobPath(app.job, '/analytics')}>Analytics</a>.
							</p>
						{:else if win && win.cases}
							{@const c = containment(win)}
							{@const e = escalation(win)}
							<div class="j15-out">
								<div><span class="label">Conversations</span><b>{win.cases}</b></div>
								<div>
									<span class="label">Contained</span><b>{pct(c)}</b>
									{#if ptDelta(containment(prevWin), c)}<span class="small muted"
											>{ptDelta(containment(prevWin), c)}</span
										>{/if}
								</div>
								<div>
									<span class="label">Escalated</span><b>{pct(e)}</b>
									{#if ptDelta(escalation(prevWin), e)}<span class="small muted"
											>{ptDelta(escalation(prevWin), e)}</span
										>{/if}
								</div>
								<div>
									<span class="label">CSAT</span><b>{win.csat ?? '—'}</b>
									<span class="small muted">{win.csat_responses} responses</span>
								</div>
							</div>
							{#if prevWin}<p class="muted small">Deltas against {prevWin.release_id}'s live window.</p>{/if}
							{#if win.moves.length}
								<h4 class="j15-h4">Moved most</h4>
								<ul class="j15-items small">
									{#each win.moves as m (m.activity)}<li>
											<span class="mono">{m.activity}</span> contained {pct(m.from)} → <b>{pct(m.to)}</b>
											<span class="muted">({m.cases} cases)</span>
										</li>{/each}
								</ul>
							{/if}
							<p class="small">
								<a href={jobPath(app.job, '/analytics')}>Analytics</a> ·
								<a href={jobPath(app.job, '/cases')}>Cases</a>
							</p>
						{:else}
							<p class="muted">No production outcomes recorded for this release.</p>
						{/if}

						{#if d.state !== 'approved'}
							<div class="j15-rollback" data-testid="j15-rollback">
								<h4 class="j15-h4">Rollback</h4>
								{#if d.state === 'applied_earlier'}
									<p class="small">
										Rolling back makes this release live again in place of {current?.id ??
											'the current release'}
										— its bundle (<span class="mono">{d.hash}</span>) and the design values it had live. You
										see what customers lose first.
									</p>
									<button
										class="btn"
										type="button"
										disabled={busy}
										title="See the impact first, then confirm"
										onclick={() => previewRollback(id)}>Roll back to {d.id}…</button
									>
								{:else if prev}
									<p class="small">
										If this release misbehaves, roll back to the one before it: its whole design comes back
										and the design changes published since are undone (you can restore them to the draft).
										Nothing is deleted; your draft is untouched.
									</p>
									<button
										class="btn"
										type="button"
										disabled={busy}
										title="See the impact first, then confirm"
										onclick={() => previewRollback(prev.id)}>Roll back to {prev.id}…</button
									>
								{:else}
									<p class="muted small">No earlier release to roll back to.</p>
								{/if}
								{#if d.rollbacks?.length}
									<h4 class="j15-h4">Rollback history</h4>
									<ul class="j15-items small" data-testid="j15-rollbacks">
										{#each d.rollbacks as x (x.id)}
											<li data-rollback={x.id}>
												<b
													>{x.direction === 'rollback' ? 'Rolled back' : 'Re-applied'} on {when(x.at)} by {x.by_name}</b
												>
												· <span class="mono">{x.from ?? '—'}</span> → <span class="mono">{x.to}</span>
												{#if x.undone.length}
													<div>
														Undid {x.undone.length} published change{x.undone.length === 1 ? '' : 's'}:
														{x.undone.map((u) => u.label).join('; ')}
													</div>
												{/if}
												{#if x.reapplied.length}
													<div>
														Brought back {x.reapplied.length}: {x.reapplied.map((u) => u.label).join('; ')}
													</div>
												{/if}
												{#if x.draft_moved}<div class="muted">
														{x.draft_moved} unpublished change{x.draft_moved === 1 ? '' : 's'} now on a different
														live value (draft untouched)
													</div>{/if}
											</li>
										{/each}
									</ul>
								{/if}
							</div>
						{/if}
					</div>
				</Card>
			</div>

			<div class="grid g2">
				<Card title="Conflicts it raised" sub="Your refinements the template changed underneath (J16).">
					<div data-testid="j15-conflicts">
						{#if !a}
							<p class="muted">Loading…</p>
						{:else if a.conflicts === null}
							<p class="muted">Conflicts are unavailable right now.</p>
						{:else if a.conflicts.length}
							<ul class="j15-conf">
								{#each a.conflicts as c (c.id)}
									<li data-conflict={c.id} data-status={c.status}>
										<div class="j15-mine-head">
											<b>{CONFLICT_KIND[c.kind] ?? c.kind}</b>
											<Chip
												text={c.status}
												variant={c.status === 'open' ? 'warn' : c.status === 'resolved' ? 'ok' : 'muted'}
											/>
										</div>
										<div class="mono small">{c.key}</div>
										<div class="muted small">
											{c.detail}{#if c.resolution}
												· {c.resolution} by {c.resolved_by_name ?? c.resolved_by}{/if}
										</div>
									</li>
								{/each}
							</ul>
							<a class="small" href={jobPath(app.job, '/release#conflicts')}>Open Conflicts</a>
						{:else if d.state === 'approved'}
							<p class="muted">Conflicts are detected when it is applied.</p>
						{:else}
							<p class="muted">Applying it clashed with none of your refinements.</p>
						{/if}
					</div>
				</Card>

				<Card title="Audit trail" sub="The approve and apply events for this release.">
					<div data-testid="j15-audit">
						{#if !a}
							<p class="muted">Loading…</p>
						{:else if a.events === null}
							<p class="muted">The audit log is unavailable right now.</p>
						{:else if a.events.length}
							{@const pubs = a.events.filter((e) => e.action === 'job.draft_published')}
							<ul class="j15-audit">
								{#each a.events.filter((e) => e.action !== 'job.draft_published') as e (e.id)}
									<li class:fail={e.status !== 'success'}>
										<span class="mono small">{e.action}</span>
										{#if e.status !== 'success'}<Chip text={e.status} variant="err" />{/if}
										<span class="small">{e.actor_name ?? e.actor_id}</span>
										<span class="muted small">{when(e.at)}</span>
										{#if e.details.code}<div class="muted small">
												{e.details.code}{#if Array.isArray(e.details.missing)}: {(
														e.details.missing as string[]
													).join(', ')}{/if}
											</div>{/if}
									</li>
								{/each}
								{#if pubs.length}<li>
										<span class="mono small">job.draft_published</span>
										<span class="small">× {pubs.length}</span>
									</li>{/if}
							</ul>
							<a class="small" href="/admin/audit">Open the audit log</a>
						{:else}
							<p class="muted">No audit events recorded for it.</p>
						{/if}
					</div>
				</Card>
			</div>

			{#if showCompare}
				<Card
					title="Compared with the current release"
					sub="What applying this release would add (+) or take away (−)."
				>
					<LoadState resource={compare}>
						{#snippet children(c)}
							{#if !c.current}
								<div class="empty">This job has no current release to compare with.</div>
							{:else if c.current === d.id}
								<div class="empty">This is the current release.</div>
							{:else}
								<p class="muted">
									Against <a class="mono" href={relHref(c.current)}>{c.current}</a>{#if c.same_hash}
										— same bundle hash, no content difference{/if}.
								</p>
								{@const changed = c.sections.filter((s) => s.added.length || s.removed.length)}
								{#if !changed.length}
									<div class="empty">No differences.</div>
								{:else}
									<div class="tbl-wrap">
										<table class="tbl dense" data-testid="release-compare">
											<thead><tr><th>Section</th><th>Added</th><th>Removed</th></tr></thead>
											<tbody>
												{#each changed as s (s.key)}
													<tr>
														<td>{s.label}</td>
														<td
															>{#each s.added as x}<div class="add mono">+ {x}</div>{:else}<span class="muted"
																	>—</span
																>{/each}</td
														>
														<td
															>{#each s.removed as r}<div class="rm mono">− {r}</div>{:else}<span
																	class="muted">—</span
																>{/each}</td
														>
													</tr>
												{/each}
											</tbody>
										</table>
									</div>
								{/if}
							{/if}
						{/snippet}
					</LoadState>
				</Card>
			{/if}

			<!-- The record: frozen content, selection, provenance -->
			<h2 class="j15-record">The frozen record</h2>
			<p class="muted small j15-record-sub">
				Exactly what this release contains, and what it was compiled from. None of it changes after the cut.
			</p>

			<Card title="Content" sub="The frozen bundle by section; new against the release before it is marked.">
				<div class="j15-sections">
					{#each d.bundle as s (s.key)}
						{@const dd = diff.find((x) => x.key === s.key)}
						{@const added = new Set(prev ? (dd?.added ?? []) : [])}
						<details class="j15-sec" data-section={s.key}>
							<summary>
								<b>{s.label}</b>
								<span class="muted small">{s.items.length}</span>
								{#if added.size}<span class="j15-new">+{added.size} new</span>{/if}
								{#if prev && dd?.removed.length}<span class="j15-gone">−{dd.removed.length}</span>{/if}
							</summary>
							<ul class="j15-items mono small">
								{#each s.items as i}<li class:j15-added={added.has(i)}>
										{#if added.has(i)}<span class="j15-sign">+</span>{/if}{i}
									</li>{:else}<li class="muted">none</li>{/each}
								{#if prev}{#each dd?.removed ?? [] as r}<li class="j15-removed">
											<s>{r}</s>
										</li>{/each}{/if}
							</ul>
						</details>
					{/each}
				</div>
			</Card>

			<div class="grid g2">
				<Card
					title="Selection at cut"
					sub="Profile values are inlined: later profile edits do not change this release."
				>
					<dl class="kvs">
						<dt>Profile</dt>
						<dd class="mono">{d.selection.profile}</dd>
						<dt>Industry</dt>
						<dd>{d.selection.industry}</dd>
						<dt>Departments</dt>
						<dd>{d.selection.departments.join(', ') || 'none'}</dd>
						{#each SELECTION_VALUE_KEYS.map((k) => [k, selectionValues(d.selection, k)] as const) as [k, v] (k)}<dt
							>
								{SEL_KEY[k] ?? k}
							</dt>
							<dd>
								{v.join(', ') || 'none'}{#if k === 'region'}<span class="muted small">
										· placeholder, selects nothing</span
									>{/if}
							</dd>{/each}
					</dl>
					<h3 class="j15-h">Profile values</h3>
					<dl class="kvs" data-testid="profile-values">
						{#each Object.entries(d.selection.profile_values) as [k, v] (k)}<dt class="mono">{k}</dt>
							<dd>{v}</dd>{/each}
					</dl>
				</Card>

				<Card title="Provenance" sub="Which platform revisions and seed versions this release compiled.">
					<p class="j15-pins">
						{#each d.provenance.pins as p (p.layer)}<Chip
								text="{p.layer}@v{p.version}"
								variant="accent"
							/>{/each}
					</p>
					<p class="small muted">
						seed versions <span class="mono">{d.provenance.seed_versions.join(', ')}</span>
					</p>
					<div class="tbl-wrap">
						<table class="tbl dense" data-testid="provenance">
							<thead><tr><th>Scope</th><th>Kind</th><th>Key</th><th class="r">Rev</th></tr></thead>
							<tbody>
								{#each d.provenance.overlay_revs as r (r.id)}
									<tr title={r.id}
										><td>{r.scope}</td><td>{r.kind}</td><td class="mono small">{r.key}</td><td class="r"
											>{r.rev}</td
										></tr
									>
								{/each}
							</tbody>
						</table>
					</div>
					<details>
						<summary>Raw selection + provenance</summary>
						<JsonView value={{ selection: d.selection, provenance: d.provenance }} />
					</details>
				</Card>
			</div>
		{:else}
			<ReleaseValidationTab {id} />
		{/if}
	{/snippet}
</LoadState>

<Drawer bind:open={rbOpen} title={rollbackPreviewTitle(rbDirection, rbTarget ?? '')} wide>
	{#if rbOpen && rbTarget}
		{#key rbTarget}
			<RollbackPreviewJ14
				id={rbTarget}
				ondone={() => {
					rbOpen = false;
					refreshAll();
				}}
				oncancel={() => (rbOpen = false)}
				ondirection={(d) => (rbDirection = d)}
			/>
		{/key}
	{/if}
</Drawer>

<style>
	.j15-rolled {
		margin-top: 4px;
	}
	.small {
		font-size: 12px;
	}
	.j15-hero {
		display: grid;
		grid-template-columns: minmax(0, 1fr) minmax(0, 1.3fr);
		gap: 18px 28px;
		padding: 16px 18px;
		margin: 0 0 14px;
		background: var(--panel);
		border: 1px solid var(--line);
		border-left: 4px solid var(--accent);
		border-radius: var(--r);
	}
	.j15-hero[data-state='current'] {
		border-left-color: var(--ok);
	}
	.j15-hero[data-state='applied_earlier'] {
		border-left-color: var(--line);
	}
	.j15-id-top {
		display: flex;
		gap: 6px;
		flex-wrap: wrap;
	}
	.j15-hash {
		margin: 8px 0 4px;
		font-size: 18px;
	}
	.j15-hash .mono {
		font-weight: 600;
	}
	.j15-immutable {
		margin: 0 0 10px;
		font-size: 12.5px;
		color: var(--muted);
	}
	.j15-immutable strong {
		color: var(--fg);
	}
	.j15-kvs {
		font-size: 13px;
	}
	.j15-life {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		grid-template-columns: repeat(3, minmax(0, 1fr));
		gap: 10px;
		align-self: center;
	}
	.j15-step {
		position: relative;
		display: grid;
		grid-template-columns: 26px minmax(0, 1fr);
		gap: 8px;
		padding: 10px;
		border: 1px solid var(--line);
		border-radius: var(--r);
		background: var(--sunk);
	}
	.j15-step p {
		margin: 4px 0 0;
		font-size: 12px;
	}
	.j15-step b {
		display: block;
		font-size: 13px;
	}
	.j15-step-st {
		font-size: 11px;
		text-transform: uppercase;
		letter-spacing: 0.06em;
		color: var(--faint);
	}
	.j15-dot {
		width: 24px;
		height: 24px;
		border-radius: 50%;
		display: grid;
		place-items: center;
		font-size: 12px;
		font-weight: 600;
		border: 2px solid var(--line);
		color: var(--muted);
		background: var(--panel);
	}
	.j15-step.done .j15-dot {
		border-color: var(--ok);
		color: var(--ok);
	}
	.j15-step.current {
		border-color: var(--accent);
	}
	.j15-step.current .j15-dot {
		border-color: var(--accent);
		color: var(--accent);
	}
	.j15-step.current .j15-step-st {
		color: var(--accent);
	}
	.j15-step.blocked {
		border-color: var(--err);
	}
	.j15-step.blocked .j15-dot {
		border-color: var(--err);
		color: var(--err);
	}
	.j15-step.blocked .j15-step-st {
		color: var(--err);
	}
	.j15-changes {
		display: grid;
		grid-template-columns: repeat(3, minmax(0, 1fr));
		gap: 14px 24px;
	}
	.j15-vs {
		grid-column: 1 / -1;
		margin: 0;
		display: flex;
		gap: 8px;
		align-items: center;
		flex-wrap: wrap;
	}
	.j15-h {
		font-size: 13px;
		margin: 0 0 8px;
	}
	.j15-h4 {
		font-size: 12.5px;
		margin: 12px 0 6px;
	}
	.j15-impact,
	.j15-mine,
	.j15-gaps,
	.j15-conf,
	.j15-audit {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 6px;
	}
	.j15-impact li {
		display: flex;
		gap: 8px;
		align-items: baseline;
		padding: 6px 8px;
		border-radius: var(--r);
		background: var(--sunk);
	}
	.j15-sign {
		font: 600 14px/1 var(--mono);
		width: 12px;
		flex: none;
	}
	.j15-impact .j15-add .j15-sign {
		color: var(--ok);
	}
	.j15-impact .j15-remove .j15-sign {
		color: var(--err);
	}
	.j15-mine li,
	.j15-conf li,
	.j15-gaps li {
		padding: 8px 10px;
		border: 1px solid var(--line-2);
		border-radius: var(--r);
	}
	.j15-mine-head {
		display: flex;
		justify-content: space-between;
		gap: 8px;
		flex-wrap: wrap;
		align-items: baseline;
	}
	.j15-wasnow {
		margin: 3px 0;
		overflow-wrap: anywhere;
	}
	.j15-gaps {
		margin-bottom: 8px;
	}
	.j15-gaps li {
		border-left: 3px solid var(--err);
	}
	.j15-needs {
		margin-top: 10px;
	}
	.j15-out {
		display: grid;
		grid-template-columns: repeat(4, minmax(0, 1fr));
		gap: 10px;
	}
	.j15-out > div {
		display: grid;
		gap: 2px;
	}
	.j15-out b {
		font-size: 20px;
	}
	.j15-rollback {
		margin-top: 14px;
		padding-top: 10px;
		border-top: 1px solid var(--line-2);
	}
	.j15-rollback .small {
		margin: 0 0 8px;
	}
	.j15-audit li {
		display: flex;
		flex-wrap: wrap;
		gap: 4px 8px;
		align-items: baseline;
		padding: 4px 0;
		border-bottom: 1px solid var(--line-2);
	}
	.j15-audit li > div {
		flex-basis: 100%;
	}
	.j15-record {
		font-size: 15px;
		margin: 22px 0 2px;
	}
	.j15-record-sub {
		margin: 0 0 10px;
	}
	.j15-sections {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(260px, 1fr));
		gap: 6px 20px;
		align-items: start;
	}
	.j15-sec summary {
		cursor: pointer;
		display: flex;
		gap: 8px;
		align-items: baseline;
		padding: 4px 0;
	}
	.j15-new {
		font-size: 11.5px;
		color: var(--ok);
		font-weight: 600;
	}
	.j15-gone {
		font-size: 11.5px;
		color: var(--err);
		font-weight: 600;
	}
	.j15-added {
		color: var(--ok);
	}
	.j15-added .j15-sign {
		display: inline-block;
	}
	.j15-items {
		list-style: none;
		margin: 0 0 8px;
		padding: 0;
	}
	.j15-items li {
		padding: 5px 0;
		border-bottom: 1px solid var(--line-2);
	}
	.j15-removed {
		color: var(--err);
	}
	.j15-pins {
		display: flex;
		flex-wrap: wrap;
		gap: 4px;
		margin: 0 0 4px;
	}
	.add {
		color: var(--ok);
	}
	.rm {
		color: var(--err);
	}
	details summary {
		cursor: pointer;
		margin-top: 10px;
	}
	@media (max-width: 1100px) {
		.j15-hero {
			grid-template-columns: minmax(0, 1fr);
		}
		.j15-changes {
			grid-template-columns: minmax(0, 1fr);
		}
	}
	@media (max-width: 640px) {
		.j15-life {
			grid-template-columns: minmax(0, 1fr);
		}
		.j15-out {
			grid-template-columns: repeat(2, minmax(0, 1fr));
		}
	}
</style>
