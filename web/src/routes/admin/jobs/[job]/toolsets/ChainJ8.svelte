<script lang="ts">
	// J8: the narrowing chain of spec 12 §2 as this page sees it. N2–N4 are intersected here (N5 is shown per
	// operation as the per-call checks); each link says who owns it, so the tenant sees what it inherits from the
	// templates (read-only) and what it controls (narrowings, routes).
	let { job, superAdmin }: { job: string; superAdmin: boolean } = $props();
	const jobBase = $derived(`/admin/jobs/${encodeURIComponent(job)}`);
	type Owner = 'platform' | 'template' | 'tenant' | 'runtime';
	const steps = $derived<
		{ n: string; label: string; owner: Owner; text: string; href: string | null; here: boolean }[]
	>([
		{
			n: 'N1',
			label: 'Vocabulary',
			owner: 'platform',
			text: 'object.verb · side effect · identity level',
			href: superAdmin ? '/platform/operations' : null,
			here: false,
		},
		{
			n: 'N2',
			label: 'Activity expects',
			owner: 'template',
			text: 'R/O operations per activity',
			href: `${jobBase}/journey`,
			here: true,
		},
		{
			n: 'N3',
			label: 'Role grants',
			owner: 'template',
			text: 'what each role may call; you can only remove',
			href: superAdmin ? '/platform/agent-roles' : null,
			here: true,
		},
		{
			n: 'N4',
			label: 'Tenant route',
			owner: 'tenant',
			text: 'a connector, built-in or fallback per env',
			href: `${jobBase}/operations`,
			here: true,
		},
		{
			n: 'N5',
			label: 'Per call',
			owner: 'runtime',
			text: 'identity level · confirm-back · hosted link',
			href: null,
			here: true,
		},
		{
			n: 'N6',
			label: 'Turn scope',
			owner: 'runtime',
			text: 'gate verdict hint; never widens',
			href: `${jobBase}/gates`,
			here: false,
		},
	]);
	const OWNER: Record<Owner, string> = {
		platform: 'platform',
		template: 'template · inherited',
		tenant: 'you · live',
		runtime: 'runner · per call',
	};
</script>

<nav class="j8c" aria-label="Narrowing chain">
	<ol class="j8c-list">
		{#each steps as s, i (s.n)}
			<li class="j8c-step o-{s.owner}" class:here={s.here}>
				<span class="j8c-n mono">{s.n}</span>
				<span class="j8c-body">
					{#if s.href}<a class="j8c-label" href={s.href}>{s.label}</a>{:else}<span class="j8c-label"
							>{s.label}</span
						>{/if}
					<span class="j8c-text">{s.text}</span>
					<span class="j8c-who">{OWNER[s.owner]}</span>
				</span>
				{#if i < steps.length - 1}<span class="j8c-op" aria-hidden="true"
						>{i === 0 ? '→' : i < 3 ? '∩' : '→'}</span
					>{/if}
			</li>
		{/each}
	</ol>
	<p class="j8c-foot">
		<strong>Toolset = N2 ∩ N3 ∩ N4</strong>, then every call is checked again against N5. Nothing on this page
		is configured by hand: it is computed from the template and your narrowings and routes, so the only ways
		to change it are to <em>remove</em> a grant here or change a route on
		<a href="{jobBase}/operations">Operations &amp; routes</a>.
	</p>
</nav>

<style>
	.j8c {
		margin-bottom: 14px;
	}
	.j8c-list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		grid-template-columns: repeat(6, minmax(0, 1fr));
		gap: 14px;
	}
	.j8c-step {
		position: relative;
		display: flex;
		gap: 8px;
		padding: 8px 10px;
		border: 1px solid var(--line-2);
		border-radius: var(--r);
		background: var(--panel);
		min-width: 0;
		opacity: 0.75;
	}
	.j8c-step.here {
		opacity: 1;
		border-color: color-mix(in srgb, var(--accent) 45%, var(--line));
	}
	.j8c-step.here.o-tenant {
		border-color: var(--cust);
		background: color-mix(in srgb, var(--cust) 7%, var(--panel));
	}
	.j8c-n {
		font-weight: 600;
		color: var(--faint);
	}
	.here .j8c-n {
		color: var(--accent);
	}
	.o-tenant .j8c-n,
	.o-tenant .j8c-who {
		color: var(--cust);
	}
	.j8c-body {
		display: grid;
		gap: 2px;
		min-width: 0;
	}
	.j8c-label {
		font-weight: 600;
		font-size: 13px;
	}
	.j8c-text {
		font-size: 12px;
		color: var(--muted);
	}
	.j8c-who {
		font-size: 10.5px;
		color: var(--faint);
		text-transform: uppercase;
		letter-spacing: 0.05em;
	}
	.j8c-op {
		position: absolute;
		right: -12px;
		top: 50%;
		transform: translateY(-50%);
		width: 10px;
		text-align: center;
		font-weight: 600;
		color: var(--faint);
	}
	.j8c-foot {
		font-size: 12.5px;
		color: var(--muted);
		margin: 8px 0 0;
		max-width: 92ch;
	}
	@media (max-width: 1100px) {
		.j8c-list {
			grid-template-columns: repeat(3, minmax(0, 1fr));
		}
		.j8c-step:nth-child(3n) .j8c-op {
			display: none;
		}
	}
	@media (max-width: 560px) {
		.j8c-list {
			grid-template-columns: repeat(2, minmax(0, 1fr));
		}
		.j8c-step .j8c-op {
			display: none;
		}
	}
</style>
