<script lang="ts">
	// J17 draft test (D54 TEST step): before a draft is published, every test scenario (an example customer phrasing
	// of a journey activity) runs against the DRAFT design on the Test environment and is compared with what
	// Production (the live release) answers. The grid is activities × phrasing styles; a regression passes live and
	// fails on the draft, and names the draft change at the design location the scenario depends on. A run is
	// recorded (env=test, draft rev) so the publish page can show the draft as tested.
	import { onDestroy, onMount } from 'svelte';
	import { api, app, guard } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { writeHint, canWrite } from '$lib/auth/roles';
	import type { DraftValidation, ScenarioResult, TestScenario } from '$lib/api/types/journey';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Kpi from '$lib/ui/Kpi.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import { hrefOf, pageById, routesOf } from '$lib/pages';
	import { journeyUi, when } from '$lib/journey/format';
	import {
		STYLES,
		TONE_LABEL,
		cellTone,
		matrixRows,
		publishVerdict,
		type CellTone,
	} from '$lib/journey/J17Validation';

	let { onchange }: { onchange?: () => void } = $props();

	const res = new Resource(() => api().draftValidation());
	onMount(() => res.refresh());
	const base = $derived(`/admin/jobs/${app.job}`);

	function pageLink(id: string): { title: string; href: string | null } {
		const p = pageById(id);
		return p ? { title: p.title, href: hrefOf(p, routesOf(p)[0]!, app.job) } : { title: id, href: null };
	}

	let running = $state(false);
	let timer: ReturnType<typeof setTimeout> | undefined;
	onDestroy(() => clearTimeout(timer));
	function run(n: number) {
		running = true;
		timer = setTimeout(async () => {
			await guard(
				() => api().runDraftValidation(),
				`Ran ${n} scenarios on the Test environment against the draft.`,
				'Not run',
			);
			running = false;
			await res.refresh();
			onchange?.();
		}, journeyUi.draftMs);
	}

	let open = $state(false);
	let pick = $state<{ s: TestScenario; r: ScenarioResult | undefined } | null>(null);
	function show(s: TestScenario, r: ScenarioResult | undefined) {
		pick = { s, r };
		open = true;
	}

	const LEGEND: CellTone[] = ['regression', 'fixed', 'known', 'pass'];
	const ICON: Record<CellTone, string> = { pass: '✓', fail: '✗', regression: '✗', fixed: '✓', known: '✗' };
	const resultMap = (v: DraftValidation) => new Map((v.runs[0]?.results ?? []).map((r) => [r.scenario, r]));
	const plural = (n: number, w: string) => `${n} ${w}${n === 1 ? '' : 's'}`;
</script>

<LoadState resource={res}>
	{#snippet children(v)}
		{@const run0 = v.runs[0]}
		{@const results = resultMap(v)}
		{@const verdict = publishVerdict(v)}
		{@const rows = matrixRows(v.scenarios)}

		<!-- Where this page sits in the D54 lifecycle -->
		<ol class="j17-flow" aria-label="Draft lifecycle">
			<li class="done">
				<span class="j17-step">1 · Draft</span>
				<span class="j17-det"
					>{plural(v.current.changes, 'unpublished change')} · rev {v.current.draft_rev}</span
				>
			</li>
			<li class="here" aria-current="step">
				<span class="j17-step">2 · Test</span>
				<span class="j17-det">this page · Test environment</span>
			</li>
			<li>
				<a class="j17-step" href="{base}/release">3 · Publish</a>
				<span class="j17-det">cut → approve → apply</span>
			</li>
			<li>
				<span class="j17-step">4 · Live</span>
				<span class="j17-det"
					>{v.current.base_release === '—'
						? 'no release applied yet'
						: `Production runs ${v.current.base_release}`}</span
				>
			</li>
		</ol>

		<!-- The decision: can this draft be published? -->
		<section class="j17-verdict tone-{verdict.tone}" data-testid="j17-verdict" aria-live="polite">
			<div class="j17-verdict-text">
				<h2>{verdict.title}</h2>
				<p>{verdict.body}</p>
				{#if v.state === 'stale' && v.changed_since.length}
					<ul class="j17-since">
						{#each v.changed_since as c (c)}<li>{c}</li>{/each}
					</ul>
				{/if}
				{#if run0}
					<p class="j17-meta">
						Last run {run0.id} · {when(run0.at)} by {run0.by_name} · env=test · draft rev {run0.draft_rev}
						<span class="mono">#{run0.draft_fp}</span> · compared with live {run0.base_release}
					</p>
				{/if}
			</div>
			<div class="j17-verdict-act">
				<button
					class="btn primary"
					type="button"
					disabled={running || !v.scenarios.length || !canWrite(session, 'job_designer')}
					title={canWrite(session, 'job_designer') ? undefined : writeHint('job_designer')}
					onclick={() => run(v.scenarios.length)}
				>
					{#if running}<span class="spinner" aria-hidden="true"></span> Running {v.scenarios.length}
						scenarios…{:else}Run on draft{/if}
				</button>
				<a class="btn" href="/chat">Try it in test chat</a>
				<a class="btn" href="{base}/release">Review &amp; publish</a>
			</div>
		</section>

		{#if !v.scenarios.length}
			<span class="sr-only" data-testid="no-scenarios">No test scenarios.</span>
		{:else}
			{#if run0}
				<div class="grid g4 j17-kpis">
					<Kpi
						label="Pass on draft"
						value="{run0.summary.pass}/{run0.summary.total}"
						note="live passes {run0.summary.live_pass}/{run0.summary.total}"
					/>
					<Kpi
						label="Regressions"
						value={String(run0.summary.regressions)}
						note="pass live, fail on the draft"
					/>
					<Kpi
						label="Fixed by draft"
						value={String(run0.summary.fixed)}
						note="fail live, pass on the draft"
					/>
					<Kpi
						label="Fail live too"
						value={String(run0.summary.fail - run0.summary.regressions)}
						note="not caused by the draft"
					/>
				</div>
			{/if}

			<Card
				cls="flush"
				title="Activities × customer phrasings"
				sub="Each cell is one example customer message, answered by the draft on the Test environment and compared with live. Open a cell for both replies."
			>
				<div class="j17-legend" aria-label="Legend">
					{#each LEGEND as t (t)}
						<span class="j17-key t-{t}"
							><span class="j17-dot" aria-hidden="true">{ICON[t]}</span>{TONE_LABEL[t]}</span
						>
					{/each}
				</div>
				<div class="tbl-wrap">
					<table class="tbl j17-matrix" data-testid="validation-matrix">
						<thead>
							<tr>
								<th scope="col">Journey activity</th>
								{#each STYLES as st (st.key)}
									<th scope="col" title={st.hint}>{st.label}</th>
								{/each}
							</tr>
						</thead>
						<tbody>
							{#each rows as row, i (row.activity)}
								{#if i === 0 || rows[i - 1]!.stage !== row.stage}
									<tr class="j17-stage"
										><th colspan={STYLES.length + 1} scope="rowgroup">{row.stage_label}</th></tr
									>
								{/if}
								<tr>
									<th scope="row" class="j17-act">
										<a href="{base}/journey/{encodeURIComponent(row.activity)}">{row.label}</a>
										<span class="cell-sub mono">{row.activity}</span>
									</th>
									{#each STYLES as st (st.key)}
										{@const s = row.cells[st.key]}
										{@const r = s ? results.get(s.id) : undefined}
										{@const tone = cellTone(r)}
										<td>
											{#if s}
												<button
													type="button"
													class="j17-cell t-{tone ?? 'none'}"
													data-testid="cell-{s.id}"
													onclick={() => show(s, r)}
													aria-label="{row.label}, {st.label}: {tone ? TONE_LABEL[tone] : 'not run'}"
												>
													<span class="j17-cell-top">
														<span class="j17-dot" aria-hidden="true">{tone ? ICON[tone] : '·'}</span>
														<span class="j17-cell-tag">{tone ? TONE_LABEL[tone] : 'not run'}</span>
													</span>
													<span class="j17-cell-style">{st.label}</span>
													<span class="j17-cell-q">“{s.phrasing}”</span>
													{#if r?.cause}<span class="j17-cell-cause">{r.cause.label}</span>{/if}
												</button>
											{:else}<span class="muted">—</span>{/if}
										</td>
									{/each}
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
			</Card>

			<div class="grid g2 j17-lower">
				<Card
					title="Draft changes under test"
					sub="What each unpublished change did to the scenarios in the last run."
				>
					{#if !v.impacts.length}
						<p class="muted">No unpublished changes: the draft equals live.</p>
					{:else}
						<ul class="j17-impacts" data-testid="j17-impacts">
							{#each v.impacts as c (c.change_id)}
								{@const p = pageLink(c.page)}
								<li>
									<div class="j17-imp-label">{c.label}</div>
									<div class="cell-sub">
										{c.by_name} · {when(c.at)} ·
										{#if p.href}<a href={p.href}>{p.title}</a>{:else}{p.title}{/if}
									</div>
									<div class="chips">
										{#if c.untested}<span class="chip warn">not tested yet</span>
										{:else if !c.covered}<span class="chip muted" title="Try it in the test chat"
												>no scenario covers it</span
											>
										{:else}
											{#if c.regressions}<span class="chip err">{plural(c.regressions, 'regression')}</span
												>{/if}
											{#if c.fixed}<span class="chip ok">{c.fixed} fixed</span>{/if}
											{#if !c.regressions && !c.fixed}<span class="chip muted"
													>{plural(c.covered, 'scenario')}, no change</span
												>{/if}
										{/if}
									</div>
								</li>
							{/each}
						</ul>
					{/if}
				</Card>
				<Card
					cls="flush"
					title="Test runs"
					sub="Each run is recorded with env=test and the draft revision it used."
				>
					{#if !v.runs.length}
						<div class="empty">Never run on this job.</div>
					{:else}
						<div class="tbl-wrap">
							<table class="tbl j17-runs" data-testid="j17-runs">
								<thead>
									<tr><th>Run</th><th>Draft</th><th>Pass</th><th>Regr.</th><th>By</th></tr>
								</thead>
								<tbody>
									{#each v.runs as r, i (r.id)}
										<tr>
											<td
												>{r.id}
												<div class="cell-sub">{when(r.at)}</div></td
											>
											<td
												>rev {r.draft_rev}
												{#if i === 0 && v.state === 'current'}<span class="chip ok">current</span>{/if}
												<div class="cell-sub">
													{plural(r.change_keys.length, 'change')} · #{r.draft_fp}
												</div></td
											>
											<td>{r.summary.pass}/{r.summary.total}</td>
											<td
												><span class="chip {r.summary.regressions ? 'err' : 'ok'}"
													>{r.summary.regressions}</span
												></td
											>
											<td>{r.by_name}</td>
										</tr>
									{/each}
								</tbody>
							</table>
						</div>
					{/if}
				</Card>
			</div>
		{/if}
	{/snippet}
</LoadState>

<Drawer bind:open title={pick ? pick.s.activity_label : 'Scenario'} wide>
	{#if pick}
		{@const s = pick.s}
		{@const r = pick.r}
		{@const tone = cellTone(r)}
		<div class="stack">
			<div>
				<div class="label">Customer says ({STYLES.find((x) => x.key === s.style)?.label})</div>
				<blockquote class="j17-quote">“{s.phrasing}”</blockquote>
			</div>
			<div>
				<div class="label">A correct reply</div>
				<p class="j17-p">{s.expected}</p>
			</div>
			{#if r}
				<div class="j17-cmp">
					<div class="j17-side t-{r.live}">
						<div class="label">Live · Production</div>
						<span class="chip {r.live === 'pass' ? 'ok' : 'err'}">{r.live}</span>
						<p class="j17-p">{r.reply_live}</p>
					</div>
					<div class="j17-side t-{r.draft}">
						<div class="label">Draft · Test environment</div>
						<span class="chip {r.draft === 'pass' ? 'ok' : 'err'}">{r.draft}</span>
						<p class="j17-p">{r.reply_draft}</p>
						{#if r.cited_draft.length}
							<div class="chips">
								{#each r.cited_draft as c (c)}<span class="chip ok mono">{c}</span>{/each}
							</div>
						{/if}
					</div>
				</div>
				{#if r.draft === 'fail' || r.live === 'fail'}
					<p class="j17-why"><b>Why it fails:</b> {s.fail_reason}</p>
				{/if}
				{#if r.cause}
					{@const p = pageLink(r.cause.page)}
					<div class="j17-cause tone-{tone === 'regression' ? 'err' : 'ok'}" data-testid="j17-cause">
						<div class="label">
							{tone === 'regression' ? 'Caused by draft change' : 'Fixed by draft change'}
						</div>
						<p class="j17-p">{r.cause.label}</p>
						<div class="row">
							{#if p.href}<a class="btn sm" href={p.href}>Open {p.title}</a>{/if}
							<a class="btn sm" href="{base}/release">Discard or publish on Release</a>
						</div>
					</div>
				{:else if tone === 'known'}
					<p class="muted">
						Fails on live too: not caused by the draft. Fix it in the knowledge store or rules.
					</p>
				{/if}
			{:else}
				<p class="muted">Not run yet. Run on draft to see the reply.</p>
			{/if}
			<div class="row">
				<a class="btn" href="/chat">Try this phrasing in test chat</a>
			</div>
		</div>
	{/if}
</Drawer>

<style>
	/* lifecycle strip */
	.j17-flow {
		list-style: none;
		margin: 0 0 12px;
		padding: 0;
		display: grid;
		grid-template-columns: repeat(4, minmax(0, 1fr));
		gap: 6px;
	}
	.j17-flow li {
		border: 1px solid var(--line);
		border-radius: var(--r);
		padding: 6px 10px;
		background: var(--panel);
		display: flex;
		flex-direction: column;
		min-width: 0;
	}
	.j17-flow li.done {
		border-color: color-mix(in srgb, var(--ok) 40%, var(--line));
	}
	.j17-flow li.here {
		border-color: var(--accent);
		background: color-mix(in srgb, var(--accent) 8%, var(--panel));
		box-shadow: inset 3px 0 0 var(--accent);
	}
	.j17-step {
		font-weight: 600;
		font-size: 13px;
	}
	.j17-det {
		font-size: 12px;
		color: var(--muted);
	}
	/* verdict */
	.j17-verdict {
		display: flex;
		flex-wrap: wrap;
		gap: 12px 24px;
		justify-content: space-between;
		align-items: flex-start;
		padding: 14px 16px;
		border: 1px solid var(--line);
		border-left: 4px solid var(--muted);
		border-radius: var(--r);
		background: var(--panel);
		margin-bottom: 12px;
	}
	.j17-verdict.tone-ok {
		border-left-color: var(--ok);
	}
	.j17-verdict.tone-warn {
		border-left-color: var(--warn);
	}
	.j17-verdict.tone-err {
		border-left-color: var(--err);
		background: color-mix(in srgb, var(--err) 5%, var(--panel));
	}
	.j17-verdict-text {
		flex: 1 1 360px;
		min-width: 0;
	}
	.j17-verdict h2 {
		margin: 0 0 4px;
		font-size: 18px;
	}
	.j17-verdict.tone-err h2 {
		color: var(--err);
	}
	.j17-verdict.tone-warn h2 {
		color: var(--warn);
	}
	.j17-verdict.tone-ok h2 {
		color: var(--ok);
	}
	.j17-verdict p {
		margin: 0;
	}
	.j17-meta {
		margin-top: 6px !important;
		font-size: 12px;
		color: var(--muted);
	}
	.j17-since {
		margin: 6px 0 0;
		padding-left: 18px;
		font-size: 13px;
	}
	.j17-verdict-act {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
	}
	.j17-kpis {
		margin-bottom: 12px;
	}
	/* matrix */
	.j17-legend {
		display: flex;
		flex-wrap: wrap;
		gap: 6px 14px;
		padding: 0 16px 10px;
		font-size: 12px;
		color: var(--muted);
	}
	.j17-key {
		display: inline-flex;
		gap: 5px;
		align-items: center;
	}
	.j17-matrix {
		table-layout: fixed;
		min-width: 720px;
	}
	.j17-matrix th:first-child {
		width: 200px;
	}
	.j17-matrix td,
	.j17-matrix th {
		vertical-align: top;
	}
	.j17-stage th {
		background: var(--sunk);
		font-size: 11px;
		text-transform: uppercase;
		letter-spacing: 0.05em;
		color: var(--muted);
		padding-top: 4px;
		padding-bottom: 4px;
	}
	.j17-matrix th.j17-act {
		font-weight: 500;
		text-align: left;
		text-transform: none;
		letter-spacing: normal;
		font-size: 13px;
		font-family: var(--sans);
		color: var(--fg);
	}
	.j17-act .cell-sub {
		display: block;
		font-weight: 400;
		font-size: 11px;
		white-space: normal;
		overflow-wrap: anywhere;
		word-break: break-all;
	}
	.j17-cell {
		all: unset;
		box-sizing: border-box;
		cursor: pointer;
		display: flex;
		flex-direction: column;
		gap: 3px;
		width: 100%;
		padding: 6px 8px;
		border-radius: var(--r);
		border: 1px solid var(--line-2);
		background: var(--panel);
		font-size: 12.5px;
		color: var(--fg);
	}
	.j17-cell:hover {
		border-color: var(--accent);
	}
	.j17-cell:focus-visible {
		outline: 2px solid var(--accent);
		outline-offset: 1px;
	}
	.j17-cell-top {
		display: flex;
		gap: 6px;
		align-items: center;
		font-weight: 600;
		font-size: 11.5px;
		text-transform: uppercase;
		letter-spacing: 0.03em;
	}
	.j17-cell-q {
		color: var(--muted);
		overflow: hidden;
		display: -webkit-box;
		-webkit-line-clamp: 2;
		line-clamp: 2;
		-webkit-box-orient: vertical;
	}
	.j17-cell-cause {
		font-size: 11.5px;
		font-weight: 500;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.j17-dot {
		display: inline-grid;
		place-items: center;
		width: 16px;
		height: 16px;
		border-radius: 50%;
		font-size: 11px;
		font-weight: 700;
		color: var(--panel);
		background: var(--muted);
		flex: none;
	}
	.t-pass .j17-dot {
		background: var(--ok);
	}
	.t-pass .j17-cell-top {
		color: var(--ok);
	}
	.t-fixed .j17-dot {
		background: var(--ok);
	}
	.j17-cell.t-fixed {
		border-color: color-mix(in srgb, var(--ok) 55%, var(--line));
		background: color-mix(in srgb, var(--ok) 8%, var(--panel));
	}
	.t-fixed .j17-cell-top,
	.t-fixed .j17-cell-cause {
		color: var(--ok);
	}
	.t-regression .j17-dot {
		background: var(--err);
	}
	.j17-cell.t-regression {
		border: 2px solid var(--err);
		background: color-mix(in srgb, var(--err) 9%, var(--panel));
	}
	.t-regression .j17-cell-top,
	.t-regression .j17-cell-cause {
		color: var(--err);
	}
	.t-known .j17-dot {
		background: var(--warn);
	}
	.t-known .j17-cell-top {
		color: var(--warn);
	}
	.j17-cell.t-known {
		border-style: dashed;
	}
	/* lower */
	.j17-lower {
		margin-top: 12px;
		align-items: start;
	}
	.j17-impacts {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 10px;
	}
	.j17-impacts li {
		padding-bottom: 10px;
		border-bottom: 1px solid var(--line-2);
	}
	.j17-impacts li:last-child {
		border-bottom: 0;
		padding-bottom: 0;
	}
	.j17-imp-label {
		font-weight: 500;
		font-size: 13px;
		overflow-wrap: anywhere;
	}
	.j17-impacts .chips {
		margin-top: 4px;
	}
	.j17-runs td {
		font-size: 13px;
		vertical-align: top;
	}
	/* drawer */
	.j17-quote {
		margin: 4px 0 0;
		padding: 8px 12px;
		border-left: 3px solid var(--accent);
		background: var(--sunk);
		border-radius: 0 var(--r) var(--r) 0;
	}
	.j17-p {
		margin: 4px 0 0;
	}
	.j17-cmp {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(220px, 1fr));
		gap: 10px;
	}
	.j17-side {
		border: 1px solid var(--line);
		border-radius: var(--r);
		padding: 10px;
	}
	.j17-side.t-fail {
		border-color: color-mix(in srgb, var(--err) 50%, var(--line));
	}
	.j17-side.t-pass {
		border-color: color-mix(in srgb, var(--ok) 50%, var(--line));
	}
	.j17-why {
		margin: 0;
		font-size: 13px;
	}
	.j17-cause {
		border-radius: var(--r);
		padding: 10px;
		border: 1px solid var(--line);
	}
	.j17-cause.tone-err {
		border-color: var(--err);
		background: color-mix(in srgb, var(--err) 6%, var(--panel));
	}
	.j17-cause.tone-ok {
		border-color: color-mix(in srgb, var(--ok) 50%, var(--line));
		background: color-mix(in srgb, var(--ok) 6%, var(--panel));
	}
	.j17-cause .row {
		margin-top: 8px;
	}
	.spinner {
		display: inline-block;
		width: 12px;
		height: 12px;
		border: 2px solid color-mix(in srgb, currentColor 30%, transparent);
		border-top-color: currentColor;
		border-radius: 50%;
		animation: j17spin 0.8s linear infinite;
		vertical-align: -2px;
	}
	@keyframes j17spin {
		to {
			transform: rotate(360deg);
		}
	}
	.j17-cell-style {
		display: none;
	}
	@media (max-width: 640px) {
		.j17-flow {
			grid-template-columns: repeat(2, minmax(0, 1fr));
		}
		/* phone: each activity becomes a block with its phrasings stacked */
		.j17-matrix {
			min-width: 0;
			table-layout: auto;
		}
		.j17-matrix,
		.j17-matrix tbody,
		.j17-matrix tr,
		.j17-matrix th,
		.j17-matrix td {
			display: block;
			width: auto !important;
		}
		.j17-matrix thead {
			display: none;
		}
		.j17-matrix td {
			border: 0;
			padding-top: 3px;
			padding-bottom: 3px;
		}
		.j17-matrix tr {
			border-bottom: 1px solid var(--line-2);
			padding-bottom: 6px;
		}
		.j17-cell-style {
			display: block;
			font-size: 11px;
			color: var(--faint);
		}
	}
</style>
