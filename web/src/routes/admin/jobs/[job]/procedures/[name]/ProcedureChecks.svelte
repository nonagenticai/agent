<script lang="ts">
	// J12 (B5 T7a, F-15, F-20): what the runtime checks around the selected version — per step its preconditions
	// (before the call), postconditions (after an ok answer), who may run it and its per-case cap (X/P only); for
	// the version its success criteria (after the last phase) and the tools it needs routed. A version is immutable
	// once created: change these in a new version. The PDDL domain is stored, never interpreted.
	import Chip from '$lib/ui/Chip.svelte';
	import type { ProcedureView } from '$lib/api/types/ontology';
	import { conditionText, takesCap } from '$lib/ontology/conditions';

	let { sel }: { sel: ProcedureView } = $props();

	const steps = $derived(
		sel.steps.filter(
			(s) =>
				(s.preconditions?.length ?? 0) + (s.postconditions?.length ?? 0) > 0 ||
				!!s.performed_by ||
				(takesCap(s.side_effect) && s.max_per_case != null),
		),
	);
	const success = $derived(sel.success_criteria ?? []);
	const tools = $derived(sel.tool_requirements ?? []);
	const capped = $derived(sel.steps.filter((s) => takesCap(s.side_effect)));
</script>

<section class="pc" data-testid="procedure-checks" aria-label="Checks">
	<h3>Checks</h3>
	{#if sel.origin === 'base'}
		<p class="muted">Template versions carry no step checks. Start a tenant version to add them.</p>
	{:else}
		<dl class="pc-v">
			<dt>Success</dt>
			<dd>
				{#each success as c (c.id)}
					<Chip text={conditionText(c)} variant="ok" title={c.id} />
				{:else}
					<span class="muted">None: the case resolves when the last phase finished.</span>
				{/each}
			</dd>
			<dt>Needs tools</dt>
			<dd>
				{#each tools as t (t)}
					<Chip text={t} variant="tool" />
				{:else}
					<span class="muted">None declared.</span>
				{/each}
			</dd>
		</dl>
		{#if steps.length}
			<div class="tbl-wrap">
				<table class="tbl dense">
					<thead
						><tr
							><th>#</th><th>Step</th><th>Before</th><th>After</th><th>Performed by</th><th>Max per case</th
							></tr
						></thead
					>
					<tbody>
						{#each steps as s (s.step_number)}
							<tr>
								<td class="r">{s.step_number}</td>
								<td>{s.name}</td>
								<td
									>{#each s.preconditions ?? [] as c (c.id)}<div class="mono">
											{conditionText(c)}
										</div>{:else}<span class="muted">—</span>{/each}</td
								>
								<td
									>{#each s.postconditions ?? [] as c (c.id)}<div class="mono">
											{conditionText(c)}
										</div>{:else}<span class="muted">—</span>{/each}</td
								>
								<td>{s.performed_by ?? 'procedure'}</td>
								<td class="r">{takesCap(s.side_effect) ? (s.max_per_case ?? 1) : '—'}</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		{:else}
			<p class="muted">
				No step conditions.{capped.length
					? ` ${capped.map((s) => s.name).join(', ')} run${capped.length === 1 ? 's' : ''} at most once per case.`
					: ''}
			</p>
		{/if}
		{#if sel.pddl_domain}
			<details class="pc-pddl">
				<summary>PDDL domain (stored as written, not interpreted)</summary>
				<pre class="mono">{sel.pddl_domain}</pre>
			</details>
		{/if}
	{/if}
</section>

<style>
	.pc {
		margin-top: 16px;
		display: grid;
		grid-template-columns: minmax(0, 1fr);
		gap: 8px;
	}
	h3 {
		margin: 0;
		font: 600 13px/1.3 var(--cond);
		letter-spacing: 0.06em;
		text-transform: uppercase;
		color: var(--muted);
	}
	.pc-v {
		display: grid;
		grid-template-columns: max-content minmax(0, 1fr);
		gap: 6px 12px;
		margin: 0;
	}
	.pc-v dt {
		color: var(--muted);
		font-size: 12px;
	}
	.pc-v dd {
		margin: 0;
		display: flex;
		flex-wrap: wrap;
		gap: 4px;
	}
	.pc-pddl pre {
		white-space: pre-wrap;
		max-height: 240px;
		overflow: auto;
	}
</style>
