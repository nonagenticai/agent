<script lang="ts">
	// P1: the tenant lifecycle as a picture (04 §7b, 06 §4.6). Every stage is a filter over the portfolio below.
	// create/import → provisioning (provision-down) → active ⇄ suspended → archived (offboard; strata keeps data).
	import type { TenantLifecycleStage } from '$lib/api/types/platform';

	let {
		stages,
		selected = $bindable(null),
	}: {
		stages: Record<TenantLifecycleStage, string[]>;
		selected?: TenantLifecycleStage | null;
	} = $props();

	const NODES: { key: TenantLifecycleStage; label: string; means: string; tone: string }[] = [
		{
			key: 'provisioning',
			label: 'Provisioning',
			means: 'Created; licence, knowledge store and ingest connections are being provisioned down.',
			tone: 'warn',
		},
		{
			key: 'active',
			label: 'Active',
			means: 'Licensed; members sign in and its jobs serve customers.',
			tone: 'ok',
		},
		{
			key: 'suspended',
			label: 'Suspended',
			means: 'Reversible pause: licence inactive, members refused, the edge stops serving.',
			tone: 'warn',
		},
		{
			key: 'archived',
			label: 'Archived',
			means:
				'Offboarded: licence revoked, users disabled, read-only. Knowledge and ingest data are retained.',
			tone: 'muted',
		},
	];

	function pick(k: TenantLifecycleStage) {
		selected = selected === k ? null : k;
	}
</script>

<div class="lc" role="group" aria-label="Tenant lifecycle" data-testid="tenant-lifecycle">
	<div class="lc-start">
		<span class="lc-start-dot" aria-hidden="true"></span>
		<span><strong>Create</strong> or <strong>import</strong></span>
	</div>
	{#each NODES as n, i (n.key)}
		<div class="lc-edge" aria-hidden="true">
			<span class="lc-edge-label"
				>{i === 0
					? 'provision down'
					: i === 1
						? 'outbox drained'
						: i === 2
							? 'suspend ⇄ resume'
							: 'offboard'}</span
			>
			<span class="lc-arrow">{i === 2 ? '⇄' : '→'}</span>
		</div>
		<button
			type="button"
			class="lc-node tone-{n.tone}"
			class:on={selected === n.key}
			class:empty={stages[n.key].length === 0}
			aria-pressed={selected === n.key}
			title={n.means}
			data-testid="lifecycle-{n.key}"
			onclick={() => pick(n.key)}
		>
			<span class="lc-head">
				<span class="lc-name">{n.label}</span>
				<span class="lc-count">{stages[n.key].length}</span>
			</span>
			<span class="lc-means">{n.means}</span>
			{#if stages[n.key].length}
				<span class="lc-who">{stages[n.key].join(' · ')}</span>
			{/if}
		</button>
	{/each}
</div>

<style>
	.lc {
		display: grid;
		grid-template-columns: auto repeat(4, auto minmax(0, 1fr));
		align-items: stretch;
		gap: 0;
	}
	.lc-start {
		display: flex;
		align-items: center;
		gap: 8px;
		font-size: 12.5px;
		color: var(--muted);
		padding-right: 4px;
	}
	.lc-start-dot {
		width: 10px;
		height: 10px;
		border-radius: 50%;
		border: 2px solid var(--accent);
		flex: none;
	}
	.lc-edge {
		display: flex;
		flex-direction: column;
		align-items: center;
		justify-content: center;
		padding: 0 6px;
		min-width: 64px;
	}
	.lc-edge-label {
		font: 500 10.5px/1.2 var(--mono);
		color: var(--faint);
		text-align: center;
		white-space: nowrap;
	}
	.lc-arrow {
		font-size: 18px;
		color: var(--muted);
		line-height: 1;
	}
	.lc-node {
		all: unset;
		box-sizing: border-box;
		cursor: pointer;
		display: flex;
		flex-direction: column;
		gap: 4px;
		padding: 10px 12px;
		border: 1px solid var(--line);
		border-top: 3px solid var(--tone);
		border-radius: var(--r);
		background: var(--panel);
		min-width: 0;
	}
	.lc-node:hover {
		background: var(--sunk);
	}
	.lc-node:focus-visible {
		outline: 2px solid var(--accent);
		outline-offset: 2px;
	}
	.lc-node.on {
		background: color-mix(in srgb, var(--tone) 10%, var(--panel));
		border-color: var(--tone);
	}
	.lc-node.empty {
		opacity: 0.7;
	}
	.tone-ok {
		--tone: var(--ok);
	}
	.tone-warn {
		--tone: var(--warn);
	}
	.tone-muted {
		--tone: var(--muted);
	}
	.lc-head {
		display: flex;
		justify-content: space-between;
		align-items: baseline;
		gap: 8px;
	}
	.lc-name {
		font: 600 13.5px/1.2 var(--cond);
	}
	.lc-count {
		font: 600 20px/1 var(--cond);
		color: var(--tone);
	}
	.lc-means {
		font-size: 11.5px;
		line-height: 1.35;
		color: var(--muted);
	}
	.lc-who {
		font-size: 12px;
		color: var(--fg);
	}
	@media (max-width: 1100px) {
		.lc {
			grid-template-columns: 1fr;
		}
		.lc-start {
			padding: 0 0 4px;
		}
		.lc-edge {
			flex-direction: row;
			gap: 8px;
			justify-content: flex-start;
			padding: 4px 0 4px 18px;
		}
		.lc-arrow {
			transform: rotate(90deg);
		}
	}
</style>
