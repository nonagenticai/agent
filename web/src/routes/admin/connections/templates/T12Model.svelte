<script lang="ts">
	// T12 concept strip: where a tenant template sits. A system is connected from a platform template (curated by
	// the platform, use it first) OR from one of your own templates (D31, on generic REST); either way a system pins
	// one version, and job routes send operations to that system. Doc 12 §4, doc 10 §4.2.
	import type { TenantTemplateUsage } from '$lib/api/types/connectors';

	let { usage, families, live }: { usage: TenantTemplateUsage | null; families: number; live: number } =
		$props();

	// Third-party vendors first: the platform's own (`Agent …`) templates are the sandbox and the generic REST base.
	const vendors = $derived(
		[...new Set((usage?.platform ?? []).map((p) => p.vendor))].filter((v) => !v.startsWith('Agent')),
	);
	const jobs = $derived([...new Map((usage?.routes ?? []).map((r) => [r.job, r.job_label])).entries()]);
</script>

<section class="t12m" data-testid="t12-model" aria-label="Where your templates fit">
	<div class="t12m-src">
		<a class="t12m-node t12m-plat" href="/admin/connections/systems">
			<span class="t12m-k">Platform catalogue · use first</span>
			<strong>{usage ? usage.platform.length : '…'} curated templates</strong>
			<span class="t12m-d"
				>{vendors.slice(0, 4).join(', ')}{vendors.length > 4 ? ` +${vendors.length - 4}` : ''} — tested and maintained
				for you. Connect them on Systems.</span
			>
		</a>
		<span class="t12m-or" aria-hidden="true">or</span>
		<div class="t12m-node t12m-own" aria-current="page">
			<span class="t12m-k">Your templates · this page</span>
			<strong>{families} template{families === 1 ? '' : 's'}, {live} active</strong>
			<span class="t12m-d"
				>For an in-house or bespoke system nothing in the catalogue covers. Built on <span class="mono"
					>generic_rest@2</span
				>.</span
			>
		</div>
	</div>
	<span class="t12m-arrow" aria-hidden="true"></span>
	<a class="t12m-node" href="/admin/connections/systems">
		<span class="t12m-k">Systems</span>
		<strong
			>{usage ? usage.pins.length : '…'} system{usage?.pins.length === 1 ? ' pins' : 's pin'} a version of yours</strong
		>
		<span class="t12m-d"
			>Base URL + write-only credentials per system. A system never floats to a newer version.</span
		>
	</a>
	<span class="t12m-arrow" aria-hidden="true"></span>
	<div class="t12m-node">
		<span class="t12m-k">Job routes</span>
		<strong>{usage ? usage.routes.length : '…'} routes served</strong>
		<span class="t12m-d">
			{#if jobs.length}
				{#each jobs as [key, label], i (key)}{i ? ', ' : ''}<a href="/admin/jobs/{key}/operations">{label}</a
					>{/each}
				send operations to those systems.
			{:else}
				No job routes to your templates yet.
			{/if}
		</span>
	</div>
</section>

<style>
	.t12m {
		display: grid;
		grid-template-columns: minmax(0, 1.25fr) 22px minmax(0, 1fr) 22px minmax(0, 1fr);
		align-items: center;
		gap: 8px;
		border: 1px solid var(--line);
		background: var(--panel);
		border-radius: var(--r);
		padding: 14px;
		margin-bottom: 16px;
	}
	.t12m-src {
		display: grid;
		gap: 6px;
	}
	.t12m-node {
		display: flex;
		flex-direction: column;
		gap: 3px;
		padding: 10px 12px;
		border-radius: 8px;
		background: var(--sunk);
		border: 1px solid var(--line);
		min-width: 0;
		color: inherit;
		text-decoration: none;
	}
	a.t12m-node:hover {
		border-color: color-mix(in srgb, var(--accent) 45%, var(--line));
	}
	.t12m-plat {
		border-style: dashed;
	}
	.t12m-own {
		border-color: color-mix(in srgb, var(--accent) 55%, var(--line));
		background: color-mix(in srgb, var(--accent) 8%, var(--panel));
	}
	.t12m-or {
		font-size: 11px;
		text-transform: uppercase;
		letter-spacing: 0.08em;
		color: var(--faint);
		text-align: center;
	}
	.t12m-k {
		font-size: 11px;
		letter-spacing: 0.06em;
		text-transform: uppercase;
		color: var(--muted);
	}
	.t12m-node strong {
		font-size: 13.5px;
	}
	.t12m-d {
		font-size: 12px;
		color: var(--muted);
		line-height: 1.4;
	}
	.t12m-arrow::before {
		content: '→';
		color: var(--faint);
		font-size: 16px;
		display: block;
		text-align: center;
	}
	@media (max-width: 860px) {
		.t12m {
			grid-template-columns: minmax(0, 1fr);
		}
		.t12m-arrow::before {
			content: '↓';
		}
	}
</style>
