<script lang="ts">
	// T11: how this pin translates the system's vocabulary into the canonical one — statuses per object, and vendor
	// faults into canonical error codes the agent either answers (user outcome) or escalates (system error).
	import Chip from '$lib/ui/Chip.svelte';
	import { faultMap, statusMapsByObject } from '$lib/connectors/T11-health';
	import type { ErrorHandling, SystemInsight } from '$lib/api/types/connectors';

	let { insight, vendor }: { insight: SystemInsight; vendor: string } = $props();
	const statuses = $derived(statusMapsByObject(insight));
	const faults = $derived(faultMap(insight));
	const HANDLING: Record<ErrorHandling, [string, 'ok' | 'err' | 'warn']> = {
		reply: ['agent replies', 'ok'],
		escalate: ['escalates', 'err'],
		unlisted: ['escalates · not in taxonomy', 'warn'],
	};
</script>

<div class="grid g2 t11-maps">
	<div>
		<h3 class="label">Status mapping</h3>
		<p class="note t11-lead">
			{vendor || 'The system'}'s statuses as the canonical object states the agent reasons about. Set by the
			template; not overridable per tenant.
		</p>
		{#if statuses.length === 0}
			<p class="muted">This template maps no statuses.</p>
		{:else}
			{#each statuses as g (g.object)}
				<div class="t11-obj">
					<span class="t11-objname">{g.object}</span>
					<span class="t11-sub">{g.ops.join(', ')}</span>
					<ul class="t11-map">
						{#each g.map as m (m.system + m.canonical)}
							<li>
								<span class="mono t11-sys">{m.system}</span><span class="t11-to" aria-hidden="true">→</span
								><Chip text={m.canonical} variant="cust" />
							</li>
						{/each}
					</ul>
				</div>
			{/each}
		{/if}
	</div>
	<div>
		<h3 class="label">Fault mapping</h3>
		<p class="note t11-lead">
			Vendor faults become canonical errors. A user outcome gets an answer; a system error hands over to a
			person.
		</p>
		{#if faults.length === 0}
			<p class="muted">No fault map — failures surface as the transport's generic errors.</p>
		{:else}
			<ul class="t11-map">
				{#each faults as f (f.system + f.code)}
					<li>
						<span class="mono t11-sys">{f.system}</span><span class="t11-to" aria-hidden="true">→</span><span
							class="mono">{f.code}</span
						>
						<Chip text={HANDLING[f.handling][0]} variant={HANDLING[f.handling][1]} />
					</li>
				{/each}
			</ul>
		{/if}
	</div>
</div>

<style>
	.t11-maps {
		margin-top: 4px;
	}
	.t11-lead {
		margin: 4px 0 10px;
	}
	.t11-obj {
		padding: 8px 0;
		border-top: 1px solid var(--line-2);
	}
	.t11-objname {
		font-weight: 600;
	}
	.t11-sub {
		display: block;
		color: var(--muted);
		font-size: 12px;
	}
	.t11-map {
		list-style: none;
		margin: 6px 0 0;
		padding: 0;
		display: grid;
		gap: 4px;
	}
	.t11-map li {
		display: flex;
		gap: 6px;
		align-items: center;
		flex-wrap: wrap;
		font-size: 12.5px;
	}
	.t11-sys {
		min-width: 0;
		overflow-wrap: anywhere;
	}
	.t11-to {
		color: var(--faint);
	}
</style>
