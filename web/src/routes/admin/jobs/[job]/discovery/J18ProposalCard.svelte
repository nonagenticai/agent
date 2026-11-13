<script lang="ts">
	// J18: one discovery proposal — what customers do (evidence), what accepting changes (live → draft, on which design
	// page), and the decision. After Accept the card follows the change through D54: draft (Test) → release → live.
	import type { DiscoveryProposal } from '$lib/api/types/journey';
	import { session } from '$lib/auth/session.svelte';
	import { canWrite, writeAttrs } from '$lib/auth/roles';
	import { STAGE_LABEL } from '$lib/journey/catalogue';
	import { J18_KIND_LABEL, J18_LIFECYCLE, J18_SOURCE_LABEL } from '$lib/journey/J18Discovery';
	import { when } from '$lib/journey/format';
	import { fmt } from '$lib/format';

	let {
		p,
		base,
		tenant,
		onaccept,
		onreject,
	}: {
		p: DiscoveryProposal;
		base: string;
		/** The tenant whose usage the drafting tokens are (D56). */
		tenant: string;
		onaccept: (p: DiscoveryProposal) => Promise<void>;
		onreject: (p: DiscoveryProposal, reason: string) => Promise<boolean>;
	} = $props();

	const PAGE: Record<string, [string, string]> = {
		J4: ['journey', 'Journey'],
		J6: ['operations', 'Operations & routes'],
		J10: ['knowledge-kinds', 'Knowledge kinds'],
	};
	const page = $derived(PAGE[p.change.page] ?? ['journey', 'Journey']);
	const can = $derived(canWrite(session, 'job_designer'));
	const KIND_VARIANT = {
		new_activity: 'cust',
		enable_activity: 'accent',
		knowledge_gap: 'work',
		operation_route: 'tool',
	} as const;

	let rejecting = $state(false);
	let reason = $state('');
	let busy = $state(false);
	async function accept() {
		busy = true;
		await onaccept(p);
		busy = false;
	}
	async function reject() {
		busy = true;
		if (await onreject(p, reason)) {
			rejecting = false;
			reason = '';
		}
		busy = false;
	}
	const stepIdx = $derived(J18_LIFECYCLE.findIndex((s) => s.key === p.status));
	const decided = $derived(p.status !== 'open');
</script>

<article class="j18-prop" class:decided data-testid="proposal-{p.id}" data-status={p.status}>
	<header class="j18-head">
		<span class="chip {KIND_VARIANT[p.kind]}">{J18_KIND_LABEL[p.kind]}</span>
		{#if p.stage}<span class="chip muted">{STAGE_LABEL[p.stage]}</span>{/if}
	</header>
	<h3>{p.title}</h3>
	<p class="j18-why">{p.why}</p>

	<div class="j18-body">
		<section class="j18-ev" aria-label="Evidence">
			<div class="j18-lbl">Evidence · {J18_SOURCE_LABEL[p.evidence.source]}</div>
			<div class="j18-count">
				<b>{fmt.n(p.evidence.count)}</b>
				{p.evidence.unit}
				{#if p.evidence.share != null}<span class="muted">· {fmt.pct(p.evidence.share)}</span>{/if}
			</div>
			<div class="j18-src muted">{p.evidence.window}</div>
			{#if p.evidence.share != null}
				<div class="j18-bar" aria-hidden="true">
					<span style:width="{Math.max(2, Math.min(100, p.evidence.share * 100 * 4))}%"></span>
				</div>
			{/if}
			<ul class="j18-ex">
				{#each p.evidence.examples as e (e.text)}
					<li>
						<q>{e.text}</q>
						{#if e.ref || e.outcome}
							<span class="j18-exmeta"
								>{#if e.ref}<span class="mono">{e.ref}</span>{/if}{#if e.ref && e.outcome}
									·
								{/if}{e.outcome ?? ''}</span
							>
						{/if}
					</li>
				{/each}
			</ul>
			{#if p.evidence.metric}<p class="j18-metric">{p.evidence.metric}</p>{/if}
		</section>

		<section class="j18-chg" aria-label="If you accept">
			<div class="j18-lbl">If you accept · <a href="{base}/{page[0]}">{page[1]}</a></div>
			<div class="j18-target">{p.change.target}</div>
			<dl class="j18-diff">
				<dt>Live now</dt>
				<dd class="live">{p.change.live}</dd>
				<dt>Draft</dt>
				<dd class="draft">{p.change.draft}</dd>
			</dl>
			{#if p.estimate}<p class="j18-est">{p.estimate}</p>{/if}
			<p class="j18-cost muted">
				Drafted with {fmt.k(p.drafted.tokens)} tokens · metered as {tenant}'s usage
			</p>
		</section>
	</div>

	<footer class="j18-foot">
		{#if p.status === 'open'}
			{#if rejecting}
				<div class="j18-reject">
					<label for="rj-{p.id}" class="j18-lbl">Why reject? (the next discovery run learns from it)</label>
					<input
						id="rj-{p.id}"
						class="input"
						bind:value={reason}
						placeholder="We do not offer this"
						disabled={!can}
					/>
					<div class="row">
						<button
							class="btn sm"
							type="button"
							{...writeAttrs(session, 'job_designer')}
							disabled={!can || busy || !reason.trim()}
							onclick={reject}>Reject proposal</button
						>
						<button class="btn sm" type="button" onclick={() => (rejecting = false)}>Cancel</button>
					</div>
				</div>
			{:else}
				<div class="row">
					<button
						class="btn sm primary"
						type="button"
						{...writeAttrs(session, 'job_designer')}
						disabled={!can || busy}
						onclick={accept}>Accept → save to draft</button
					>
					<button
						class="btn sm"
						type="button"
						{...writeAttrs(session, 'job_designer')}
						disabled={!can || busy}
						onclick={() => (rejecting = true)}>Reject…</button
					>
				</div>
				<span class="j18-note muted">Not live on accept: it runs in Test until you publish.</span>
			{/if}
		{:else if p.status === 'rejected'}
			<span class="chip muted">rejected</span>
			<span class="j18-note">
				{when(p.decided_at)} by {p.decided_by}{#if p.reason}: “{p.reason}”{/if}
			</span>
		{:else if p.status === 'discarded'}
			<span class="chip warn">draft discarded</span>
			<span class="j18-note"
				>Accepted {when(p.decided_at)}; the draft change was discarded before publishing.</span
			>
		{:else}
			<ol class="j18-life" aria-label="Where this change is">
				<li class="done">Accepted <span class="muted">{when(p.decided_at)}</span></li>
				{#each J18_LIFECYCLE as s, i (s.key)}
					<li class:done={i < stepIdx} class:now={i === stepIdx}>
						{s.label}
						{#if i === stepIdx && p.release}<span class="mono">{p.release}</span>{/if}
					</li>
				{/each}
			</ol>
			<span class="j18-note">
				{#if p.status === 'draft'}
					<a href="/chat">Try it in the test chat</a> · <a href="{base}/release">Review &amp; publish</a>
				{:else if p.status === 'in_release'}
					Bundled in the approved cut — <a href="{base}/release">apply it</a> to go live.
				{:else}
					Live in production{#if p.release}&nbsp;since <span class="mono">{p.release}</span>{/if}.
				{/if}
			</span>
		{/if}
	</footer>
</article>

<style>
	.j18-prop {
		border: 1px solid var(--line);
		border-radius: var(--r);
		padding: 12px 14px;
		display: grid;
		gap: 8px;
		background: var(--panel);
		min-width: 0;
	}
	.j18-prop.decided {
		background: var(--sunk);
	}
	.j18-head {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		align-items: center;
	}
	h3 {
		margin: 0;
		font-size: 15px;
	}
	.j18-why {
		margin: 0;
		font-size: 13px;
		color: var(--muted);
	}
	.j18-body {
		display: grid;
		grid-template-columns: minmax(0, 1.2fr) minmax(0, 1fr);
		gap: 14px;
	}
	.j18-lbl {
		font: 600 10.5px/1.4 var(--cond);
		text-transform: uppercase;
		letter-spacing: 0.06em;
		color: var(--muted);
	}
	.j18-count b {
		font-size: 20px;
	}
	.j18-count {
		font-size: 13px;
	}
	.j18-src {
		font-size: 12px;
		overflow-wrap: anywhere;
	}
	.j18-bar {
		height: 4px;
		border-radius: 2px;
		background: var(--line-2);
		margin: 4px 0;
		overflow: hidden;
	}
	.j18-bar span {
		display: block;
		height: 100%;
		background: var(--cust);
	}
	.j18-ex {
		list-style: none;
		margin: 6px 0 0;
		padding: 0;
		display: grid;
		gap: 4px;
		font-size: 12.5px;
	}
	.j18-ex q {
		font-style: italic;
	}
	.j18-exmeta {
		display: block;
		font-size: 11.5px;
		color: var(--muted);
	}
	.j18-metric,
	.j18-est,
	.j18-cost {
		margin: 4px 0 0;
		font-size: 12.5px;
	}
	.j18-cost {
		font-size: 11.5px;
	}
	.j18-chg {
		border-left: 2px solid var(--accent);
		padding-left: 12px;
		min-width: 0;
	}
	.j18-target {
		font-weight: 600;
		font-size: 13px;
		margin: 2px 0 4px;
	}
	.j18-diff {
		display: grid;
		grid-template-columns: auto 1fr;
		gap: 2px 10px;
		margin: 0;
		font-size: 12.5px;
	}
	.j18-diff dt {
		color: var(--muted);
	}
	.j18-diff dd {
		margin: 0;
		overflow-wrap: anywhere;
	}
	.j18-diff dd.live {
		text-decoration: line-through;
		text-decoration-color: var(--muted);
		color: var(--muted);
	}
	.j18-diff dd.draft {
		color: var(--accent);
		font-weight: 600;
	}
	.j18-foot {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 8px 12px;
		border-top: 1px solid var(--line-2);
		padding-top: 8px;
	}
	.j18-note {
		font-size: 12.5px;
	}
	.j18-reject {
		display: grid;
		gap: 6px;
		width: 100%;
	}
	.j18-life {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-wrap: wrap;
		gap: 4px;
		font-size: 12px;
	}
	.j18-life li {
		padding: 2px 8px;
		border-radius: 999px;
		border: 1px solid var(--line);
		color: var(--muted);
	}
	.j18-life li.done {
		color: var(--ok);
		border-color: color-mix(in srgb, var(--ok) 45%, transparent);
	}
	.j18-life li.now {
		color: var(--fg);
		border-color: var(--accent);
		background: color-mix(in srgb, var(--accent) 12%, transparent);
		font-weight: 600;
	}
	@media (max-width: 720px) {
		.j18-body {
			grid-template-columns: 1fr;
		}
	}
</style>
