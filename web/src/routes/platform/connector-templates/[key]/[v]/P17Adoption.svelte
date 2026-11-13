<script lang="ts">
	// P17 "who runs this template": every tenant system on any version of the key, and whether it can move to the
	// newest active version. Moving is the tenant's explicit re-pin on T11 (doc 11 §6.1 rule 4) — never pushed.
	import type { ConnectorTemplate, TemplateInstanceUse } from '$lib/api/types/connectors';
	import { upgradeVerdict } from '$lib/connectors/P17-diff';
	import Chip from '$lib/ui/Chip.svelte';

	let {
		current,
		instances,
		latest,
	}: { current: string; instances: TemplateInstanceUse[]; latest: ConnectorTemplate | null } = $props();

	const rows = $derived(
		instances
			.map((i) => ({ i, v: upgradeVerdict(i, latest) }))
			.sort((a, b) => Number(b.i.pinned_version === current) - Number(a.i.pinned_version === current)),
	);
	const onThis = $derived(instances.filter((i) => i.pinned_version === current).length);
</script>

<p class="p17-lead" data-testid="p17-adoption-summary">
	{#if instances.length === 0}
		No tenant system uses this template yet.
	{:else}
		<b>{onThis}</b> of {instances.length} tenant {instances.length === 1 ? 'system runs' : 'systems run'} @{current}.
		{#if latest}Newest active: <span class="mono">@{latest.version}</span>.{/if}
	{/if}
</p>
{#if rows.length}
	<ul class="p17-adopt">
		{#each rows as { i, v } (i.instance_id)}
			<li class:is-here={i.pinned_version === current}>
				<div class="p17-who">
					<span><b>{i.tenant}</b> · {i.label}</span>
					<span class="cell-sub mono">{i.instance_id} · {i.env} · {i.routed_ops.length} routed ops</span>
				</div>
				<div class="p17-pin">
					<Chip
						text="@{i.pinned_version}"
						variant={i.pinned_version === current ? 'accent' : 'muted'}
						title="Pinned version"
					/>
					{#if v.kind === 'current'}
						<Chip text="up to date" variant="ok" title={v.note} />
					{:else if v.kind === 'can_upgrade'}
						<Chip text="can re-pin → @{v.to}" variant="work" />
					{:else if v.kind === 'blocked'}
						<Chip text="blocked → @{v.to}" variant="err" />
					{:else}
						<Chip text="no target" variant="muted" title={v.note} />
					{/if}
					{#if i.status !== 'ok'}<Chip text={i.status} variant="err" title="Instance status" />{/if}
				</div>
				{#if v.kind === 'can_upgrade'}
					<p class="cell-sub p17-why">Tenant re-pins on its system page: {v.warnings.join(' · ')}.</p>
				{:else if v.kind === 'blocked'}
					<p class="cell-sub p17-why s-err">{v.reasons.join(' · ')}.</p>
				{/if}
			</li>
		{/each}
	</ul>
{/if}

<style>
	.p17-lead {
		margin: 0 0 8px;
		font-size: 13px;
	}
	.p17-adopt {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
	}
	.p17-adopt li {
		display: grid;
		grid-template-columns: minmax(0, 1fr) auto;
		gap: 4px 12px;
		padding: 10px 0;
		border-bottom: 1px solid var(--line-2);
	}
	.p17-adopt li:last-child {
		border-bottom: 0;
	}
	.p17-adopt li.is-here {
		border-left: 3px solid var(--accent);
		padding-left: 10px;
	}
	.p17-pin {
		display: flex;
		flex-wrap: wrap;
		gap: 5px;
		justify-content: flex-end;
		align-items: start;
	}
	.p17-why {
		grid-column: 1 / -1;
		margin: 0;
	}
	@media (max-width: 560px) {
		.p17-adopt li {
			grid-template-columns: minmax(0, 1fr);
		}
		.p17-pin {
			justify-content: flex-start;
		}
	}
</style>
