<script lang="ts">
	// J12 procedure map: one card per context space (request type) — what the agent must know first (required slots,
	// tenant-added marked), what it does (the running version's step shape), and where every version stands (running,
	// shadowed base, in review, drafts). Edit slots = a required-slots override, saved to
	// draft (D54): it reaches customers with the next release; the live slots show next to the draft ones.
	import { api, app, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { writeAttrs } from '$lib/auth/roles';
	import Chip from '$lib/ui/Chip.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';
	import type { Resource } from '$lib/load.svelte';
	import type { Paged } from '$lib/api/types/core';
	import type { ContextSpace, ProcedureView } from '$lib/api/types/ontology';
	import Meter from '$lib/ui/Meter.svelte';
	import StepShape from './StepShape.svelte';
	import { lanes, reachText, successView } from './view';
	import { areaOfContextSpace } from '$lib/context-space-key';

	let {
		spaces,
		procedures,
		release = null,
	}: {
		spaces: Resource<Paged<ContextSpace>>;
		procedures: ProcedureView[];
		release?: string | null;
	} = $props();
	const CONFLICT = {
		base_removed: 'The template no longer has this space; resolve it on the release page',
		base_changed: 'The template changed the base slots under your refinement; resolve it on the release page',
	} as const;

	let open = $state(false);
	let sel = $state<ContextSpace | null>(null);
	let slots = $state('');
	let start = $state('');
	let saving = $state(false);
	const dirty = $derived(open && slots !== start);

	function edit(s: ContextSpace) {
		sel = s;
		slots = s.required.join(', ');
		start = slots;
		open = true;
	}
	async function save(required: string[] | null) {
		if (!sel) return;
		saving = true;
		const key = sel.key;
		const ok = await guard(
			() => api().saveContextSpaceRequired(key, required),
			required === null
				? `Saved to draft: ${key} uses the base slots again after the next release.`
				: `Saved to draft: the required slots of ${key}. Live after the next release.`,
		);
		saving = false;
		if (ok) {
			start = slots;
			open = false;
			await spaces.refresh();
		}
	}
	const parse = (s: string) =>
		s
			.split(',')
			.map((x) => x.trim())
			.filter(Boolean);
</script>

<div class="ps-map" data-testid="procedure-map">
	{#if spaces.value.state === 'ok'}
		{#each spaces.value.data.items as s (s.key)}
			{@const rows = procedures.filter((p) => p.name === s.procedure)}
			{@const l = lanes(rows, release)}
			{@const eff = l.live}
			{@const liveSlots = s.live_required ?? s.required}
			{@const slotsDraft = liveSlots.join(',') !== s.required.join(',')}
			{@const shown = eff ?? l.review[0] ?? l.drafts[0] ?? null}
			{@const ok = eff ? successView(eff.success_rate) : null}
			{@const href = `/admin/jobs/${app.job}/procedures/${s.procedure}`}
			<article class="ps-card" class:ps-none={!eff}>
				<header class="ps-head">
					<div>
						<h3 class="ps-title">{s.label}</h3>
						<span class="cell-title mono">{s.key}</span>
						{#if areaOfContextSpace(s.key)}<span
								class="ps-area"
								title="Area: a grouping of context spaces inside the department (D60)"
								>area {areaOfContextSpace(s.key)}</span
							>{/if}
					</div>
					<span class="chips ps-flags">
						{#if s.origin === 'tenant'}<Chip
								text="Tenant"
								variant="accent"
								title="Added by this tenant"
							/>{/if}
						{#if s.overridden}<Chip
								text="slots refined"
								variant="accent"
								title="Required slots refined by this tenant"
							/>{/if}
						{#if s.conflict}<a href="/admin/jobs/{app.job}/release#conflicts" title={CONFLICT[s.conflict]}
								><Chip text={s.conflict} variant="warn" /></a
							>{/if}
					</span>
				</header>
				<dl class="ps-rows">
					<dt>Asks for</dt>
					<dd class="ps-slots">
						<span class="chips">
							{#each s.required as slot (slot)}
								<Chip
									text={slot}
									variant={s.base_required.includes(slot) ? 'muted' : 'accent'}
									title={s.base_required.includes(slot) ? 'Base slot' : 'Added by this tenant'}
								/>
							{:else}<span class="muted">nothing: answers straight away</span>{/each}
						</span>
						{#if slotsDraft}
							<span class="ps-draftslots"
								><Chip text="draft" variant="warn" title="Saved to draft; not live until the next release" />
								live: <span class="mono">{liveSlots.join(', ') || 'none'}</span></span
							>
						{/if}
						<button
							type="button"
							class="btn sm ps-edit"
							{...writeAttrs(session, 'job_designer')}
							aria-label="Edit slots of {s.key}"
							onclick={() => edit(s)}>Edit slots</button
						>
					</dd>
					<dt>Does</dt>
					<dd>
						<a class="mono ps-proc" {href}>{s.procedure}</a>
						{#if shown}
							<StepShape steps={shown.steps} outside={shown.outside_grant} />
						{/if}
					</dd>
					<dt>Versions</dt>
					<dd class="ps-lane">
						{#if eff}
							<a class="ps-v run" href="{href}?v={eff.version}" title={reachText(eff, eff, release)}>
								<b class="mono">{eff.version}</b> live · {eff.origin}
							</a>
						{:else}
							<span class="ps-v none">Nothing live: cases are handed over</span>
						{/if}
						{#if l.pending}
							<a
								class="ps-v next"
								href="{href}?v={l.pending.version}"
								title={reachText(l.pending, eff, release)}
								><b class="mono">{l.pending.version}</b> published · next release</a
							>
						{/if}
						{#each l.shadowed as v (v.version)}
							<a
								class="ps-v dim"
								href="{href}?v={v.version}"
								title="Base version from the template; the tenant version runs"
								><b class="mono">{v.version}</b> base · shadowed</a
							>
						{/each}
						{#each l.review as v (v.version)}
							<a class="ps-v review" href="{href}?v={v.version}"
								><b class="mono">{v.version}</b> in review · {v.reviewers.length} approved</a
							>
						{/each}
						{#each l.drafts as v (v.version)}
							<a class="ps-v draft" href="{href}?v={v.version}"
								><b class="mono">{v.version}</b> draft{#if v.test_runs?.length}
									· {v.test_runs.length} test{v.test_runs.length === 1 ? '' : 's'}{/if}</a
							>
							{#if v.outside_grant.length}<Chip
									text="{v.outside_grant.length} outside grant"
									variant="err"
									title="{v.outside_grant.join(', ')}: refused at CANARY and at release cut (409)"
								/>{/if}
						{/each}
					</dd>
				</dl>
				<footer class="ps-foot">
					{#if eff}
						<span class="mono">{eff.runs_7d.toLocaleString('en-GB')}</span>
						<span class="muted">cases · 7 days</span>
						{#if ok}<span class="ps-meter"><Meter value={eff.success_rate ?? 0} cls={ok.cls} /></span><span
								class="mono">{ok.text}</span
							>{:else}<span class="muted">no runs yet</span>{/if}
					{:else}
						<span class="muted"
							>Publish a version and release it to let the agent handle {s.label.toLowerCase()}.</span
						>
					{/if}
					<a
						class="ps-activity mono"
						href="/admin/jobs/{app.job}/journey/{encodeURIComponent(s.activity)}"
						title="Journey activity this space serves">{s.activity}</a
					>
				</footer>
			</article>
		{/each}
	{:else if spaces.value.state === 'unavailable'}
		<Unavailable reason={spaces.value.reason} retry={spaces.retry} />
	{:else if spaces.value.state === 'empty'}
		<div class="empty" data-testid="load-empty">
			No context spaces yet. They come with a pinned job template that has activities.
		</div>
	{:else}
		<p class="muted" role="status">Loading…</p>
	{/if}
</div>

<Drawer bind:open title={sel ? `Required slots · ${sel.key}` : 'Required slots'} {dirty}>
	{#if sel}
		<div class="stack">
			<p class="muted">
				The qualifier asks for every required slot before {sel.procedure} runs. Changes are saved to draft: test
				them in the test chat; customers get them when the next release is approved and applied.
			</p>
			<div class="field">
				<label for="cs-slots">Required slots (comma separated, snake_case)</label>
				<input id="cs-slots" class="input mono" bind:value={slots} />
			</div>
			<p class="note">
				Live now: <span class="mono">{(sel.live_required ?? sel.required).join(', ') || 'none'}</span>
			</p>
			<p class="note">Base declares: <span class="mono">{sel.base_required.join(', ') || 'none'}</span></p>
			{#if sel.conflict === 'base_changed'}
				<div class="callout gap">
					<strong>Base changed under this override</strong><span
						>Reverting accepts the base slots and closes the conflict.</span
					>
				</div>
			{/if}
			<div class="row">
				<button
					type="button"
					class="btn primary"
					{...writeAttrs(session, 'job_designer')}
					disabled={saving || writeAttrs(session, 'job_designer').disabled}
					onclick={() => save(parse(slots))}>Save slots</button
				>
				{#if sel.overridden}
					<button
						type="button"
						class="btn"
						{...writeAttrs(session, 'job_designer')}
						disabled={saving || writeAttrs(session, 'job_designer').disabled}
						onclick={() => save(null)}>Revert to base</button
					>
				{/if}
				<button type="button" class="btn" onclick={() => (open = false)}>Cancel</button>
			</div>
		</div>
	{/if}
</Drawer>

<style>
	.row {
		display: flex;
		gap: 8px;
		flex-wrap: wrap;
	}
	.ps-map {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(min(100%, 430px), 1fr));
		gap: 12px;
	}
	.ps-card {
		display: grid;
		grid-template-rows: auto 1fr auto;
		gap: 10px;
		padding: 12px 14px;
		border: 1px solid var(--line);
		border-radius: var(--r);
		background: var(--panel);
		min-width: 0;
	}
	.ps-card.ps-none {
		border-style: dashed;
	}
	.ps-head {
		display: flex;
		justify-content: space-between;
		gap: 8px;
		align-items: flex-start;
	}
	.ps-title {
		margin: 0 0 2px;
		font: 600 15px/1.2 var(--cond);
	}
	.ps-head .cell-title {
		font-size: 11.5px;
		color: var(--muted);
		font-weight: 400;
		word-break: break-all;
	}
	.ps-flags {
		justify-content: flex-end;
	}
	.ps-rows {
		display: grid;
		grid-template-columns: 70px minmax(0, 1fr);
		gap: 8px 10px;
		margin: 0;
		align-items: start;
	}
	.ps-rows dt {
		font: 600 10.5px/20px var(--cond);
		letter-spacing: 0.04em;
		text-transform: uppercase;
		color: var(--faint);
	}
	.ps-rows dd {
		margin: 0;
		display: flex;
		flex-wrap: wrap;
		gap: 6px 10px;
		align-items: center;
		min-width: 0;
	}
	.ps-slots {
		justify-content: space-between;
	}
	.ps-proc {
		font-size: 12.5px;
		font-weight: 600;
	}
	.ps-lane {
		gap: 6px !important;
	}
	.ps-v {
		display: inline-flex;
		gap: 5px;
		align-items: center;
		padding: 2px 8px;
		border-radius: 999px;
		font-size: 12px;
		text-decoration: none;
		color: var(--fg);
		border: 1px solid var(--line);
		white-space: nowrap;
	}
	.ps-v:hover {
		border-color: var(--accent);
	}
	.ps-v.run {
		border-color: color-mix(in srgb, var(--ok) 55%, transparent);
		background: color-mix(in srgb, var(--ok) 11%, transparent);
	}
	.ps-v.review {
		border-color: color-mix(in srgb, var(--warn) 55%, transparent);
		color: var(--warn);
	}
	.ps-v.next {
		border-color: color-mix(in srgb, var(--accent) 55%, transparent);
		color: var(--accent);
	}
	.ps-draftslots {
		display: inline-flex;
		gap: 6px;
		align-items: center;
		font-size: 12px;
		color: var(--muted);
		flex-basis: 100%;
	}
	.ps-v.draft {
		border-style: dashed;
		color: var(--muted);
	}
	.ps-v.dim {
		color: var(--faint);
		text-decoration: line-through;
		text-decoration-color: var(--faint);
	}
	.ps-v.none {
		color: var(--err);
		border-color: color-mix(in srgb, var(--err) 45%, transparent);
	}
	.ps-rel {
		font: 500 10.5px/1.4 var(--mono);
		color: var(--ok);
	}
	.ps-foot {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		align-items: center;
		padding-top: 8px;
		border-top: 1px solid var(--line-2);
		font-size: 12.5px;
	}
	.ps-meter {
		width: 70px;
		display: inline-flex;
	}
	.ps-activity {
		margin-left: auto;
		font-size: 11.5px;
	}
	.ps-area {
		font-size: 11.5px;
		color: var(--muted);
	}
</style>
