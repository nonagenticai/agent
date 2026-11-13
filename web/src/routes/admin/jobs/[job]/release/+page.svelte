<script lang="ts">
	// J14 Release = "Review & publish" (D54) + J16 Conflicts as the `#conflicts` tab. Nothing a tenant changes is live
	// on save: changes collect in the draft, are tried in the test chat, a cut (approveRelease) freezes them with any
	// template upgrade into an immutable release, and apply makes it current in Production. Approve and apply are two
	// steps with two audit events (05 §4.2–4.3, K41); apply refuses while readiness gaps remain (K25 → J3; Production
	// routes only, 12 §3.5). Rollback (05 §4.4) puts an earlier release's WHOLE design live again — selection, pins and
	// tenant design values — after an impact preview + confirm (`?rollback=<id>` opens it); published changes it undoes
	// stay listed as "rolled back" with a one-click "restore to draft". The current draft is never touched.
	// From cb [job]/release/+page.svelte@45ee128 and sc releases/+page.svelte@71aa7af minus push/drift (D3, E8).
	import { onMount } from 'svelte';
	import { replaceState } from '$app/navigation';
	import { api, app, guard, guardOutcome } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { canWrite, writeHint, writeAttrs } from '$lib/auth/roles';
	import { ApiError } from '$lib/api/errors';
	import { ReleaseError } from '$lib/api/areas/release';
	import { Resource } from '$lib/load.svelte';
	import { jobPath } from '$lib/scope';
	import { hrefOf, pageById, routesOf } from '$lib/pages';
	import { selectionValues, type Paged } from '$lib/api/types/core';
	import type { DraftValidation, ValidationCut } from '$lib/api/types/journey';
	import type { TestCase } from '$lib/api/types/live';
	import type {
		ApproveResult,
		BundleViolation,
		DraftChange,
		DraftSummary,
		JobTarget,
		PublishedChange,
		ReadinessBoard,
		ReadinessItem,
		ReleaseReadiness,
		ReleaseRow,
		ReleaseStatus,
		RollbackDirection,
	} from '$lib/api/types/release';
	import { releasesOf, testEvidence, testResultOf, type ChangeTest } from '$lib/release/pipeline-J14';
	import { checklistOf } from '$lib/release/checklist';
	import {
		approveReason,
		currentStep,
		goLiveReason,
		releaseTabOf,
		type Check,
		type FlowInput,
		type ReleaseTab,
	} from '$lib/release/flow';
	import ReleaseFlow from '$lib/release/ReleaseFlow.svelte';
	import { pendingKindsLine, uncoveredChip } from '$lib/release/pending-kinds';
	import RollbackPreviewJ14 from '$lib/release/RollbackPreviewJ14.svelte';
	import { rollbackPreviewTitle } from '$lib/release/J15-detail';
	import Callout from '$lib/ui/Callout.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import { L, layerRef, plain, releaseLabel } from '$lib/ui/labels';
	import TechDetail from '$lib/ui/TechDetail.svelte';
	import PageHead from '$lib/ui/PageHead.svelte';
	import Tabs from '$lib/ui/Tabs.svelte';
	import RefinementsPanel from '../refinements/RefinementsPanel.svelte';
	import ConflictsPanel from './ConflictsPanel.svelte';
	import SelectionEditor from './SelectionEditor.svelte';

	const target = new Resource<JobTarget>(
		() => api().releaseTarget(),
		() => false,
	);
	const status = new Resource<ReleaseStatus>(
		() => api().releaseStatus(),
		() => false,
	);
	const releases = new Resource<Paged<ReleaseRow>>(() => api().releaseList());
	// D67: the backend's verdict on what the next step would refuse on. Approve cuts the DRAFT (and the approve route
	// checks the draft), so before a release is waiting the gate is the draft's readiness; Go live applies the waiting
	// release, so then it is that release's (`of=release`). Reading the release before the first one is cut would
	// check the live design, where nothing the draft adds (its routes) exists yet, and Approve could never be enabled.
	const readinessOf = {
		draft: new Resource<ReleaseReadiness>(
			() => api().releaseReadiness(undefined, { of: 'draft' }),
			() => false,
		),
		release: new Resource<ReleaseReadiness>(
			() => api().releaseReadiness(),
			() => false,
		),
	};
	// The go-live checklist (J3), read the same way: the blockers count comes from here, so this page and J3 (linked
	// with the same `of`) can never disagree.
	const boardOf = {
		draft: new Resource<ReadinessBoard>(
			() => api().readinessBoard({ of: 'draft' }),
			() => false,
		),
		release: new Resource<ReadinessBoard>(
			() => api().readinessBoard(),
			() => false,
		),
	};
	const drafts = new Resource<DraftChange[]>(
		() => api().draftDiff(),
		() => false,
	);
	// D80: the kinds still on sample data (`design::PENDING`) — not part of any release yet.
	const summary = new Resource<DraftSummary>(
		() => api().draftSummary(),
		() => false,
	);
	const pendingLine = $derived(
		summary.value.state === 'ok' ? pendingKindsLine(summary.value.data.pending_kinds) : null,
	);
	const validation = new Resource<ValidationCut[]>(
		async () => (await api().validationCuts()).items,
		() => false,
	);
	// D54 "tested before publish": the J17 scenario runs on the draft and the L2 saved test cases.
	const draftTest = new Resource<DraftValidation>(
		() => api().draftValidation(),
		() => false,
	);
	const testCases = new Resource<TestCase[]>(
		async () => (await api().testCases()).items,
		() => false,
	);
	// Published changes a rollback took out of Production (restorable to the draft).
	const published = new Resource<PublishedChange[]>(
		() => api().publishedChanges(),
		() => false,
	);
	// Conflicts still to decide (a choice saved to draft is decided, not open — J16).
	const openCount = new Resource<number>(
		async () => (await api().conflictBoard('open')).open,
		() => false,
	);
	function refreshAll() {
		void target.refresh();
		void status.refresh();
		void readinessOf.draft.refresh();
		void readinessOf.release.refresh();
		void boardOf.draft.refresh();
		void boardOf.release.refresh();
		void releases.refresh();
		void drafts.refresh();
		void summary.refresh();
		void validation.refresh();
		void draftTest.refresh();
		void testCases.refresh();
		void openCount.refresh();
		void published.refresh();
	}
	/** D15: template upgrades the next cut adopts (opt-in; a cut otherwise keeps the job's pins). */
	let adopt = $state<string[]>([]);
	function toggleAdopt(layer: string, on: boolean) {
		adopt = on ? [...new Set([...adopt, layer])] : adopt.filter((l) => l !== layer);
	}

	// Tabs: changes (default) · history · template-updates. `?tab=` picks one; `#conflicts` (the old J16 address)
	// opens the changes tab with the conflicts expanded.
	let tab = $state<ReleaseTab>('changes');
	let conflictsOpen = $state(false);
	function syncUrl() {
		if (typeof location === 'undefined') return;
		const t = releaseTabOf(location.search, location.hash);
		tab = t.tab;
		if (t.conflictsOpen) conflictsOpen = true;
	}
	onMount(() => {
		syncUrl();
		refreshAll();
		const rb = typeof location !== 'undefined' ? new URLSearchParams(location.search).get('rollback') : null;
		if (rb) openRollback(rb);
	});
	/** The release whose rollback preview is open (Drawer): impact first, then confirm. */
	let rollbackId = $state<string | null>(null);
	let rollbackOpen = $state(false);
	/** The open preview's direction: the drawer title says roll back, re-apply or apply (not always "roll back"). */
	let rollbackDirection = $state<RollbackDirection | null>(null);
	function openRollback(id: string) {
		rollbackId = id;
		rollbackDirection = null;
		rollbackOpen = true;
	}
	$effect(() => {
		const t = tab;
		if (typeof location === 'undefined') return;
		const u = new URL(location.href);
		if ((u.searchParams.get('tab') ?? 'changes') === t) return;
		if (t === 'changes') u.searchParams.delete('tab');
		else u.searchParams.set('tab', t);
		u.hash = '';
		try {
			replaceState(u, {});
		} catch {
			/* no router (unit tests): the tab still switches */
		}
	});

	const writer = $derived(canWrite(session, 'release_manager'));
	const wa = $derived(writeAttrs(session, 'release_manager'));
	/** Discarding and restoring draft changes are job design (§R.1 Job designer). */
	const wd = $derived(writeAttrs(session, 'job_designer'));
	let busy = $state(false);
	type Refusal = {
		code: string;
		message: string;
		blocking: ReadinessItem[];
		violations: BundleViolation[];
		release?: string;
	};
	let refusal = $state<Refusal | null>(null);
	let editorOpen = $state(false);
	let editorDirty = $state(false);

	/** `?stale=selection` replays the race where someone saved the selection after this page loaded. */
	function seenAt(t: JobTarget): string | null {
		const stale =
			typeof location !== 'undefined' && new URLSearchParams(location.search).get('stale') === 'selection';
		return stale ? '2026-10-18T07:30:00Z' : t.selection_updated_at;
	}
	function toRefusal(e: unknown, release?: string): Refusal | null {
		if (!(e instanceof ApiError) || e.status === 403) return null;
		return {
			code: e.code,
			message: e.message,
			blocking: e instanceof ReleaseError ? e.blocking : [],
			violations: e instanceof ReleaseError ? e.violations : [],
			release,
		};
	}

	/** Approve = cut the draft into an immutable release. Going live is the separate, second step (K41). */
	async function approve() {
		if (busy || target.value.state !== 'ok') return;
		const t = target.value.data;
		busy = true;
		refusal = null;
		let res: ApproveResult | null = null;
		const adopting = [...adopt];
		const out = await guardOutcome(
			async () =>
				(res = await api().approveRelease({
					apply: false,
					selection_seen_at: seenAt(t),
					...(adopting.length ? { adopt_pins: adopting } : {}),
				})),
			() => {
				const r = res!;
				if (!r.created) return `No changes since ${releaseLabel(r.release.id)}.`;
				const up = adopting.length
					? ` It adopts ${adopting.join(', ')}; check Conflicts for refinements the upgrade moved.`
					: '';
				return `Approved ${releaseLabel(r.release.id)}. Go live when you are ready.${up}`;
			},
			'Not approved',
		);
		busy = false;
		if (out.kind !== 'ok') refusal = toRefusal(out.error);
		else adopt = [];
		refreshAll();
	}

	async function applyRow(r: Pick<ReleaseRow, 'id'>, rollback: boolean) {
		if (busy) return;
		busy = true;
		refusal = null;
		let applied = true;
		let published = 0;
		const out = await guardOutcome(
			async () => {
				const res = await api().applyRelease(r.id);
				applied = res.applied;
				published = res.published ?? 0;
			},
			() =>
				!applied
					? `${releaseLabel(r.id)} is already current.`
					: rollback
						? `Rolled back to ${releaseLabel(r.id)}; it is current now. Conflicts were recomputed.`
						: `Applied ${releaseLabel(r.id)}; it is current now.${published ? ` ${published} draft change${published === 1 ? '' : 's'} went live.` : ''}`,
			rollback ? 'Not rolled back' : 'Not applied',
		);
		busy = false;
		if (out.kind !== 'ok') refusal = toRefusal(out.error, r.id);
		refreshAll();
	}

	async function restore(ids: string[] | 'all') {
		if (busy) return;
		busy = true;
		await guard(
			() => api().restoreRolledBack(ids),
			'Restored to the draft. Test it, then publish it with the next release.',
		);
		busy = false;
		refreshAll();
	}

	async function discard(d: DraftChange) {
		if (busy) return;
		busy = true;
		await guard(() => api().discardDraft(d.id), 'Discarded from the draft. Production keeps the live value.');
		busy = false;
		refreshAll();
	}

	const when = (iso: string | null) =>
		iso
			? new Date(iso).toLocaleString('en-GB', { dateStyle: 'medium', timeStyle: 'short', timeZone: 'UTC' })
			: '—';
	const STATE: Record<ReleaseRow['state'], [string, 'ok' | 'accent' | 'muted']> = {
		current: ['Live', 'ok'],
		approved: ['Approved, not applied', 'accent'],
		applied_earlier: ['Applied earlier', 'muted'],
	};
	const label = (list: { key: string; label: string }[], k: string | null) =>
		k == null ? '—' : (list.find((x) => x.key === k)?.label ?? k);
	const conflictCount = $derived(openCount.value.state === 'ok' ? openCount.value.data : undefined);
	const pageHref = (id: string) => {
		const p = pageById(id);
		return p ? { href: hrefOf(p, routesOf(p)[0]!, app.job), title: p.title } : null;
	};

	/** The pipeline needs all of status, releases and drafts; validation is optional (null = unreadable). */
	const loaded = <T,>(r: Resource<T>): T | null =>
		r.value.state === 'ok' || r.value.state === 'empty' ? r.value.data : null;
	const input = $derived.by(() => {
		const st = loaded(status);
		const rl = loaded(releases);
		const dr = loaded(drafts);
		if (!st || !rl || !dr) return null;
		return {
			status: st,
			releases: rl.items,
			drafts: dr,
			validation: validation.value.state === 'ok' ? validation.value.data : null,
			draftTest: draftTest.value.state === 'ok' ? draftTest.value.data : null,
			testCases: testCases.value.state === 'ok' ? testCases.value.data : null,
			adopt,
		};
	});
	const evidence = $derived(
		input && (input.draftTest || input.testCases)
			? testEvidence(input.drafts, input.draftTest, input.testCases)
			: null,
	);
	const TEST_CHIP: Record<ChangeTest, [string, 'ok' | 'warn' | 'err']> = {
		tested: ['Tested', 'ok'],
		stale: ['Stale', 'warn'],
		not_tested: ['Not tested', 'err'],
		checklist: ['Go-live checklist', 'ok'],
	};
	const rel = $derived(input ? releasesOf(input) : { current: null, waiting: null });
	const gateOf = $derived<'draft' | 'release'>(rel.waiting ? 'release' : 'draft');
	const readiness = $derived(readinessOf[gateOf]);
	const board = $derived(boardOf[gateOf]);
	const checklistHref = $derived(jobPath(app.job, gateOf === 'draft' ? '/readiness?of=draft' : '/readiness'));
	const tests = $derived(
		validation.value.state === 'ok'
			? validation.value.data.filter((c) => c.state !== 'applied_earlier').map((c) => testResultOf(c)!)
			: [],
	);
	const blockers = $derived(
		board.value.state === 'ok'
			? checklistOf(board.value.data).mustFix.length
			: readiness.value.state === 'ok'
				? readiness.value.data.blocking.length
				: 0,
	);
	const upgrades = $derived(status.value.state === 'ok' ? status.value.data.newer_pins : []);
	const draftList = $derived(loaded(drafts) ?? []);
	const changeCount = $derived(draftList.length + adopt.length);
	const testsNeeded = $derived(
		changeCount > 0 && !!evidence && evidence.state !== 'nothing' && evidence.state !== 'tested',
	);
	const flow = $derived<FlowInput>({
		changes: changeCount,
		blockers,
		conflicts: conflictCount ?? 0,
		testsNeeded,
		waiting: !!rel.waiting,
		canApprove: writer,
	});
	const step = $derived(currentStep(flow));
	const checks = $derived.by<Check[]>(() => {
		const out: Check[] = [
			{
				key: 'checklist',
				label: 'Go-live checklist',
				state: blockers ? 'err' : 'ok',
				detail: blockers ? `${blockers} to fix before going live` : 'Nothing blocks going live',
				href: checklistHref,
				linkLabel: 'Open checklist',
			},
			{
				key: 'conflicts',
				label: 'Conflicts',
				state: conflictCount ? 'err' : 'ok',
				detail: conflictCount
					? `${conflictCount} to resolve, listed under your changes`
					: 'None with the template',
			},
		];
		const ev = evidence;
		out.push({
			key: 'tests',
			label: L.validation_grid,
			state: !ev || ev.state === 'nothing' ? 'muted' : ev.state === 'tested' ? 'ok' : 'err',
			detail: !ev
				? 'Results unavailable right now'
				: ev.state === 'nothing'
					? 'Nothing to test'
					: ev.state === 'tested'
						? ev.counts.tested
							? 'Every change has been tried'
							: 'Nothing the test chat can try: the go-live checklist proves these changes'
						: `${ev.counts.not_tested + ev.counts.stale} changes not tried on this draft`,
			href: jobPath(app.job, '/validation'),
			linkLabel: 'Run the test suite',
		});
		out.push({
			key: 'approval',
			label: 'Approval',
			state: rel.waiting ? 'ok' : 'muted',
			detail: rel.waiting
				? `${releaseLabel(rel.waiting.id)} approved by ${
						rel.waiting.approved_by
							? (rel.waiting.names[rel.waiting.approved_by] ?? rel.waiting.approved_by)
							: 'a release manager'
					}`
				: 'Needs approval by a release manager',
		});
		return out;
	});
	// Until the draft, status and releases have loaded the page cannot say what is blocking.
	const approveWhy = $derived(!input ? 'Checking…' : (approveReason(flow) ?? (busy ? 'Working…' : null)));
	const goLiveWhy = $derived(
		!input
			? 'Checking…'
			: (goLiveReason(flow) ?? (!writer ? writeHint('release_manager') : busy ? 'Working…' : null)),
	);
	const AREA: Record<string, string> = {
		routes: 'Routes',
		ontology: 'Ontology',
		rules: 'Business rules',
		gates: 'Gates',
		toolsets: 'Toolsets',
		knowledge: 'Knowledge',
		settings: 'Settings',
		journey: 'Journey',
		procedures: 'Procedures',
		refinements: 'Refinements',
	};
</script>

<PageHead
	id="J14"
	sub="Review your changes, then go live."
	help="Nothing you change is live on save: it collects in the draft, you test it, approving freezes it into a release, and going live puts that release in front of customers."
/>

{#if !writer}
	<Callout kind="info" title="Read only"
		>You can review the draft, releases and conflicts. Approving, going live and editing the selection:
		{writeHint('release_manager').toLowerCase()}. Discarding and restoring draft changes:
		{writeHint('job_designer').toLowerCase()}.</Callout
	>
{/if}

{#if status.value.state === 'ok'}
	{@const s = status.value.data}
	{#if s.selection_error}
		<Callout kind="err" title="Selection error"
			>{s.selection_error} Edit the selection before approving.
			<button
				class="btn sm"
				type="button"
				{...wa}
				onclick={() => ((editorDirty = false), (editorOpen = true))}>Fix the selection</button
			></Callout
		>
	{/if}
	<p class="j14-editsel">
		<button
			class="btn sm"
			type="button"
			data-testid="edit-selection-main"
			disabled={wa.disabled || target.value.state !== 'ok'}
			title={wa.title}
			onclick={() => ((editorDirty = false), (editorOpen = true))}>Edit selection</button
		>
		<span class="muted small">Profile, industry, departments, areas, channels and region of this job.</span>
	</p>
	<div class="j14-status" data-testid="release-status">
		{#if s.base_changed}
			<Chip
				text="Base changed since your release"
				variant="warn"
				title={s.newer_pins.length
					? s.newer_pins
							.map((p) => `${layerRef(`${p.layer}@v${p.latest}`)} is tagged; you use v${p.pinned}`)
							.join('; ')
					: 'The selection or the platform layers compile to a different bundle'}
			/>
		{/if}
		{#if s.draft_hash}<span class="muted small"
				>Draft compiles to <TechDetail><span class="mono">{s.draft_hash}</span></TechDetail></span
			>{/if}
	</div>
{/if}

{#if refusal}
	<div class="callout err refusal" role="alert" data-testid="release-refusal" data-code={refusal.code}>
		<strong>Refused<TechDetail>: {refusal.code}</TechDetail></strong>
		<div>
			<p>{plain(refusal.message)}</p>
			{#if refusal.blocking.length}
				<ul data-testid="missing-list">
					{#each refusal.blocking as b (b.subject)}
						<li>
							<TechDetail><span class="mono">{b.subject}</span> —</TechDetail>{' '}{plain(b.message)}.
							<a href={b.href}>Fix</a>
						</li>
					{/each}
				</ul>
			{/if}
			{#if refusal.code === 'release_not_ready'}
				<p class="muted small">
					Fix the missing items (see the <a href={checklistHref}>go-live checklist</a>), or edit the selection
					— for example drop the Telegram channel — and approve again.
				</p>
			{:else if refusal.code === 'nothing_to_publish'}
				<p class="muted small">
					Save a change on a design page first; it lands in the draft and shows up here.
				</p>
			{/if}
			{#if refusal.violations.length}
				<ul data-testid="violation-list">
					{#each refusal.violations as v (v.path + v.message)}<li>
							<code>{v.path}</code>
							{v.message}
						</li>{/each}
				</ul>
			{/if}
			{#if refusal.code === 'selection_changed'}
				<button
					class="btn sm"
					type="button"
					onclick={() => {
						refusal = null;
						refreshAll();
					}}>Reload the selection</button
				>
			{/if}
			<button class="btn sm" type="button" onclick={() => (refusal = null)}>Dismiss</button>
		</div>
	</div>
{/if}

<Tabs
	items={[
		['changes', 'Changes', changeCount],
		['history', 'History'],
		['template-updates', 'Template updates', upgrades.length],
	]}
	bind:active={tab}
	label="Release views"
>
	{#if tab === 'history'}
		{@render historyTab()}
	{:else if tab === 'template-updates'}
		{@render templateTab()}
	{:else}
		<ReleaseFlow
			{step}
			{checks}
			approveReason={approveWhy}
			goLiveReason={goLiveWhy}
			goLiveLabel={rel.waiting ? releaseLabel(rel.waiting.id) : null}
			onapprove={() => approve()}
			ongolive={() => rel.waiting && applyRow(rel.waiting, false)}
		>
			<Card
				title="What's in this release"
				sub="Every change saved to the draft, with what Production runs today. The test chat runs the draft."
			>
				{#if pendingLine}
					<p class="muted small" data-testid="pending-kinds">{pendingLine}</p>
				{/if}
				<LoadState resource={drafts}>
					{#snippet children(list)}
						{#if !list.length && !upgrades.length}
							<div class="empty" data-testid="draft-empty">
								Nothing to release. Production runs {rel.current
									? releaseLabel(rel.current.id)
									: 'no release yet'}.
							</div>
						{:else}
							<ul class="j14-changes" data-testid="draft-diff">
								{#each list as d (d.id)}
									{@const pg = pageHref(d.page)}
									{@const unc = uncoveredChip(d.uncovered)}
									<li data-draft={d.id}>
										<div class="j14-change-head">
											<input
												type="checkbox"
												class="pick"
												checked
												disabled
												aria-label="Included: {plain(d.label)}"
												title="Every saved change goes into the next release. Discard it to leave it out."
											/>
											<Chip text={AREA[d.area] ?? d.area} />
											<span class="j14-change-label">{plain(d.label)}</span>
											{#if d.in_release}<Chip
													text="In {releaseLabel(d.in_release)}"
													variant="accent"
													title="Already in that approved release; it goes live when it is applied"
												/>{/if}
											{#if d.live_moved}<Chip
													text={d.live_moved.matches_live
														? 'Matches live since rollback'
														: 'Live moved by rollback'}
													variant="warn"
													title="Rolling back to {d.live_moved.release} changed the live value here from {d
														.live_moved.was ?? 'not set'}; your draft value is unchanged"
												/>{/if}
											{#if unc}<Chip text={unc.text} variant="warn" title={unc.title} />{/if}
										</div>
										<div class="j14-diff">
											<span class="j14-side live"
												><span class="j14-tag">Live</span>{plain(d.live ?? 'not set')}</span
											>
											<span class="j14-side draft"
												><span class="j14-tag">Draft</span>{plain(d.draft ?? 'removed')}</span
											>
										</div>
										<div class="j14-change-foot muted small">
											{d.by_name} · {when(d.at)}
											{#if pg?.href}· <a href={pg.href}>{pg.title}</a>{/if}
											<button
												class="btn sm"
												type="button"
												disabled={wd.disabled || busy}
												title={wd.title ?? 'Throw this change away; the live value stays'}
												onclick={() => discard(d)}>Discard</button
											>
										</div>
									</li>
								{/each}
								{#each upgrades as u (u.layer)}
									{@const on = adopt.includes(u.layer)}
									<li data-upgrade={u.layer} class:j14-adopt-on={on}>
										<div class="j14-change-head">
											<Chip text="Template update" variant="warn" />
											<span class="j14-change-label">{layerRef(u.layer)}</span>
											{#if u.in_release}XX variant="accent" title="That approved cut already adopts it; it
												goes live when it is applied" />{/if}
										</div>
										<div class="j14-diff">
											<span class="j14-side live"><span class="j14-tag">Live</span>v{u.pinned}</span>
											<span class="j14-side draft"><span class="j14-tag">Available</span>v{u.latest}</span>
										</div>
										{#if !u.in_release}
											<label class="j14-adopt">
												<input
													type="checkbox"
													checked={on}
													disabled={wa.disabled || busy}
													title={wa.title}
													data-testid="adopt-{u.layer}"
													onchange={(e) => toggleAdopt(u.layer, e.currentTarget.checked)}
												/>
												<span
													><b>Adopt v{u.latest} in this release</b> — optional. Conflicts it raises with your
													changes show up below after you approve; Production keeps v{u.pinned} until you go live.</span
												>
											</label>
										{/if}
									</li>
								{/each}
							</ul>
						{/if}
					{/snippet}
				</LoadState>
			</Card>

			<details
				class="conf"
				data-testid="conflicts-section"
				open={conflictsOpen || !!conflictCount}
				ontoggle={(e) => (conflictsOpen = e.currentTarget.open)}
			>
				<summary>Conflicts with template updates ({conflictCount ?? 0})</summary>
				<ConflictsPanel onchange={refreshAll} />
			</details>

			<Card
				title="Test"
				sub="Has each change been tried on the draft? The test environment runs the draft; Production keeps the live release until you go live."
			>
				{#snippet actions()}
					<a class="btn sm" href="/chat">Test in test chat</a>
					<a class="btn sm" href={jobPath(app.job, '/validation')}>Run the test suite</a>
				{/snippet}
				{#if evidence}
					{@const ev = evidence}
					{#if ev.state === 'nothing'}
						<div class="empty" data-testid="test-evidence" data-state="nothing">
							Nothing unpublished to test. The test chat runs what Production runs.
						</div>
					{:else}
						<div class="j14-tsum" data-testid="test-evidence" data-state={ev.state}>
							<span class="j14-tc ok"><b>{ev.counts.tested}</b> tested</span>
							<span class="j14-tc warn" class:zero={!ev.counts.stale}><b>{ev.counts.stale}</b> stale</span>
							<span class="j14-tc err" class:zero={!ev.counts.not_tested}
								><b>{ev.counts.not_tested}</b> not tested</span
							>
						</div>
						<p class="small j14-tsrc">
							<b>{L.validation_grid}:</b>
							{#if ev.run}
								draft rev {ev.run.draft_rev}, {when(ev.run.at)} by {ev.run.by} — {ev.run.pass} of {ev.run
									.total} pass{#if ev.run.fixed}, {ev.run.fixed} fixed by the draft{/if}.
								{#if ev.regressions}<Chip
										text="{ev.regressions} regression{ev.regressions === 1 ? '' : 's'}"
										variant="err"
									/>{/if}
								{#if ev.run_state === 'stale'}<Chip text="Draft changed since" variant="warn" />{/if}
							{:else}
								never run on this draft.
							{/if}
							<br /><b>Test cases (test chat):</b>
							{#if ev.cases.total}
								{ev.cases.current} on this draft, {ev.cases.stale} saved on an older draft.
							{:else}
								none saved yet — save a test conversation from the test chat.
							{/if}
						</p>
						<ul class="j14-tests" data-testid="test-changes">
							{#each ev.changes as x (x.change.id)}
								<li data-test-change={x.change.id} data-state={x.state}>
									<Chip text={TEST_CHIP[x.state][0]} variant={TEST_CHIP[x.state][1]} />
									<span class="j14-tests-label">{plain(x.change.label)}</span>
									<span class="muted small j14-tests-by"
										>{x.scenarios
											? `${x.scenarios} scenario${x.scenarios === 1 ? '' : 's'}`
											: 'no scenario'}{x.regressions
											? ` · ${x.regressions} regression${x.regressions === 1 ? '' : 's'}`
											: ''}{x.fixed ? ` · ${x.fixed} fixed` : ''}{x.cases
											? ` · ${x.cases} test case${x.cases === 1 ? '' : 's'}`
											: ''}{x.stale_cases
											? ` · ${x.stale_cases} stale test case${x.stale_cases === 1 ? '' : 's'}`
											: ''}</span
									>
								</li>
							{/each}
						</ul>
						{#if ev.unexercised.length}
							<div class="j14-untested" data-testid="test-unexercised">
								<strong
									>No test exercised {ev.unexercised.length === 1
										? 'this change'
										: `these ${ev.unexercised.length} changes`}:</strong
								>
								<ul>
									{#each ev.unexercised as d (d.id)}<li>{plain(d.label)}</li>{/each}
								</ul>
								<span class="small"
									>Try {ev.unexercised.length === 1 ? 'it' : 'them'} in the <a href="/chat">test chat</a>
									and save the conversation as a test case, or add a scenario in the
									<a href={jobPath(app.job, '/validation')}>test suite</a>.</span
								>
							</div>
						{/if}
					{/if}
				{:else if draftTest.value.state === 'unavailable' && testCases.value.state === 'unavailable'}
					<p class="muted">Test results are unavailable right now.</p>
				{/if}
				{#if validation.value.state === 'ok' && tests.length}
					<details class="j14-cuts">
						<summary>Test results for each release ({tests.length})</summary>
						<table class="tbl" data-testid="release-tests">
							<thead><tr><th>Release</th><th>Test run</th><th>Result</th></tr></thead>
							<tbody>
								{#each tests as t (t.release)}
									<tr>
										<td>{releaseLabel(t.release)}</td>
										<td>{t.ran_at ? when(t.ran_at) : 'Not run yet'}</td>
										<td>
											{#if t.ran_at}
												<Chip text="{t.pass} pass" variant="ok" />
												{#if t.rejected}<Chip text="{t.rejected} rejected" variant="err" />{/if}
												{#if t.pending}<Chip text="{t.pending} pending" variant="muted" />{/if}
											{:else}<a href={jobPath(app.job, '/validation')}>Run the test suite</a>{/if}
										</td>
									</tr>
								{/each}
							</tbody>
						</table>
					</details>
				{/if}
			</Card>

			{#snippet extra()}
				{#if blockers && readiness.value.state === 'ok' && readiness.value.data.blocking.length}
					<ul class="j14-gaps" data-testid="release-blockers">
						{#each readiness.value.data.blocking as b (b.subject)}
							<li>
								<TechDetail><span class="mono">{b.subject}</span> —</TechDetail>{' '}{plain(b.message)}
								<a href={b.href}>Fix</a>
							</li>
						{/each}
					</ul>
				{/if}
			{/snippet}
			{#snippet below()}
				<Card
					title="Releases"
					sub="A release never changes once approved. Approve and Go live are two steps."
				>
					{@render releasePair()}
				</Card>
			{/snippet}
		</ReleaseFlow>
	{/if}
</Tabs>

{#snippet releasePair()}
	{#if rel.current || rel.waiting}
		<div class="j14-pair">
			{#if rel.waiting}
				{@const w = rel.waiting}
				<div class="j14-rel waiting" class:blocked={blockers > 0} data-testid="release-waiting">
					<span class="j14-eyebrow">Approved, waiting</span>
					<a class="j14-rel-id" href={jobPath(app.job, `/release/${w.id}`)}>{releaseLabel(w.id)}</a>
					<span class="muted small"
						>by {w.approved_by ? (w.names[w.approved_by] ?? w.approved_by) : '—'} · {when(
							w.approved_at,
						)}{#if w.draft_count}&nbsp;· {w.draft_count} draft changes{/if}</span
					>
					{#if blockers > 0}
						<Chip text="Blocked by {blockers} {blockers === 1 ? 'item' : 'items'} to fix" variant="err" />
					{:else}
						<Chip text="Ready to go live" variant="ok" />
					{/if}
				</div>
			{/if}
			{#if rel.current}
				{@const c = rel.current}
				<div class="j14-rel live" data-testid="release-live">
					<span class="j14-eyebrow">Live in Production</span>
					<a class="j14-rel-id" href={jobPath(app.job, `/release/${c.id}`)}>{releaseLabel(c.id)}</a>
					<span class="muted small"
						>since {when(c.last_applied_at ?? c.released_at)}{#if c.snapshot_places != null}
							· snapshot of {c.snapshot_places} design places{/if} · {c.counts.context_spaces ?? 0} spaces ·
						{c.counts.activities ?? 0} activities · {c.counts.gates ?? 0} gates</span
					>
				</div>
			{/if}
		</div>
	{:else}
		<div class="empty">Nothing live yet. Your first release puts the agent in Production.</div>
	{/if}
{/snippet}

{#snippet historyTab()}
	{#if published.value.state === 'ok'}
		{@const gone = published.value.data.filter((c) => c.rolled_back && !c.restored_to_draft)}
		{#if gone.length}
			<div class="j14-history">
				<Card
					title="Rolled back — not in Production"
					sub="Published changes a rollback took out of Production. They stay in the history; restore them to the draft to test and publish them again."
				>
					{#snippet actions()}
						<button
							class="btn sm primary"
							type="button"
							data-testid="restore-all"
							disabled={wd.disabled || busy}
							title={wd.title ?? 'Put them back in the draft (not live until published)'}
							onclick={() => restore('all')}>Restore {gone.length} to draft</button
						>
					{/snippet}
					<ul class="j14-changes" data-testid="rolled-back">
						{#each gone as c (c.id)}
							<li data-rolled={c.id}>
								<div class="j14-change-head">
									<Chip text={AREA[c.area] ?? c.area} />
									<span class="j14-change-label">{plain(c.label)}</span>
									<Chip text="Rolled back" variant="warn" />
								</div>
								<div class="j14-change-foot muted small">
									Published in {c.published_in} · rolled back to {c.rolled_back!.to} by {c.rolled_back!
										.by_name}
									· {when(c.rolled_back!.at)}
									<button
										class="btn sm"
										type="button"
										disabled={wd.disabled || busy}
										title={wd.title ?? 'Put this change back in the draft'}
										onclick={() => restore([c.id])}>Restore to draft</button
									>
								</div>
							</li>
						{/each}
					</ul>
				</Card>
			</div>
		{/if}
	{/if}
	<div class="j14-history">
		<Card
			title="History"
			sub="Every release, newest first. Rolling back re-applies an older release with its whole design (selection, templates, your design values); it keeps its id and you see the impact first."
		>
			<LoadState resource={releases}>
				{#snippet empty()}
					<div class="empty" data-testid="no-release">No release yet. Approve one on the Changes tab.</div>
				{/snippet}
				{#snippet children(page)}
					<div class="tbl-wrap">
						<table class="tbl" data-testid="release-history">
							<thead>
								<tr
									><th>Release</th><th>State</th><th>Approved</th><th>Went live</th><th aria-label="Actions"
									></th></tr
								>
							</thead>
							<tbody>
								{#each page.items as r (r.id)}
									<tr data-release={r.id} data-state={r.state}>
										<td>
											<a class="nowrap" href={jobPath(app.job, `/release/${r.id}`)}>{releaseLabel(r.id)}</a>
											<div class="muted small">
												<TechDetail><span class="mono">{r.hash}</span></TechDetail>
												{#if r.snapshot_places != null}· snapshot {r.snapshot_places} places{/if}
												{#if r.draft_count}· {r.draft_count} draft changes{/if}
												{#each r.upgrades ?? [] as u (u.layer)}· {layerRef(u.layer)} v{u.from} → v{u.to}{/each}
											</div>
										</td>
										<td>
											<Chip text={STATE[r.state][0]} variant={STATE[r.state][1]} />
											{#if r.rolled_back}<div class="muted small" data-testid="rolled-back-{r.id}">
													Rolled back {when(r.rolled_back.at)} by {r.rolled_back.by_name} → {releaseLabel(
														r.rolled_back.to,
													)}
												</div>{/if}
										</td>
										<td>
											{#if r.approved_by}{r.names[r.approved_by] ?? r.approved_by}
												<div class="muted small">{when(r.approved_at)}</div>
											{:else}<span class="muted">Not approved yet</span>{/if}
										</td>
										<td>
											{#if r.applied_by && r.last_applied_at}{r.names[r.applied_by] ?? r.applied_by}
												<div class="muted small">{when(r.last_applied_at)}</div>
											{:else}<span class="muted">Never went live</span>{/if}
										</td>
										<td class="r nowrap">
											{#if r.state === 'approved'}
												<button
													class="btn sm primary"
													type="button"
													disabled={wa.disabled || busy || blockers > 0}
													title={wa.title ??
														(blockers > 0
															? `Fix ${blockers} go-live blocker${blockers === 1 ? '' : 's'}`
															: undefined)}
													onclick={() => applyRow(r, false)}>Go live</button
												>
											{:else if r.state === 'applied_earlier'}
												<button
													class="btn sm"
													type="button"
													disabled={busy}
													title="See what customers lose and get back, then confirm"
													data-testid="rollback-{r.id}"
													onclick={() => openRollback(r.id)}
													>{r.rolled_back ? 'Apply again…' : 'Roll back to this…'}</button
												>
											{:else}
												<span class="muted small">Running now</span>
											{/if}
										</td>
									</tr>
								{/each}
							</tbody>
						</table>
					</div>
				{/snippet}
			</LoadState>
		</Card>
	</div>
{/snippet}

{#snippet templateTab()}
	<div class="j14-history">
		<Card
			title="Template updates"
			sub="Newer versions of the templates your agent is built from. Choose the ones to adopt on the Changes tab."
		>
			{#if upgrades.length}
				<ul class="j14-changes" data-testid="template-updates">
					{#each upgrades as u (u.layer)}
						<li data-upgrade-row={u.layer}>
							<div class="j14-change-head">
								<Chip text="Template update" variant="warn" />
								<span class="j14-change-label">{layerRef(u.layer)}</span>
								<span class="muted small">v{u.pinned} → v{u.latest}</span>
								{#if u.in_release}<Chip text="In {releaseLabel(u.in_release)}" variant="accent" />{/if}
							</div>
						</li>
					{/each}
				</ul>
				<button class="btn sm" type="button" onclick={() => (tab = 'changes')}
					>Choose on the Changes tab</button
				>
			{:else}
				<div class="empty" data-testid="template-updates-empty">
					Your agent runs the latest template versions.
				</div>
			{/if}
		</Card>
	</div>
	<div class="j14-history">
		<Card
			title={L.template_pin}
			sub="The templates this agent is built from. A change here is part of the next release too."
		>
			{#snippet actions()}
				<button
					class="btn sm"
					type="button"
					disabled={wa.disabled || target.value.state !== 'ok'}
					title={wa.title}
					onclick={() => ((editorDirty = false), (editorOpen = true))}>Edit selection</button
				>
			{/snippet}
			<LoadState resource={target}>
				{#snippet children(t)}
					{#if !t.selection}
						<div class="empty">No selection yet. Edit the selection to choose a profile.</div>
					{:else}
						{@const sel = t.selection}
						<dl class="kvs" data-testid="selection">
							<dt>Profile</dt>
							<dd>{label(t.options.profiles, sel.profile)}</dd>
							<dt>Industry</dt>
							<dd>{label(t.options.industries, sel.industry)}</dd>
							<dt>Departments</dt>
							<dd data-testid="selection-departments">
								{sel.departments
									.map((d, i) => {
										const o = t.options.departments.find((x) => x.key === d);
										const name = o?.extends
											? `${o.label} (extends ${o.extends})`
											: label(t.options.departments, d);
										return i === 0 ? `${name} — primary` : name;
									})
									.join(', ') || 'none'}
							</dd>
							<dt>Areas</dt>
							<dd>{sel.areas.map((a) => label(t.options.areas, a)).join(', ') || 'none'}</dd>
							{#each [t.options.dimensions.channel, t.options.dimensions.region] as dim (dim.key)}
								<dt>{dim.label}</dt>
								<dd class:j14-placeholder={dim.kind === 'placeholder'}>
									{selectionValues(sel, dim.key)
										.map((v) => label(dim.values, v))
										.join(', ') || 'none'}
									{#if dim.kind === 'placeholder'}<span class="muted small">
											· recorded, selects nothing</span
										>{/if}
								</dd>
							{/each}
							<dt>Last edited</dt>
							<dd>{t.selection_updated_by_name ?? '—'} · {when(t.selection_updated_at)}</dd>
						</dl>
					{/if}
					{#if t.history.length}
						<details class="j14-audit">
							<summary>Selection audit ({t.history.length})</summary>
							<ul class="list-plain">
								{#each t.history as h (h.at)}
									<li>
										<span class="mono small">{h.action}</span> · {when(h.at)} · {h.actor_name ?? h.actor} —
										{h.change}
									</li>
								{/each}
							</ul>
						</details>
					{/if}
				{/snippet}
			</LoadState>
		</Card>
	</div>
	<div class="j14-history">
		<h2 class="j14-h">{L.refinements}</h2>
		<RefinementsPanel embedded />
	</div>
{/snippet}

<svelte:window onhashchange={syncUrl} />

<Drawer bind:open={rollbackOpen} title={rollbackPreviewTitle(rollbackDirection, rollbackId ?? '')} wide>
	{#if rollbackOpen && rollbackId}
		{#key rollbackId}
			<RollbackPreviewJ14
				id={rollbackId}
				ondone={() => {
					rollbackOpen = false;
					refreshAll();
				}}
				oncancel={() => (rollbackOpen = false)}
				ondirection={(d) => (rollbackDirection = d)}
			/>
		{/key}
	{/if}
</Drawer>

<Drawer bind:open={editorOpen} title="Edit selection" wide dirty={editorDirty}>
	{#if editorOpen && target.value.state === 'ok'}
		<SelectionEditor
			target={target.value.data}
			bind:dirty={editorDirty}
			onsaved={() => {
				editorOpen = false;
				refusal = null;
				refreshAll();
			}}
			oncancel={() => (editorOpen = false)}
		/>
	{/if}
</Drawer>

<style>
	.j14-editsel {
		display: flex;
		gap: 10px;
		align-items: center;
		margin: 0 0 10px;
	}
	.small {
		font-size: 12px;
	}
	.nowrap {
		white-space: nowrap;
	}
	.j14-eyebrow {
		font-size: 11px;
		font-weight: 600;
		letter-spacing: 0.04em;
		text-transform: uppercase;
		color: var(--muted);
	}
	.j14-gaps {
		margin: 0 0 12px;
		padding-left: 18px;
		font-size: 13px;
	}
	.j14-status {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
		align-items: center;
		margin: 0 0 12px;
	}
	.refusal ul {
		margin: 6px 0;
		padding-left: 18px;
	}
	.refusal p {
		margin: 0 0 4px;
	}
	.j14-changes {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 0;
	}
	.j14-changes li {
		padding: 10px 0;
		border-bottom: 1px solid var(--line);
		display: grid;
		gap: 6px;
		min-width: 0;
	}
	.j14-changes li:first-child {
		padding-top: 0;
	}
	.j14-changes li:last-child {
		border-bottom: 0;
		padding-bottom: 0;
	}
	.j14-change-head {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		align-items: center;
	}
	.j14-change-label {
		font-weight: 600;
		overflow-wrap: anywhere;
	}
	.j14-diff {
		display: grid;
		grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
		gap: 6px;
		font-size: 13px;
	}
	@media (max-width: 560px) {
		.j14-diff {
			grid-template-columns: minmax(0, 1fr);
		}
	}
	.j14-side {
		padding: 5px 8px;
		border-radius: 5px;
		background: var(--sunk);
		overflow-wrap: anywhere;
	}
	.j14-side.draft {
		background: color-mix(in srgb, var(--accent) 10%, var(--panel));
	}
	.j14-tag {
		display: block;
		font-size: 10.5px;
		font-weight: 600;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		color: var(--muted);
	}
	.j14-change-foot {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 4px 6px;
	}
	.j14-change-foot .btn {
		margin-left: auto;
	}
	.j14-adopt {
		display: flex;
		gap: 8px;
		align-items: flex-start;
		margin-top: 8px;
		padding: 8px 10px;
		border: 1px dashed var(--line);
		border-radius: 6px;
		font-size: 13px;
		cursor: pointer;
	}
	.j14-adopt input {
		margin-top: 3px;
	}
	.j14-adopt-on .j14-adopt {
		border-style: solid;
		border-color: var(--accent);
		background: color-mix(in srgb, var(--accent) 6%, var(--panel));
	}
	.j14-tsum {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
		margin-bottom: 8px;
	}
	.j14-tc {
		display: inline-flex;
		align-items: baseline;
		gap: 4px;
		padding: 4px 10px;
		border-radius: 999px;
		border: 1px solid var(--line);
		font-size: 13px;
	}
	.j14-tc b {
		font-size: 16px;
	}
	.j14-tc.ok b {
		color: var(--ok);
	}
	.j14-tc.warn b {
		color: var(--warn);
	}
	.j14-tc.err b {
		color: var(--err);
	}
	.j14-tc.zero {
		opacity: 0.6;
	}
	.j14-tsrc {
		margin: 0 0 8px;
		line-height: 1.6;
	}
	.j14-tests {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 6px;
	}
	.j14-tests li {
		display: flex;
		flex-wrap: wrap;
		align-items: baseline;
		gap: 6px;
		padding: 6px 0;
		border-top: 1px solid var(--line);
	}
	.j14-tests-label {
		flex: 1 1 200px;
		min-width: 0;
		overflow-wrap: anywhere;
	}
	.j14-untested {
		margin-top: 10px;
		padding: 8px 10px;
		border-left: 3px solid var(--err);
		background: color-mix(in srgb, var(--err) 5%, var(--panel));
		border-radius: 4px;
	}
	.j14-untested ul {
		margin: 4px 0;
		padding-left: 18px;
	}
	.j14-cuts {
		margin-top: 10px;
	}
	.j14-cuts summary {
		cursor: pointer;
		font-size: 13px;
	}
	.j14-pair {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(200px, 1fr));
		gap: 10px;
		margin-bottom: 14px;
	}
	.j14-rel {
		display: grid;
		gap: 4px;
		align-content: start;
		justify-items: start;
		padding: 10px 12px;
		border: 1px solid var(--line);
		border-left: 3px solid var(--ok);
		border-radius: 6px;
		min-width: 0;
	}
	.j14-rel.waiting {
		border-left-color: var(--accent);
	}
	.j14-rel.waiting.blocked {
		border-left-color: var(--err);
	}
	.j14-rel-id {
		font-size: 15px;
		font-weight: 600;
	}
	.j14-pair:last-child {
		margin-bottom: 0;
	}
	.conf {
		border: 1px solid var(--line);
		border-radius: 8px;
		background: var(--panel);
		padding: 10px 14px;
	}
	.conf summary {
		cursor: pointer;
		font-weight: 600;
	}
	.pick {
		margin: 0;
	}
	.j14-history {
		margin-top: 14px;
	}
	.j14-h {
		font-size: 15px;
		margin: 0 0 6px;
	}
	.j14-audit {
		margin-top: 10px;
	}
	.j14-audit summary {
		cursor: pointer;
		font-weight: 600;
	}
</style>
