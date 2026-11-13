<script lang="ts">
	// P15 spec 12 §2: an agent's toolset is a chain of narrowings N1…N6. This page owns N3; the strip says where it
	// sits and links to the pages that own the other links of the chain.
	let { job }: { job: string } = $props();
	const jobBase = $derived(`/admin/jobs/${encodeURIComponent(job)}`);
	const steps = $derived([
		{
			n: 'N1',
			label: 'Vocabulary',
			who: 'platform',
			text: 'object.verb, side effect, identity level',
			href: '/platform/operations',
		},
		{
			n: 'N2',
			label: 'Job expects',
			who: 'template',
			text: 'R/O ops per activity',
			href: `${jobBase}/operations`,
		},
		{ n: 'N3', label: 'Role grant', who: 'this page', text: 'what each principal may call', href: null },
		{
			n: 'N4',
			label: 'Tenant route',
			who: 'tenant admin',
			text: 'connector or fallback',
			href: `${jobBase}/operations`,
		},
		{
			n: 'N5',
			label: 'Case state',
			who: 'runner, per call',
			text: 'identity level, confirm-back',
			href: null,
		},
		{
			n: 'N6',
			label: 'Turn scope',
			who: 'gate verdict',
			text: 'hint only, never widens',
			href: '/platform/gates',
		},
	]);
</script>

<nav class="nc" aria-label="Narrowing chain">
	<ol class="nc-list">
		{#each steps as s (s.n)}
			<li class="nc-step" class:here={s.n === 'N3'} aria-current={s.n === 'N3' ? 'step' : undefined}>
				<span class="nc-n mono">{s.n}</span>
				<span class="nc-body">
					{#if s.href}<a class="nc-label" href={s.href}>{s.label}</a>{:else}<span class="nc-label"
							>{s.label}</span
						>{/if}
					<span class="nc-text">{s.text}</span>
					<span class="nc-who">{s.who}</span>
				</span>
			</li>
		{/each}
	</ol>
	<p class="nc-foot">
		Toolset = N2 ∩ <strong>N3</strong> ∩ N4, filtered per call by N5. A grant here is necessary, never
		sufficient — and it is the widest any tenant gets: tenants can only narrow it (see a job's
		<a href="{jobBase}/toolsets">Toolsets</a>).
	</p>
</nav>

<style>
	.nc {
		margin-bottom: 14px;
	}
	.nc-list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		grid-template-columns: repeat(6, minmax(0, 1fr));
		gap: 6px;
	}
	.nc-step {
		display: flex;
		gap: 8px;
		padding: 8px 10px;
		border: 1px solid var(--line-2);
		border-radius: var(--r);
		background: var(--panel);
		min-width: 0;
	}
	.nc-step.here {
		border-color: var(--accent);
		background: color-mix(in srgb, var(--accent) 9%, var(--panel));
	}
	.nc-n {
		font-weight: 600;
		color: var(--faint);
	}
	.here .nc-n {
		color: var(--accent);
	}
	.nc-body {
		display: grid;
		gap: 2px;
		min-width: 0;
	}
	.nc-label {
		font-weight: 600;
		font-size: 13px;
	}
	.nc-text {
		font-size: 12px;
		color: var(--muted);
	}
	.nc-who {
		font-size: 11px;
		color: var(--faint);
		text-transform: uppercase;
		letter-spacing: 0.04em;
	}
	.nc-foot {
		font-size: 12.5px;
		color: var(--muted);
		margin: 8px 0 0;
	}
	@media (max-width: 1100px) {
		.nc-list {
			grid-template-columns: repeat(3, minmax(0, 1fr));
		}
	}
	@media (max-width: 560px) {
		.nc-list {
			grid-template-columns: repeat(2, minmax(0, 1fr));
		}
	}
</style>
