<script lang="ts">
	// Top of the sidebar: which Workspace and which Agent you are working in. The workspace name goes to Workspace home;
	// a super admin also gets a "Change" button (the tenant picker). The Agent switcher keeps the same page on another
	// agent, and is hidden for a workspace with no agents.
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { app, selectJob } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { jobPath, switchJobPath } from '$lib/scope';
	import { scopeForRoute } from '$lib/pages';
	import { L } from '$lib/ui/labels';

	let {
		tenantLabel,
		onpickworkspace,
		onnavigate,
	}: { tenantLabel: string; onpickworkspace: () => void; onnavigate?: () => void } = $props();

	const inAgent = $derived(scopeForRoute(page.route.id) === 'agent');
	const ALL = '__all__';

	function choose(key: string) {
		onnavigate?.();
		if (key === ALL) {
			void goto('/admin/jobs');
			return;
		}
		const target = switchJobPath(page.url.pathname, key);
		if (target) void goto(target);
		else {
			selectJob(key);
			void goto(jobPath(key));
		}
	}
</script>

<div class="scope" data-testid="scope-switcher">
	<div class="scope-row">
		<span class="scope-k" id="scope-ws">{L.tenant}</span>
		<div class="ws">
			<a href="/admin" class="ws-name" aria-labelledby="scope-ws ws-name-text" onclick={() => onnavigate?.()}
				><span id="ws-name-text">{tenantLabel}</span></a
			>
			{#if session.superAdmin}
				<button
					class="ws-change"
					type="button"
					data-testid="tenant-button"
					title="The workspace every page acts in"
					onclick={() => onpickworkspace()}>Change</button
				>
			{/if}
		</div>
	</div>
	{#if app.jobs.length}
		<label class="scope-row">
			<span class="scope-k">{L.job}</span>
			<select
				class="input agent-pick"
				aria-label={L.job}
				data-testid="agent-switcher"
				value={inAgent ? app.job : ALL}
				onchange={(e) => choose(e.currentTarget.value)}
			>
				<option value={ALL}>All {L.jobs.toLowerCase()}</option>
				{#each app.jobs as j (j.key)}<option value={j.key}>{j.label}</option>{/each}
			</select>
		</label>
	{/if}
</div>

<style>
	.scope {
		display: grid;
		gap: 8px;
		padding: 0 10px 10px;
		border-bottom: 1px solid var(--sb-hi, var(--side-hi));
		margin-bottom: 4px;
	}
	.scope-row {
		display: grid;
		gap: 3px;
		min-width: 0;
	}
	.scope-k {
		font: 600 10.5px/1 var(--cond);
		letter-spacing: 0.1em;
		text-transform: uppercase;
		color: var(--sb-muted, var(--side-muted));
		padding: 0 2px;
	}
	.ws {
		display: flex;
		align-items: center;
		gap: 6px;
		min-width: 0;
	}
	.ws-name {
		flex: 1;
		min-width: 0;
		padding: 6px 8px;
		border-radius: 6px;
		background: var(--sb-hi, var(--side-hi));
		color: var(--sb-strong, #fff);
		text-decoration: none;
		font-weight: 600;
		font-size: 13.5px;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.ws-change {
		flex: none;
		border: 0;
		background: none;
		padding: 4px 6px;
		border-radius: 4px;
		color: var(--sb-fg, var(--side-fg));
		font: inherit;
		font-size: 12.5px;
		text-decoration: underline;
		cursor: pointer;
	}
	.agent-pick {
		width: 100%;
		background: var(--sb-hi, var(--side-hi));
		color: var(--sb-fg, var(--side-fg));
		border-color: transparent;
	}
</style>
