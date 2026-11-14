<script lang="ts">
	// J18 Discovery (tenant-scoped derivation, 11§7 + 10§8, plan p8): discovery reads the tenant's real traffic (ticket
	// exports, live conversations, knowledge-search misses, shadow traffic) and proposes changes to this job — new or
	// hidden activities, missing knowledge, operations to route — each with its evidence. The tenant_admin decides:
	// Accept writes the change to the DRAFT (D54: never live on save; Test → Publish → Live), Reject dismisses it.
	// Classification and drafting are metered as this tenant's usage (D56: same pipeline + funding lanes as turns, not a
	// separate bill; shown on T5 Usage). Coverage + scorecard and shadow mode stay below.
	import { onDestroy, onMount } from 'svelte';
	import { goto } from '$app/navigation';
	import { api, app, guard } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { writeHint, canWrite, writeAttrs } from '$lib/auth/roles';
	import type { DiscoveryImport, DiscoveryProposal, DiscoveryState } from '$lib/api/types/journey';
	import DraftBar from '$lib/release/DraftBar.svelte';
	import Seg from '$lib/ui/Seg.svelte';
	import J18Flow from './J18Flow.svelte';
	import J18ImportDrawer from './J18ImportDrawer.svelte';
	import { DEFAULT_WEIGHT, MAX_WEIGHT, parseWeight } from './import';
	import J18ProposalCard from './J18ProposalCard.svelte';
	import { J18_KIND_LABEL, tallyJ18 } from '$lib/journey/J18Discovery';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import Meter from '$lib/ui/Meter.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import { STAGE_LABEL } from '$lib/journey/catalogue';
	import { FUNDING_LABEL, STEP_LABEL, costLine, journeyUi, when } from '$lib/journey/format';
	import { fmt } from '$lib/format';
	import type { GeneratorEstimate, GeneratorRun } from '$lib/api/types/journey';
	import { PROVIDERS_LINK, providerMissing, tenantRunHref } from '$lib/journey/B8Generator';
	import { tokens } from '$lib/journey/format';

	const res = new Resource(
		() => api().discoveryState(),
		() => false,
	);
	let poll: ReturnType<typeof setTimeout> | undefined;
	function schedule() {
		clearTimeout(poll);
		const v = res.value;
		if (v.state === 'ok' && v.data.imports[0]?.status === 'classifying')
			poll = setTimeout(async () => {
				await res.refresh();
				schedule();
			}, journeyUi.pollMs);
	}
	onMount(async () => {
		await res.refresh();
		schedule();
	});
	onDestroy(() => clearTimeout(poll));
	const base = $derived(`/admin/jobs/${app.job}`);
	const can = $derived(canWrite(session, 'job_designer'));

	let shownId = $state('');
	const current = (s: DiscoveryState): DiscoveryImport | undefined =>
		s.imports.find((i) => i.id === shownId) ?? s.imports[0];

	// ---- upload (previewed first, F-52) ----
	let uploadOpen = $state(false);
	let uploading = $state(false);
	async function uploaded() {
		shownId = '';
		await res.refresh();
		schedule();
	}
	async function uploadRows(file: File | string, rows: number) {
		uploading = true;
		const name = typeof file === 'string' ? file : file.name;
		const ok = await guard(
			() => api().uploadDiscoveryImport({ file, rows }),
			`${name} uploaded — classifying ${fmt.n(rows)} rows.`,
			'Not uploaded',
		);
		uploading = false;
		if (ok) {
			shownId = '';
			await res.refresh();
			schedule();
		}
	}

	// ---- delete an import (a classifying one is cancelled first), scorecard weights (F-51) ----
	let confirmDelete = $state('');
	async function removeImport(i: DiscoveryImport) {
		const ok = await guard(
			() => api().deleteDiscoveryImport(i.id),
			i.status === 'classifying' ? `Cancelled and deleted ${i.file}.` : `Deleted ${i.file}.`,
			'Not deleted',
		);
		if (ok) {
			confirmDelete = '';
			shownId = '';
			await res.refresh();
			schedule();
		}
	}
	let weightText = $state<Record<string, string>>({});
	let weightProblem = $state<Record<string, string>>({});
	async function setWeight(activity: string, label: string) {
		const t = parseWeight(weightText[activity] ?? '');
		if (!t.ok) {
			weightProblem[activity] = t.problem;
			return;
		}
		delete weightProblem[activity];
		if (
			await guard(
				() => api().setScorecardWeight(activity, t.weight),
				`Weight of ${label} set to ${t.weight}.`,
				'Weight not set',
			)
		) {
			delete weightText[activity];
			await res.refresh();
		}
	}

	let tick = $state(0);
	async function acceptP(p: DiscoveryProposal) {
		if (
			await guard(
				() => api().acceptDiscoveryProposal(p.id),
				`Saved to draft: ${p.title}. Not live — try it in the test chat, then publish.`,
				'Not accepted',
			)
		) {
			recent = [...recent, p.id];
			await res.refresh();
			tick++;
		}
	}
	async function rejectP(p: DiscoveryProposal, reason: string) {
		const ok = await guard(
			() => api().rejectDiscoveryProposal(p.id, reason),
			`Rejected: ${p.title}. Nothing changes.`,
			'Not rejected',
		);
		if (ok) {
			recent = [...recent, p.id];
			await res.refresh();
		}
		return !!ok;
	}
	let show = $state('open');
	// Decided in this visit: they stay in the Open view so the decision's outcome (draft → test → publish) is visible.
	let recent = $state<string[]>([]);
	let kindF = $state('all');
	const visible = (ps: DiscoveryProposal[]) =>
		ps.filter(
			(p) =>
				(show === 'all' ||
					(show === 'open' ? p.status === 'open' || recent.includes(p.id) : p.status !== 'open')) &&
				(kindF === 'all' || p.kind === kindF),
		);

	let shadowEnv = $state<'test' | 'prod'>('test');
	async function shadow(on: boolean) {
		if (
			await guard(
				() => (on ? api().startShadowMode(shadowEnv) : api().stopShadowMode()),
				on ? 'Shadow mode on: every conversation hands over.' : 'Shadow mode off.',
			)
		)
			await res.refresh();
	}

	let runOpen = $state(false);
	// 409 generator_provider_missing on start: the tenant has no generator model bound
	let noProvider = $state(false);
	let runDesc = $state('');
	// The usage range before the first LLM call (D56: usage on the tenant's own funding lane, never a bill).
	let est = $state<GeneratorEstimate | null>(null);
	let estErr = $state('');
	$effect(() => {
		if (!runOpen) return;
		est = null;
		estErr = '';
		api()
			.estimateGeneratorRun({ scope: 'tenant' })
			.then((e) => (est = e))
			.catch((e) => (estErr = e instanceof Error ? e.message : 'Estimate unavailable.'));
	});
	async function startRun(s: DiscoveryState) {
		// The API names no industry/department on the discovery state; an empty pair means "this job's own":
		// POST /api/generator/runs derives them from the job's selection.
		const ind = s.industry ?? '';
		const dep = s.department ?? '';
		let started: GeneratorRun | null = null;
		if (
			await guard(async () => {
				noProvider = false;
				try {
					started = await api().startGeneratorRun({
						scope: 'tenant',
						industry: ind,
						department: dep,
						job: s.job,
						description: runDesc,
					});
				} catch (e) {
					if (providerMissing(e)) noProvider = true;
					throw e;
				}
			}, 'Tenant generator run started — its tokens are metered as your usage (D56).')
		) {
			runOpen = false;
			runDesc = '';
			const run = started as GeneratorRun | null;
			if (run) await goto(tenantRunHref(app.job, run.id));
			else await res.refresh();
		}
	}
	// A weight above 100 can lift Valuable past 5: the dots stop at 5, the title keeps the number.
	const dots = (n: number) => {
		const k = Math.max(0, Math.min(5, Math.round(n)));
		return '●'.repeat(k) + '○'.repeat(5 - k);
	};
</script>

<PageHead
	id="J18"
	sub="What your customers actually ask, turned into proposed changes to this job. You accept or reject each one; accepted changes go to the draft and reach customers only when you publish."
>
	{#snippet actions()}
		<button
			class="btn primary"
			type="button"
			{...writeAttrs(session, 'job_designer')}
			disabled={!can || uploading}
			onclick={() => (uploadOpen = true)}>{uploading ? 'Uploading…' : 'Upload export'}</button
		>
	{/snippet}
</PageHead>

<DraftBar page="J18" {tick} />

<LoadState resource={res}>
	{#snippet children(s)}
		{@const imp = current(s)}
		{@const ps = s.proposals ?? []}
		{@const tally = tallyJ18(ps)}
		{@const tenantName = s.usage?.tenant ?? 'this tenant'}
		<div class="stack">
			{#if s.shadow.active}
				<div class="shadow-banner" role="status" data-testid="shadow-banner">
					<b>Shadow mode · {s.shadow.env}</b>
					Every conversation hands over to staff (handover-only routes); gates and clustering label the live traffic.
					Started
					{when(s.shadow.started_at)} by {s.shadow.started_by} · {s.shadow.conversations} conversations, {s
						.shadow.labelled}
					labelled.
				</div>
			{/if}

			<J18Flow signals={s.signals ?? []} {tally} {base} />

			<div class="j18-main">
				<div class="stack">
					<Card
						title="Proposals for this job"
						sub="Each one is backed by real traffic. Accept saves it to the draft (not live); reject dismisses it."
					>
						{#snippet actions()}
							<Seg
								label="Show"
								bind:active={show}
								items={[
									['open', `Open (${tally.open})`],
									['decided', `Decided (${ps.length - tally.open})`],
									['all', 'All'],
								]}
							/>
						{/snippet}
						<div class="stack" data-testid="proposals">
							{#if ps.length}
								<div class="j18-kinds" role="group" aria-label="Kind">
									{#each ['all', 'new_activity', 'enable_activity', 'knowledge_gap', 'operation_route'] as k (k)}
										{@const n = ps.filter((p) => k === 'all' || p.kind === k).length}
										{#if n}
											<button
												type="button"
												class="btn sm"
												class:primary={kindF === k}
												aria-pressed={kindF === k}
												onclick={() => (kindF = k)}
												>{k === 'all' ? 'All kinds' : J18_KIND_LABEL[k as DiscoveryProposal['kind']]} · {n}</button
											>
										{/if}
									{/each}
								</div>
							{/if}
							{#each visible(ps) as p (p.id)}
								<J18ProposalCard {p} {base} tenant={tenantName} onaccept={acceptP} onreject={rejectP} />
							{:else}
								<p class="muted" data-testid="no-proposals">
									{#if !ps.length}
										No proposals yet. Upload a ticket or chat export, or turn on shadow mode: discovery needs
										real traffic to propose anything.
									{:else if show === 'open'}
										Nothing open — every proposal is decided. New ones appear as traffic comes in.
									{:else}
										No proposals in this view.
									{/if}
								</p>
							{/each}
						</div>
					</Card>

					{#if !imp}
						<Card title="Start with a ticket export">
							<div class="empty" data-testid="no-import">
								<p>
									Upload a Zendesk, Freshdesk or ServiceNow export (CSV, JSON or XLSX, one ticket per row).
									The activity gate classifies every row into this job’s activities; what nothing covers
									becomes a proposal.
								</p>
								<p class="muted">
									Classification and drafting tokens are metered as {tenantName}'s usage, like conversation
									turns (<a href="/admin/usage">Usage</a>).
								</p>
								<button
									class="btn"
									type="button"
									{...writeAttrs(session, 'job_designer')}
									onclick={() => uploadRows('servicenow-export-2026-10.csv', 3120)}
									>Use sample export (3,120 tickets)</button
								>
							</div>
						</Card>
					{:else if imp.status === 'classifying'}
						<Card title="Classifying {imp.file}">
							{#snippet actions()}
								{@render deleteBtn(imp)}
							{/snippet}
							<div class="stack" role="status" data-testid="classifying">
								<p>The GA gate is classifying {fmt.n(imp.rows)} rows into activities…</p>
								<Meter value={imp.progress} />
								<p class="muted small">
									{Math.round(imp.progress * 100)} % · uploaded {when(imp.uploaded_at)} by {imp.uploaded_by}
								</p>
							</div>
						</Card>
					{:else}
						{@const covered = imp.volumes.reduce((n, v) => n + v.tickets, 0)}
						{@const clustered = imp.clusters.reduce((n, c) => n + c.tickets, 0)}
						<Card
							cls="flush"
							title="What the agent already covers"
							sub="{imp.file} · {fmt.n(
								imp.rows,
							)} tickets classified into this job’s activities. Scorecard: Valuable = volume × value · Feasible = operations routed · Usable = containment live."
						>
							{#snippet actions()}
								{@render deleteBtn(imp)}
								{#if s.imports.length > 1}
									<select class="input env" aria-label="Import" bind:value={shownId}>
										{#each s.imports as i (i.id)}<option value={i.id}>{i.file}</option>{/each}
									</select>
								{/if}
							{/snippet}
							<div class="j18-cov">
								<div class="j18-covbar" aria-label="Coverage of the export">
									<span class="c" style:width="{(covered / imp.rows) * 100}%"></span>
									<span class="u" style:width="{(clustered / imp.rows) * 100}%"></span>
									<span class="t" style:width="{(imp.long_tail / imp.rows) * 100}%"></span>
								</div>
								<div class="j18-covleg small">
									<span><i class="c"></i><b>{fmt.pct(imp.coverage, 0)}</b> covered · {fmt.n(covered)}</span>
									<span
										><i class="u"></i>{fmt.n(clustered)} in {imp.clusters.length} uncovered clusters → proposals</span
									>
									<span><i class="t"></i>{fmt.n(imp.long_tail)} long tail (no cluster ≥ 50)</span>
								</div>
							</div>
							<div class="tbl-wrap">
								<table class="tbl" data-testid="volumes">
									<thead>
										<tr>
											<th>Activity</th>
											<th>Stage</th>
											<th class="r">Tickets</th>
											<th class="r">Share</th>
											<th>Valuable</th>
											<th>Feasible</th>
											<th>Usable</th>
											<th
												title="Scales Valuable: a whole number from 0 to {MAX_WEIGHT}; {DEFAULT_WEIGHT} is the default"
												>Weight</th
											>
										</tr>
									</thead>
									<tbody>
										{#each imp.volumes as v (v.activity)}
											<tr>
												<td
													><a href="{base}/journey/{encodeURIComponent(v.activity)}">{v.label}</a>
													<div class="cell-sub mono">{v.activity}</div></td
												>
												<td>{STAGE_LABEL[v.stage]}</td>
												<td class="r">{fmt.n(v.tickets)}</td>
												<td class="r">{fmt.pct(v.tickets / imp.rows)}</td>
												<td class="dots" title="{v.valuable} of 5">{dots(v.valuable)}</td>
												<td
													><span class="chip {v.feasible >= 1 ? 'ok' : v.feasible >= 0.5 ? 'warn' : 'err'}"
														>{fmt.pct(v.feasible, 0)}</span
													></td
												>
												<td>{v.usable == null ? '—' : fmt.pct(v.usable, 0)}</td>
												<td class="wt" data-testid="weight-{v.activity}">
													<div class="row nowrap">
														<input
															class="input wt-in mono"
															inputmode="numeric"
															aria-label="Scorecard weight for {v.label}"
															placeholder={String(v.weight ?? DEFAULT_WEIGHT)}
															bind:value={weightText[v.activity]}
															disabled={!can}
														/>
														<button
															class="btn sm"
															type="button"
															aria-label="Set weight for {v.label}"
															{...writeAttrs(session, 'job_designer')}
															onclick={() => setWeight(v.activity, v.label)}>Set</button
														>
													</div>
													{#if weightProblem[v.activity]}<div class="cell-sub err-text" role="alert">
															{weightProblem[v.activity]}
														</div>{/if}
												</td>
											</tr>
										{/each}
									</tbody>
								</table>
							</div>
						</Card>
					{/if}
				</div>

				<aside class="stack">
					<Card title="Where the evidence comes from">
						<ul class="plain j18-sources">
							{#each s.signals ?? [] as g (g.source)}
								<li class:off={!g.active}>
									<span class="split"
										><b>{g.label}</b>
										<span class="mono">{g.active ? `${fmt.n(g.count)} ${g.unit}` : 'off'}</span></span
									>
									<span class="cell-sub">{g.window}</span>
								</li>
							{/each}
						</ul>
					</Card>

					{#if s.usage}
						<Card
							title="Usage"
							sub="Discovery's LLM tokens are metered like conversation turns: same pipeline, same funding lanes, attributed to {s
								.usage.tenant}."
						>
							<ul class="plain j18-bill" data-testid="discovery-usage">
								{#each s.usage.lines as l (l.what)}
									<li>
										<span class="split"><span>{l.what}</span><b class="mono">{fmt.k(l.tokens)} tok</b></span>
										<span class="cell-sub"
											>{l.detail} · {FUNDING_LABEL[l.funding]} · activity {l.activity}</span
										>
									</li>
								{/each}
								<li class="j18-total">
									<span class="split"><b>Total</b><b class="mono">{fmt.k(s.usage.tokens)} tok</b></span>
									<span class="cell-sub"
										>part of your usage on <a href="/admin/usage">Usage</a> — not a separate bill</span
									>
								</li>
							</ul>
						</Card>
					{/if}

					<Card
						title="Shadow mode"
						sub="No export? Go live with handover-only routes: staff answer everything while discovery labels the traffic."
					>
						{#if s.shadow.active}
							<p>On in <b>{s.shadow.env}</b> since {when(s.shadow.started_at)}.</p>
							<button
								class="btn"
								type="button"
								{...writeAttrs(session, 'job_designer')}
								onclick={() => shadow(false)}>Stop shadow mode</button
							>
						{:else}
							<div class="row">
								<select class="input env" bind:value={shadowEnv} aria-label="Environment" disabled={!can}>
									<option value="test">test</option>
									<option value="prod">prod</option>
								</select>
								<button
									class="btn"
									type="button"
									{...writeAttrs(session, 'job_designer')}
									onclick={() => shadow(true)}>Start shadow mode</button
								>
							</div>
						{/if}
					</Card>

					<Card
						title="Tenant generator runs"
						sub="For a bigger gap: the generator drafts several activities at once. Refinements only (D8)."
					>
						{#snippet actions()}
							<button
								class="btn sm"
								type="button"
								{...writeAttrs(session, 'job_designer')}
								disabled={!can}
								onclick={() => (runOpen = true)}>Start a run</button
							>
						{/snippet}
						<ul class="plain">
							{#each s.runs as r (r.id)}
								<li>
									<a class="mono" href={tenantRunHref(app.job, r.id)} data-testid="run-link"><b>#{r.id}</b></a
									>
									<span class="chip {r.status === 'packaged' ? 'ok' : 'accent'}">{r.status}</span>
									{STEP_LABEL[r.current_step]} · {r.output.join(', ')}
									<div class="cell-sub">{costLine(r)}</div>
								</li>
							{:else}
								<li class="muted">No tenant runs for this job.</li>
							{/each}
						</ul>
					</Card>
				</aside>
			</div>
		</div>

		<Drawer bind:open={runOpen} title="Start a tenant generator run">
			<div class="stack">
				<Callout kind="info" title="Metered as your usage">
					The LLM drafts extension activities for {s.industry ? `${s.industry} / ` : ''}{s.job}. Output:
					refinements only (D8), saved to the draft like every other change. Its tokens are recorded as your
					usage (activity generator) on the same funding lanes as conversation turns and show on
					<a href="/admin/usage">Usage</a>.
				</Callout>
				<div class="small" data-testid="run-estimate" aria-live="polite">
					{#if est}
						Expected usage: <b>{tokens(est.tokens_low)}–{tokens(est.tokens_high)}</b> tokens · {FUNDING_LABEL[
							est.funding
						]} · usage of {est.tenant_name ?? 'your workspace'}
					{:else if estErr}<span class="muted">{estErr}</span>
					{:else}<span class="muted">Estimating…</span>{/if}
				</div>
				{#if noProvider}
					<Callout kind="gap" title="No generator model is bound">
						<a href={PROVIDERS_LINK}>Set up a generator model in Providers</a>, then start the run again.
					</Callout>
				{/if}
				<div class="field">
					<label for="tr-desc">What should it cover?</label>
					<textarea
						id="tr-desc"
						class="input"
						rows="3"
						placeholder="Invoice VAT number requests from business customers"
						bind:value={runDesc}></textarea>
				</div>
				<div class="row">
					<button
						class="btn primary"
						type="button"
						{...writeAttrs(session, 'job_designer')}
						onclick={() => startRun(s)}>Start run</button
					>
					<button class="btn" type="button" onclick={() => (runOpen = false)}>Cancel</button>
				</div>
			</div>
		</Drawer>
	{/snippet}
</LoadState>

{#snippet deleteBtn(i: DiscoveryImport)}
	{#if confirmDelete === i.id}
		<span class="row nowrap" data-testid="confirm-delete-import">
			<button class="btn sm danger" type="button" onclick={() => removeImport(i)}
				>{i.status === 'classifying' ? 'Cancel classification and delete' : 'Delete import'}</button
			>
			<button class="btn sm" type="button" onclick={() => (confirmDelete = '')}>Keep</button>
		</span>
	{:else}
		<button
			class="btn sm"
			type="button"
			aria-label="Delete import {i.file}"
			{...writeAttrs(session, 'job_designer')}
			onclick={() => (confirmDelete = i.id)}>Delete import</button
		>
	{/if}
{/snippet}

<J18ImportDrawer bind:open={uploadOpen} onuploaded={uploaded} />

<style>
	.wt-in {
		width: 5.5em;
	}
	.err-text {
		color: var(--err);
	}
	.nowrap {
		flex-wrap: nowrap;
	}
	.small {
		font-size: 12.5px;
	}
	.shadow-banner {
		padding: 10px 14px;
		border-radius: var(--r);
		background: color-mix(in srgb, var(--work) 14%, var(--panel));
		border: 1px solid color-mix(in srgb, var(--work) 40%, transparent);
		color: var(--fg);
	}
	.dots {
		letter-spacing: 1px;
		color: var(--accent);
		white-space: nowrap;
	}
	.plain {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 8px;
	}
	.env {
		width: auto;
	}
	.j18-main {
		display: grid;
		grid-template-columns: minmax(0, 1fr) 320px;
		gap: 16px;
		align-items: start;
	}
	.j18-kinds {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
	}
	.j18-cov {
		padding: 0 16px 12px;
		display: grid;
		gap: 6px;
	}
	.j18-covbar {
		display: flex;
		height: 10px;
		border-radius: 5px;
		overflow: hidden;
		background: var(--line-2);
	}
	.j18-covbar .c,
	.j18-covleg i.c {
		background: var(--ok);
	}
	.j18-covbar .u,
	.j18-covleg i.u {
		background: var(--work);
	}
	.j18-covbar .t,
	.j18-covleg i.t {
		background: var(--faint);
	}
	.j18-covleg {
		display: flex;
		flex-wrap: wrap;
		gap: 4px 16px;
	}
	.j18-covleg i {
		display: inline-block;
		width: 9px;
		height: 9px;
		border-radius: 2px;
		margin-right: 5px;
	}
	.j18-sources li.off {
		opacity: 0.6;
	}
	.j18-sources .split,
	.j18-bill .split {
		gap: 8px;
	}
	.j18-total {
		border-top: 1px solid var(--line-2);
		padding-top: 6px;
	}
	@media (max-width: 1100px) {
		.j18-main {
			grid-template-columns: 1fr;
		}
	}
</style>
