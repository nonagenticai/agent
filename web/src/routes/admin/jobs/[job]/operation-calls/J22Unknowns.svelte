<script lang="ts">
	// J22 "Needs a person": unknown outcomes. A write was sent to the tenant's system and no answer came back, so
	// nobody knows whether it landed. The runner never retries it (12 §3.3 step 10); a person checks the system with
	// the operation's verify read and records what they found. Recording it clears the case alert.
	import { api, app, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { writeAttrs } from '$lib/auth/roles';
	import { SUPPORT_ROLES_NOTE } from '$lib/runtime/support-D55';
	import { jobPath } from '$lib/scope';
	import { fmt } from '$lib/format';
	import type { UnknownOutcome } from '$lib/api/types/runtime';
	import { when } from '$lib/runtime/labels';
	import { systemHref } from '$lib/runtime/J22-calls';
	import Chip from '$lib/ui/Chip.svelte';

	let { items, checked30d, onchange }: { items: UnknownOutcome[]; checked30d: number; onchange: () => void } =
		$props();

	const open = $derived(items.filter((u) => !u.call.check));
	const done = $derived(items.filter((u) => u.call.check));
	let notes = $state<Record<string, string>>({});

	async function record(u: UnknownOutcome, verdict: 'landed' | 'not_landed') {
		const ok = await guard(
			() => api().checkUnknownOutcome(u.call.call_id, verdict, notes[u.call.call_id] ?? ''),
			verdict === 'landed'
				? `Recorded: ${u.call.op} went through. Case ${u.case.case_id} updated.`
				: `Recorded: ${u.call.op} did not go through. Case ${u.case.case_id} updated.`,
			'Not recorded',
		);
		if (ok) onchange();
	}
</script>

<section class="j22u" data-testid="unknown-outcomes" aria-labelledby="j22u-h">
	<header class="j22u-head">
		<h2 id="j22u-h">
			Needs a person
			{#if open.length}<span class="j22u-count">{open.length}</span>{/if}
		</h2>
		<p>
			Unknown outcome = a write was sent and the system never answered. It may or may not have happened, so
			the agent will <strong>never retry it by itself</strong>. Check the system, then record what you found.
		</p>
	</header>

	{#if !open.length}
		<p class="j22u-none" data-testid="unknown-none">
			<span class="j22u-dot" aria-hidden="true"></span>
			No unknown outcomes are waiting.{#if checked30d}
				{fmt.n(checked30d)} in the last 30 days were checked.{/if}
		</p>
	{/if}

	{#each open as u (u.call.call_id)}
		{@const sys = u.call.instance ?? 'the system'}
		{@const href = systemHref(u.call.instance)}
		<article class="j22u-item" data-testid="unknown-{u.call.call_id}">
			<div class="j22u-what">
				<div class="j22u-op">
					<span class="mono strong">{u.call.op}</span>
					{#if u.call.side_effect}<Chip
							text={u.call.side_effect}
							variant="err"
							title="Side-effect class {u.call.side_effect}"
						/>{/if}
					<span class="muted">on</span>
					{#if href}<a class="mono" {href}>{sys}</a>{:else}<span class="mono">{sys}</span>{/if}
					{#if u.call.template}<span class="mono faint">{u.call.template}</span>{/if}
				</div>
				<div class="j22u-case">
					<a class="mono" href={jobPath(app.job, `/cases/${encodeURIComponent(u.case.case_id)}`)}
						>{u.case.case_id}</a
					>
					{u.case.title} · {u.case.customer_name} · {when(u.call.at)} ·
					<span class="mono">{u.call.call_id}</span>
				</div>
				{#if u.call.detail}<p class="j22u-detail">{u.call.detail}</p>{/if}
				<ol class="j22u-steps">
					<li>
						Look in <strong>{sys}</strong>{#if u.call.verify_read}
							— the agent’s verify read for this is <span class="mono">{u.call.verify_read}</span>{/if}.
					</li>
					<li>Record what it shows. The case is updated and its alert cleared; nothing is sent again.</li>
				</ol>
			</div>
			<div class="j22u-act">
				<label class="field">
					<span class="label">What you saw (optional)</span>
					<input
						class="input"
						placeholder="e.g. refund rf_8812 in Shopify, 08:41"
						bind:value={notes[u.call.call_id]}
						{...writeAttrs(session, 'operator')}
					/>
				</label>
				<div class="j22u-btns">
					<button
						class="btn primary"
						type="button"
						{...writeAttrs(session, 'operator')}
						onclick={() => record(u, 'landed')}>It went through</button
					>
					<button
						class="btn"
						type="button"
						{...writeAttrs(session, 'operator')}
						onclick={() => record(u, 'not_landed')}>It did not go through</button
					>
				</div>
				{#if writeAttrs(session, 'operator').disabled}<p class="muted small" data-testid="support-roles-note">
						{SUPPORT_ROLES_NOTE}
					</p>{/if}
			</div>
		</article>
	{/each}

	{#if done.length}
		<ul class="j22u-done">
			{#each done as u (u.call.call_id)}
				<li>
					<span class="mono">{u.call.op}</span> ·
					<a class="mono" href={jobPath(app.job, `/cases/${encodeURIComponent(u.case.case_id)}`)}
						>{u.case.case_id}</a
					>
					— checked by {u.call.check!.by_name}:
					<strong>{u.call.check!.verdict === 'landed' ? 'went through' : 'did not go through'}</strong>
					{#if u.call.check!.note}<span class="muted">“{u.call.check!.note}”</span>{/if}
				</li>
			{/each}
		</ul>
	{/if}
</section>

<style>
	.j22u {
		border: 1px solid color-mix(in srgb, var(--err) 45%, var(--line));
		border-left: 4px solid var(--err);
		background: var(--panel);
		border-radius: var(--r);
		padding: 14px 16px;
		margin-bottom: 16px;
	}
	.j22u-head h2 {
		margin: 0;
		font-size: 15px;
		display: flex;
		align-items: center;
		gap: 8px;
	}
	.j22u-count {
		background: var(--err);
		color: var(--panel);
		border-radius: 10px;
		font-size: 12px;
		padding: 0 8px;
		line-height: 20px;
	}
	.j22u-head p {
		margin: 4px 0 10px;
		color: var(--muted);
		font-size: 13px;
		max-width: 80ch;
	}
	.j22u-none {
		margin: 0;
		display: flex;
		align-items: center;
		gap: 8px;
		color: var(--muted);
	}
	.j22u-dot {
		width: 8px;
		height: 8px;
		border-radius: 50%;
		background: var(--ok);
	}
	.j22u-item {
		display: grid;
		grid-template-columns: minmax(0, 1fr) minmax(240px, 320px);
		gap: 16px;
		border-top: 1px solid var(--line-2);
		padding-top: 12px;
		margin-top: 4px;
	}
	.j22u-op {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 6px;
		font-size: 14px;
	}
	.strong {
		font-weight: 600;
	}
	.faint {
		color: var(--faint);
		font-size: 12px;
	}
	.j22u-case {
		font-size: 13px;
		color: var(--muted);
		margin-top: 2px;
	}
	.j22u-detail {
		margin: 8px 0 0;
		padding: 6px 10px;
		background: var(--sunk);
		border-radius: var(--r);
		font-family: var(--mono);
		font-size: 12px;
	}
	.j22u-steps {
		margin: 8px 0 0;
		padding-left: 18px;
		font-size: 13px;
	}
	.j22u-act {
		display: flex;
		flex-direction: column;
		gap: 8px;
	}
	.j22u-btns {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
	}
	.j22u-done {
		margin: 10px 0 0;
		padding: 8px 0 0 18px;
		border-top: 1px solid var(--line-2);
		font-size: 13px;
		color: var(--muted);
	}
	@media (max-width: 760px) {
		.j22u-item {
			grid-template-columns: 1fr;
		}
	}
</style>
