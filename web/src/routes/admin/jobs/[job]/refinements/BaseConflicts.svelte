<script lang="ts">
	// J13: where the template base moved underneath a refinement (05 §4.6). Never auto-resolved; the choice is the
	// admin's — accept the base (drop yours) or keep yours on top. Either choice is saved to the draft (D54).
	import { api, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { writeAttrs } from '$lib/auth/roles';
	import type { ReleaseConflict } from '$lib/api/types/release';
	import {
		CONFLICT_ACTION,
		KIND_LABEL,
		OBJECT_LABEL,
		conflictActions,
		conflictText,
		type ConflictOp,
	} from '$lib/release/conflicts';

	let { conflicts, href, onchange }: { conflicts: ReleaseConflict[]; href: string; onchange: () => void } =
		$props();

	const wa = $derived(writeAttrs(session, 'job_designer'));
	let busy = $state('');
	async function resolve(c: ReleaseConflict, op: ConflictOp) {
		if (busy) return;
		busy = `${c.id}:${op}`;
		const ok = await guard(
			() => api().resolveConflict(c.id, op),
			`${CONFLICT_ACTION[op].done}: ${c.key}. Saved to draft — live when you publish.`,
			'Not resolved',
		);
		busy = '';
		if (ok) onchange();
	}
</script>

{#if conflicts.length}
	<section class="bc" aria-labelledby="bc-h" data-testid="j13-conflicts">
		<header class="bc-head">
			<h2 id="bc-h">
				The base changed under {conflicts.length} of your refinement{conflicts.length === 1 ? '' : 's'}
			</h2>
			<a {href}>All conflicts</a>
		</header>
		<p class="muted bc-lead">
			Your version still wins until you choose. Decide before you publish, so the test chat shows what
			customers will get.
		</p>
		<ul class="bc-list">
			{#each conflicts as c (c.id)}
				<li class="bc-row" data-conflict={c.id}>
					<div class="bc-what">
						<span class="chip warn">{KIND_LABEL[c.kind]}</span>
						<span class="muted">{OBJECT_LABEL[c.object] ?? c.object}</span>
						<span class="mono">{c.key}</span>
					</div>
					<div class="bc-why">
						<b>{conflictText(c.kind)}</b>
						<span class="muted">{c.detail}</span>
					</div>
					<div class="bc-act">
						{#each conflictActions(c) as op (op)}
							<button
								class="btn sm"
								type="button"
								disabled={wa.disabled || busy !== ''}
								title={wa.title ?? CONFLICT_ACTION[op].hint}
								onclick={() => resolve(c, op)}>{CONFLICT_ACTION[op].label}</button
							>
						{/each}
					</div>
				</li>
			{/each}
		</ul>
	</section>
{/if}

<style>
	.bc {
		border: 1px solid color-mix(in srgb, var(--warn) 45%, var(--line));
		border-left: 4px solid var(--warn);
		background: var(--panel);
		border-radius: var(--r);
		padding: 14px 16px;
		margin: 0 0 16px;
	}
	.bc-head {
		display: flex;
		flex-wrap: wrap;
		justify-content: space-between;
		gap: 4px 12px;
		align-items: baseline;
	}
	.bc-head h2 {
		font-size: 15px;
		margin: 0;
	}
	.bc-lead {
		margin: 2px 0 8px;
		font-size: 13px;
	}
	.bc-list {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.bc-row {
		display: grid;
		grid-template-columns: minmax(0, 1fr) minmax(0, 1.6fr) auto;
		gap: 6px 14px;
		align-items: center;
		padding: 9px 0;
		border-top: 1px solid var(--line-2);
		font-size: 13px;
	}
	@media (max-width: 1000px) {
		.bc-row {
			grid-template-columns: minmax(0, 1fr);
		}
	}
	.bc-what {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		align-items: center;
		overflow-wrap: anywhere;
	}
	.bc-why {
		display: grid;
		gap: 2px;
	}
	.bc-act {
		display: flex;
		flex-wrap: wrap;
		gap: 4px;
		justify-content: flex-end;
	}
	@media (max-width: 1000px) {
		.bc-act {
			justify-content: flex-start;
		}
	}
</style>
