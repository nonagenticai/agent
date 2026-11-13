<script lang="ts">
	// J7 Gates: which gate questions this job asks at each point of a conversation turn (P1 message, P2 request,
	// R1 resolution), what each one did in the last 7 days, and what the tenant has refined. Questions and their
	// options come from the template layers (read-only here); the tenant relabels / hides options and tunes
	// thresholds. Every refinement is saved to the job's draft (D54): tried in the test chat, it reaches customers
	// when a release containing it is published (cut → approve → apply). Safety gates fail closed
	// to a person and cannot be hidden (D29). Refine in the drawer (GateDrawer). The playground runs a message through
	// one point (draft or live, D54); the verdict log lists the newest answers per case.
	import { onMount } from 'svelte';
	import { api, app, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { canWrite as canWriteFn, isSuperAdmin, writeHint } from '$lib/auth/roles';
	import { Resource } from '$lib/load.svelte';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Freshness from '$lib/ui/Freshness.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Lock from '$lib/ontology/Lock.svelte';
	import { fmt } from '$lib/format';
	import {
		SHIPS_TEXT,
		TURN,
		acted,
		actedText,
		changeText,
		draftToast,
		ruleText,
	} from '$lib/ontology/gatesJ7';
	import DraftBar from '$lib/release/DraftBar.svelte';
	import type { EffGateQuestion, GateChange, GatePoint, JobGates } from '$lib/api/types/ontology';
	import GateDrawer from './GateDrawer.svelte';
	import J7TurnFlow from './J7TurnFlow.svelte';
	import GatePlayground from './GatePlayground.svelte';
	import GateVerdictLog from './GateVerdictLog.svelte';
	import GateAddQuestion from './GateAddQuestion.svelte';
	import { RUN_POINT_LABEL, RUN_POINT, ruleSummary } from '$lib/ontology/gateRules';

	const res = new Resource(
		() => api().jobGates(),
		(d: JobGates) => d.questions.length === 0,
	);
	onMount(() => res.refresh());
	const canWrite = $derived(canWriteFn(session, 'job_designer'));
	const sa = $derived(isSuperAdmin(session));

	let point = $state<GatePoint | 'all'>('all');
	let open = $state(false);
	let selKey = $state<string | null>(null);
	const data = $derived(res.value.state === 'ok' ? res.value.data : null);
	const sel = $derived(data?.questions.find((q) => q.key === selKey) ?? null);
	const selChanges = $derived((data?.changes ?? []).filter((c) => c.gate === selKey));
	function show(key: string) {
		selKey = key;
		open = true;
	}
	// F-17 (B5 T8a): a question on / off for this job, saved to draft; safety questions are always on.
	let busyKey = $state<string | null>(null);
	async function setEnabled(key: string, enabled: boolean) {
		busyKey = key;
		await guard(
			() => api().setGateEnabled(key, enabled),
			draftToast(enabled ? `${key} is asked again in this job.` : `${key} is turned off for this job.`),
		);
		busyKey = null;
		await res.refresh();
	}

	// Review marker (gates_unreviewed): the tenant read the template's questions and accepts them as they are. A
	// draft write at a reserved setting key; it changes no question. Idempotent, so it is offered at any time.
	let reviewing = $state(false);
	async function markReviewed() {
		reviewing = true;
		await guard(
			() => api().reviewGates(),
			draftToast('Gate questions marked as reviewed; no question changed.'),
		);
		reviewing = false;
		await res.refresh();
	}

	const SHIP_VARIANT: Record<GateChange['ships'], 'ok' | 'accent' | 'warn'> = {
		live: 'ok',
		approved: 'accent',
		next_cut: 'warn',
	};
	const count = (d: JobGates, s: GateChange['ships']) =>
		(d.changes ?? []).filter((c) => c.ships === s).length;

	/** Groups of one gate point: safety (fail closed) first, then the fail-open questions. */
	function groups(d: JobGates, p: GatePoint) {
		const qs = d.questions.filter((q) => q.point === p);
		const safety = qs.filter((q) => q.safety);
		const rest = qs.filter((q) => !q.safety);
		const restLabel =
			p === 'R1'
				? 'Quality checks · fail open'
				: p === 'P2'
					? 'Request checks · fail open'
					: 'Routing and signals · fail open';
		const restSub =
			p === 'R1'
				? 'When laya is down the draft is sent unchecked by these.'
				: 'When laya is down the turn continues without the verdict.';
		return [
			{
				key: 'safety',
				label: 'Safety · fail closed',
				sub: 'Any “true” stops the bot: a person takes over, or the text is redacted. Tenants cannot hide these.',
				rows: safety,
			},
			{ key: 'rest', label: restLabel, sub: restSub, rows: rest },
		].filter((g) => g.rows.length);
	}
	const top = (q: EffGateQuestion) => {
		const visible = q.options.filter((o) => !o.hidden && o.verdicts > 0);
		const total = visible.reduce((a, o) => a + o.verdicts, 0);
		return [...visible]
			.sort((a, b) => b.verdicts - a.verdicts)
			.slice(0, 3)
			.map((o) => ({ label: o.label, pct: total ? o.verdicts / total : 0 }));
	};
	const changesOf = (d: JobGates, key: string) => (d.changes ?? []).filter((c) => c.gate === key);
	const p95 = (d: JobGates) => Math.max(0, ...d.questions.map((q) => q.stats.latency_p95_ms ?? 0)) || null;
	const timeouts = (d: JobGates) => d.questions.reduce((a, q) => a + q.stats.timeouts, 0);
</script>

<PageHead
	id="J7"
	sub="The questions this job asks about every message, request and draft reply. Templates set the questions; you tune labels, options and thresholds for your customers. Your changes are saved to draft: try them in the test chat, then publish them with a release."
>
	{#snippet actions()}
		<Freshness resource={res} />
		<button
			type="button"
			class="btn"
			data-testid="mark-gates-reviewed"
			disabled={!canWrite || reviewing}
			title={canWrite
				? 'You have read these questions and accept them as they are. Saved to draft; changes nothing.'
				: writeHint('job_designer')}
			onclick={markReviewed}>Mark gates reviewed</button
		>
		<a class="btn" href="/admin/jobs/{app.job}/cases">Verdicts per case</a>
	{/snippet}
</PageHead>

<DraftBar page="J7" tick={res.value} />

<LoadState resource={res} empty="This job has no gates yet. They come with the job template.">
	{#snippet children(d)}
		<GatePlayground tick={res.value} />
		<J7TurnFlow questions={d.questions} {point} onpick={(p) => (point = p)} />
		<p class="note jg-window">
			{#if d.window_from}
				Last 7 days, since {d.window_from.slice(0, 10)} · laya p95 {fmt.ms(p95(d))} · {fmt.n(timeouts(d))} timeouts.
				“Unsure” = below the threshold; “outages” = laya did not answer and the fail policy applied.
			{:else}
				<strong>No verdicts yet.</strong> This job is not on the bus, so no message has been gated. Counts and latency
				appear with the first traffic.
			{/if}
			{#if point !== 'all'}
				<button type="button" class="linkish" onclick={() => (point = 'all')}>Show all gate points</button>
			{/if}
		</p>

		<div class="jg-cols">
			<div class="jg-main">
				{#each TURN.filter((s) => point === 'all' || s.point === point) as s (s.point)}
					{@const gs = groups(d, s.point)}
					{#if gs.length}
						<section class="jg-point" aria-label="{s.point} {s.label}">
							<header class="jg-point-h">
								<span class="mono jg-pt">{s.point}</span>
								<h2>{s.label}</h2>
								<span class="muted">{s.decides}</span>
							</header>
							{#each gs as g (g.key)}
								<div class="jg-group" class:safety={g.key === 'safety'}>
									<div class="jg-group-h">
										{#if g.key === 'safety'}<Lock />{/if}
										<b>{g.label}</b> <span class="muted">{g.sub}</span>
									</div>
									<ul class="jg-list">
										{#each g.rows as q (q.key)}
											{@const ch = changesOf(d, q.key)}
											<li>
												<button
													type="button"
													class="jg-q"
													aria-label="Open {q.key}: {q.question}"
													onclick={() => show(q.key)}
												>
													<span class="jg-key mono"
														>{q.key}{#if q.safety}<Lock />{/if}</span
													>
													<span class="jg-body">
														<span class="jg-question">{q.question}</span>
														<span class="jg-rule">
															<span class="mono">{ruleText(q)}</span> → {q.effect}
														</span>
														<span class="jg-rule mono"
															>{RUN_POINT_LABEL[RUN_POINT[q.point]].toLowerCase()} · {ruleSummary(
																q.type,
																q.rule,
															)} → {q.action} · fail {q.fail}</span
														>
														<span class="jg-chips">
															{#if q.origin === 'tenant'}
																<Chip
																	text="added by your team"
																	variant="accent"
																	title="Your own question; it can be deleted from its drawer"
																/>
															{:else}
																<Chip
																	text="from {q.layer}"
																	title="Inherited from the {q.layer} template layer; the question itself is read-only here"
																/>
															{/if}
															{#if q.rule_refined}<Chip text="rule refined" variant="accent" />{/if}
															{#if q.generated_from}<Chip
																	text="options generated at cut"
																	variant="tool"
																	title="Built from {q.generated_from}"
																/>{/if}
															{#each ch as c, i (i)}
																<Chip
																	text="{changeText(c)} · {SHIPS_TEXT[c.ships]}"
																	variant={SHIP_VARIANT[c.ships]}
																	title="{c.by_name}, {c.at.slice(0, 16).replace('T', ' ')}"
																/>
															{/each}
														</span>
													</span>
													<span class="jg-week">
														{#each top(q) as t (t.label)}
															<span class="jg-bar"
																><span class="jg-bar-l">{t.label}</span><span class="meter"
																	><i class="s-cust" style="width:{t.pct * 100}%"></i></span
																><span class="mono">{Math.round(t.pct * 100)}%</span></span
															>
														{:else}
															<span class="muted"
																>{q.generated_from?.startsWith('run time') && q.stats.verdicts_7d
																	? 'options built per case'
																	: ''}</span
															>
														{/each}
														<span class="jg-acted">
															<span class:hot={q.safety && (acted(q) ?? 0) > 0}>{actedText(q)}</span>
															{#if q.stats.fail_open || q.stats.fail_closed}
																<span class="warm"
																	>{q.stats.fail_open + q.stats.fail_closed} outage{q.stats.fail_open +
																		q.stats.fail_closed ===
																	1
																		? ''
																		: 's'}</span
																>
															{/if}
														</span>
													</span>
												</button>
												<label
													class="jg-toggle"
													title={q.safety
														? 'A safety question is always asked (D29).'
														: canWrite
															? 'Ask this question in this job. Saved to draft.'
															: writeHint('job_designer')}
												>
													<input
														type="checkbox"
														checked={q.enabled}
														disabled={q.safety || !canWrite || busyKey === q.key}
														onchange={(e) => setEnabled(q.key, e.currentTarget.checked)}
													/>
													{q.enabled ? 'Asked' : 'Off'}
													{#if !q.enabled}<Chip text="off · draft" variant="warn" />{/if}
												</label>
											</li>
										{/each}
									</ul>
								</div>
							{/each}
						</section>
					{/if}
				{/each}
			</div>

			<aside class="jg-side card" aria-label="Your refinements">
				<h2>Your refinements</h2>
				<p class="muted jg-side-sub">
					Saved here, a change goes to the draft. Try it in the test chat; customers get it when a release
					that contains it is published (cut, approved, applied).
				</p>
				<ol class="jg-ship">
					<li class:has={count(d, 'next_cut') > 0}>
						<b class="mono">{count(d, 'next_cut')}</b>
						<span>Draft, not published</span>
						<small class="muted">in the test chat now</small>
					</li>
					<li class:has={count(d, 'approved') > 0}>
						<b class="mono">{count(d, 'approved')}</b>
						<span>In approved release</span>
						<small class="muted mono">{d.releases?.approved?.id ?? 'none'}</small>
					</li>
					<li class:has={count(d, 'live') > 0}>
						<b class="mono">{count(d, 'live')}</b>
						<span>Live</span>
						<small class="muted mono">{d.releases?.live?.id ?? 'no release yet'}</small>
					</li>
				</ol>
				{#if (d.changes ?? []).length}
					<ul class="jg-changes">
						{#each d.changes ?? [] as c, i (i)}
							<li>
								<button type="button" class="jg-change" onclick={() => show(c.gate)}>
									<span class="mono jg-ckey">{c.gate}</span>
									<span class="jg-ctext">{changeText(c)}</span>
									<Chip text={SHIPS_TEXT[c.ships]} variant={SHIP_VARIANT[c.ships]} />
									<small class="muted">{c.by_name} · {c.at.slice(0, 10)}</small>
								</button>
							</li>
						{/each}
					</ul>
				{:else}
					<p class="muted">
						No refinements yet: this job asks the questions exactly as the templates wrote them.
					</p>
				{/if}
				<div class="jg-side-links">
					<a href="/chat">Test in test chat</a>
					<a href="/admin/jobs/{app.job}/release">Review &amp; publish</a>
					<a href="/admin/jobs/{app.job}/refinements">All refinements of this job</a>
					{#if sa}<a href="/platform/gates">Edit the template questions (Gate library)</a>{/if}
				</div>
				{#if !canWrite}<p class="note">
						{writeHint('job_designer')} to relabel options or tune thresholds.
					</p>{/if}
				{#if !sa}
					<p class="note">
						Template questions and options come with the templates; you can relabel, hide and tune them, and
						add yes/no questions of your own.
					</p>
				{/if}
				<details class="jg-add">
					<summary>Add a question of your own</summary>
					<GateAddQuestion
						{canWrite}
						onadded={async (k) => {
							await res.refresh();
							show(k);
						}}
					/>
				</details>
			</aside>
		</div>
		<GateVerdictLog />
	{/snippet}
</LoadState>

<GateDrawer
	bind:open
	gate={sel}
	changes={selChanges}
	{canWrite}
	live={data?.releases?.live?.id ?? null}
	onchanged={() => res.refresh()}
/>

<style>
	.jg-window {
		display: flex;
		flex-wrap: wrap;
		gap: 4px 12px;
		align-items: baseline;
		margin: 6px 0 14px;
	}
	.linkish {
		background: none;
		border: 0;
		padding: 0;
		color: var(--accent);
		cursor: pointer;
		font: inherit;
		text-decoration: underline;
	}
	.jg-cols {
		display: grid;
		grid-template-columns: minmax(0, 1fr) 320px;
		gap: 16px;
		align-items: start;
	}
	.jg-point {
		margin-bottom: 18px;
	}
	.jg-point-h {
		display: flex;
		flex-wrap: wrap;
		align-items: baseline;
		gap: 4px 10px;
		margin-bottom: 8px;
	}
	.jg-point-h h2 {
		margin: 0;
		font: 600 16px/1.3 var(--cond);
	}
	.jg-pt {
		color: var(--tool);
		font-weight: 600;
	}
	.jg-group {
		border: 1px solid var(--line);
		border-radius: var(--r);
		background: var(--panel);
		margin-bottom: 10px;
		overflow: hidden;
	}
	.jg-group.safety {
		border-color: color-mix(in srgb, var(--err) 40%, var(--line));
	}
	.jg-group-h {
		padding: 8px 12px;
		font-size: 12.5px;
		background: var(--sunk);
		border-bottom: 1px solid var(--line-2);
		display: flex;
		flex-wrap: wrap;
		gap: 2px 8px;
		align-items: baseline;
	}
	.jg-group.safety .jg-group-h {
		background: color-mix(in srgb, var(--err) 7%, var(--panel));
		color: var(--err);
	}
	.jg-group.safety .jg-group-h .muted {
		color: var(--muted);
	}
	.jg-list {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.jg-toggle {
		display: flex;
		align-items: center;
		gap: 6px;
		padding: 0 12px 8px;
		font-size: 12px;
		color: var(--muted);
	}
	.jg-list li + li {
		border-top: 1px solid var(--line-2);
	}
	.jg-q {
		display: grid;
		grid-template-columns: 44px minmax(0, 1fr) 240px;
		gap: 12px;
		width: 100%;
		padding: 10px 12px;
		background: none;
		border: 0;
		text-align: left;
		color: var(--fg);
		font: inherit;
		cursor: pointer;
	}
	.jg-q:hover,
	.jg-q:focus-visible {
		background: color-mix(in srgb, var(--accent) 5%, var(--panel));
	}
	.jg-key {
		font-weight: 600;
		display: flex;
		gap: 3px;
		align-items: baseline;
		color: var(--tool);
	}
	.jg-body {
		display: flex;
		flex-direction: column;
		gap: 3px;
		min-width: 0;
	}
	.jg-question {
		font-weight: 500;
	}
	.jg-rule {
		font-size: 12.5px;
		color: var(--muted);
	}
	.jg-rule .mono {
		color: var(--fg);
	}
	.jg-chips {
		display: flex;
		flex-wrap: wrap;
		gap: 4px;
		margin-top: 2px;
	}
	.jg-week {
		display: flex;
		flex-direction: column;
		gap: 2px;
		font-size: 11.5px;
	}
	.jg-bar {
		display: grid;
		grid-template-columns: minmax(0, 1fr) 64px 32px;
		gap: 6px;
		align-items: center;
	}
	.jg-bar-l {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.jg-bar .meter {
		width: 64px;
	}
	.jg-bar .mono {
		text-align: right;
	}
	.jg-acted {
		display: flex;
		gap: 8px;
		margin-top: 2px;
		color: var(--muted);
	}
	.hot {
		color: var(--err);
		font-weight: 600;
	}
	.warm {
		color: var(--warn);
		font-weight: 600;
	}
	.jg-side {
		position: sticky;
		top: 12px;
		padding: 14px;
	}
	.jg-side h2 {
		margin: 0 0 4px;
		font: 600 15px/1.3 var(--cond);
	}
	.jg-side-sub {
		font-size: 12.5px;
		margin: 0 0 10px;
	}
	.jg-ship {
		list-style: none;
		margin: 0 0 12px;
		padding: 0;
		display: grid;
		gap: 0;
	}
	.jg-ship li {
		display: grid;
		grid-template-columns: 28px minmax(0, 1fr) auto;
		gap: 8px;
		align-items: baseline;
		padding: 5px 8px;
		border-left: 2px solid var(--line);
		font-size: 12.5px;
	}
	.jg-ship li.has {
		border-left-color: var(--accent);
	}
	.jg-ship b {
		font-size: 15px;
	}
	.jg-changes {
		list-style: none;
		margin: 0 0 10px;
		padding: 0;
	}
	.jg-change {
		display: grid;
		grid-template-columns: 30px minmax(0, 1fr) auto;
		gap: 2px 8px;
		width: 100%;
		padding: 7px 4px;
		background: none;
		border: 0;
		border-top: 1px solid var(--line-2);
		text-align: left;
		color: var(--fg);
		font: inherit;
		font-size: 12.5px;
		cursor: pointer;
	}
	.jg-change:hover {
		background: var(--sunk);
	}
	.jg-ckey {
		font-weight: 600;
		color: var(--tool);
	}
	.jg-change small {
		grid-column: 2 / -1;
	}
	.jg-add {
		margin-top: 10px;
		font-size: 13px;
	}
	.jg-add summary {
		cursor: pointer;
		color: var(--accent);
		margin-bottom: 8px;
	}
	.jg-side-links {
		display: flex;
		flex-direction: column;
		gap: 4px;
		font-size: 13px;
		margin-bottom: 6px;
	}
	@media (max-width: 1180px) {
		.jg-cols {
			grid-template-columns: minmax(0, 1fr);
		}
		.jg-side {
			position: static;
			order: -1;
		}
	}
	@media (max-width: 720px) {
		.jg-q {
			grid-template-columns: 36px minmax(0, 1fr);
		}
		.jg-week {
			grid-column: 2;
		}
	}
</style>
