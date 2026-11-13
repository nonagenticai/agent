<script lang="ts">
	// D69 draft bar for a TENANT-WIDE design page (provider bindings + slots, connector mapping overrides, tenant
	// connector templates). Nothing saved here is live on save: it goes to the draft of every job, is tried in the test
	// chat, and reaches a job's Production when that job publishes a release. Shows the draft-vs-live diff per job.
	import { api, app } from '$lib/app.svelte';
	import type { TenantDesignRow } from '$lib/api/types/release';

	let {
		kinds,
		tick = 0,
		only,
		label,
	}: {
		/** Ledger kinds this page edits (`llm_use`, `provider_slot`, `mapping_override`, `tenant_template`). */
		kinds: string[];
		/** Change it after a save to re-read the diff. */
		tick?: unknown;
		/** Keep only some rows (e.g. one connector instance's overrides). */
		only?: (r: TenantDesignRow) => boolean;
		/** One line naming what a row is (defaults to its key). */
		label?: (r: TenantDesignRow) => string;
	} = $props();

	let rows = $state<TenantDesignRow[] | null>(null);
	$effect(() => {
		void tick;
		api()
			.tenantDesignDiff(kinds)
			.then((d) => (rows = only ? d.rows.filter(only) : d.rows))
			.catch(() => (rows = null));
	});
</script>

<div class="draft-bar" class:has={(rows?.length ?? 0) > 0} data-testid="tenant-draft-bar">
	<span class="draft-bar-n">
		{#if !rows || rows.length === 0}
			Changes you save here go to the draft of every job; each job's Production gets them when it publishes.
		{:else}
			<b>{rows.length} unpublished change{rows.length === 1 ? '' : 's'} here</b>
			<span class="muted">· saved to draft, not live until each job publishes</span>
		{/if}
	</span>
	<span class="draft-bar-links">
		<a href="/chat">Test in test chat</a>
		<a class="btn sm" href="/admin/jobs/{app.job}/release">Review &amp; publish</a>
	</span>
	{#if rows?.length}
		<ul class="tdb-rows">
			{#each rows as r (r.key)}
				<li data-testid="tenant-live-vs-draft">
					<span class="tdb-what">{label ? label(r) : r.key}</span>
					<span>Draft: <b>{r.draft ?? '—'}</b></span>
					{#each r.jobs as j (j.job)}
						<span class="muted">Live in {j.job_label}: <b>{j.live ?? '—'}</b></span>
					{/each}
				</li>
			{/each}
		</ul>
		<span class="muted tdb-foot"
			>Not live yet in {[...new Set(rows.flatMap((r) => r.jobs.map((j) => j.job_label)))].join(', ')}.</span
		>
	{/if}
</div>

<style>
	.draft-bar {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		justify-content: space-between;
		gap: 8px 16px;
		padding: 8px 12px;
		margin: 0 0 14px;
		border: 1px solid var(--line);
		border-left: 3px solid var(--line);
		border-radius: 6px;
		font-size: 13px;
	}
	.draft-bar.has {
		border-left-color: var(--accent);
	}
	.draft-bar-links {
		display: flex;
		align-items: center;
		gap: 12px;
	}
	.tdb-rows {
		flex-basis: 100%;
		margin: 0;
		padding: 0;
		list-style: none;
		display: grid;
		gap: 6px;
	}
	.tdb-rows li {
		display: flex;
		flex-wrap: wrap;
		gap: 4px 14px;
		min-width: 0;
		overflow-wrap: anywhere;
	}
	.tdb-what {
		font-family: var(--mono);
		font-size: 12px;
	}
	.tdb-foot {
		flex-basis: 100%;
	}
</style>
