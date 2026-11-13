<script lang="ts">
	// J5 Activity detail: one use case = activity × objects × operations (11§4.1). Leads with what the customer
	// wants and what the agent does about it (gates → procedure → systems → state change), then how it performs
	// live, then the detail (operations and routes, sample dialogue, JSON examples, knowledge kinds). Shows what the
	// templates define (read-only) vs what this tenant refined. D54: every tenant change is saved to DRAFT, tested in the
	// Test environment (test chat), and reaches customers only when a release publishes it (approve → apply).
	import { onMount } from 'svelte';
	import { api, app, guard } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { isSuperAdmin, writeAttrs } from '$lib/auth/roles';
	import type {
		ActivityDetail,
		DialogueTurn,
		UseCaseCardPatch,
		UseCaseLayerKind,
	} from '$lib/api/types/journey';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import JsonView from '$lib/ui/JsonView.svelte';
	import Meter from '$lib/ui/Meter.svelte';
	import Seg from '$lib/ui/Seg.svelte';
	import { opLabel } from '$lib/journey/catalogue';
	import { when } from '$lib/journey/format';
	import { EFFECT_TEXT, ROUTE_CHIP, pct, routeText, towardsTarget } from '$lib/journey/J5Routes';
	import UseCaseFlow from './UseCaseFlow.svelte';

	let { activity }: { activity: string } = $props();
	const res = new Resource(
		() => api().jobActivity(activity),
		() => false,
	);
	onMount(() => res.refresh());
	const base = $derived(`/admin/jobs/${app.job}`);
	let example = $state('success');
	let renaming = $state(false);
	let label = $state('');
	// F-44: the card editor (a draft refinement of the template's card) and its reset.
	let editingCard = $state(false);
	let cSummary = $state('');
	let cExamples = $state('');
	let cMetricLabel = $state('');
	let cMetricTarget = $state('');
	let cDialogue = $state('');
	function openCard(d: ActivityDetail) {
		cSummary = d.use_case?.summary ?? '';
		cExamples = (d.use_case?.examples ?? []).join('\n');
		cMetricLabel = d.metric.label;
		cMetricTarget = d.metric.target;
		cDialogue = d.sample_dialogue.map((t) => `${t.from}: ${t.text}`).join('\n');
		editingCard = !editingCard;
	}
	/** Only the fields that differ from what the card shows now: a refinement holds just what changed. */
	function cardPatch(d: ActivityDetail): UseCaseCardPatch {
		const patch: UseCaseCardPatch = {};
		if (cSummary.trim() && cSummary.trim() !== d.use_case?.summary) patch.summary = cSummary.trim();
		const ex = cExamples
			.split('\n')
			.map((x) => x.trim())
			.filter(Boolean);
		if (ex.length && ex.join('\n') !== (d.use_case?.examples ?? []).join('\n')) patch.examples = ex;
		if (
			cMetricLabel.trim() &&
			(cMetricLabel.trim() !== d.metric.label || (cMetricTarget.trim() || '—') !== d.metric.target)
		)
			patch.metric = {
				key: d.metric.key,
				label: cMetricLabel.trim(),
				target: cMetricTarget.trim() || '—',
			};
		const turns: DialogueTurn[] = cDialogue
			.split('\n')
			.map((l) => l.trim())
			.filter(Boolean)
			.map((l) => {
				const m = /^(customer|agent|system):\s*(.*)$/.exec(l);
				return { from: (m?.[1] ?? 'customer') as DialogueTurn['from'], text: m ? m[2]! : l };
			});
		if (turns.length && JSON.stringify(turns) !== JSON.stringify(d.sample_dialogue))
			patch.sample_dialogue = turns;
		return patch;
	}

	const LAYER: Record<UseCaseLayerKind, string> = {
		base: 'Base',
		industry: 'Industry',
		department: 'Department',
		job: 'Job',
		tenant: 'Your refinement',
	};

	async function write(fn: () => Promise<unknown>, ok: string) {
		if (await guard(fn, ok)) {
			renaming = false;
			editingCard = false;
			await res.refresh();
		}
	}
</script>

<LoadState resource={res}>
	{#snippet children(d)}
		{@const a = d.activity}
		{@const u = d.use_case}
		{@const progress = towardsTarget(d.metric.current, d.metric.target)}
		<PageHead id="J5" title={a.label} sub="{d.stage.label} stage · {a.actor} activity · {a.key}">
			{#snippet actions()}
				<a class="btn" href="{base}/journey">Back to journey</a>
				<button
					class="btn"
					type="button"
					{...writeAttrs(session, 'job_designer')}
					onclick={() => {
						label = a.label;
						renaming = !renaming;
					}}>Rename</button
				>
				{#if a.refinement !== 'hidden'}
					<button
						class="btn"
						type="button"
						{...writeAttrs(session, 'job_designer')}
						onclick={() => openCard(d)}>Edit card</button
					>
				{/if}
				{#if u?.lineage.some((l) => l.kind === 'tenant' && l.card === 'refined')}
					<button
						class="btn"
						type="button"
						{...writeAttrs(session, 'job_designer')}
						onclick={() =>
							write(
								() => api().resetJobUseCaseCard(a.key),
								'Saved to draft: template card restored. Test it, then publish.',
							)}>Reset to template</button
					>
				{/if}
				{#if a.refinement === 'hidden'}
					<button
						class="btn"
						type="button"
						{...writeAttrs(session, 'job_designer')}
						onclick={() =>
							write(
								() => api().restoreJobActivity(a.key),
								`Saved to draft: ${a.label} shown again. Test it, then publish.`,
							)}>Show again</button
					>
				{:else if a.refinement !== 'extension'}
					<button
						class="btn danger"
						type="button"
						{...writeAttrs(session, 'job_designer')}
						onclick={() =>
							write(
								() => api().hideJobActivity(a.key),
								`Saved to draft: ${a.label} hidden. Test it, then publish.`,
							)}>Hide activity</button
					>
				{/if}
			{/snippet}
		</PageHead>

		<div class="stack">
			<div class="row">
				<span class="chip {a.actor === 'customer' ? 'cust' : 'work'}">{a.actor} activity</span>
				{#if a.refinement === 'hidden'}<span class="chip dead">hidden by you — not offered to customers</span
					>{/if}
				{#if a.refinement === 'extension'}<span class="chip cust">your extension</span>{/if}
				{#if a.refinement === 'relabelled'}<span class="chip work">renamed · template “{a.base_label}”</span
					>{/if}
				{#if u?.pending.length}<span class="chip warn" data-testid="draft-badge"
						>draft · {u.pending.length} unpublished change{u.pending.length === 1 ? '' : 's'}</span
					>{/if}
				{#if a.also_in_stages.length}<span class="muted">Also in {a.also_in_stages.join(', ')}</span>{/if}
			</div>

			{#if d.unrouted_required.length}
				<Callout kind="err" title="Required operation unrouted">
					{#each d.unrouted_required as x (x.op + x.env)}
						<div><span class="mono">{x.op}</span> has no route in <b>{x.env}</b>.</div>
					{/each}
					Release apply returns 409 <code>release_not_ready</code> until it is routed or falls back to
					<code>agent_cases</code>. <a href="{base}/operations">Route it</a>
				</Callout>
			{/if}

			{#if editingCard}
				<form
					class="card j5-rename"
					data-testid="card-editor"
					onsubmit={(e) => {
						e.preventDefault();
						const patch = cardPatch(d);
						if (!Object.keys(patch).length) {
							editingCard = false;
							return;
						}
						write(
							() => api().refineJobUseCaseCard(a.key, patch),
							'Saved to draft: use case card. Test it, then publish.',
						);
					}}
				>
					<p class="muted">
						Your changes are drafts laid over the template’s card, field by field. Scenarios stay the
						platform’s.
					</p>
					<label class="field">
						<span class="label">Summary</span>
						<textarea class="input" rows="2" bind:value={cSummary}></textarea>
					</label>
					<label class="field">
						<span class="label">Customer phrasings (one per line)</span>
						<textarea class="input" rows="3" bind:value={cExamples}></textarea>
					</label>
					<div class="row">
						<label class="field">
							<span class="label">North-star metric</span>
							<input class="input" bind:value={cMetricLabel} />
						</label>
						<label class="field">
							<span class="label">Target</span>
							<input class="input" bind:value={cMetricTarget} />
						</label>
					</div>
					<label class="field">
						<span class="label">Sample dialogue (<code>customer:</code> / <code>agent:</code> per line)</span>
						<textarea class="input" rows="4" bind:value={cDialogue}></textarea>
					</label>
					<div class="row">
						<button class="btn primary" type="submit" {...writeAttrs(session, 'job_designer')}
							>Save card</button
						>
						<button class="btn" type="button" onclick={() => (editingCard = false)}>Cancel</button>
					</div>
				</form>
			{/if}

			{#if renaming}
				<form
					class="card j5-rename"
					onsubmit={(e) => {
						e.preventDefault();
						write(
							() => api().relabelJobActivity(a.key, label),
							'Saved to draft: new label. Test it, then publish.',
						);
					}}
				>
					<label class="field">
						<span class="label">Label customers and staff see</span>
						<input class="input" bind:value={label} />
					</label>
					<div class="row">
						<button class="btn primary" type="submit" {...writeAttrs(session, 'job_designer')}
							>Save label</button
						>
						{#if a.refinement === 'relabelled'}
							<button
								class="btn"
								type="button"
								{...writeAttrs(session, 'job_designer')}
								onclick={() =>
									write(() => api().restoreJobActivity(a.key), 'Saved to draft: template label restored.')}
								>Use template label “{a.base_label}”</button
							>
						{/if}
						<button class="btn" type="button" onclick={() => (renaming = false)}>Cancel</button>
					</div>
				</form>
			{/if}

			<div class="j5-top">
				<section class="card j5-hero" data-testid="use-case">
					<span class="j5-eyebrow">The use case</span>
					<p class="j5-summary">{u?.summary ?? a.trigger}</p>
					<div class="j5-facts">
						<div>
							<span class="j5-k">Identity needed</span>
							{#if u}
								<b>Level {u.identity.level} · {u.identity.label}</b>
								<span class="cell-sub">{u.identity.how}</span>
							{:else}<b>—</b>{/if}
						</div>
						<div>
							<span class="j5-k">North-star · {d.metric.label}</span>
							<b>{d.metric.current ?? 'not live yet'}</b>
							<span class="muted"> target {d.metric.target}</span>
							{#if progress != null}<Meter value={progress} cls={progress >= 1 ? 's-ok' : 's-warn'} />{/if}
						</div>
						<div data-testid="context-space">
							<span class="j5-k">Context space</span>
							{#if d.context_space}<b class="mono">{d.context_space}</b>{:else}<b>—</b>
								<span class="cell-sub">Not part of a knowledge area yet.</span>{/if}
						</div>
						<div>
							<span class="j5-k">Last 30 days</span>
							{#if u?.live}
								<b>{u.live.cases.toLocaleString('en')} cases</b>
								<span class="cell-sub"
									>{pct(u.live.contained, u.live.cases)} handled without a person · {pct(
										u.live.escalated,
										u.live.cases,
									)} handed over</span
								>
							{:else}
								<b>No traffic</b>
								<span class="cell-sub">No conversations about it yet.</span>
							{/if}
						</div>
					</div>
				</section>

				<Card
					title="Where it comes from"
					sub="Template layers are read-only here. Your refinements are saved to draft, tested in the test chat, and reach customers when you publish a release."
				>
					{#if u}
						<ol class="j5-lineage" data-testid="lineage">
							{#each u.lineage as l, i (`${i}:${l.kind}:${l.tag ?? ''}`)}
								<li class:j5-mine={l.kind === 'tenant'}>
									<span class="j5-k">{LAYER[l.kind]}</span>
									{#if l.tag}<span class="mono">{l.tag}</span>{/if}
									<span class="cell-sub">{l.contributes}</span>
								</li>
							{/each}
						</ol>
						{#if u.pending.length}
							<div class="j5-pending" data-testid="draft">
								<b>Draft, not published:</b>
								{#each u.pending as p, i (p.at + i)}{i ? '; ' : ''}{p.what} ({when(p.at)}){/each}.
								{#if u.live_vs_draft}<div class="j5-diff" data-testid="live-vs-draft">
										<span>Live: <b>{u.live_vs_draft.live}</b></span><span
											>Draft: <b>{u.live_vs_draft.draft}</b></span
										>
									</div>{/if}
								<div class="row j5-draft-actions">
									<a class="btn sm" href="/chat">Test in test chat</a>
									<a class="btn sm primary" href="{base}/release">Review &amp; publish</a>
								</div>
							</div>
						{/if}
						<p class="j5-more">
							Routes per system: <a href="{base}/operations">Operations &amp; routes</a>
							{#if isSuperAdmin(session)}· Template: <a href="/platform/journeys/retail">journey designer</a
								>{/if}
						</p>
					{/if}
				</Card>
			</div>

			{#if u}
				<Card
					title="How the agent handles it"
					sub="One conversation, left to right: recognise the request, run the procedure against your systems, check the reply."
				>
					<UseCaseFlow {d} {u} {base} />
				</Card>
			{/if}

			<div class="grid g2">
				<Card title="Recent cases" sub="What actually happened in conversations about this activity.">
					{#if u?.outcomes.length}
						<div class="row j5-outcomes">
							{#each u.outcomes as o (o.code)}
								<span class="chip {o.code === 'ok' ? 'ok' : o.code.startsWith('open:') ? 'muted' : 'warn'}"
									>{o.code} × {o.count}</span
								>
							{/each}
						</div>
					{/if}
					{#if u?.recent_cases.length}
						<table class="tbl dense">
							<thead><tr><th>Case</th><th>What</th><th>Outcome</th></tr></thead>
							<tbody>
								{#each u.recent_cases as c (c.case_id)}
									<tr>
										<td class="mono"><a href="{base}/cases/{c.case_id}">{c.case_id}</a></td>
										<td
											>{c.title}<span class="cell-sub"
												>{when(c.started_at)} · {c.env} · id {c.identity_level}</span
											></td
										>
										<td class="mono">{c.outcome ?? c.status}</td>
									</tr>
								{/each}
							</tbody>
						</table>
					{:else}
						<p class="muted">No cases yet for this activity.</p>
					{/if}
				</Card>
				<Card title="Sample conversation" sub="What a good conversation for this use case looks like.">
					<div class="dialogue" data-testid="dialogue">
						{#each d.sample_dialogue as t, i (i)}
							<div class="turn {t.from}">
								<span class="j5-who"
									>{t.from === 'agent'
										? 'Agent'
										: t.from === 'customer'
											? 'Customer'
											: 'Gates / operations'}</span
								>
								<p>{t.text}</p>
							</div>
						{/each}
					</div>
				</Card>
			</div>

			<Card
				title="Objects and operations"
				sub="What the agent may touch for this activity. R = required for go-live, O = optional (you may leave it unrouted)."
			>
				<ul class="j5-objects">
					{#each a.objects as o (o.object)}
						<li>
							<span class="mono"><b>{o.object}</b></span>
							<span class="chip {o.role === 'subject' ? 'accent' : 'muted'}">{o.role}</span>
							<span class="muted">{o.fields.join(' · ')}</span>
							{#if o.required_inputs.length}<span class="chip warn">needs {o.required_inputs.join(', ')}</span
								>{/if}
						</li>
					{:else}
						<li class="muted">No objects.</li>
					{/each}
				</ul>
				{#if d.operations.length}
					<div class="tbl-wrap">
						<table class="tbl" data-testid="ops">
							<thead>
								<tr>
									<th>Operation</th>
									<th>R/O</th>
									<th>Effect</th>
									<th>Identity</th>
									<th>prod</th>
									<th>test</th>
								</tr>
							</thead>
							<tbody>
								{#each d.operations as o (opLabel(o))}
									<tr>
										<td class="mono">{opLabel(o)}<span class="cell-sub">{o.modality}</span></td>
										<td
											><span class="chip {o.required ? 'accent' : 'muted'}">{o.required ? 'R' : 'O'}</span
											></td
										>
										<td>{EFFECT_TEXT[o.side_effect]}</td>
										<td class="mono">{o.identity_level}</td>
										{#each o.routes as r (r.env)}
											<td>
												<span class="chip {o.required && r.kind === 'unrouted' ? 'err' : ROUTE_CHIP[r.kind]}"
													>{routeText(r)}</span
												>
												{#if r.kind === 'instance' && !r.contract_tested}<span class="cell-sub j5-warn"
														>write unverified</span
													>{/if}
											</td>
										{/each}
									</tr>
								{/each}
							</tbody>
						</table>
					</div>
				{:else}
					<p class="muted" data-testid="ops">No runtime operations: a company-side content activity.</p>
				{/if}
			</Card>

			<div class="grid g2">
				<Card
					title="Operation results"
					sub="The canonical JSON the operations return, on success and on a typical error."
				>
					<Seg
						items={[
							['success', 'Success'],
							['error', 'Error'],
						]}
						bind:active={example}
						label="Example"
					/>
					<div class="j5-ex">
						<JsonView value={example === 'success' ? d.success_example : d.error_example} />
					</div>
				</Card>
				<Card
					title="Knowledge it reads ({d.stage.label})"
					sub="What your knowledge store must hold so replies can cite policy."
				>
					<ul class="j5-kinds">
						{#each d.knowledge_kinds as k (k.kind)}
							<li>
								<span class="mono">{k.kind}</span>
								{#if k.present}<span class="chip ok">in {k.store}</span>{:else}<span class="chip warn"
										>missing{k.required ? ' · required' : ''}</span
									>{/if}
								<span class="cell-sub">{k.description}</span>
							</li>
						{:else}
							<li class="muted">No knowledge kinds for this stage.</li>
						{/each}
					</ul>
					<p class="j5-more"><a href="{base}/knowledge-kinds">Knowledge kinds</a></p>
				</Card>
			</div>
		</div>
	{/snippet}
</LoadState>

<style>
	.j5-top {
		display: grid;
		grid-template-columns: minmax(0, 1.7fr) minmax(0, 1fr);
		gap: 16px;
		align-items: start;
	}
	/* hero sizes to its content */
	.j5-hero {
		align-self: start;
		padding: 18px 20px;
		display: flex;
		flex-direction: column;
		gap: 12px;
	}
	.j5-eyebrow,
	.j5-k {
		font: 600 11px/1.2 var(--cond);
		letter-spacing: 0.07em;
		text-transform: uppercase;
		color: var(--muted);
		display: block;
	}
	.j5-summary {
		margin: 0;
		font-size: 17px;
		line-height: 1.5;
		max-width: 70ch;
	}
	.j5-facts {
		display: grid;
		grid-template-columns: repeat(3, minmax(0, 1fr));
		gap: 16px;
		border-top: 1px solid var(--line-2);
		padding-top: 12px;
	}
	.j5-facts b {
		display: inline-block;
		margin-top: 3px;
	}
	.j5-facts :global(.meter) {
		display: block;
		margin-top: 6px;
	}
	.j5-lineage {
		list-style: none;
		margin: 0;
		padding: 0 0 0 14px;
		display: grid;
		grid-template-columns: minmax(0, 1fr);
		min-width: 0;
		gap: 8px;
		border-left: 2px solid var(--line);
	}
	.j5-lineage li {
		position: relative;
		min-width: 0;
		overflow-wrap: anywhere;
	}
	.j5-lineage li::before {
		content: '';
		position: absolute;
		left: -20px;
		top: 4px;
		width: 8px;
		height: 8px;
		border-radius: 50%;
		background: var(--line);
	}
	.j5-lineage li.j5-mine::before {
		background: var(--accent);
	}
	.j5-lineage li.j5-mine .j5-k {
		color: var(--accent);
	}
	.j5-pending {
		margin: 10px 0 0;
		font-size: 12.5px;
		padding: 6px 8px;
		border-radius: 6px;
		background: color-mix(in srgb, var(--warn) 10%, var(--panel));
	}
	.j5-diff {
		display: flex;
		flex-wrap: wrap;
		gap: 4px 14px;
		margin-top: 6px;
	}
	.j5-draft-actions {
		margin-top: 8px;
		gap: 6px;
	}
	.j5-more {
		margin: 10px 0 0;
		font-size: 12.5px;
		color: var(--muted);
	}
	.j5-rename {
		padding: 14px 16px;
		display: grid;
		gap: 10px;
		max-width: 560px;
	}
	.j5-outcomes {
		margin-bottom: 8px;
		gap: 6px;
	}
	.j5-objects {
		list-style: none;
		margin: 0 0 12px;
		padding: 0;
		display: grid;
		gap: 6px;
	}
	.j5-objects li {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
		align-items: center;
		font-size: 13px;
	}
	.j5-kinds {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 8px;
	}
	.j5-warn {
		color: var(--warn);
	}
	.j5-ex {
		margin-top: 10px;
		max-height: 340px;
		overflow: auto;
	}
	.dialogue {
		display: grid;
		gap: 8px;
	}
	.turn {
		max-width: 88%;
		padding: 8px 10px;
		border-radius: 10px;
		background: var(--sunk);
	}
	.turn p {
		margin: 2px 0 0;
	}
	.turn.customer {
		justify-self: start;
		border-left: 3px solid var(--cust);
	}
	.turn.agent {
		justify-self: end;
		background: color-mix(in srgb, var(--accent) 10%, var(--panel));
	}
	.turn.system {
		justify-self: center;
		background: transparent;
		border: 1px dashed var(--line);
		font: 12px var(--mono);
		color: var(--muted);
	}
	.j5-who {
		font-size: 11px;
		color: var(--muted);
		text-transform: uppercase;
		letter-spacing: 0.06em;
	}
	@media (max-width: 1000px) {
		.j5-top {
			grid-template-columns: minmax(0, 1fr);
		}
	}
	@media (max-width: 640px) {
		.j5-facts {
			grid-template-columns: minmax(0, 1fr);
		}
		.j5-summary {
			font-size: 15.5px;
		}
	}
</style>
