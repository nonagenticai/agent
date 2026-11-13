<script lang="ts">
	// J14/J15 rollback impact preview (05 §4.4 + owner 2026-10-19 "implement rollback properly"). Rolling back makes an
	// earlier release current again — its WHOLE design: selection, template pins AND the tenant design changes it had
	// live. Before anything happens this shows what customers lose and get back, how many published changes are
	// undone (each stays in the history and can be restored to the draft in one click afterwards), which unpublished
	// changes now sit on a different live value (the draft itself is untouched), and any readiness gap apply would
	// refuse on. Confirm applies the release.
	import { onMount } from 'svelte';
	import { api, guardOutcome } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { writeAttrs } from '$lib/auth/roles';
	import { Resource } from '$lib/load.svelte';
	import type { ApplyResult, RollbackDirection, RollbackPreview } from '$lib/api/types/release';
	import { customerImpact } from '$lib/release/J15-detail';
	import Callout from '$lib/ui/Callout.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';

	let {
		id,
		ondone,
		oncancel,
		ondirection,
	}: {
		id: string;
		ondone: (r: ApplyResult | null) => void;
		oncancel: () => void;
		/** The loaded preview's direction (rollback / re-apply / apply), for the host drawer's title. */
		ondirection?: (d: RollbackDirection) => void;
	} = $props();

	const preview = new Resource<RollbackPreview>(
		() => api().rollbackPreview(id),
		() => false,
	);
	onMount(() => preview.refresh());
	$effect(() => {
		const v = preview.value;
		if (v.state === 'ok' || v.state === 'empty') ondirection?.(v.data.direction);
	});

	const wa = $derived(writeAttrs(session, 'release_manager'));
	let busy = $state(false);
	const plural = (n: number, w: string) => `${n} ${w}${n === 1 ? '' : 's'}`;

	async function confirm(p: RollbackPreview) {
		if (busy) return;
		busy = true;
		let res: ApplyResult | null = null;
		const out = await guardOutcome(
			async () => (res = await api().applyRelease(id)),
			() =>
				p.direction === 'rollback'
					? `Rolled back to ${id}. ${plural(res?.rolled_back ?? 0, 'published change')} undone; restore them to the draft from Review & publish if you want them back.`
					: `${id} is live again.${res?.reapplied ? ` ${plural(res.reapplied, 'change')} back in Production.` : ''}`,
			p.direction === 'rollback' ? 'Not rolled back' : 'Not applied',
		);
		busy = false;
		ondone(out.kind === 'ok' ? res : null);
	}
</script>

<LoadState resource={preview}>
	{#snippet children(p)}
		{@const impact = customerImpact(p.bundle_after, p.bundle_now)}
		{@const back = p.direction === 'rollback'}
		<div class="rb-wrap" data-testid="rollback-preview" data-direction={p.direction}>
			<p class="rb-flow">
				<span class="mono">{p.current ?? 'nothing live'}</span>
				<span aria-hidden="true">→</span>
				<b class="mono">{p.target}</b>
				<Chip text={back ? 'Rollback' : 'Re-apply'} variant={back ? 'warn' : 'accent'} />
			</p>
			<p class="rb-lead">
				{back
					? `Production goes back to ${p.target} as it was: its selection, template versions and your design values.`
					: `Production gets ${p.target} back as it was cut: its selection, template versions and design values.`}
				<b data-testid="rollback-count"
					>{plural(p.design_changes, 'design place')} change{p.design_changes === 1 ? 's' : ''} live.</b
				>
			</p>

			{#if p.missing.length}
				<Callout kind="err" title="Apply would refuse: 409 release_not_ready">
					{p.target} needs
					{#each p.missing as m, i (m.dependency + m.slot)}{i ? ', ' : ''}<span class="mono"
							>{m.dependency}.{m.slot}</span
						>{/each}. Fix {p.missing.length === 1 ? 'it' : 'them'} first (Readiness), then roll back.
				</Callout>
			{/if}

			<section class="rb-sec" data-testid="rollback-undone">
				<h3>
					{back ? 'Customers lose' : 'Customers get back'}
					<span class="muted small"
						>({plural((back ? p.undone : p.reapplied).length, 'published change')})</span
					>
				</h3>
				{#if (back ? p.undone : p.reapplied).length}
					<ul class="rb-list">
						{#each back ? p.undone : p.reapplied as c (c.id)}
							<li data-change={c.id}>
								<span class="rb-label">{c.label}</span>
								<span class="muted small">published in <span class="mono">{c.published_in}</span></span>
								<span class="rb-diff small">
									<span class="rb-now"><span class="rb-tag">Now</span>{c.now ?? 'not set'}</span>
									<span class="rb-after"><span class="rb-tag">After</span>{c.after ?? 'not set'}</span>
								</span>
							</li>
						{/each}
					</ul>
					{#if back}
						<p class="muted small">
							Undone changes stay in the history, marked “rolled back”. Afterwards you can restore them to the
							draft in one click and publish them again.
						</p>
					{/if}
				{:else}
					<p class="muted small">
						{back
							? `No design change of yours was published after ${p.target}.`
							: 'No rolled-back change comes back with it.'}
					</p>
				{/if}
			</section>

			<section class="rb-sec">
				<h3>Selection and template</h3>
				{#if impact.length || p.selection_change || p.pins.length}
					<ul class="rb-list plain small" data-testid="rollback-template">
						{#each impact as l (l.section + l.item)}
							<li class="rb-{l.tone}">{l.tone === 'add' ? '+' : '−'} {l.text}</li>
						{/each}
						{#if p.selection_change}<li>Selection: {p.selection_change}</li>{/if}
						{#each p.pins as x (x.layer)}
							<li><span class="mono">{x.layer}</span> v{x.from ?? '—'} → <b>v{x.to}</b></li>
						{/each}
					</ul>
				{:else}
					<p class="muted small">Same selection and template versions.</p>
				{/if}
			</section>

			<section class="rb-sec" data-testid="rollback-draft">
				<h3>Your draft</h3>
				<p class="small">
					Untouched: unpublished changes keep their draft value; the test chat runs exactly what it ran
					before.
				</p>
				{#if p.draft.length}
					<p class="small">
						{plural(p.draft.length, 'unpublished change')} will sit on a different live value:
					</p>
					<ul class="rb-list">
						{#each p.draft as d (d.id)}
							<li data-draft={d.id}>
								<span class="rb-label">{d.label}</span>
								<span class="small"
									>live {d.live_now ?? 'not set'} → <b>{d.live_after ?? 'not set'}</b> · draft {d.draft ??
										'removed'}</span
								>
								{#if d.matches_live}<Chip
										text="Becomes live with it"
										variant="ok"
										title="Your draft value is what this release has here: nothing left to publish, so it leaves the draft list"
									/>{/if}
							</li>
						{/each}
					</ul>
				{/if}
			</section>

			<div class="rb-actions">
				<button
					class="btn primary"
					type="button"
					data-testid="rollback-confirm"
					disabled={wa.disabled || busy || p.missing.length > 0}
					title={wa.title}
					onclick={() => confirm(p)}>{back ? `Roll back to ${p.target}` : `Apply ${p.target}`}</button
				>
				<button class="btn" type="button" onclick={oncancel}>Cancel</button>
				<span class="muted small">Audited: one event for the rollback, one per change it undoes.</span>
			</div>
		</div>
	{/snippet}
</LoadState>

<style>
	.rb-wrap {
		display: flex;
		flex-direction: column;
		gap: 14px;
		min-width: 0;
	}
	.rb-flow {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: 8px;
		margin: 0;
		font-size: 15px;
	}
	.rb-lead {
		margin: 0;
	}
	.rb-sec h3 {
		font-size: 13px;
		margin: 0 0 6px;
	}
	.rb-list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 8px;
	}
	.rb-list > li {
		display: flex;
		flex-direction: column;
		gap: 3px;
		padding: 8px 10px;
		border: 1px solid var(--line);
		border-radius: 6px;
		background: var(--panel);
		min-width: 0;
		overflow-wrap: anywhere;
	}
	.rb-list.plain > li {
		display: block;
		padding: 2px 0;
		border: 0;
		background: none;
	}
	.rb-label {
		font-weight: 600;
	}
	.rb-diff {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
	}
	.rb-diff > span {
		padding: 2px 6px;
		border-radius: 4px;
		border: 1px solid var(--line);
	}
	.rb-now {
		border-left: 3px solid var(--err) !important;
	}
	.rb-after {
		border-left: 3px solid var(--ok) !important;
	}
	.rb-tag {
		font-size: 10px;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		color: var(--muted);
		margin-right: 6px;
	}
	.rb-add {
		color: var(--ok);
	}
	.rb-remove {
		color: var(--err);
	}
	.rb-actions {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 8px;
	}
	.small {
		font-size: 12px;
	}
</style>
