<script lang="ts">
	// J3 Readiness: can this job's next release be applied? A release meets two checks on its way to customers
	// (05 §4.3 step 3, 10 §5.2 step 9, 12 §3.5): the APPLY check (K25 W5 slots + routing_ready + the selection
	// hierarchy + every department's required knowledge, D110/D115 → 409 release_not_ready) and the GO-LIVE check
	// (rule values, contract tests, knowledge warnings, gate review, procedures — apply passes, but the agent would run
	// with holes; enforced at apply from M4). Only PRODUCTION
	// routes count for apply (12 §3.5); a Test-env route gap is a separate warning — it blocks testing the draft,
	// never publishing. The page leads with
	// that verdict, lists each gap (why it blocks, what introduced it, who fixes it, link to fix), then shows every
	// kind of thing the job needs (template asks vs you supply) and the 10-step path to go-live.
	import { untrack } from 'svelte';
	import { page } from '$app/state';
	import { api, app } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { canWrite, writeHint } from '$lib/auth/roles';
	import { goto } from '$app/navigation';
	import { Resource } from '$lib/load.svelte';
	import { jobPath } from '$lib/scope';
	import { day } from '$lib/tenant/ui';
	import type { ReadinessArea, ReadinessBoard } from '$lib/api/types/release';
	import { boardOf } from '$lib/release/readiness-J3';
	import {
		alsoFixes,
		checklistOf,
		ownerLabel,
		statusSentence,
		passingSummary,
		byOwner,
	} from '$lib/release/checklist';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Meter from '$lib/ui/Meter.svelte';
	import MilestoneChip from '$lib/ui/MilestoneChip.svelte';
	import { L, plain, releaseLabel } from '$lib/ui/labels';
	import TechDetail from '$lib/ui/TechDetail.svelte';
	import PageHead from '$lib/ui/PageHead.svelte';
	import StepFlow from '$lib/ui/StepFlow.svelte';
	import Freshness from '$lib/ui/Freshness.svelte';
	import ChecklistRow from './ChecklistRow.svelte';
	import ActionButton from '$lib/ui/ActionButton.svelte';
	import DraftBar from '$lib/release/DraftBar.svelte';

	/** `?of=draft` checks the draft (D67 `readinessBoard({of})`); anything else, the release. */
	const of = $derived(boardOf(page.url.searchParams));
	const board = new Resource<ReadinessBoard>(
		() => api().readinessBoard(of ? { of } : {}),
		() => false,
	);
	// on mount and whenever `?of=` changes (same route, so no remount)
	$effect(() => {
		void of;
		untrack(() => board.refresh());
	});
	const writer = $derived(canWrite(session, 'job_designer'));
	const releaser = $derived(canWrite(session, 'release_manager'));

	type Tone = 'ok' | 'warn' | 'err' | 'muted';
	function areaChip(b: ReadinessBoard, a: ReadinessArea): { text: string; variant: Tone } {
		const mine = b.gaps.filter((g) => g.area === a.key);
		if (mine.some((g) => g.blocks === 'apply')) return { text: 'Blocks go-live', variant: 'err' };
		if (mine.some((g) => g.blocks === 'go_live') || a.status === 'gap')
			return { text: 'Before go-live', variant: 'warn' };
		if (mine.length) return { text: 'Test env gap', variant: 'warn' };
		if (a.status === 'warn') return { text: 'Before go-live', variant: 'warn' };
		if (a.status === 'todo') return { text: 'Not started', variant: 'muted' };
		return { text: 'Ready', variant: 'ok' };
	}
	const METER: Record<Tone, string> = { ok: 's-ok', warn: 's-warn', err: 's-err', muted: 's-muted' };
</script>

<PageHead
	id="J3"
	sub="Can this agent's next release go live?"
	help="Every check is something the release needs from your environment. Each item to fix says why it blocks, who fixes it, and where."
/>
<Freshness resource={board} />

<DraftBar page="J3" tick={board.value} />

<LoadState resource={board}>
	{#snippet children(b)}
		{@const c = checklistOf(b)}
		{@const also = c.firstFix ? alsoFixes(b, c.firstFix) : 0}
		{@const tone = c.mustFix.length ? 'err' : c.noticeable.length ? 'warn' : 'ok'}
		{@const noRelease = b.verdict === 'no_release'}
		{@const prepareReason = !releaser
			? writeHint('release_manager')
			: c.mustFix.length
				? `Fix the ${c.mustFix.length} ${c.mustFix.length === 1 ? 'item' : 'items'} above first.`
				: null}

		<section class="j3-hero {tone}" data-testid="readiness-verdict" aria-labelledby="j3-verdict">
			<div class="j3-hero-text">
				<div class="j3-rel">
					{#if b.release}
						<span class="muted">Checking</span>
						<a href={jobPath(app.job, `/release/${b.release.id}`)}>{releaseLabel(b.release.id)}</a>
						<Chip text={b.release.state} variant={b.release.state === 'current' ? 'ok' : 'accent'} />
						{#if b.release.approved_by_name && b.release.approved_at}
							<span class="muted"
								>approved by {b.release.approved_by_name} on {day(b.release.approved_at)}</span
							>
						{/if}
					{:else}
						<span class="muted">No release prepared yet · checking your current setup</span>
					{/if}
				</div>
				<h2 id="j3-verdict">{statusSentence(c)}</h2>
				{#if c.firstFix}
					<p>
						Start with “{plain(c.firstFix.title)}”.{#if also > 0}
							It also fixes {also} other {also === 1 ? 'item' : 'items'}.{/if}
					</p>
				{:else if noRelease}
					<p>Nothing has been prepared for this agent yet.</p>
				{/if}
			</div>
			<div class="j3-actions">
				{#if c.firstFix}
					{@const href = c.firstFix.href}
					<ActionButton primary onclick={() => goto(href)}>Fix first item</ActionButton>
				{/if}
				<ActionButton
					primary={!c.mustFix.length}
					disabledReason={prepareReason}
					onclick={() => goto(jobPath(app.job, '/release'))}>{L.cut}</ActionButton
				>
				<button type="button" class="btn" onclick={() => board.refresh()}>Re-check</button>
			</div>
		</section>
		<div
			class="j3-progress {tone}"
			role="progressbar"
			aria-label="{c.pass} of {c.total} checks pass"
			aria-valuemin={0}
			aria-valuemax={c.total}
			aria-valuenow={c.pass}
		>
			<i style="width:{c.total ? (c.pass / c.total) * 100 : 0}%"></i>
		</div>

		{#if c.mustFix.length}
			<section class="j3-sec" data-testid="readiness-missing" aria-labelledby="j3-apply">
				<header>
					<h3 id="j3-apply">
						Must fix before going live <span class="j3-count err">{c.mustFix.length}</span>
					</h3>
					<p class="muted">
						These stop a release going live<TechDetail>
							(<span class="mono">409 release_not_ready</span>)</TechDetail
						>. Grouped by who can fix them. <MilestoneChip
							milestone="M1"
							note="M1: apply checks required settings only"
						/>
						<MilestoneChip milestone="M4" note="M4: routes and required knowledge join" />
					</p>
				</header>
				<Card>
					{#each byOwner(c.mustFix) as grp (grp.owner)}
						<h5 class="j3-owner">{ownerLabel(grp.owner)}</h5>
						<ul class="j3-list">
							{#each grp.items as it (it.key)}
								<ChecklistRow item={it} tone="err" canFix={writer} primary={it.key === c.firstFix?.key} />
							{/each}
						</ul>
					{/each}
				</Card>
			</section>
		{/if}

		{#if c.noticeable.length}
			<section class="j3-sec" data-testid="readiness-golive" aria-labelledby="j3-live">
				<header>
					<h3 id="j3-live">
						Customers would notice <span class="j3-count warn">{c.noticeable.length}</span>
					</h3>
					<p class="muted">
						These will not block going live, but the agent hands over or escalates where the hole is. <MilestoneChip
							milestone="M4"
							note="M4: apply enforces these"
						/>
					</p>
				</header>
				<Card>
					<ul class="j3-list">
						{#each c.noticeable as it (it.key)}
							<ChecklistRow item={it} tone="warn" canFix={writer} />
						{/each}
					</ul>
				</Card>
			</section>
		{/if}

		<details class="j3-passing" data-testid="readiness-passing">
			<summary>{passingSummary(c)}</summary>
			{#if c.passing.length}
				<ul>
					{#each c.passing as a (a.key)}
						<li><a href={a.href}>{a.label}</a> <span class="muted">{plain(a.summary)}</span></li>
					{/each}
				</ul>
			{/if}
		</details>

		<Card
			title="Everything this job needs"
			sub="The template layers decide what is needed (inherited, read-only here); you supply the rest on the linked page. A change there is saved to draft, tried in the test chat, and reaches customers when you publish the next release."
		>
			<ul class="j3-areas" data-testid="readiness-areas">
				{#each b.areas as a (a.key)}
					{@const c = areaChip(b, a)}
					<li data-area={a.key}>
						<div class="j3-area-main">
							<div class="j3-area-title">
								<a href={a.href}>{a.label}</a>
								<Chip text={c.text} variant={c.variant} />
								<MilestoneChip milestone={a.enforced_from} note="apply {a.enforced_from}" />
							</div>
							<p class="j3-q">{plain(a.question)}</p>
							<p class="muted small">{plain(a.summary)}</p>
						</div>
						<div class="j3-area-split">
							<span><small>Template asks</small>{plain(a.template_part)}</span>
							<span><small>You supply</small>{plain(a.tenant_part)}</span>
						</div>
						<div class="j3-area-meter">
							{#if a.total > 0}
								<Meter value={a.ready / a.total} cls={METER[c.variant]} />
								<span class="mono">{a.ready}/{a.total}</span>
							{/if}
							<a class="small" href={a.href}>{a.page} →</a>
						</div>
					</li>
				{/each}
			</ul>
		</Card>

		<Card
			title="Path to go-live"
			sub="The order a workspace admin works in: template → operations → systems → instances → routes → mapping → rules → test → go live → operate."
		>
			<StepFlow
				steps={b.steps.map((s) => ({ ...s, detail: s.detail && plain(s.detail) }))}
				label="Go-live steps"
			/>
		</Card>
	{/snippet}
</LoadState>

<style>
	.small {
		font-size: 12px;
	}
	.j3-hero {
		display: grid;
		grid-template-columns: minmax(0, 1.4fr) auto;
		gap: 20px;
		padding: 18px 20px;
		border: 1px solid var(--line);
		border-left: 4px solid var(--muted);
		border-radius: 8px;
		background: var(--panel);
		margin-bottom: 18px;
	}
	.j3-hero.err {
		border-left-color: var(--err);
		background: color-mix(in srgb, var(--err) 4%, var(--panel));
	}
	.j3-hero.warn {
		border-left-color: var(--warn);
		background: color-mix(in srgb, var(--warn) 5%, var(--panel));
	}
	.j3-hero.ok {
		border-left-color: var(--ok);
	}
	.j3-rel {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 6px 8px;
		font-size: 13px;
	}
	.j3-hero h2 {
		margin: 10px 0 4px;
		font-size: 21px;
		line-height: 1.25;
	}
	.j3-hero.err h2 {
		color: var(--err);
	}
	.j3-hero p {
		margin: 0;
		max-width: 62ch;
	}
	.j3-actions {
		display: flex;
		flex-wrap: wrap;
		align-items: flex-start;
		align-content: flex-start;
		gap: 8px;
	}
	.j3-progress {
		height: 8px;
		border-radius: 999px;
		background: var(--line-2);
		overflow: hidden;
		margin-bottom: 18px;
	}
	.j3-progress i {
		display: block;
		height: 100%;
		background: var(--ok);
	}
	.j3-progress.err i {
		background: var(--err);
	}
	.j3-progress.warn i {
		background: var(--warn);
	}
	.j3-owner {
		margin: 12px 0 0;
		font-size: 12px;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		color: var(--muted);
	}
	.j3-owner:first-child {
		margin-top: 0;
	}
	.j3-list {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.j3-passing {
		margin-bottom: 18px;
		border: 1px solid var(--line);
		border-radius: 8px;
		background: var(--panel);
		padding: 10px 14px;
		font-size: 13px;
	}
	.j3-passing summary {
		cursor: pointer;
		font-weight: 600;
	}
	.j3-passing summary:focus-visible {
		outline: 2px solid var(--accent);
		outline-offset: 2px;
	}
	.j3-passing ul {
		margin: 8px 0 0;
		padding-left: 18px;
	}
	.j3-sec {
		margin-bottom: 18px;
	}
	.j3-sec header h3 {
		margin: 0;
		font-size: 15px;
		display: flex;
		align-items: center;
		gap: 8px;
	}
	.j3-sec header p {
		margin: 2px 0 10px;
		font-size: 13px;
	}
	.j3-count {
		font: 600 12px/1 var(--mono);
		padding: 3px 7px;
		border-radius: 999px;
	}
	.j3-count.err {
		color: var(--err);
		background: color-mix(in srgb, var(--err) 14%, transparent);
	}
	.j3-count.warn {
		color: var(--warn);
		background: color-mix(in srgb, var(--warn) 15%, transparent);
	}
	.j3-areas {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.j3-areas li {
		display: grid;
		grid-template-columns: minmax(0, 1.3fr) minmax(0, 1fr) 150px;
		gap: 8px 16px;
		align-items: center;
		padding: 10px 0;
		border-top: 1px solid var(--line-2);
	}
	.j3-areas li:first-child {
		border-top: 0;
	}
	.j3-area-title {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 6px 8px;
		font-weight: 600;
	}
	.j3-q {
		margin: 2px 0 0;
		font-size: 13px;
	}
	.j3-area-main p.muted {
		margin: 2px 0 0;
		overflow-wrap: anywhere;
	}
	.j3-area-split {
		display: grid;
		gap: 4px;
		font-size: 12.5px;
	}
	.j3-area-split small {
		display: block;
		color: var(--muted);
		font-size: 11px;
		text-transform: uppercase;
		letter-spacing: 0.04em;
	}
	.j3-area-meter {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 4px 8px;
		justify-content: flex-end;
	}
	.j3-area-meter a {
		flex-basis: 100%;
		text-align: right;
	}
	@media (max-width: 860px) {
		.j3-hero {
			grid-template-columns: minmax(0, 1fr);
			padding: 14px;
		}
		.j3-areas li {
			grid-template-columns: minmax(0, 1fr);
		}
		.j3-area-meter {
			justify-content: flex-start;
		}
		.j3-area-meter a {
			text-align: left;
		}
	}
</style>
