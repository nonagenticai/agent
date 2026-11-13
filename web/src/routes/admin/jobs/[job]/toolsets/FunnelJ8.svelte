<script lang="ts">
	// J8: one toolset as a funnel — how many operations survive each link of the chain, and which ones each link
	// cut. The last box is the toolset itself.
	import type { Toolset } from '$lib/api/types/ontology';
	import { opLabel } from '$lib/ontology/catalogue';
	import { funnel } from '$lib/ontology/toolsetsJ8';

	let { ts }: { ts: Toolset } = $props();
	const stages = $derived(funnel(ts));
	const pool = $derived(Math.max(1, ts.rows.length));
	const callable = $derived(ts.rows.filter((r) => r.effective).length);
</script>

<ol class="j8f" aria-label="How the toolset is narrowed">
	{#each stages as s (s.key)}
		<li class="j8f-st k-{s.key}" class:cutting={s.cut.length > 0}>
			<span class="j8f-top">
				<span class="j8f-label">{s.label}</span>
				<span class="j8f-who">{s.who}</span>
			</span>
			<span class="j8f-n mono">{s.kept}</span>
			<span class="j8f-bar" aria-hidden="true"><i style="width:{(100 * s.kept) / pool}%"></i></span>
			{#if s.key !== 'pool'}
				{#if s.cut.length}
					<span class="j8f-cut">
						<span class="j8f-minus">−{s.cut.length}</span>
						{#each s.cut as r (opLabel(r.op, r.variant))}<span class="mono j8f-op"
								>{opLabel(r.op, r.variant)}</span
							>{/each}
					</span>
				{:else}
					<span class="j8f-none">cuts nothing</span>
				{/if}
			{/if}
		</li>
	{/each}
	<li class="j8f-st j8f-result" class:empty={callable === 0}>
		<span class="j8f-top"><span class="j8f-label">Toolset</span><span class="j8f-who">callable</span></span>
		<span class="j8f-n mono">{callable}</span>
		<span class="j8f-none">then checked per call (N5)</span>
	</li>
</ol>

<style>
	.j8f {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(140px, 1fr));
		gap: 8px;
	}
	.j8f-st {
		display: grid;
		align-content: start;
		gap: 4px;
		padding: 8px 10px;
		border: 1px solid var(--line-2);
		border-radius: 6px;
		background: var(--panel);
		min-width: 0;
	}
	.j8f-st.cutting {
		border-color: color-mix(in srgb, var(--warn) 45%, var(--line));
	}
	.j8f-st.k-narrowed.cutting {
		border-color: var(--cust);
		background: color-mix(in srgb, var(--cust) 6%, var(--panel));
	}
	.j8f-top {
		display: flex;
		flex-direction: column;
		gap: 1px;
	}
	.j8f-label {
		font-weight: 600;
		font-size: 12.5px;
	}
	.j8f-who {
		font-size: 10px;
		text-transform: uppercase;
		letter-spacing: 0.05em;
		color: var(--faint);
		white-space: nowrap;
	}
	.j8f-n {
		font-size: 20px;
		font-weight: 600;
		line-height: 1.1;
	}
	.j8f-bar {
		height: 4px;
		border-radius: 2px;
		background: var(--line-2);
		overflow: hidden;
	}
	.j8f-bar i {
		display: block;
		height: 100%;
		background: var(--accent);
	}
	.j8f-cut {
		display: flex;
		flex-wrap: wrap;
		gap: 3px 6px;
		font-size: 11.5px;
	}
	.j8f-minus {
		font-weight: 600;
		color: var(--warn);
	}
	.k-narrowed .j8f-minus {
		color: var(--cust);
	}
	.j8f-op {
		color: var(--muted);
		text-decoration: line-through;
		text-decoration-color: color-mix(in srgb, var(--warn) 60%, transparent);
		overflow-wrap: anywhere;
	}
	.j8f-none {
		font-size: 11.5px;
		color: var(--faint);
	}
	.j8f-result {
		border-color: color-mix(in srgb, var(--ok) 55%, var(--line));
		background: color-mix(in srgb, var(--ok) 7%, var(--panel));
	}
	.j8f-result .j8f-n {
		color: var(--ok);
	}
	.j8f-result.empty {
		border-color: var(--line-2);
		background: var(--panel);
	}
	.j8f-result.empty .j8f-n {
		color: var(--faint);
	}
</style>
