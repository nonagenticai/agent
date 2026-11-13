<script lang="ts">
	// "Customer satisfaction" (SW-6): the tenant-wide CSAT report for the Support manager and the tenant admin, on the
	// workspace's start page. Mean rating, response rate, the 1-5 distribution, the by-channel and by-resolver tables
	// and the low scores (1-2) with the customer's comment, each linking to its conversation. The window is the requests
	// ASKED in the last 7 / 30 / 90 days. Everyone else sees nothing: the server refuses them (403) and so does this tile.
	import { api } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { CHANNEL_LABEL } from '$lib/conversations/labels';
	import { workspaceHref } from '$lib/support/links';
	import { relative } from '$lib/support/time';
	import type { CsatReport } from '$lib/api/types/support';

	const RANGES = [7, 30, 90] as const;
	let days = $state<(typeof RANGES)[number]>(30);
	let report = $state<CsatReport | null>(null);
	let error = $state<string | null>(null);

	const allowed = $derived(session.member && (session.has('tenant_admin') || session.has('support_manager')));
	const label = (c: string) => (CHANNEL_LABEL as Record<string, string>)[c] ?? c;
	const mean = (m: number | null) => (m == null ? '–' : m.toFixed(1));
	const pct = (r: number | null) => (r == null ? '–' : `${Math.round(r * 100)}%`);
	const top = $derived(report ? Math.max(1, ...Object.values(report.distribution)) : 1);

	$effect(() => {
		if (!allowed) return;
		const d = days;
		let gone = false;
		report = null;
		error = null;
		api()
			.supportCsat({ from: new Date(Date.now() - d * 86_400_000).toISOString() })
			.then((r) => {
				if (!gone) report = r;
			})
			.catch((e) => {
				if (!gone) error = e instanceof Error ? e.message : 'Could not load the satisfaction report.';
			});
		return () => (gone = true);
	});
</script>

{#if allowed}
	<section class="csat" aria-label="Customer satisfaction" data-testid="csat-report">
		<header>
			<h2>Customer satisfaction</h2>
			<label class="range">
				<span class="muted">Asked in the last</span>
				<select aria-label="Window" bind:value={days}>
					{#each RANGES as r (r)}<option value={r}>{r} days</option>{/each}
				</select>
			</label>
		</header>
		{#if error}
			<p class="err" role="alert">{error}</p>
		{:else if !report}
			<p class="muted">Loading…</p>
		{:else if report.asked === 0}
			<p class="muted" data-testid="csat-empty">No rating was asked for in this window.</p>
		{:else}
			<div class="tiles">
				<div class="tile">
					<span class="v" data-testid="csat-mean">{mean(report.mean)}</span><span class="k"
						>Mean rating (of 5)</span
					>
				</div>
				<div class="tile">
					<span class="v" data-testid="csat-rate">{pct(report.response_rate)}</span>
					<span class="k">Response rate · {report.answered} of {report.asked} asked</span>
				</div>
			</div>

			<div class="dist" role="list" aria-label="Ratings">
				{#each ['5', '4', '3', '2', '1'] as k (k)}
					{@const n = report.distribution[k as '1']}
					<div class="bar" role="listitem" aria-label="{k} stars: {n}">
						<span class="star">{k}★</span>
						<span class="track"><span class="fill" style:width="{(n / top) * 100}%"></span></span>
						<span class="n">{n}</span>
					</div>
				{/each}
			</div>

			<div class="tables">
				<table aria-label="By channel">
					<thead><tr><th>Channel</th><th>Asked</th><th>Answered</th><th>Mean</th></tr></thead>
					<tbody>
						{#each report.by_channel as c (c.channel)}
							<tr
								><td>{label(c.channel)}</td><td>{c.asked}</td><td>{c.answered}</td><td>{mean(c.mean)}</td></tr
							>
						{/each}
					</tbody>
				</table>
				<table aria-label="By resolver">
					<thead><tr><th>Asked by</th><th>Asked</th><th>Answered</th><th>Mean</th></tr></thead>
					<tbody>
						{#each report.by_resolver as r (`${r.asked_by}|${r.sub ?? ''}`)}
							<tr>
								<td>{r.asked_by === 'agent' ? 'Agent' : (r.name ?? r.sub ?? 'Staff')}</td>
								<td>{r.asked}</td><td>{r.answered}</td><td>{mean(r.mean)}</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>

			<h3>Low scores</h3>
			{#if report.low_scores.length === 0}
				<p class="muted">No rating of 1 or 2 in this window.</p>
			{:else}
				<ul class="low" aria-label="Low scores">
					{#each report.low_scores as l (l.case_id)}
						<li>
							<span class="rating" aria-label="{l.rating} out of 5">{l.rating}★</span>
							<span class="meta muted">{label(l.channel)} · {relative(l.answered_at)}</span>
							<a href={workspaceHref(l.job_key, l.conversation_id)}>Open conversation</a>
							{#if l.comment}<q class="comment">{l.comment}</q>{/if}
						</li>
					{/each}
				</ul>
			{/if}
		{/if}
	</section>
{/if}

<style>
	.csat {
		display: grid;
		gap: 12px;
		text-align: left;
		width: min(760px, 100%);
		padding: 14px 16px;
		border: 1px solid var(--line);
		border-radius: 10px;
		background: var(--panel);
	}
	header {
		display: flex;
		justify-content: space-between;
		align-items: baseline;
		gap: 12px;
		flex-wrap: wrap;
	}
	h2,
	h3 {
		margin: 0;
		font-size: 15px;
	}
	h3 {
		font-size: 13px;
	}
	.muted {
		color: var(--muted);
	}
	.err {
		color: var(--err);
	}
	.range {
		display: flex;
		gap: 6px;
		align-items: baseline;
	}
	.tiles {
		display: flex;
		gap: 12px;
		flex-wrap: wrap;
	}
	.tile {
		display: grid;
		padding: 8px 14px;
		border: 1px solid var(--line);
		border-radius: 8px;
	}
	.v {
		font-size: 26px;
		font-weight: 650;
	}
	.k {
		font-size: 12px;
		color: var(--muted);
	}
	.dist {
		display: grid;
		gap: 4px;
	}
	.bar {
		display: grid;
		grid-template-columns: 28px 1fr 32px;
		gap: 8px;
		align-items: center;
	}
	.track {
		height: 10px;
		background: color-mix(in srgb, var(--accent) 10%, var(--panel));
		border-radius: 5px;
		overflow: hidden;
	}
	.fill {
		display: block;
		height: 100%;
		background: var(--accent);
	}
	.n {
		text-align: right;
		font-variant-numeric: tabular-nums;
	}
	.tables {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(280px, 1fr));
		gap: 14px;
	}
	table {
		border-collapse: collapse;
		width: 100%;
	}
	th,
	td {
		text-align: left;
		padding: 3px 6px;
		border-bottom: 1px solid var(--line);
		font-size: 13px;
	}
	.low {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 8px;
	}
	.low li {
		display: grid;
		grid-template-columns: auto 1fr auto;
		gap: 2px 10px;
		align-items: baseline;
	}
	.rating {
		font-weight: 650;
	}
	.comment {
		grid-column: 1 / -1;
		white-space: pre-wrap;
		overflow-wrap: anywhere;
	}
</style>
