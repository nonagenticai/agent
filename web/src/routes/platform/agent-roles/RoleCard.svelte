<script lang="ts">
	// P15 one agent role: principal, max side effect, grants (object, verb, variant, modalities); derived grants of
	// the procedure role per activity (read-only); add/remove grants for qualifier and post-gate.
	import { api, guard } from '$lib/app.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import type { AgentRoleKey, AgentRoleView, Modality, SideEffect } from '$lib/api/types/ontology';
	import { OPERATION_CATALOGUE, catalogueOp, opLabel } from '$lib/ontology/catalogue';
	import { SIDE_EFFECT_LABEL, SIDE_EFFECT_VARIANT, title, when } from '$lib/ontology/shared';
	import { grantsFromExpectations } from '$lib/ontology/roleGrants';
	import { baseOp, peakSideEffect } from './logic';

	let { role, job, onchanged }: { role: AgentRoleView; job: string; onchanged: () => Promise<void> } =
		$props();

	const isDerived = $derived(role.grants.some((g) => g.derive_from_activity));
	// D119: qualifier / post-gate grants no layer sets derive from the job's operation expectations
	const fromExpectations = $derived(grantsFromExpectations(role.grants));
	const editable = $derived(!isDerived && role.enabled);
	const why = $derived(
		isDerived
			? 'Derived from each activity: edit the activity, not the role.'
			: !role.enabled
				? 'Disabled in v1.'
				: undefined,
	);
	const MODALITIES: Modality[] = ['observe', 'request', 'do'];
	let op = $state('');
	let variant = $state('');
	let mods = $state<Modality[]>(['observe']);
	let busy = $state(false);

	async function run(fn: () => Promise<unknown>, ok: string) {
		busy = true;
		const done = await guard(fn, ok);
		busy = false;
		if (done) await onchanged();
		return done;
	}
	async function add(e: SubmitEvent) {
		e.preventDefault();
		const label = opLabel(op, variant.trim() || null);
		if (
			await run(
				() =>
					api().addAgentRoleGrant(job, role.role as AgentRoleKey, {
						op,
						variant: variant.trim() || null,
						modalities: mods,
					}),
				`${role.label} may now call ${label}.`,
			)
		) {
			op = '';
			variant = '';
		}
	}
	function toggle(m: Modality, on: boolean) {
		mods = on ? [...new Set([...mods, m])] : mods.filter((x) => x !== m);
	}
</script>

<div id="role-{role.role}" class="rc-anchor">
	<Card title={role.label} sub="Principal used by {role.used_by}">
		{#snippet actions()}
			<span class="chips">
				{#if role.enabled}<Chip text="enabled" variant="ok" />{:else}<Chip
						text="disabled in v1"
						variant="dead"
					/>{/if}
				{#if isDerived}<Chip text="grants derived" variant="tool" />{/if}
				{#if fromExpectations}<Chip text="grants from the job's expectations" variant="tool" />{/if}
			</span>
		{/snippet}
		<div class="meta">
			<label class="field inline">
				<span class="label">Max side effect</span>
				<select
					class="input sm mono"
					value={role.max_side_effect}
					disabled={!editable || busy}
					title={why}
					onchange={async (e) => {
						const el = e.currentTarget;
						const max = el.value as SideEffect;
						const ok = await run(
							() => api().setAgentRoleMaxSideEffect(job, role.role as AgentRoleKey, max),
							`${role.label} max side effect is now ${max}.`,
						);
						// A refused change puts the select back on the stored value.
						if (!ok) el.value = role.max_side_effect;
					}}
				>
					{#each ['R', 'W', 'X', 'P'] as se (se)}<option value={se}
							>{SIDE_EFFECT_LABEL[se as SideEffect]}</option
						>{/each}
				</select>
			</label>
			<span class="muted">{role.note}</span>
			<span class="muted upd">updated {when(role.updated_at)} by {role.updated_by ?? '—'}</span>
		</div>

		{#if !role.enabled}
			<Callout kind="info" title="Defined, not running">
				No agent runs as {role.role} in v1. If enabled later it gets observe + request operations of the job, never
				X or P.
			</Callout>
		{:else if isDerived}
			<p class="rc-lead">
				Never hand-authored: in each activity the procedure may call exactly the operations that activity
				declares — a return can never call <span class="mono">order.cancel</span>. To change a grant, change
				the activity (<a href="/admin/jobs/{encodeURIComponent(job)}/journey">Journey</a>). Staff /chat tests
				the same set.
			</p>
			<div class="tbl-wrap">
				<table class="tbl dense rc-derived">
					<thead><tr><th>Activity</th><th>Peak</th><th>Derived grant</th></tr></thead>
					<tbody>
						{#each role.derived as d (d.activity)}
							{@const peak = peakSideEffect(d.ops)}
							<tr>
								<td
									><span class="cell-title">{d.label}</span><span class="cell-sub mono">{d.activity}</span
									></td
								>
								<td
									><Chip
										text={peak}
										variant={SIDE_EFFECT_VARIANT[peak]}
										title="Highest side effect: {SIDE_EFFECT_LABEL[peak]}"
									/></td
								>
								<td>
									<span class="chips">
										{#each d.ops as o (o)}
											{@const c = catalogueOp(baseOp(o))}
											<Chip
												text={o}
												variant={c ? SIDE_EFFECT_VARIANT[c.side_effect] : 'muted'}
												title={c ? SIDE_EFFECT_LABEL[c.side_effect] : undefined}
											/>
										{/each}
									</span>
								</td>
							</tr>
						{:else}
							<tr
								><td colspan="3" class="muted"
									>This template has no activities yet (generator run in progress).</td
								></tr
							>
						{/each}
					</tbody>
				</table>
			</div>
		{:else}
			<div class="tbl-wrap">
				<table class="tbl dense">
					<thead
						><tr>
							<th>Operation</th><th>Modalities</th><th>Side effect</th><th
								title="Minimum case identity level the runner requires (N5)">Identity</th
							><th></th></tr
						></thead
					>
					<tbody>
						{#each role.grants as g (`${g.object}.${g.verb}{${g.variant ?? ''}}`)}
							{@const c = catalogueOp(`${g.object}.${g.verb}`)}
							<tr>
								<td class="mono"
									>{opLabel(`${g.object}.${g.verb}`, g.variant)}{#if g.derived}
										<Chip text="derived" variant="tool" />{/if}</td
								>
								<td>{g.modalities.join(', ')}</td>
								<td
									>{#if c}<Chip
											text={SIDE_EFFECT_LABEL[c.side_effect]}
											variant={SIDE_EFFECT_VARIANT[c.side_effect]}
										/>{/if}</td
								>
								<td class="mono">{c?.identity_level ?? '—'}</td>
								<td class="r">
									<button
										type="button"
										class="btn sm"
										disabled={busy}
										aria-label="Remove {g.object}.{g.verb} from {role.label}"
										onclick={() =>
											run(
												() =>
													api().removeAgentRoleGrant(
														job,
														role.role as AgentRoleKey,
														`${g.object}.${g.verb}`,
														g.variant,
													),
												`Removed ${opLabel(`${g.object}.${g.verb}`, g.variant)} from ${role.label}.`,
											)}>Remove</button
									>
								</td>
							</tr>
						{:else}
							<tr><td colspan="5" class="muted">No grants: this role cannot call anything.</td></tr>
						{/each}
					</tbody>
				</table>
			</div>
			<form class="add" onsubmit={add}>
				<select class="input sm mono" aria-label="Operation" bind:value={op}>
					<option value="">Add an operation…</option>
					{#each OPERATION_CATALOGUE as o (o.op)}
						<option value={o.op}>{o.op} · {o.side_effect} · {title(o.modality)}</option>
					{/each}
				</select>
				<input
					class="input sm mono"
					aria-label="Variant"
					placeholder="variant (optional)"
					bind:value={variant}
				/>
				{#each MODALITIES as m (m)}
					<label class="check"
						><input
							type="checkbox"
							checked={mods.includes(m)}
							onchange={(e) => toggle(m, e.currentTarget.checked)}
						/>
						{m}</label
					>
				{/each}
				<button type="submit" class="btn sm primary" disabled={busy || !op}>Add grant</button>
			</form>
		{/if}
	</Card>
</div>

<style>
	.rc-anchor {
		scroll-margin-top: 70px;
	}
	@media (max-width: 560px) {
		.rc-derived thead {
			display: none;
		}
		.rc-derived tr {
			display: grid;
			grid-template-columns: 1fr auto;
		}
		.rc-derived td:last-child {
			grid-column: 1 / -1;
		}
		.rc-derived td:not(:last-child) {
			border-bottom: 0;
		}
	}
	.rc-lead {
		font-size: 12.5px;
		color: var(--muted);
		margin: 0 0 8px;
	}
	.meta {
		display: flex;
		gap: 12px;
		align-items: center;
		flex-wrap: wrap;
		margin-bottom: 10px;
	}
	.inline {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		margin: 0;
	}
	.upd {
		font-size: 12px;
		margin-left: auto;
	}
	.input.sm {
		width: auto;
		padding: 4px 8px;
		font-size: 12.5px;
	}
	.add {
		display: flex;
		gap: 8px;
		align-items: center;
		flex-wrap: wrap;
		margin-top: 10px;
	}
	.check {
		display: inline-flex;
		gap: 4px;
		align-items: center;
		font-size: 12.5px;
	}
</style>
