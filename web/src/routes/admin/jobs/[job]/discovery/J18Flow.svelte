<script lang="ts">
	// J18: the discovery model at a glance — real traffic → proposals → your decision → draft (Test) → release → live.
	// Every number is a count of this job's proposals in that state; D54: nothing is live before a publish.
	import type { DiscoverySignalSummary } from '$lib/api/types/journey';
	import { fmt } from '$lib/format';
	import type { tallyJ18 } from '$lib/journey/J18Discovery';

	let {
		signals,
		tally,
		base,
	}: { signals: DiscoverySignalSummary[]; tally: ReturnType<typeof tallyJ18>; base: string } = $props();
	const active = $derived(signals.filter((s) => s.active));
</script>

<ol class="j18-flow" data-testid="discovery-flow" aria-label="How discovery changes this job">
	<li class="j18-node src">
		<span class="j18-step">1 · Your traffic</span>
		<b>{active.length} of {signals.length} sources</b>
		<span class="j18-cap">
			{#each active as s, i (s.source)}{i ? ' · ' : ''}{fmt.n(s.count)} {s.unit}{/each}
			{#if !active.length}none yet: upload an export{/if}
		</span>
	</li>
	<li class="j18-node">
		<span class="j18-step">2 · Proposals</span>
		<b>{tally.open} open</b>
		<span class="j18-cap">drafted by discovery, with evidence</span>
	</li>
	<li class="j18-node you">
		<span class="j18-step">3 · You decide</span>
		<b>Accept or reject</b>
		<span class="j18-cap">accept = a change in the draft · {tally.rejected} rejected</span>
	</li>
	<li class="j18-node">
		<span class="j18-step">4 · Draft → Test</span>
		<b>{tally.draft} in draft</b>
		<span class="j18-cap">runs only in the <a href="/chat">test chat</a></span>
	</li>
	<li class="j18-node">
		<span class="j18-step">5 · Publish</span>
		<b>{tally.in_release} in a release</b>
		<span class="j18-cap"><a href="{base}/release">review &amp; publish</a></span>
	</li>
	<li class="j18-node live">
		<span class="j18-step">6 · Live</span>
		<b>{tally.live} live</b>
		<span class="j18-cap">customers get it</span>
	</li>
</ol>

<style>
	.j18-flow {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		grid-template-columns: repeat(6, minmax(0, 1fr));
		gap: 18px;
	}
	.j18-node {
		position: relative;
		display: grid;
		gap: 2px;
		align-content: start;
		padding: 10px 12px;
		border: 1px solid var(--line);
		border-top: 3px solid var(--line);
		border-radius: var(--r);
		background: var(--panel);
		min-width: 0;
	}
	.j18-node:not(:last-child)::after {
		content: '→';
		position: absolute;
		right: -15px;
		top: 50%;
		transform: translateY(-50%);
		color: var(--muted);
		font-size: 13px;
	}
	.j18-node.src {
		border-top-color: var(--cust);
	}
	.j18-node.you {
		border-top-color: var(--accent);
		background: color-mix(in srgb, var(--accent) 7%, var(--panel));
	}
	.j18-node.live {
		border-top-color: var(--ok);
	}
	.j18-step {
		font: 600 10.5px/1.3 var(--cond);
		text-transform: uppercase;
		letter-spacing: 0.06em;
		color: var(--muted);
	}
	.j18-node b {
		font-size: 15px;
	}
	.j18-cap {
		font-size: 12px;
		color: var(--muted);
		overflow-wrap: anywhere;
	}
	@media (max-width: 1100px) {
		.j18-flow {
			grid-template-columns: repeat(3, minmax(0, 1fr));
		}
		.j18-node:nth-child(3)::after {
			content: none;
		}
	}
	@media (max-width: 640px) {
		.j18-flow {
			grid-template-columns: 1fr;
			gap: 14px;
		}
		.j18-node:not(:last-child)::after {
			content: '↓';
			right: 50%;
			top: auto;
			bottom: -14px;
			transform: none;
		}
	}
</style>
