<script lang="ts">
	// T11 upgrade decision: what re-pinning this system to the newest active template version changes for THIS
	// tenant (P17 diff vocabulary), whether it is safe (an op a job routes here must not disappear), and the act itself.
	// Instances keep their pin until the tenant re-pins; re-pinning re-runs the contract tests (doc 11 §6.1 rule 4).
	import Chip from '$lib/ui/Chip.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { writeAttrs } from '$lib/auth/roles';
	import type { SystemDetail, SystemInsight } from '$lib/api/types/connectors';

	let {
		d,
		insight,
		writable,
		repinning,
		onrepin,
	}: {
		d: SystemDetail;
		insight: SystemInsight | null;
		writable: boolean;
		repinning: boolean;
		onrepin: (version: string) => void;
	} = $props();

	const i = $derived(d.instance);
	const up = $derived(insight?.upgrade ?? null);
	const changed = $derived(up ? up.ops.filter((o) => o.change !== 'same') : []);
	const routedHere = $derived(new Set(d.used_by.flatMap((u) => u.ops)));
	const CHANGE = {
		added: ['added', 'ok'],
		removed: ['removed', 'err'],
		changed: ['changed', 'accent'],
		same: ['same', 'muted'],
	} as const;
</script>

{#if i.platform}
	<p class="muted">
		Platform connectors are provisioned down by Agent releases; their version moves with the platform, not
		with a re-pin here.
	</p>
{:else if !up && d.newer_version}
	<div class="split">
		<p class="muted">@{d.newer_version} is active. No diff is available for this template.</p>
		<button
			class="btn"
			type="button"
			{...writeAttrs(session, 'integrations_manager')}
			disabled={!writable || repinning}
			onclick={() => onrepin(d.newer_version!)}
			>{repinning ? 'Re-pinning…' : `Re-pin to @${d.newer_version}`}</button
		>
	</div>
{:else if !up}
	<p class="muted">
		{i.template_key}@{i.template_version} is the newest active version. When the platform publishes a newer one,
		it shows up here with exactly what changes for this system.
	</p>
{:else}
	<div class="t11-up">
		<div class="t11-lineage" aria-label="Version lineage">
			<span class="t11-ver t11-cur">
				<span class="label">Pinned now</span>
				<span class="mono">@{up.from}</span>
			</span>
			<span class="t11-arrow" aria-hidden="true">→</span>
			<span class="t11-ver t11-next">
				<span class="label">Newest active</span>
				<span class="mono">@{up.to}</span>
			</span>
			<span class="t11-counts">
				<Chip text="{up.counts.changed} changed" variant={up.counts.changed ? 'accent' : 'muted'} />
				{#if up.counts.added}<Chip text="{up.counts.added} added" variant="ok" />{/if}
				{#if up.counts.removed}<Chip text="{up.counts.removed} removed" variant="err" />{/if}
				<Chip text="{up.counts.same} unchanged" variant="muted" />
			</span>
		</div>

		{#if up.verdict.kind === 'blocked'}
			<Callout kind="err" title="Re-pinning would strand a route">
				{up.verdict.reasons.join('; ')}. Re-route those operations first (Operations &amp; routes), then
				re-pin.
			</Callout>
		{:else}
			<Callout kind="info" title="Safe to re-pin">
				No operation a job routes here disappears. {up.verdict.warnings.join('. ')}. Until the re-run is
				green, the re-tested operations are not ready.
			</Callout>
		{/if}

		{#if up.meta.length}
			<h3 class="label t11-h">Template-level changes</h3>
			<div class="tbl-wrap">
				<table class="tbl dense">
					<thead><tr><th>What</th><th>@{up.from}</th><th>@{up.to}</th></tr></thead>
					<tbody>
						{#each up.meta as m (m.label)}
							<tr>
								<td>{m.label}</td>
								<td class="t11-before">{m.before}</td>
								<td>{m.after}</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
			{#if up.meta.some((m) => m.label.startsWith('Webhooks'))}
				<p class="note">
					Webhooks stay inert in v1: the agent pulls and verifies; nothing is delivered to it.
				</p>
			{/if}
		{/if}

		<h3 class="label t11-h">Operation changes</h3>
		{#if changed.length === 0}
			<p class="muted">No operation changes — only the template-level lines above.</p>
		{:else}
			<ul class="list-plain">
				{#each changed as o (o.op)}
					<li class="t11-opchange">
						<span class="row">
							<span class="mono">{o.op}</span>
							<Chip text={CHANGE[o.change][0]} variant={CHANGE[o.change][1]} />
							{#if routedHere.has(o.op)}<Chip text="routed here" variant="muted" />{/if}
						</span>
						{#if o.details.length}
							<ul class="t11-details">
								{#each o.details as line (line)}<li class="mono">{line}</li>{/each}
							</ul>
						{/if}
					</li>
				{/each}
			</ul>
		{/if}

		<div class="actions t11-act">
			<button
				class="btn primary"
				type="button"
				{...writeAttrs(session, 'integrations_manager')}
				disabled={!writable || repinning || up.verdict.kind === 'blocked'}
				onclick={() => onrepin(up.to)}>{repinning ? 'Re-pinning…' : `Re-pin to @${up.to}`}</button
			>
			<span class="muted t11-small">Stay on @{up.from}: nothing changes until you re-pin.</span>
		</div>
	</div>
{/if}

<style>
	.t11-up {
		display: grid;
		gap: 12px;
	}
	.t11-lineage {
		display: flex;
		align-items: center;
		gap: 12px;
		flex-wrap: wrap;
	}
	.t11-ver {
		display: grid;
		gap: 2px;
		padding: 8px 12px;
		border: 1px solid var(--line);
		border-radius: var(--r);
		background: var(--sunk);
	}
	.t11-next {
		border-color: var(--accent);
	}
	.t11-arrow {
		color: var(--faint);
	}
	.t11-counts {
		display: flex;
		gap: 6px;
		flex-wrap: wrap;
	}
	.t11-h {
		margin: 6px 0 0;
	}
	.t11-before {
		color: var(--muted);
	}
	.t11-opchange {
		display: grid;
		gap: 4px;
		justify-content: stretch;
	}
	.t11-details {
		margin: 0;
		padding-left: 18px;
		font-size: 12px;
		color: var(--muted);
	}
	.t11-act {
		margin-top: 4px;
	}
	.t11-small {
		font-size: 12px;
	}
</style>
