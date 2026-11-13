<script lang="ts">
	// J12 detail: one procedure. The version lane says where every version stands for customers (runs now, in the
	// live release, shadowed, in review, draft, retired); the selected version shows as a flow from its context space
	// through its steps (what each does, which operation, which inputs, what it needs) to the outcomes, marked
	// against the running version. The aside holds the decision: where it stands, the grant check, and the next
	// move (DRAFT → CANARY → approve → PUBLISHED by a Release manager, D88; roll back from ARCHIVED). Test runs and history are tabs.
	// from cb:web/src/routes/admin/jobs/[job]/procedures/+page.svelte@45ee128 (detail drawer → own page)
	import { onMount } from 'svelte';
	import { api, app, guardOutcome } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { canWrite as canWriteFn, writeAttrs, writeHint } from '$lib/auth/roles';
	import { Resource } from '$lib/load.svelte';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Status from '$lib/ui/Status.svelte';
	import Tabs from '$lib/ui/Tabs.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import StateDiagram from '$lib/ui/StateDiagram.svelte';
	import type { JobSummary } from '$lib/api/types/core';
	import type { ProcedureView, VersionStatus } from '$lib/api/types/ontology';
	import { when } from '$lib/ontology/shared';
	import {
		OUTCOME_LABEL,
		historyText,
		liveOf,
		nextStatuses,
		pendingOf,
		reachText,
		successView,
	} from '../view';
	import NewVersionDrawer from '../NewVersionDrawer.svelte';
	import ProcedureFlow from './ProcedureFlow.svelte';
	import ProcedureChecks from './ProcedureChecks.svelte';
	import VersionLane from './VersionLane.svelte';

	let { name, version = null }: { name: string; version?: number | null } = $props();

	const versions = new Resource(() => api().procedureVersions(name));
	const spaces = new Resource(() => api().contextSpaces());
	const job = new Resource<JobSummary>(() => api().job(app.job));
	onMount(() => {
		versions.refresh();
		spaces.refresh();
		job.refresh();
	});
	const release = $derived(job.value.state === 'ok' ? (job.value.data.current_release?.id ?? null) : null);

	const canWrite = $derived(canWriteFn(session, 'job_designer'));
	/** §R.1: approving and publishing (incl. a roll back to PUBLISHED) are the Release manager's. */
	const canPublish = $derived(canWriteFn(session, 'release_manager'));
	let picked = $state<number | null>(null);
	let tab = $state('flow');
	let busy = $state(false);
	let confirmRollback = $state(false);
	let creating = $state(false);

	function pick(rows: ProcedureView[]): ProcedureView {
		const want = picked ?? version;
		return rows.find((p) => p.version === want) ?? rows.find((p) => p.effective) ?? rows[0]!;
	}
	function choose(v: number) {
		picked = v;
		confirmRollback = false;
	}
	const me = $derived(session.user?.sub ?? '');

	async function act(p: ProcedureView, fn: () => Promise<unknown>, ok: string) {
		if (busy) return;
		busy = true;
		await guardOutcome(fn, ok, 'Refused');
		confirmRollback = false;
		picked = p.version;
		await versions.refresh();
		busy = false;
	}
	const approve = (p: ProcedureView) =>
		act(p, () => api().approveProcedure(p.name, p.version), `Approved ${p.name}@${p.version}.`);
	const move = (p: ProcedureView, to: VersionStatus) =>
		act(
			p,
			() => api().setProcedureStatus(p.name, p.version, to),
			p.status === 'ARCHIVED' && to === 'PUBLISHED'
				? `Rolled ${p.name} back to ${p.version}: live again with the next release.`
				: to === 'PUBLISHED'
					? `Published ${p.name}@${p.version}: it goes live with the next release.`
					: `${p.name}@${p.version} is now ${to}.`,
		);

	const LIFE_STATES = [
		{ id: 'DRAFT', initial: true },
		{ id: 'CANARY' },
		{ id: 'PUBLISHED' },
		{ id: 'ARCHIVED' },
		{ id: 'DISCARDED', terminal: true },
	];
	const LIFE_MOVES = [
		{ from: 'DRAFT', to: 'CANARY', label: 'review' },
		{ from: 'CANARY', to: 'PUBLISHED', label: 'publish' },
		{ from: 'PUBLISHED', to: 'ARCHIVED', label: 'replaced' },
		{ from: 'ARCHIVED', to: 'PUBLISHED', label: 'roll back' },
		{ from: 'DRAFT', to: 'DISCARDED' },
		{ from: 'CANARY', to: 'DISCARDED' },
	];
</script>

<PageHead
	id="J12"
	title="Procedure · {name}"
	sub="What the agent does for this request type, version by version. Customers get the live version (the one the live release carries). A new version is a draft: test it in the test chat, have it reviewed, then a Release manager publishes it with a release."
>
	{#snippet actions()}
		<a class="btn" href="/admin/jobs/{app.job}/procedures">All procedures</a>
		<button
			type="button"
			class="btn primary"
			{...writeAttrs(session, 'job_designer')}
			onclick={() => (creating = true)}>New version</button
		>
	{/snippet}
</PageHead>

<LoadState resource={versions} empty="This procedure has no versions.">
	{#snippet children(rows)}
		{@const sel = pick(rows)}
		{@const eff = liveOf(rows, release)}
		{@const pending = pendingOf(rows, release)}
		{@const space =
			spaces.value.state === 'ok'
				? (spaces.value.data.items.find((x) => x.key === sel.context_space) ?? null)
				: null}
		{@const ok = successView(sel.success_rate)}
		{@const approvedByMe = !!me && sel.reviewers.includes(me)}
		{@const tests = sel.test_runs ?? []}
		<section class="pd-lane" aria-label="Versions">
			<VersionLane {rows} current={sel.version} {release} onpick={choose} />
			<p class="pd-runs">
				{#if eff}Live in production: <b class="mono">{eff.version}</b> ({eff.origin === 'tenant'
						? 'your version'
						: 'from the template'}), carried by release
					<a class="mono" href="/admin/jobs/{app.job}/release/{release}">{release}</a>.
				{:else}<b class="err-text">Nothing live: cases in this space are handed over to staff.</b>{/if}
				{#if pending}<span class="pd-pending"
						><b class="mono">{pending.version}</b> is published and goes live with the next release.</span
					>{/if}
			</p>
		</section>

		<div class="pd">
			<div class="pd-main">
				<Tabs
					items={[
						['flow', 'Flow', sel.steps.length],
						['tests', 'Test runs', tests.length],
						['reviews', 'Reviews & history', sel.history.length],
					]}
					bind:active={tab}
				/>
				{#if tab === 'flow'}
					<ProcedureFlow {sel} {eff} {space} onpick={choose} />
					<ProcedureChecks {sel} />
				{:else if tab === 'tests'}
					<div class="pd-tests" data-testid="procedure-tests">
						<p class="muted pd-p">
							Staff test chats run this draft against the test routes (env <span class="mono">test</span>),
							never production systems. Try the cases that should fail too.
							<a href="/chat">Open the test chat</a>
						</p>
						{#each tests as t, i (i)}
							{@const o = OUTCOME_LABEL[t.outcome]!}
							<article class="pd-test">
								<div class="pd-test-head">
									<Chip text={o[0]} variant={o[1]} />
									<span class="muted">step {t.reached_step} of {sel.steps.length}</span>
									<span class="muted pd-test-at">{when(t.at)} · {t.actor}</span>
								</div>
								<q>{t.utterance}</q>
								<span class="pd-test-note">{t.note}</span>
							</article>
						{:else}
							<div class="empty">
								{#if sel.status === 'PUBLISHED' || sel.status === 'ARCHIVED'}
									No staff tests recorded for this version. {sel.runs_7d
										? `It handled ${sel.runs_7d.toLocaleString('en-GB')} customer cases in the last 7 days.`
										: ''}
									<a href="/admin/jobs/{app.job}/cases">Cases</a>
								{:else}
									Not tested yet. Run it in the test chat before asking for review.
								{/if}
							</div>
						{/each}
					</div>
				{:else}
					<StateDiagram
						states={LIFE_STATES}
						transitions={LIFE_MOVES}
						current={sel.status}
						title="Procedure version lifecycle"
					/>
					{#if sel.origin === 'tenant'}
						<Callout kind="info" title="Reviewers: {sel.reviewers.length}">
							{sel.reviewers.length
								? `Approved by ${sel.reviewers.join(' and ')}.`
								: 'Nobody has reviewed this version yet.'}
							Approving records a review; publishing needs the Release manager role (D88), not a reviewer count.
						</Callout>
					{/if}
					<ol class="pr-hist">
						{#each sel.history as h, i (i)}
							<li class:bad={!h.ok}>
								<span class="mono pr-hist-at">{when(h.at)}</span>
								<span>{historyText(h)}</span>
								<span class="muted">by {h.actor}</span>
							</li>
						{:else}
							<li class="muted">No recorded changes.</li>
						{/each}
					</ol>
				{/if}
			</div>

			<aside class="pd-side">
				<Card title="{sel.name}@{sel.version}" sub={sel.description}>
					<div class="chips head-chips">
						<Status word={sel.status} />
						<Chip
							text={sel.origin === 'tenant' ? 'Tenant version' : 'Base version'}
							variant={sel.origin === 'tenant' ? 'accent' : 'muted'}
						/>
						{#if eff && sel.version === eff.version}<Chip
								text="live"
								variant="ok"
							/>{:else if pending && sel.version === pending.version}<Chip
								text="next release"
								variant="accent"
							/>{:else if sel.status === 'DRAFT' || sel.status === 'CANARY'}<Chip
								text="draft · not live"
								variant="warn"
							/>{/if}
						{#if sel.shadowed}<Chip text="shadowed by tenant {eff?.version ?? ''}" variant="dead" />{/if}
					</div>
					<p class="pd-reach" data-testid="procedure-reach">{reachText(sel, eff, release)}</p>
					<dl class="kvs">
						<dt>Context space</dt>
						<dd class="mono">{sel.context_space}</dd>
						<dt>Activity grant</dt>
						<dd>
							<span class="chips">
								{#each sel.activity_grant as g (g)}<Chip text={g} variant="tool" />{:else}<span class="muted"
										>none</span
									>{/each}
							</span>
						</dd>
						<dt>Production, 7 days</dt>
						<dd>
							{#if eff && sel.version === eff.version}
								{sel.runs_7d ? `${sel.runs_7d.toLocaleString('en-GB')} cases` : 'no cases'}{ok
									? ` · ${ok.text} success`
									: ''}
							{:else}<span class="muted">not live: no production cases</span>{/if}
						</dd>
						<dt>Updated</dt>
						<dd>{when(sel.updated_at)}</dd>
					</dl>
					{#if sel.shadowed}
						<Callout kind="info" title="Shadowed by a tenant version">
							The release also carries your version {eff?.version}, so production runs it instead of this base
							version. Archiving your version (saved to draft, live after the next release) makes the base
							version run again.
						</Callout>
					{/if}
					{#if sel.outside_grant.length}
						<Callout kind="err" title="{sel.outside_grant.join(', ')} outside the activity grant">
							A step calls an operation the activity does not grant. Moving this version to CANARY or
							PUBLISHED, or cutting a release with it, is refused with 409 <span class="mono"
								>operation_outside_grant</span
							>. <a href="/admin/jobs/{app.job}/toolsets">Toolsets</a>
						</Callout>
					{/if}
					{#if sel.origin === 'base'}
						<p class="note">
							Base versions come from the pinned job template and change only with a template release. Start a
							tenant version to change what this job does.
						</p>
					{/if}
					<div class="pd-next">
						<span class="label">Next step</span>
						<div class="row pd-links">
							<a class="btn sm" href="/chat">Test in test chat</a>
							<a class="btn sm" href="/admin/jobs/{app.job}/release">Review &amp; publish</a>
						</div>
						<div class="row">
							{#if sel.origin === 'base'}
								<span class="muted">Status follows the template.</span>
								<button
									type="button"
									class="btn"
									{...writeAttrs(session, 'job_designer')}
									onclick={() => (creating = true)}>Start a tenant version from it</button
								>
							{:else}
								{#each nextStatuses(sel.status) as to (to)}
									{#if to === 'PUBLISHED' && sel.status === 'ARCHIVED'}
										{#if confirmRollback}
											<span class="pr-confirm" role="group" aria-label="Confirm roll back">
												<span
													>Re-publish {sel.version}? {eff && eff.origin === 'tenant'
														? `${eff.version} moves to ARCHIVED.`
														: ''}</span
												>
												<button
													class="btn primary"
													type="button"
													disabled={busy}
													onclick={() => move(sel, to)}>Roll back to {sel.version}</button
												>
												<button class="btn" type="button" onclick={() => (confirmRollback = false)}
													>Cancel</button
												>
											</span>
										{:else}
											<button
												class="btn"
												type="button"
												{...writeAttrs(session, 'release_manager')}
												onclick={() => (confirmRollback = true)}>Roll back to this version</button
											>
										{/if}
									{:else if to === 'PUBLISHED'}
										<button
											class="btn"
											type="button"
											disabled={!canPublish || busy || approvedByMe}
											title={!canPublish
												? writeHint('release_manager')
												: approvedByMe
													? 'You approved this version'
													: 'Adds you as a reviewer; it does not publish'}
											onclick={() => approve(sel)}>Approve</button
										>
										{#if approvedByMe}<span class="muted">You approved this version.</span>{/if}
										<button
											class="btn primary"
											type="button"
											disabled={!canPublish || busy}
											title={!canPublish
												? writeHint('release_manager')
												: 'Adds it to the next release; it goes live when that release is applied'}
											onclick={() => move(sel, to)}>Publish</button
										>
									{:else}
										<button
											class="btn"
											class:danger={to === 'DISCARDED'}
											type="button"
											{...writeAttrs(session, 'job_designer')}
											disabled={!canWrite || busy}
											onclick={() => move(sel, to)}>Move to {to}</button
										>
									{/if}
								{:else}
									<span class="muted">No further moves from {sel.status}.</span>
								{/each}
							{/if}
						</div>
					</div>
				</Card>
			</aside>
		</div>
		<NewVersionDrawer
			bind:open={creating}
			from="{sel.name}@{sel.version}"
			procedures={rows}
			spaces={spaces.value.state === 'ok' ? spaces.value.data.items : []}
			oncreated={() => versions.refresh()}
		/>
	{/snippet}
</LoadState>

<style>
	.pd-lane {
		margin-bottom: 14px;
	}
	.pd-pending {
		margin-left: 6px;
		color: var(--accent);
	}
	.pd-runs {
		margin: 8px 0 0;
		font-size: 13px;
	}
	.err-text {
		color: var(--err);
	}
	.pd {
		display: grid;
		grid-template-columns: minmax(0, 1fr) minmax(300px, 380px);
		gap: 16px;
		align-items: start;
	}
	@media (max-width: 1000px) {
		.pd {
			grid-template-columns: 1fr;
		}
		.pd-side {
			order: -1;
			position: static;
		}
	}
	.pd-main {
		display: grid;
		gap: 12px;
		min-width: 0;
	}
	.pd-side {
		display: grid;
		gap: 12px;
		min-width: 0;
		position: sticky;
		top: 12px;
	}
	.head-chips {
		margin-bottom: 8px;
	}
	.pd-reach {
		margin: 0 0 10px;
		font-size: 13px;
		font-weight: 600;
	}
	.pd-next {
		display: grid;
		gap: 6px;
		margin-top: 12px;
		padding-top: 10px;
		border-top: 1px solid var(--line-2);
	}
	.pd-p {
		margin: 0 0 8px;
		font-size: 12.5px;
	}
	.pd-tests {
		display: grid;
		gap: 8px;
	}
	.pd-test {
		display: grid;
		gap: 4px;
		padding: 10px 12px;
		border: 1px solid var(--line);
		border-radius: var(--r);
		background: var(--panel);
	}
	.pd-test-head {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
		align-items: center;
		font-size: 12px;
	}
	.pd-test-at {
		margin-left: auto;
	}
	.pd-test q {
		font-size: 13.5px;
	}
	.pd-test-note {
		font-size: 12.5px;
		color: var(--muted);
	}
	.pr-hist {
		display: grid;
		gap: 4px;
		padding-left: 18px;
		font-size: 13px;
	}
	.pr-hist li {
		display: flex;
		gap: 8px;
		flex-wrap: wrap;
	}
	.pr-hist li.bad {
		color: var(--err);
	}
	.pr-hist-at {
		color: var(--faint);
	}
	.row {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
		align-items: center;
	}
	.pr-confirm {
		display: inline-flex;
		gap: 8px;
		align-items: center;
		flex-wrap: wrap;
	}
</style>
