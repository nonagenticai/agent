<script lang="ts">
	// P15 Agent roles (super_admin): per job template, the agent roles and their grants (N3). The procedure role's
	// grants are derived from each activity and read-only; assistant is defined but disabled in v1 (K50);
	// qualifier and post-gate grants are edited here, never above the role's max side effect.
	import { onMount } from 'svelte';
	import { api } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Freshness from '$lib/ui/Freshness.svelte';
	import Seg from '$lib/ui/Seg.svelte';
	import Card from '$lib/ui/Card.svelte';
	import RoleCard from './RoleCard.svelte';
	import RoleLadder from './RoleLadder.svelte';
	import NarrowingChain from './NarrowingChain.svelte';

	// The job templates come from the API (the first one is picked); the roles reload whenever the pick changes.
	let templates = $state<[string, string][]>([]);
	let job = $state('');
	const res = new Resource(() => api().agentRoles(job));
	onMount(async () => {
		try {
			templates = (await api().agentRoleJobs()).map((j): [string, string] => [j.job, j.job]);
		} catch {
			templates = [];
		}
		job = templates[0]?.[0] ?? '';
		if (!job) res.refresh(); // nothing to pick: show the refusal / empty state
	});
	$effect(() => {
		if (job) res.refresh();
	});
</script>

<PageHead
	id="P15"
	sub="Agents never get free tools. Each principal gets a fixed grant under a side-effect ceiling, per job template — decide here what each one may call, and keep it to the least it needs."
>
	{#snippet actions()}
		<Freshness resource={res} />
	{/snippet}
</PageHead>

<div class="split ar-bar">
	<Seg label="Job template" items={templates} bind:active={job} />
	<span class="muted ar-hint"
		>Audit names a call’s principal <span class="mono">&lt;role&gt;@&lt;consumer&gt;</span>.</span
	>
</div>

{#if job}<NarrowingChain {job} />{/if}

<LoadState resource={res} empty="This job template defines no agent roles.">
	{#snippet children(page)}
		<Card
			title="Who may do what"
			sub="Four principals act on a case; each sees only its own grant. The runner re-checks every call, allow or deny."
		>
			<RoleLadder roles={page.items} />
		</Card>
		<h2 class="ar-sec">Grants per role</h2>
		<div class="roles">
			{#each page.items as r (r.role)}
				<RoleCard role={r} {job} onchanged={() => res.refresh()} />
			{/each}
		</div>
	{/snippet}
</LoadState>

<style>
	.ar-bar {
		align-items: center;
		gap: 12px;
		margin-bottom: 12px;
	}
	.ar-hint {
		font-size: 12.5px;
	}
	.ar-sec {
		font-size: 15px;
		margin: 20px 0 10px;
	}
	.roles {
		display: grid;
		gap: 12px;
	}
</style>
