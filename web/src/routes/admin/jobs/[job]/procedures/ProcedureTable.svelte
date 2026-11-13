<script lang="ts">
	// J12 version table: status filter, origin, shadowed/effective, step count, reviewers, 7-day runs and success,
	// and the cut-time grant check. Rows open the procedure page.
	// from cb:web/src/routes/admin/jobs/[job]/procedures/+page.svelte@45ee128 (list table)
	import { goto } from '$app/navigation';
	import { app } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { writeAttrs } from '$lib/auth/roles';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Meter from '$lib/ui/Meter.svelte';
	import Seg from '$lib/ui/Seg.svelte';
	import Status from '$lib/ui/Status.svelte';
	import type { ProcedureView } from '$lib/api/types/ontology';
	import { liveOf, statusFilters, stepSummary, successView } from './view';

	let {
		rows,
		onstartfrom,
		release = null,
	}: { rows: ProcedureView[]; onstartfrom: (key: string) => void; release?: string | null } = $props();
	const isLive = (p: ProcedureView) =>
		liveOf(
			rows.filter((r) => r.name === p.name),
			release,
		)?.version === p.version;
	let filter = $state('all');
	const shown = $derived(filter === 'all' ? rows : rows.filter((p) => p.status === filter));
	const day = (iso: string) => iso.slice(0, 10);
	const href = (p: ProcedureView) => `/admin/jobs/${app.job}/procedures/${p.name}?v=${p.version}`;
</script>

<div class="split pr-bar">
	<Seg label="Status" items={statusFilters(rows)} bind:active={filter} />
	<span class="muted pr-hint"
		>Runs and success count the last 7 days of cases recorded against each version.</span
	>
</div>
<Card cls="flush">
	<div class="tbl-wrap">
		<table class="tbl clickable">
			<thead>
				<tr>
					<th>Procedure</th><th>Version</th><th>Status</th><th>Context space</th><th class="r">Steps</th><th
						>Grant check</th
					><th class="r">Reviewers</th><th class="r">Runs 7d</th><th>Success</th><th></th>
				</tr>
			</thead>
			<tbody>
				{#each shown as p (p.name + '@' + p.version)}
					{@const sum = stepSummary(p.steps)}
					{@const ok = successView(p.success_rate)}
					<tr
						tabindex="0"
						onclick={() => goto(href(p))}
						onkeydown={(e) => e.key === 'Enter' && goto(href(p))}
					>
						<td>
							<a class="cell-title mono" href={href(p)} onclick={(e) => e.stopPropagation()}>{p.name}</a>
							<span class="cell-sub nowrap">updated {day(p.updated_at)}</span>
						</td>
						<td class="mono">{p.version}</td>
						<td class="nowrap">
							<Status word={p.status} />
							<Chip
								text={p.origin === 'tenant' ? 'Tenant' : 'Base'}
								variant={p.origin === 'tenant' ? 'accent' : 'muted'}
							/>
							{#if p.shadowed}<Chip
									text="shadowed"
									variant="dead"
									title="The release also carries a tenant version, which runs instead"
								/>{/if}
							{#if isLive(p)}<Chip text="live" variant="ok" />{:else if p.effective_draft ?? p.effective}<Chip
									text="next release"
									variant="accent"
									title="Published; goes live when the next release is applied"
								/>{/if}
						</td>
						<td class="mono">{p.context_space}</td>
						<td class="r"
							>{sum.steps} <span class="muted">in {sum.phases} phase{sum.phases === 1 ? '' : 's'}</span></td
						>
						<td>
							{#if p.outside_grant.length}
								<Chip
									text="{p.outside_grant.length} outside grant"
									variant="err"
									title={p.outside_grant.join(', ')}
								/>
							{:else}<span class="muted">inside grant</span>{/if}
						</td>
						<td class="r">
							{#if p.origin === 'tenant'}<span
									class:warn-text={p.status === 'CANARY' && p.reviewers.length === 0}
									>{p.reviewers.length}</span
								>{:else}<span class="muted">template</span>{/if}
						</td>
						<td class="r mono">{p.runs_7d.toLocaleString('en-GB')}</td>
						<td class="pr-success"
							>{#if ok}<Meter value={p.success_rate ?? 0} cls={ok.cls} />
								<span class="mono">{ok.text}</span>{:else}<span class="muted">—</span>{/if}</td
						>
						<td class="r nowrap">
							<button
								type="button"
								class="btn sm"
								{...writeAttrs(session, 'job_designer')}
								title={writeAttrs(session, 'job_designer').title ??
									'New tenant DRAFT copied from this version'}
								onclick={(e) => {
									e.stopPropagation();
									onstartfrom(`${p.name}@${p.version}`);
								}}>Start from</button
							>
						</td>
					</tr>
				{:else}
					<tr><td colspan="10" class="muted">No procedures with this status.</td></tr>
				{/each}
			</tbody>
		</table>
	</div>
</Card>

<style>
	.pr-bar {
		margin-bottom: 8px;
		align-items: center;
		gap: 12px;
	}
	.pr-hint {
		font-size: 12.5px;
	}
	.pr-success {
		display: flex;
		align-items: center;
		gap: 6px;
		min-width: 110px;
	}
	.warn-text {
		color: var(--warn);
		font-weight: 600;
	}
	td :global(.chip) {
		margin-left: 4px;
	}
</style>
