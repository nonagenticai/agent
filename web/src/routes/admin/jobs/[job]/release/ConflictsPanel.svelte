<script lang="ts">
	// J16 Conflicts — refinement vs base (05 §4.6, D15, D54). A release cut (a template upgrade or a new cut) moved the
	// base under one of the tenant's refinements. Each conflict shows the base before → after next to the tenant's row,
	// what customers get while it stays open, and the choices. A choice is a draft change: it ships with the next
	// publish (D54). Never auto-resolved; never blocks a publish (an apply supersedes and recomputes the open ones).
	import { api, app, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { writeAttrs } from '$lib/auth/roles';
	import { Resource } from '$lib/load.svelte';
	import { jobPath } from '$lib/scope';
	import type { ConflictBoard, ConflictCard } from '$lib/api/types/release';
	import {
		CONFLICT_ACTION,
		OBJECT_LABEL,
		conflictActions,
		conflictText,
		type ConflictOp,
	} from '$lib/release/conflicts';
	import {
		KIND_TITLE,
		MINE_LABEL,
		STAGE_LABEL,
		STAGE_VARIANT,
		causeText,
		choiceEffect,
		choiceText,
		compactRows,
		fieldRows,
		openEffect,
		stageOf,
	} from '$lib/release/conflicts-J16';
	import { keyLabel, plain, releaseLabel } from '$lib/ui/labels';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Seg from '$lib/ui/Seg.svelte';

	let { onchange }: { onchange: () => void } = $props();
	let status = $state<'open' | 'all'>('open');
	const board = new Resource<ConflictBoard>(() => api().conflictBoard(status));
	$effect(() => {
		void status;
		void board.refresh();
	});
	const wa = $derived(writeAttrs(session, 'job_designer'));
	let busy = $state(false);
	const when = (iso: string | null) =>
		iso
			? new Date(iso).toLocaleString('en-GB', { dateStyle: 'medium', timeStyle: 'short', timeZone: 'UTC' })
			: '—';

	async function resolve(c: ConflictCard, op: ConflictOp) {
		if (busy) return;
		busy = true;
		const ok = await guard(
			() => api().resolveConflict(c.id, op),
			`${CONFLICT_ACTION[op].done}: ${c.key}. Saved to draft — goes live with the next publish.`,
			'Not resolved',
		);
		busy = false;
		if (ok) {
			await board.refresh();
			onchange();
		}
	}
	async function undo(c: ConflictCard) {
		const id = c.draft?.id;
		if (busy || !id) return;
		busy = true;
		const ok = await guard(
			() => api().discardDraft(id),
			`Decision on ${c.key} discarded. The conflict is open again.`,
			'Not discarded',
		);
		busy = false;
		if (ok) {
			await board.refresh();
			onchange();
		}
	}
	const CHOICE_TITLE: Record<ConflictOp, string> = { rebase: 'Keep mine', revert: 'Take the template’s' };
</script>

<Card
	title="Conflicts"
	sub="Where a new release moved the template underneath one of your refinements. You decide each one; nothing resolves itself."
>
	{#snippet actions()}
		<Seg
			items={[
				['open', 'To decide'],
				['all', 'All (superseded, resolved)'],
			]}
			bind:active={status}
			label="Conflicts to show"
		/>
	{/snippet}

	<!-- The concept in one picture: base before → base now, with your refinement on top. -->
	<div class="cf-model" aria-label="How a conflict arises">
		<div class="cf-lane">
			<span class="cf-tag mine">Your refinement</span>
			<span class="cf-arrow" aria-hidden="true">made on ↓</span>
			<span class="cf-tag base">Template base · before</span>
		</div>
		<span class="cf-move" aria-hidden="true">release moves the base →</span>
		<div class="cf-lane">
			<span class="cf-tag mine warn">Your refinement <small>(unchanged)</small></span>
			<span class="cf-arrow" aria-hidden="true">no longer fits ↓</span>
			<span class="cf-tag base">Template base · now</span>
		</div>
		<p class="cf-model-copy">
			Until you choose, Production keeps running <strong>your refinement as it is</strong>. Each choice is
			<strong>saved to draft</strong>, can be tried in the test chat, and goes live with the next publish.
		</p>
	</div>

	{#if board.value.state === 'ok'}
		{@const b = board.value.data}
		<div class="cf-sum" data-testid="conflict-summary">
			<div class="cf-sum-item" class:warn={b.open > 0}>
				<strong>{b.open}</strong><span>to decide</span>
			</div>
			<div class="cf-sum-item">
				<strong>{b.decided_in_draft}</strong><span>decided in draft</span>
			</div>
			<div class="cf-sum-item">
				<strong>No</strong><span>publish blocked</span>
			</div>
			<p class="cf-sum-copy muted">
				Open conflicts do not block publishing. Each publish re-checks them against the new base: open ones
				are superseded and raised again if they still apply.
				{#if b.draft_count}
					The draft holds {b.draft_count} unpublished change{b.draft_count === 1 ? '' : 's'}.
				{/if}
				<a href={jobPath(app.job, '/release')}>Review &amp; publish</a> ·
				<a href={jobPath(app.job, '/refinements')}>All your refinements</a>
			</p>
		</div>
	{/if}

	<LoadState
		resource={board}
		empty={status === 'open'
			? 'No conflicts to decide. Your refinements sit cleanly on the current release.'
			: 'No conflicts yet.'}
	>
		{#snippet children(page)}
			{#if !page.items.length}
				<p class="muted cf-empty" data-testid="release-conflicts">
					{status === 'open'
						? 'No conflicts to decide. Your refinements sit cleanly on the current release.'
						: 'No conflicts yet.'}
				</p>
			{:else}
				<div class="cf-list" data-testid="release-conflicts">
					{#each page.items as c (c.id)}
						{@const stage = stageOf(c)}
						{@const ops = conflictActions(c)}
						{@const rows = fieldRows(c.base_before, c.base_after, c.mine, {
							mineHidden: c.tenant_row === 'tombstone',
							mineAbout: c.base_subject === c.key ? 'row' : 'child',
						})}
						{@const compact = compactRows(
							c.base_before,
							c.base_after,
							rows,
							c.tenant_row === 'tombstone'
								? 'hidden'
								: c.base_subject !== c.key
									? '—'
									: c.mine
										? String(c.mine.label ?? c.key)
										: '—',
						)}
						<article
							class="cf-card"
							class:open={stage === 'open'}
							class:quiet={stage === 'superseded' || stage === 'resolved' || stage === 'published'}
							data-conflict={c.id}
							data-status={c.status}
							data-stage={stage}
						>
							<header class="cf-head">
								<div class="cf-title">
									<span class="cf-kind">{KIND_TITLE[c.kind]}</span>
									<span class="cf-obj">{OBJECT_LABEL[c.object] ?? c.object}</span>
									<code class="cf-key">{plain(c.key)}</code>
								</div>
								<Chip text={STAGE_LABEL[stage]} variant={STAGE_VARIANT[stage]} />
							</header>
							<p class="cf-src muted">
								{#if c.raised_by_cut}
									{plain(causeText(c) ?? 'Release')} · raised by the cut
									<a class="mono" href={jobPath(app.job, `/release/${c.release_id}`)}
										>{releaseLabel(c.release_id)}</a
									>
									that adopts it, {when(c.detected_at)} — not applied yet, so Production is unaffected
								{:else}
									{plain(causeText(c) ?? 'Release')} · detected when
									<a class="mono" href={jobPath(app.job, `/release/${c.release_id}`)}
										>{releaseLabel(c.release_id)}</a
									>
									was applied, {when(c.detected_at)}
								{/if}
							</p>

							<div class="cf-why">
								<strong>Why it conflicts.</strong>
								{plain(conflictText(c.kind))}
								<span class="muted">{plain(c.detail)}</span>
							</div>

							<div class="cf-diff-wrap scroll-x">
								<table class="cf-diff">
									<caption class="sr-only">Template base before and now, and your refinement</caption>
									<thead>
										<tr>
											<th scope="col">Field</th>
											<th scope="col"
												>Template before{#if c.base_subject !== c.key}<div class="cf-sub mono">
														{plain(c.base_subject)}
													</div>{/if}</th
											>
											<th scope="col">Template now</th>
											<th scope="col" class="cf-mine-col"
												>Yours <span class="cf-sub">· {MINE_LABEL[c.tenant_row]}</span></th
											>
										</tr>
									</thead>
									<tbody>
										{#if !compact.shown.length}
											<tr><td colspan="4" class="muted">No field differs.</td></tr>
										{/if}
										{#each compact.shown as r (r.field)}
											<tr>
												<th scope="row">{keyLabel(r.field)}</th>
												<td class:cf-gone={c.base_before === null}>{plain(r.before)}</td>
												<td class:cf-moved={r.base_moved} class:cf-gone={c.base_after === null}
													>{r.after === '—' && c.base_after === null ? 'removed' : plain(r.after)}</td
												>
												<td class:cf-differs={r.mine_differs}>{plain(r.mine)}</td>
											</tr>
										{/each}
										{#if c.base_subject !== c.key && c.mine}
											<tr>
												<th scope="row">{plain(c.key)}</th>
												<td colspan="2" class="muted">child of {plain(c.base_subject)}</td>
												<td class="cf-differs"
													>{String(c.mine.label ?? c.key)}{c.mine.type ? ` (${c.mine.type})` : ''}</td
												>
											</tr>
										{/if}
									</tbody>
								</table>
								<p class="cf-legend muted">
									{#if compact.unchanged.length}Same in all three: {compact.unchanged
											.map(keyLabel)
											.join(', ')}.
									{/if}
									<span class="cf-sw moved"></span> the template moved ·
									<span class="cf-sw differs"></span> yours differs from the template now
								</p>
							</div>

							<div class="cf-impact">
								<strong
									>{c.raised_by_cut
										? `Once ${releaseLabel(c.release_id)} is applied`
										: stage === 'open'
											? 'Customers right now'
											: 'Customer effect'}.</strong
								>
								{plain(c.impact?.effect ?? openEffect(c))}
								{#if c.impact}
									<span class="cf-n"
										>{c.impact.conversations_30d} conversation{c.impact.conversations_30d === 1 ? '' : 's'}
										touched this in the last 30 days</span
									>
								{/if}
							</div>

							{#if stage === 'open'}
								<div class="cf-choices" role="group" aria-label="Choices for {plain(c.key)}">
									{#each ops as op (op)}
										<div class="cf-choice">
											<div class="cf-choice-t">{CHOICE_TITLE[op]}</div>
											<p class="muted">{plain(choiceEffect(c, op))}</p>
											<button
												class="btn sm"
												class:primary={op === 'rebase' || ops.length === 1}
												type="button"
												disabled={wa.disabled || busy}
												title={wa.title ?? CONFLICT_ACTION[op].hint}
												onclick={() => resolve(c, op)}>{CONFLICT_ACTION[op].label}</button
											>
										</div>
									{/each}
									<div class="cf-choice">
										<div class="cf-choice-t">Edit it</div>
										<p class="muted">
											{c.kind === 'parent_missing'
												? 'Restore the parent as your own row, then keep this one.'
												: 'Rework your refinement on top of the new template, e.g. add what it now requires.'}
										</p>
										<a
											class="btn sm"
											href={jobPath(app.job, c.kind === 'parent_missing' ? '/ontology' : c.edit_path)}
											>{c.kind === 'parent_missing' ? 'Open ontology' : 'Open refinements'}</a
										>
									</div>
								</div>
								<p class="cf-note muted">
									Your choice is saved to draft; it goes live with the next publish.
								</p>
							{:else if stage === 'draft' && c.draft}
								<div class="cf-decided" data-testid="conflict-draft">
									<span
										><strong>{choiceText(c.decision?.choice)}</strong>
										— saved to draft by {c.decision?.by_name ?? c.decision?.by}, {when(
											c.decision?.at ?? null,
										)}. Still open: Production keeps your refinement until a release carrying this choice is
										applied.
										{c.draft.in_release
											? `It ships when ${c.draft.in_release} is applied.`
											: 'Cut a release on Review & publish to include it.'}</span
									>
									<span class="cf-decided-act">
										<a class="btn sm" href="/chat">Test in test chat</a>
										{#if c.draft.id}
											<button
												class="btn sm"
												type="button"
												disabled={wa.disabled || busy}
												title={wa.title ?? 'Throw this decision away; the conflict opens again.'}
												onclick={() => undo(c)}>Undo decision</button
											>
										{:else}
											<span class="muted small"
												>Already in {c.draft.in_release}; it cannot be undone there.</span
											>
										{/if}
									</span>
								</div>
							{:else if stage === 'published' || stage === 'resolved'}
								<p class="cf-note muted">
									{choiceText(c.resolution)} — {c.resolved_by_name ?? c.resolved_by}, {when(
										c.resolved_at,
									)}{#if c.published_in}; live since
										<a class="mono" href={jobPath(app.job, `/release/${c.published_in}`)}>{c.published_in}</a
										>{/if}.
								</p>
							{:else}
								<p class="cf-note muted">
									Superseded: a later apply re-checked it against the new base and raised it again where it
									still applied.
								</p>
							{/if}
						</article>
					{/each}
				</div>
			{/if}
		{/snippet}
	</LoadState>
</Card>

<style>
	.cf-model {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 10px 14px;
		padding: 12px;
		border: 1px dashed var(--line);
		border-radius: 8px;
		margin-bottom: 14px;
	}
	.cf-lane {
		display: flex;
		flex-direction: column;
		align-items: flex-start;
		gap: 2px;
	}
	.cf-tag {
		font-size: 12px;
		font-weight: 600;
		padding: 3px 8px;
		border-radius: 6px;
		border: 1px solid var(--line);
	}
	.cf-tag.base {
		background: var(--sunk);
	}
	.cf-tag.mine {
		background: color-mix(in srgb, var(--accent) 10%, var(--panel));
		border-color: var(--accent);
	}
	.cf-tag.mine.warn {
		border-color: var(--warn);
	}
	.cf-arrow,
	.cf-move {
		font-size: 11px;
		color: var(--muted);
	}
	.cf-move {
		font-weight: 600;
	}
	.cf-model-copy {
		flex: 1 1 260px;
		margin: 0;
		font-size: 13px;
	}
	.cf-sum {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 10px 18px;
		margin-bottom: 14px;
	}
	.cf-sum-item {
		display: flex;
		align-items: baseline;
		gap: 6px;
	}
	.cf-sum-item strong {
		font-size: 20px;
	}
	.cf-sum-item.warn strong {
		color: var(--warn);
	}
	.cf-sum-item span {
		font-size: 12px;
		color: var(--muted);
	}
	.cf-sum-copy {
		flex: 1 1 320px;
		margin: 0;
		font-size: 12px;
	}
	.cf-empty {
		margin: 6px 0;
	}
	.cf-list {
		display: flex;
		flex-direction: column;
		gap: 14px;
	}
	.cf-card {
		border: 1px solid var(--line);
		border-radius: 8px;
		padding: 12px 14px;
		min-width: 0;
	}
	.cf-card.open {
		border-left: 3px solid var(--warn);
	}
	.cf-card.quiet {
		opacity: 0.85;
	}
	.cf-head {
		display: flex;
		justify-content: space-between;
		align-items: flex-start;
		gap: 10px;
	}
	.cf-title {
		display: flex;
		flex-wrap: wrap;
		align-items: baseline;
		gap: 4px 8px;
		min-width: 0;
	}
	.cf-kind {
		font-weight: 700;
	}
	.cf-obj {
		font-size: 12px;
		color: var(--muted);
	}
	.cf-key {
		font-size: 12px;
		overflow-wrap: anywhere;
	}
	.cf-src {
		margin: 2px 0 8px;
		font-size: 12px;
	}
	.cf-why,
	.cf-impact {
		font-size: 13px;
		margin: 8px 0;
	}
	.cf-n {
		display: inline-block;
		margin-left: 4px;
		font-size: 12px;
		font-weight: 600;
	}
	.cf-diff-wrap {
		overflow-x: auto;
	}
	.cf-diff :is(td, th) {
		overflow-wrap: anywhere;
	}
	.cf-diff {
		width: 100%;
		border-collapse: collapse;
		font-size: 12px;
	}
	.cf-diff th,
	.cf-diff td {
		text-align: left;
		padding: 4px 8px;
		border-bottom: 1px solid var(--line);
		vertical-align: top;
	}
	.cf-diff thead th {
		font-size: 11px;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		color: var(--muted);
	}
	.cf-sub {
		text-transform: none;
		letter-spacing: 0;
		font-weight: 400;
	}
	.cf-moved {
		background: color-mix(in srgb, var(--warn) 16%, transparent);
	}
	.cf-differs {
		background: color-mix(in srgb, var(--accent) 14%, transparent);
	}
	.cf-gone {
		color: var(--muted);
		font-style: italic;
	}
	.cf-legend {
		margin: 4px 0 0;
		font-size: 11px;
	}
	.cf-sw {
		display: inline-block;
		width: 10px;
		height: 10px;
		border-radius: 2px;
		vertical-align: -1px;
	}
	.cf-sw.moved {
		background: color-mix(in srgb, var(--warn) 40%, transparent);
	}
	.cf-sw.differs {
		background: color-mix(in srgb, var(--accent) 35%, transparent);
	}
	.cf-choices {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(190px, 1fr));
		gap: 10px;
		margin-top: 10px;
	}
	.cf-choice {
		border: 1px solid var(--line);
		border-radius: 6px;
		padding: 8px 10px;
		display: flex;
		flex-direction: column;
		align-items: flex-start;
		gap: 4px;
	}
	.cf-choice p {
		margin: 0;
		font-size: 12px;
		flex: 1;
	}
	.cf-choice-t {
		font-weight: 600;
		font-size: 13px;
	}
	.cf-note {
		font-size: 12px;
		margin: 8px 0 0;
	}
	.cf-decided {
		display: flex;
		flex-wrap: wrap;
		justify-content: space-between;
		align-items: center;
		gap: 8px;
		margin-top: 10px;
		padding: 8px 10px;
		border-radius: 6px;
		background: color-mix(in srgb, var(--accent) 10%, transparent);
		font-size: 13px;
	}
	.cf-decided-act {
		display: flex;
		gap: 6px;
	}
	.sr-only {
		position: absolute;
		width: 1px;
		height: 1px;
		overflow: hidden;
		clip: rect(0 0 0 0);
		white-space: nowrap;
	}
</style>
