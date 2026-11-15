<script lang="ts">
	// P20: the gate-2 chain (C1, K35) as a left-to-right flow. The dashed boundary marks where keys and tokens
	// stop: the edge never holds LICENSE_TOKEN_ENC_KEY(S) or a sealed token — only the verdict row.
	let { cacheS, refreshS, maxAgeS }: { cacheS: number; refreshS: number; maxAgeS: number } = $props();
</script>

<ol class="gf" aria-label="Gate 2: how a licence reaches the edge">
	<li class="gf-node">
		<span class="gf-k">Licences service</span>
		<span class="gf-d"
			>Holds the tenant's <code>agent</code> licence. Introspect answers active · inactive · revoked · expired.</span
		>
	</li>
	<li class="gf-arrow" aria-hidden="true">→</li>
	<li class="gf-node">
		<span class="gf-k">agent-api</span>
		<span class="gf-d"
			>Opens the sealed token by its kid, introspects (cache {cacheS} s), rewrites the row every ≤{refreshS} s —
			at once on <code>licence:changed</code>.</span
		>
	</li>
	<li class="gf-wall" aria-hidden="true"><span>keys + tokens stop here</span></li>
	<li class="gf-node">
		<span class="gf-k">Verdict row</span>
		<span class="gf-d"
			><code>tenant_licence_verdict</code>: status · checked_at · reason. No token, no key.</span
		>
	</li>
	<li class="gf-arrow" aria-hidden="true">→</li>
	<li class="gf-node gf-edge">
		<span class="gf-k">Edge (inbox drain)</span>
		<span class="gf-d"
			><span class="gf-ok">Fresh + active → serve.</span>
			<span class="gf-no">Missing, not active, or older than {maxAgeS / 60} min → refuse</span> with one neutral
			text; audited once per tenant per hour.</span
		>
	</li>
</ol>

<style>
	.gf {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		grid-template-columns: minmax(0, 1fr) auto minmax(0, 1fr) auto minmax(0, 1fr) auto minmax(0, 1.2fr);
		align-items: stretch;
		gap: 8px;
	}
	.gf-node {
		border: 1px solid var(--line);
		background: var(--sunk);
		border-radius: 8px;
		padding: 10px 12px;
		display: grid;
		align-content: start;
		gap: 4px;
		min-width: 0;
	}
	.gf-edge {
		border-color: color-mix(in srgb, var(--accent) 45%, var(--line));
	}
	.gf-k {
		font: 600 13px/1.2 var(--cond);
		letter-spacing: 0.02em;
	}
	.gf-d {
		font-size: 12px;
		color: var(--muted);
	}
	.gf-ok {
		color: var(--ok);
		font-weight: 600;
	}
	.gf-no {
		color: var(--err);
		font-weight: 600;
	}
	.gf-arrow {
		align-self: center;
		color: var(--muted);
		font-size: 18px;
	}
	.gf-wall {
		align-self: stretch;
		display: flex;
		align-items: center;
		border-left: 2px dashed var(--warn);
		padding-left: 6px;
		max-width: 64px;
	}
	.gf-wall span {
		font: 600 10px/1.2 var(--cond);
		text-transform: uppercase;
		letter-spacing: 0.06em;
		color: var(--warn);
	}
	@media (max-width: 900px) {
		.gf {
			grid-template-columns: minmax(0, 1fr);
		}
		.gf-arrow {
			justify-self: center;
			transform: rotate(90deg);
			line-height: 1;
		}
		.gf-wall {
			max-width: none;
			border-left: 0;
			border-top: 2px dashed var(--warn);
			padding: 4px 0 0;
			justify-content: center;
		}
	}
</style>
