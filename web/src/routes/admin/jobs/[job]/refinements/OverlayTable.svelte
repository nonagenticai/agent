<script lang="ts">
	// One refinable object, base (release) beside yours (from cb refinements/OverlayTable.svelte@45ee128).
	import { onMount } from 'svelte';
	import { api, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { writeAttrs } from '$lib/auth/roles';
	import { Resource } from '$lib/load.svelte';
	import type { DraftChange, OverlayLayers, RefinementOp, ReleaseConflict } from '$lib/api/types/release';
	import { draftAt, overlayDraftKey } from '$lib/release/J13-workspace';
	import {
		ACTION_DONE,
		ACTION_LABEL,
		STATE_LABEL,
		STATE_VARIANT,
		actionsFor,
		sideBySide,
		summaryOf,
		type SideObject,
		type SideRow,
	} from '$lib/release/refinements';
	import { KIND_LABEL } from '$lib/release/conflicts';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Freshness from '$lib/ui/Freshness.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';

	let {
		object,
		conflicts,
		drafts = [],
		onchange,
		edit,
		conflictsHref,
	}: {
		object: SideObject;
		conflicts: ReleaseConflict[];
		/** The job's unpublished changes (D54): rows they touch are marked Draft with their live value. */
		drafts?: DraftChange[];
		/** After a refinement write: the page re-reads the job (draft_rev) and the conflicts. */
		onchange: () => void;
		/** Opens the page's editor for a visible row (context spaces). */
		edit?: (row: SideRow, entities: string[]) => void;
		conflictsHref: string;
	} = $props();

	const layers = new Resource<OverlayLayers>(
		() => api().refinementLayers(object),
		(d) => d.base.length === 0 && d.tenant.length === 0,
	);
	onMount(() => layers.refresh());
	const wa = $derived(writeAttrs(session, 'job_designer'));
	let busy = $state('');

	async function act(row: SideRow, op: RefinementOp) {
		if (busy) return;
		busy = `${row.key}:${op}`;
		const ok = await guard(
			() => api().refine(op, row.object, row.key),
			`${ACTION_DONE[op]}: ${row.key}. Saved to draft — live when you publish.`,
			'Not changed',
		);
		busy = '';
		if (ok) {
			await layers.refresh();
			onchange();
		}
	}
</script>

<Freshness resource={layers} />
<LoadState resource={layers} empty="Neither the release nor this tenant defines any.">
	{#snippet children(data)}
		{@const rows = sideBySide(object, data.base, data.tenant, conflicts)}
		<Card cls="flush">
			<div class="tbl-wrap">
				<table class="tbl" data-testid="side-by-side-{object}">
					<thead>
						<tr
							><th>Key</th><th>State</th><th>Base (release)</th><th>Yours</th><th aria-label="Actions"
							></th></tr
						>
					</thead>
					<tbody>
						{#each rows as row (row.key)}
							{@const pending = draftAt(drafts, overlayDraftKey(object, row.key))}
							<tr data-key={row.key} class:struck={row.state === 'tombstone'} class:drafted={!!pending}>
								<td class="mono">{row.key}</td>
								<td>
									<Chip text={STATE_LABEL[row.state]} variant={STATE_VARIANT[row.state]} />
									{#if pending}<Chip text="Draft" variant="accent" title="Unpublished: {pending.label}" />
										<div class="muted small live-was">
											Live: <span class="mono">{pending.live ?? 'release value'}</span>
										</div>{/if}
									{#if row.conflict}<a href={conflictsHref} title={row.conflict.detail}
											><Chip text="Conflict: {KIND_LABEL[row.conflict.kind]}" variant="err" /></a
										>{/if}
									{#if row.base?.required === true}<Chip
											text="Required"
											variant="muted"
											title="Required by the job template; cannot be hidden"
										/>{/if}
								</td>
								{#if row.state === 'override'}
									<td>
										{#each row.diff as d}<div>
												<span class="muted">{d.field}:</span> <span class="mono">{d.base}</span>
											</div>
										{:else}<span class="muted">Same as yours</span>{/each}
									</td>
									<td>
										{#each row.diff as d}<div>
												<span class="muted">{d.field}:</span> <span class="mono">{d.tenant}</span>
											</div>
										{:else}<span class="muted">No changes</span>{/each}
										{#if row.tenant}<div class="muted small">
												by {row.tenant.updated_by_name ?? row.tenant.updated_by} · {row.tenant.updated_at.slice(
													0,
													10,
												)}
											</div>{/if}
									</td>
								{:else if row.state === 'base'}
									<td>{summaryOf(object, row.base!)}</td>
									<td class="muted">Uses the base</td>
								{:else if row.state === 'extension'}
									<td class="muted">Not in the release</td>
									<td>
										{row.tenant?.item ? summaryOf(object, row.tenant.item) : ''}
										<div class="muted small">
											by {row.tenant?.updated_by_name ?? row.tenant?.updated_by} · {row.tenant?.updated_at.slice(
												0,
												10,
											)}
										</div>
									</td>
								{:else}
									<td>{row.base ? summaryOf(object, row.base) : 'Gone from the release'}</td>
									<td class="muted"
										>Hidden by you <span class="small"
											>({row.tenant?.updated_by_name ?? row.tenant?.updated_by})</span
										></td
									>
								{/if}
								<td class="r actions-cell">
									{#if edit && row.state !== 'tombstone'}
										<button
											class="btn sm"
											type="button"
											disabled={wa.disabled || busy !== ''}
											title={wa.title}
											onclick={() => edit(row, data.entities)}>Edit</button
										>
									{/if}
									{#each actionsFor(row) as op}
										<button
											class="btn sm"
											type="button"
											disabled={wa.disabled || busy !== ''}
											title={wa.title}
											onclick={() => act(row, op)}>{ACTION_LABEL[op]}</button
										>
									{/each}
								</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		</Card>
	{/snippet}
</LoadState>

<style>
	.struck td:first-child {
		text-decoration: line-through;
	}
	.small {
		font-size: 12px;
	}
	.drafted td:first-child {
		box-shadow: inset 3px 0 0 var(--accent);
	}
	.live-was {
		margin-top: 4px;
	}
	.actions-cell {
		white-space: nowrap;
	}
	.actions-cell .btn + .btn {
		margin-left: 4px;
	}
</style>
