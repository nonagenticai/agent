<script lang="ts">
	// T13: the path one customer message takes — customer → channel → edge webhook → gate 2 → job@release.
	// Every hop states its health in words (never colour alone); horizontal on desktop, vertical on phone.
	import type { Hop } from '$lib/tenant/channelsT13';
	let { hops }: { hops: Hop[] } = $props();
</script>

<ol class="t13-path" aria-label="Path of a customer message" data-testid="reach-path">
	{#each hops as h, i (h.key)}
		<li class="t13-hop {h.state}" data-hop={h.key}>
			<div class="t13-hop-head">
				<span class="t13-hop-label">{h.label}</span>
				<span class="t13-hop-state">{h.stateText}</span>
			</div>
			<div class="t13-hop-value">
				{#if h.href}<a href={h.href}>{h.value}</a>{:else}{h.value}{/if}
			</div>
			<div class="t13-hop-detail">{h.detail}</div>
		</li>
		{#if i < hops.length - 1}<li class="t13-arrow" aria-hidden="true">→</li>{/if}
	{/each}
</ol>

<style>
	.t13-path {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		grid-template-columns: repeat(4, minmax(0, 1fr) auto) minmax(0, 1fr);
		align-items: stretch;
		gap: 6px;
	}
	.t13-hop {
		--tone: var(--muted);
		border: 1px solid var(--line);
		border-top: 3px solid var(--tone);
		border-radius: 8px;
		background: var(--panel);
		padding: 10px 12px;
		min-width: 0;
		display: grid;
		gap: 4px;
		align-content: start;
	}
	.t13-hop.ok {
		--tone: var(--ok);
	}
	.t13-hop.warn {
		--tone: var(--warn);
	}
	.t13-hop.err {
		--tone: var(--err);
	}
	.t13-hop.off {
		--tone: var(--line);
		background: var(--sunk);
	}
	.t13-hop-head {
		display: flex;
		flex-wrap: wrap;
		justify-content: space-between;
		gap: 6px;
		align-items: baseline;
	}
	.t13-hop-label {
		font: 600 11px/1.2 var(--cond);
		text-transform: uppercase;
		letter-spacing: 0.06em;
		color: var(--muted);
	}
	.t13-hop-state {
		font: 600 11px/1.2 var(--mono, monospace);
		color: var(--tone);
		white-space: nowrap;
	}
	.t13-hop.off .t13-hop-state {
		color: var(--muted);
	}
	.t13-hop-value {
		font-weight: 600;
		font-size: 13.5px;
		overflow-wrap: anywhere;
	}
	.t13-hop-detail {
		font-size: 12px;
		color: var(--muted);
		line-height: 1.35;
		/* the release detail carries a full sha256: wrap it inside the hop, never widen the page */
		overflow-wrap: anywhere;
		min-width: 0;
	}
	.t13-arrow {
		align-self: center;
		color: var(--muted);
		font-size: 16px;
	}
	@media (max-width: 900px) {
		.t13-path {
			grid-template-columns: minmax(0, 1fr);
		}
		.t13-arrow {
			justify-self: center;
			transform: rotate(90deg);
			line-height: 1;
		}
	}
</style>
