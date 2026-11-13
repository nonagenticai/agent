<script lang="ts">
	// J22 Operation calls — every operation the agent called on a system (durable operation_call log, 12 §3.7, §9).
	// Leads with the decisions: unknown outcomes a person must check (never auto-retried, 12 §3.3 step 10), then
	// failure patterns by op × system (worst first), then the log. Each call ends one of five ways: ok, denied by the
	// runner, a user outcome the agent answers, a system error it escalates, or unknown (doc 10 §3.1).
	import { onMount } from 'svelte';
	import { page } from '$app/state';
	import { api, app } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import { setQuery } from '$lib/query';
	import { jobPath } from '$lib/scope';
	import { fmt } from '$lib/format';
	import type { CallHandling, Env, OperationCall, RouteKind } from '$lib/api/types/runtime';
	import { CALL_STATE_LABEL, CALL_STATE_WORD, ROUTE_LABEL, ROUTE_VARIANT, when } from '$lib/runtime/labels';
	import {
		ENDING,
		ENDINGS,
		IDEMPOTENCY_LABEL,
		SIDE_EFFECT,
		endingOf,
		failurePatterns,
		mixOf,
		statKey,
		systemHref,
		type Ending,
	} from '$lib/runtime/J22-calls';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Freshness from '$lib/ui/Freshness.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Status from '$lib/ui/Status.svelte';
	import Pagination from '$lib/ui/Pagination.svelte';
	import J22Patterns from './J22Patterns.svelte';
	import J22Unknowns from './J22Unknowns.svelte';

	const q = (k: string) => page.url.searchParams.get(k) ?? '';
	const op = $derived(q('op'));
	const route = $derived(q('route') as RouteKind | '');
	const outcome = $derived(q('outcome'));
	const handling = $derived(q('handling') as CallHandling | '');
	const env = $derived(q('env') as Env | '');
	const instance = $derived(q('instance'));
	const caseId = $derived(q('case_id'));
	const before = $derived(q('before'));
	const filtered = $derived(!!(op || route || outcome || handling || env || instance || caseId));

	const stats = new Resource(() => api().operationStats());
	const unknowns = new Resource(() => api().unknownOutcomes());
	const log = new Resource(() =>
		api().operationCalls({
			op,
			route,
			outcome,
			handling,
			env,
			instance: instance || undefined,
			case_id: caseId || undefined,
			before: before || null,
		}),
	);
	onMount(() => {
		void stats.refresh();
		void unknowns.refresh();
	});
	$effect(() => {
		void [op, route, outcome, handling, env, instance, caseId, before];
		log.value = { state: 'loading' };
		void log.refresh();
	});

	const ops = $derived(
		stats.value.state === 'ok'
			? [...new Set(stats.value.data.items.map((s) => s.op.split('{')[0]!))].sort()
			: [],
	);
	const systems = $derived(
		stats.value.state === 'ok'
			? [...new Set(stats.value.data.items.flatMap((s) => (s.instance ? [s.instance] : [])))].sort()
			: [],
	);
	const ROUTES: RouteKind[] = ['instance', 'builtin', 'fallback', 'deep_link', 'not_offered'];
	// One "Outcome" select drives either `outcome` (state/code) or `handling` (answered/escalated).
	const OUTCOMES: [string, string][] = [
		['ok', 'OK'],
		['denied', 'Denied'],
		['h:reply', 'Failed · answered to the customer'],
		['h:escalate', 'Failed · escalated to staff'],
		['unknown_outcome', 'Unknown outcome'],
		['error', 'Any failure'],
		['rate_limited', 'rate_limited'],
		['case_cap_reached', 'case_cap_reached'],
		['invalid_state:in_transit', 'invalid_state:in_transit'],
		['not_entitled', 'not_entitled'],
		['upstream_unavailable', 'upstream_unavailable'],
	];
	const outcomeSel = $derived(handling ? `h:${handling}` : outcome);
	function pickOutcome(v: string) {
		if (v.startsWith('h:')) setQuery({ handling: v.slice(2), outcome: '' });
		else setQuery({ outcome: v, handling: '' });
	}
	/** Tile click → the matching log filter. */
	function filterEnding(e: Ending) {
		const f: Record<Ending, Record<string, string>> = {
			ok: { outcome: 'ok', handling: '' },
			denied: { outcome: 'denied', handling: '' },
			replied: { outcome: '', handling: 'reply' },
			escalated: { outcome: '', handling: 'escalate' },
			unknown: { outcome: 'unknown_outcome', handling: '' },
		};
		void setQuery(f[e]).then(() => document.getElementById('j22-log')?.scrollIntoView({ block: 'start' }));
	}
	const clearAll = () =>
		setQuery({ op: '', route: '', outcome: '', handling: '', env: '', instance: '', case_id: '' });

	const both = {
		get refreshing() {
			return stats.refreshing || log.refreshing || unknowns.refreshing;
		},
		get updatedAt() {
			return log.updatedAt;
		},
		refresh: async () => {
			await Promise.all([stats.refresh(), log.refresh(), unknowns.refresh()]);
		},
	};

	const endingCell = (c: OperationCall) => endingOf(c);
	const FOR_CUSTOMER: Partial<Record<Ending, string>> = {
		replied: 'answered to the customer',
		escalated: 'escalated to staff',
		unknown: 'a person checks',
		denied: 'refused before sending',
	};
</script>

<PageHead
	id="J22"
	sub="Every operation the agent called on your systems, and how each call ended for the customer. Start with what needs a person, then the operations that fail most."
>
	{#snippet actions()}
		<Freshness resource={both} />
	{/snippet}
</PageHead>

<LoadState resource={stats} empty="No operations have been called for this job yet.">
	{#snippet children(s)}
		{@const mix = s.items.reduce(
			(a, x) => {
				const m = mixOf(x);
				for (const k of ENDINGS) a[k] += m[k];
				return a;
			},
			{ ok: 0, denied: 0, replied: 0, escalated: 0, unknown: 0 } as Record,
		)}
		{@const calls = s.items.reduce((a, x) => a + x.calls, 0)}
		{@const pat = failurePatterns(s.items)}

		<section class="j22-ends" aria-label="How calls ended, last 30 days">
			<div class="j22-ends-head">
				<h2>How {fmt.n(calls)} calls ended · 30 days</h2>
				<span class="muted">Click one to filter the log.</span>
			</div>
			<div class="j22-ends-bar" aria-hidden="true">
				{#each ENDINGS as k (k)}
					{#if mix[k]}<span class="b {ENDING[k].cls}" style="flex-grow:{mix[k]}"></span>{/if}
				{/each}
			</div>
			<div class="j22-ends-tiles">
				{#each ENDINGS as k (k)}
					<button
						type="button"
						class="j22-tile {ENDING[k].cls}"
						data-testid="ending-{k}"
						onclick={() => filterEnding(k)}
					>
						<span class="t-label">{ENDING[k].label}</span>
						<span class="t-val">{fmt.n(mix[k])}</span>
						<span class="t-pct">{fmt.pct(calls ? mix[k] / calls : null)}</span>
						<span class="t-hint">{ENDING[k].hint}</span>
					</button>
				{/each}
			</div>
		</section>

		<LoadState resource={unknowns}>
			{#snippet empty()}
				<J22Unknowns items={[]} checked30d={mix.unknown} onchange={() => void both.refresh()} />
			{/snippet}
			{#snippet children(u)}
				<J22Unknowns
					items={u.items}
					checked30d={Math.max(0, mix.unknown - u.items.filter((x) => !x.call.check).length)}
					onchange={() => void both.refresh()}
				/>
			{/snippet}
		</LoadState>

		<Card
			title="Failure patterns · 30 days"
			sub="Operation × system, worst failure rate first. Blue = the agent answered the customer; amber = escalated to staff; red = nobody knows yet."
		>
			{#snippet actions()}
				<a class="btn sm" href={jobPath(app.job, '/operations')}>Operations & routes</a>
				<a class="btn sm" href="/admin/connections/systems">Systems</a>
			{/snippet}
			<J22Patterns patterns={pat.patterns} />
			<details class="j22-all">
				<summary>
					All {s.items.length} operations · {pat.healthy.length} under 2 % failing
				</summary>
				<div class="tbl-wrap">
					<table class="tbl dense clickable" data-testid="op-stats">
						<thead>
							<tr
								><th>Operation</th><th>Route → system</th><th class="r">Calls</th><th class="r">Failing</th
								><th class="r">Denied</th><th class="r">p95</th></tr
							>
						</thead>
						<tbody>
							{#each s.items as r (statKey(r))}
								{@const rate = r.calls ? r.errors / r.calls : null}
								<tr onclick={() => setQuery({ op: r.op })}>
									<td class="mono"
										>{r.op}
										{#if r.side_effect}<span class="cell-sub" title={SIDE_EFFECT[r.side_effect].hint}
												>{SIDE_EFFECT[r.side_effect].hint}</span
											>{/if}</td
									>
									<td
										><Chip text={ROUTE_LABEL[r.route]} variant={ROUTE_VARIANT[r.route]} />
										{#if r.instance}<span class="mono muted">{r.instance}</span>{/if}</td
									>
									<td class="r">{fmt.n(r.calls)}</td>
									<td class="r" class:bad={rate != null && rate >= 0.1}>{fmt.pct(rate)}</td>
									<td class="r">{fmt.n(r.denied)}</td>
									<td class="r">{fmt.ms(r.p95_ms)}</td>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
			</details>
		</Card>
	{/snippet}
</LoadState>

<div id="j22-log"></div>
<Card
	title="Call log"
	sub="Newest first. Test calls ran the draft in the Test environment; production calls ran the live release."
>
	{#snippet actions()}
		<label class="field">
			<span class="label">Operation</span>
			<select class="input" value={op} onchange={(e) => setQuery({ op: e.currentTarget.value })}>
				<option value="">All operations</option>
				{#if op && !ops.includes(op)}<option value={op}>{op}</option>{/if}
				{#each ops as o (o)}<option value={o}>{o}</option>{/each}
			</select>
		</label>
		<label class="field">
			<span class="label">System</span>
			<select class="input" value={instance} onchange={(e) => setQuery({ instance: e.currentTarget.value })}>
				<option value="">All systems</option>
				{#each systems as o (o)}<option value={o}>{o}</option>{/each}
			</select>
		</label>
		<label class="field">
			<span class="label">Route</span>
			<select class="input" value={route} onchange={(e) => setQuery({ route: e.currentTarget.value })}>
				<option value="">All routes</option>
				{#each ROUTES as r (r)}<option value={r}>{ROUTE_LABEL[r]}</option>{/each}
			</select>
		</label>
		<label class="field">
			<span class="label">Env</span>
			<select class="input" value={env} onchange={(e) => setQuery({ env: e.currentTarget.value })}>
				<option value="">Prod + test</option>
				<option value="prod">Production (live)</option>
				<option value="test">Test (draft)</option>
			</select>
		</label>
		<label class="field">
			<span class="label">Outcome</span>
			<select class="input" value={outcomeSel} onchange={(e) => pickOutcome(e.currentTarget.value)}>
				<option value="">All outcomes</option>
				{#each OUTCOMES as [v, l] (v)}<option value={v}>{l}</option>{/each}
			</select>
		</label>
	{/snippet}
	{#if caseId || filtered}
		<p class="note">
			{#if caseId}Only case <span class="mono">{caseId}</span>.{/if}
			<button class="btn sm" type="button" onclick={clearAll}>Clear filters</button>
		</p>
	{/if}
	<LoadState resource={log}>
		{#snippet empty()}
			{#if filtered}
				No calls match these filters.
				<button class="btn sm" type="button" onclick={clearAll}>Clear filters</button>
			{:else}
				No operation calls yet.
			{/if}
		{/snippet}
		{#snippet children(p)}
			<div class="tbl-wrap">
				<table class="tbl dense" data-testid="op-log">
					<thead>
						<tr
							><th>When · case</th><th>Operation</th><th>System</th><th>Outcome</th><th>For the customer</th
							><th>Safety</th><th class="r">Latency</th></tr
						>
					</thead>
					<tbody>
						{#each p.items as c (c.call_id)}
							{@const e = endingCell(c)}
							{@const href = systemHref(c.instance)}
							<tr class="j22-row {e === 'pending' ? '' : ENDING[e].cls}">
								<td class="nowrap"
									>{when(c.at)}
									<span class="cell-sub"
										><a class="mono" href={jobPath(app.job, `/cases/${encodeURIComponent(c.case_id)}`)}
											>{c.case_id}</a
										>
										{#if c.env === 'test'}<Chip
												text="test · draft"
												variant="accent"
												title="Ran the draft in the Test environment"
											/>{:else}<span class="muted">prod</span>{/if}</span
									></td
								>
								<td class="mono"
									>{c.op}{#if c.attempt > 1}<span class="cell-sub">attempt {c.attempt}</span>{/if}</td
								>
								<td>
									{#if c.route !== 'instance'}<Chip
											text={ROUTE_LABEL[c.route]}
											variant={ROUTE_VARIANT[c.route]}
										/>{/if}
									{#if href}<a class="mono" {href}>{c.instance}</a
										>{:else if c.instance && c.instance !== 'builtin'}<span class="mono">{c.instance}</span
										>{/if}
									{#if c.template && c.template !== 'deep_link'}<span class="cell-sub mono">{c.template}</span
										>{/if}
								</td>
								<td
									><Status word={CALL_STATE_WORD[c.state]} label={CALL_STATE_LABEL[c.state]} />
									{#if c.outcome !== 'ok' && c.outcome !== c.state}<span class="cell-sub mono"
											>{c.outcome}</span
										>{/if}</td
								>
								<td class="j22-for">
									{#if e !== 'pending' && e !== 'ok'}{FOR_CUSTOMER[e]}{#if c.check}<span class="cell-sub"
												>checked: {c.check.verdict === 'landed' ? 'went through' : 'did not go through'}</span
											>{/if}{:else}<span class="muted">—</span>{/if}
								</td>
								<td class="j22-safe">
									{#if c.side_effect}<Chip
											text={c.side_effect}
											variant={c.side_effect === 'R' ? 'muted' : 'tool'}
											title={SIDE_EFFECT[c.side_effect].hint}
										/>{/if}
									{#if c.idempotency && c.side_effect !== 'R'}<span class="cell-sub"
											>{IDEMPOTENCY_LABEL[c.idempotency] ?? c.idempotency}</span
										>{/if}
									{#if c.confirm_ref}<span class="cell-sub" title="Customer message that confirmed this call"
											>confirmed · <span class="mono">{c.confirm_ref}</span></span
										>{/if}
								</td>
								<td class="r nowrap">{fmt.ms(c.latency_ms)}<span class="cell-sub mono">{c.call_id}</span></td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
			<Pagination next={p.next} count={p.items.length} busy={log.refreshing} />
		{/snippet}
	</LoadState>
</Card>

<style>
	.j22-ends {
		background: var(--panel);
		border: 1px solid var(--line);
		border-radius: var(--r);
		padding: 14px 16px;
		margin-bottom: 16px;
	}
	.j22-ends-head {
		display: flex;
		flex-wrap: wrap;
		align-items: baseline;
		justify-content: space-between;
		gap: 8px;
	}
	.j22-ends-head h2 {
		margin: 0;
		font-size: 15px;
	}
	.j22-ends-head .muted {
		font-size: 12px;
	}
	.j22-ends-bar {
		display: flex;
		gap: 2px;
		height: 10px;
		margin: 10px 0 12px;
		border-radius: 5px;
		overflow: hidden;
	}
	.j22-ends-bar .b {
		min-width: 4px;
	}
	.j22-ends-tiles {
		display: grid;
		grid-template-columns: repeat(5, minmax(0, 1fr));
		gap: 8px;
	}
	.j22-tile {
		text-align: left;
		display: grid;
		grid-template-columns: 1fr auto;
		gap: 2px 8px;
		padding: 8px 10px;
		border: 1px solid var(--line);
		border-top: 3px solid var(--j22c);
		border-radius: var(--r);
		background: var(--panel);
		color: var(--fg);
		font: inherit;
		cursor: pointer;
	}
	.j22-tile:hover {
		background: var(--sunk);
	}
	.t-label {
		grid-column: 1 / -1;
		font-size: 12px;
		font-weight: 600;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		color: var(--muted);
	}
	.t-val {
		font-family: var(--cond);
		font-size: 22px;
		font-weight: 600;
	}
	.t-pct {
		align-self: end;
		font-size: 12px;
		color: var(--muted);
	}
	.t-hint {
		grid-column: 1 / -1;
		font-size: 12px;
		color: var(--muted);
		line-height: 1.35;
	}
	.e-ok {
		--j22c: var(--ok);
	}
	.e-denied {
		--j22c: var(--muted);
	}
	.e-replied {
		--j22c: var(--accent);
	}
	.e-escalated {
		--j22c: var(--warn);
	}
	.e-unknown {
		--j22c: var(--err);
	}
	.j22-ends-bar .b {
		background: var(--j22c);
	}
	.j22-all {
		margin-top: 12px;
		border-top: 1px solid var(--line-2);
		padding-top: 10px;
	}
	.j22-all summary {
		cursor: pointer;
		font-size: 13px;
		color: var(--accent);
	}
	.bad {
		color: var(--err);
	}
	tr.j22-row td:first-child {
		box-shadow: inset 3px 0 0 var(--j22c, transparent);
	}
	tr.j22-row.e-ok td:first-child {
		box-shadow: none;
	}
	tr.j22-row.e-unknown td {
		background: color-mix(in srgb, var(--err) 7%, transparent);
	}
	.j22-for {
		font-size: 13px;
	}
	@media (max-width: 1100px) {
		.j22-ends-tiles {
			grid-template-columns: repeat(3, minmax(0, 1fr));
		}
	}
	@media (max-width: 560px) {
		.j22-ends-tiles {
			grid-template-columns: repeat(2, minmax(0, 1fr));
		}
	}
</style>
