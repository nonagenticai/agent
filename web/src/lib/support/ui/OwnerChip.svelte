<script lang="ts">
	// Who answers this conversation: the agent, nobody yet (queue), or a staff member ("You" when it is the viewer).
	import Icon from '$lib/ui/Icon.svelte';
	import type { OwnerView } from '$lib/api/types/support';

	let { owner, meSub = null }: { owner: OwnerView; meSub?: string | null } = $props();
	const mine = $derived(meSub != null && owner.owner === `user:${meSub}`);
	const label = $derived(
		owner.owner === 'agent'
			? 'Agent'
			: owner.owner === 'queue'
				? 'Unassigned'
				: mine
					? 'You'
					: (owner.name ?? 'Staff'),
	);
</script>

<span
	class="oc"
	data-testid="owner-chip"
	class:agent={owner.owner === 'agent'}
	class:queue={owner.owner === 'queue'}
	class:mine
>
	{#if owner.owner === 'agent'}<Icon name="bot" size={14} />{/if}
	<span class="lbl">{label}</span>
</span>

<style>
	.oc {
		display: inline-flex;
		align-items: center;
		gap: 5px;
		padding: 2px 9px;
		border-radius: 999px;
		border: 1px solid var(--line);
		background: var(--sunk);
		font-size: 12px;
		font-weight: 500;
		white-space: nowrap;
	}
	.agent {
		color: var(--tool);
	}
	.queue {
		color: var(--warn);
		border-color: color-mix(in srgb, var(--warn) 40%, var(--line));
	}
	.mine {
		color: var(--ok);
		border-color: color-mix(in srgb, var(--ok) 40%, var(--line));
	}
</style>
