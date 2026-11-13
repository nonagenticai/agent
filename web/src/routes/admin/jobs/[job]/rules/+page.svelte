<script lang="ts">
	// J9 Business rules: the thresholds the business owns (RET-01, WAR-01, PAY-04 for retail). The template defines the
	// slot — id, inputs, outcomes, which activity evaluates it (10 §2.1, 11 §2.2) — and ships it in a release; the
	// tenant fills in the values and links the policy passage the agent cites. `rule.evaluate{…}` is a built-in agent
	// operation (K49). Values are a tenant refinement (D54): saved to the job's draft, run in the test chat, and live in
	// Production once a release publishes them; each case records the release it ran on. A rule without values is a "placeholder": the agent escalates, and it blocks the
	// go-live checklist (10 §5.2 step 7).
	import { onMount } from 'svelte';
	import { api, app, guard } from '$lib/app.svelte';
	import DraftBar from '$lib/release/DraftBar.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { canWrite, isSuperAdmin, writeHint, writeAttrs } from '$lib/auth/roles';
	import { Resource } from '$lib/load.svelte';
	import { jobPath } from '$lib/scope';
	import type { Paged } from '$lib/api/types/core';
	import type { BusinessRule, PolicyPassage, RuleEvaluation } from '$lib/api/types/release';
	import Callout from '$lib/ui/Callout.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import PageHead from '$lib/ui/PageHead.svelte';
	import RuleCard from './RuleCard.svelte';
	import RuleAuthor from '$lib/rules/RuleAuthor.svelte';
	let authorOpen = $state(false);
	let authorRule = $state<BusinessRule | null>(null);
	function author(rule: BusinessRule | null) {
		authorRule = rule;
		authorOpen = true;
	}
	import { outcomeVariant, withUnit } from './J9-view';

	const rules = new Resource<Paged<BusinessRule>>(() => api().businessRules());
	const passages = new Resource<PolicyPassage[]>(
		() => api().rulePassages(),
		() => false,
	);
	onMount(() => {
		void rules.refresh();
		void passages.refresh();
	});
	const writer = $derived(canWrite(session, 'job_designer'));
	const sa = $derived(isSuperAdmin(session));
	const wa = $derived(writeAttrs(session, 'job_designer'));
	const base = $derived(jobPath(app.job));
	const when = (iso: string | null) =>
		iso
			? new Date(iso).toLocaleString('en-GB', { dateStyle: 'medium', timeStyle: 'short', timeZone: 'UTC' })
			: '—';

	// Values drawer + "what this change does" preview
	let editing = $state<BusinessRule | null>(null);
	let valuesOpen = $state(false);
	let draft = $state<Record<string, string>>({});
	let busy = $state(false);
	const dirty = $derived(!!editing && editing.values.some((v) => (draft[v.key] ?? '') !== (v.value ?? '')));
	function editValues(r: BusinessRule) {
		editing = r;
		draft = Object.fromEntries(r.values.map((v) => [v.key, v.value ?? '']));
		impact = [];
		valuesOpen = true;
	}
	const draftValues = () =>
		Object.fromEntries((editing?.values ?? []).map((v) => [v.key, (draft[v.key] ?? '').trim() || null]));

	type Impact = { case_id: string; before: RuleEvaluation; after: RuleEvaluation };
	let impact = $state<Impact[]>([]);
	let impactSeq = 0;
	$effect(() => {
		const r = editing;
		const open = valuesOpen;
		const values = draftValues();
		if (!r || !open || !r.decisions?.length) return;
		const seq = ++impactSeq;
		const t = setTimeout(async () => {
			try {
				const rows = await Promise.all(
					r.decisions!.map(async (d) => ({
						case_id: d.case_id,
						before: await api().evaluateRule(r.id, d.facts, undefined, d.at),
						after: await api().evaluateRule(r.id, d.facts, values, d.at),
					})),
				);
				if (seq === impactSeq) impact = rows;
			} catch {
				if (seq === impactSeq) impact = [];
			}
		}, 150);
		return () => clearTimeout(t);
	});

	async function saveValues() {
		if (!editing || busy) return;
		busy = true;
		const r = editing;
		const values = draftValues();
		const ok = await guard(
			() => api().saveBusinessRule(r.id, values),
			`Saved ${r.id} to draft — try it in the test chat; customers get it when you publish.`,
		);
		busy = false;
		if (ok) {
			valuesOpen = false;
			void rules.refresh();
		}
	}

	// Passage drawer
	let linking = $state<BusinessRule | null>(null);
	let linkOpen = $state(false);
	let choice = $state('');
	function openLink(r: BusinessRule) {
		linking = r;
		choice = r.passage?.id ?? '';
		linkOpen = true;
		if (passages.value.state !== 'ok') void passages.refresh();
	}
	async function saveLink(passageId: string | null) {
		if (!linking || busy) return;
		busy = true;
		const r = linking;
		const ok = await guard(
			() => api().linkRulePassage(r.id, passageId),
			(passageId ? `${r.id} now cites its policy passage.` : `Unlinked the passage from ${r.id}.`) +
				' Saved to draft.',
		);
		busy = false;
		if (ok) {
			linkOpen = false;
			void rules.refresh();
		}
	}
</script>

<PageHead
	id="J9"
	sub="The numbers and exceptions your business decides — return window, warranty period, what to say about a pending charge. Before the agent promises a return, a claim or a refund, it checks the rule with your values."
>
	{#snippet actions()}
		<button class="btn primary" type="button" {...wa} onclick={() => author(null)}
			>Create business rule</button
		>
		<a class="btn sm" href="{base}/readiness">Readiness</a>
		<a class="btn sm" href="{base}/release">Releases</a>
	{/snippet}
</PageHead>

{#if !writer}
	<Callout kind="info" title="Read only"
		>You can read every rule and try it out. Setting values or passages: {writeHint(
			'job_designer',
		).toLowerCase()}.</Callout
	>
{/if}

<section class="j9model" aria-label="How a business rule works" data-testid="rule-model">
	<ol class="j9flow">
		<li>
			<span class="j9step">1</span>
			<b>Facts</b>
			<span class="small muted">The agent reads the order, payment or asset (delivery date, status…).</span>
		</li>
		<li class="mine">
			<span class="j9step">2</span>
			<b>Rule + your values</b>
			<span class="small muted"
				><span class="mono">rule.evaluate</span> compares the facts with your thresholds.</span
			>
		</li>
		<li>
			<span class="j9step">3</span>
			<b>Outcome</b>
			<span class="small"
				><Chip text="eligible" variant="ok" />
				<Chip text="ineligible:RET-01" variant="err" />
				<Chip text="escalate" variant="warn" /></span
			>
		</li>
		<li>
			<span class="j9step">4</span>
			<b>Agent acts</b>
			<span class="small muted">Opens the case, or explains why not — citing your policy passage.</span>
		</li>
	</ol>
	<div class="j9own">
		<div>
			<b class="small">From the template — read-only here</b>
			<p class="small muted">
				Template rule definitions and their facts and outcomes. Changes arrive with a new <a
					href="{base}/release">release</a
				>.
			</p>
		</div>
		<div class="mine">
			<b class="small">Yours — set here</b>
			<p class="small muted">
				Your own rule definitions, conditions, values and policy passages. A save goes to the draft: the test
				chat uses it at once, customers get it when you publish a release. Decisions already made stay as they
				were — each case records the release it ran on.
			</p>
		</div>
	</div>
</section>

<DraftBar page="J9" tick={rules.value} />

<LoadState resource={rules}>
	{#snippet empty()}
		<div class="empty">
			No business rules yet. Create a rule for this job, or add rules through its template.
		</div>
	{/snippet}
	{#snippet children(page)}
		{@const placeholders = page.items.filter((r) => r.placeholder)}
		{@const cited = page.items.filter((r) => r.passage).length}
		<div class="j9sum" aria-label="Summary">
			<span><b>{page.items.length}</b> rules</span>
			<span><b>{page.items.length - placeholders.length}</b> with values</span>
			<span class:warn={placeholders.length > 0}><b>{placeholders.length}</b> placeholder</span>
			<span class:warn={cited < page.items.length}><b>{cited}</b>/{page.items.length} cite a policy</span>
		</div>
		{#if placeholders.length}
			<Callout
				kind="gap"
				title="{placeholders.map((r) => r.id).join(', ')} {placeholders.length === 1
					? 'is a placeholder'
					: 'are placeholders'}"
			>
				A rule without values cannot be evaluated: the agent hands those cases to a person. This blocks
				<a href="{base}/readiness">Readiness</a> (go-live), not apply.
			</Callout>
		{/if}
		<div class="j9rules" data-testid="business-rules">
			{#each page.items as r (r.id)}
				<RuleCard
					rule={r}
					{base}
					superAdmin={sa}
					{wa}
					{when}
					onvalues={editValues}
					onpassage={openLink}
					ondefinition={author}
				/>
			{/each}
		</div>
		<p class="j9floors" data-testid="jurisdiction-note">
			<span class="chip sm">future · D59</span>
			<b>Rule floors from jurisdiction packs.</b> Country law (Netherlands, Belgium, Germany…) will come as jurisdiction
			packs (industry × country) that add legal knowledge kinds and rule floors — e.g. a statutory minimum return
			window a rule value cannot go below. No pack exists yet: today every value here is yours, checked only against
			the template’s suggestion.
		</p>
	{/snippet}
</LoadState>

<RuleAuthor bind:open={authorOpen} rule={authorRule} onchanged={() => rules.refresh()} />

<Drawer bind:open={valuesOpen} title={editing ? `${editing.id} · ${editing.label}` : ''} {dirty}>
	{#if editing}
		<form
			class="stack"
			novalidate
			onsubmit={(e) => {
				e.preventDefault();
				saveValues();
			}}
		>
			<p class="muted">
				Evaluated by {editing.applies_to.join(', ')}. Leave a value empty to keep it a placeholder.
			</p>
			{#each editing.values as v (v.key)}
				<div class="j9field">
					<label for="rv-{v.key}"
						>{v.label}{#if !['text', 'list'].includes(v.unit)}
							({v.unit}){/if}</label
					>
					{#if v.unit === 'text'}
						<textarea id="rv-{v.key}" class="input" rows="2" bind:value={draft[v.key]}></textarea>
					{:else}
						<input
							id="rv-{v.key}"
							class="input"
							inputmode={v.unit === 'list' ? 'text' : 'numeric'}
							bind:value={draft[v.key]}
						/>
					{/if}
					<span class="muted small"
						>{v.hint}
						{#if v.suggested != null}
							Template suggests <b>{withUnit(v.suggested, v.unit)}</b>.
							{#if (draft[v.key] ?? '') !== v.suggested}<button
									type="button"
									class="j9link"
									onclick={() => (draft[v.key] = v.suggested ?? '')}>Use it</button
								>{/if}
						{/if}</span
					>
				</div>
			{/each}

			<section class="j9impact" aria-label="What this change does" data-testid="rule-impact">
				<b class="small">What this change does</b>
				<ul class="small">
					<li>
						Saved to draft: the <a href="/chat">test chat</a> uses it for the next {editing.id} evaluation; customers
						get it when you <a href="{base}/release">publish</a>.
					</li>
					<li>
						Once published, open cases keep the decision they already got; a later turn that re-checks uses
						the new value.
					</li>
				</ul>
				{#if impact.length}
					<div class="small muted">Recent decisions, re-checked with these values:</div>
					<ul class="j9impact-rows">
						{#each impact as row (row.case_id)}
							<li data-impact={row.case_id} class:flip={row.before.code !== row.after.code}>
								<span class="mono">{row.case_id}</span>
								<Chip text={row.before.code} variant={outcomeVariant(row.before.outcome)} />
								→
								<Chip text={row.after.code} variant={outcomeVariant(row.after.outcome)} />
								<span class="small">{row.before.code === row.after.code ? 'unchanged' : 'would change'}</span>
							</li>
						{/each}
					</ul>
				{/if}
			</section>

			<div class="j9row">
				<button type="submit" class="btn primary" disabled={busy || !dirty}
					>{busy ? 'Saving…' : 'Save values'}</button
				>
				<button type="button" class="btn" onclick={() => (valuesOpen = false)}>Cancel</button>
			</div>
		</form>
	{/if}
</Drawer>

<Drawer bind:open={linkOpen} title={linking ? `Policy passage for ${linking.id}` : ''}>
	{#if linking}
		<p class="muted small">
			When {linking.id} says no, the agent quotes this passage so the customer sees your own policy, not a made-up
			reason.
		</p>
		<LoadState
			resource={passages}
			empty="No policy passages yet: add a policy document to a knowledge store first."
		>
			{#snippet children(list)}
				<fieldset class="j9passages" aria-label="Policy passages">
					{#each list as p (p.id)}
						<label class="j9passage">
							<input type="radio" name="passage" value={p.id} bind:group={choice} />
							<span
								><b>{p.title}</b> <span class="mono muted small">{p.kind}</span><br /><span class="small"
									>{p.excerpt}</span
								></span
							>
						</label>
					{/each}
				</fieldset>
				<div class="j9row">
					<button
						class="btn primary"
						type="button"
						disabled={busy || !choice || choice === linking?.passage?.id}
						onclick={() => saveLink(choice)}>Link passage</button
					>
					{#if linking?.passage}<button
							class="btn"
							type="button"
							disabled={busy}
							onclick={() => saveLink(null)}>Unlink</button
						>{/if}
				</div>
			{/snippet}
		</LoadState>
	{/if}
</Drawer>

<style>
	.small {
		font-size: 12px;
	}
	.j9model {
		display: grid;
		gap: 10px;
		margin-bottom: 16px;
	}
	.j9flow {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		grid-template-columns: repeat(4, minmax(0, 1fr));
		gap: 8px;
		counter-reset: none;
	}
	.j9flow li {
		position: relative;
		display: grid;
		gap: 4px;
		align-content: start;
		padding: 10px 12px 10px 40px;
		background: var(--panel);
		border: 1px solid var(--line);
		border-radius: var(--r);
		min-width: 0;
	}
	.j9flow li:not(:last-child)::after {
		content: '→';
		position: absolute;
		right: -8px;
		top: 50%;
		transform: translate(50%, -50%);
		color: var(--muted);
		font-weight: 700;
		z-index: 1;
	}
	.j9flow li.mine,
	.j9own .mine {
		border-color: var(--accent);
		background: color-mix(in srgb, var(--accent) 7%, var(--panel));
	}
	.j9step {
		position: absolute;
		left: 10px;
		top: 10px;
		width: 22px;
		height: 22px;
		border-radius: 50%;
		display: grid;
		place-items: center;
		font-size: 12px;
		font-weight: 700;
		background: var(--sunk);
		color: var(--fg);
	}
	.j9own {
		display: grid;
		grid-template-columns: 1fr 1fr;
		gap: 8px;
	}
	.j9own > div {
		padding: 8px 12px;
		border: 1px dashed var(--line);
		border-radius: var(--r);
	}
	.j9own p {
		margin: 2px 0 0;
	}
	.j9sum {
		display: flex;
		flex-wrap: wrap;
		gap: 6px 18px;
		margin: 0 0 10px;
		font-size: 13px;
		color: var(--muted);
	}
	.j9sum b {
		color: var(--fg);
		font-size: 15px;
	}
	.j9sum .warn,
	.j9sum .warn b {
		color: var(--warn);
	}
	.j9rules {
		display: grid;
		gap: 14px;
		margin-top: 10px;
	}
	.j9field {
		display: grid;
		gap: 4px;
	}
	.j9link {
		font: inherit;
		background: none;
		border: 0;
		padding: 0;
		color: var(--accent);
		text-decoration: underline;
		cursor: pointer;
	}
	.j9impact {
		border: 1px solid var(--line);
		border-radius: var(--r);
		padding: 10px 12px;
		background: var(--sunk);
		display: grid;
		gap: 6px;
	}
	.j9impact ul {
		margin: 0;
		padding-left: 18px;
	}
	.j9impact-rows {
		list-style: none;
		padding: 0 !important;
		display: grid;
		gap: 6px;
	}
	.j9impact-rows li {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		align-items: center;
	}
	.j9impact-rows li.flip {
		font-weight: 600;
		color: var(--warn);
	}
	.j9row {
		display: flex;
		gap: 8px;
		margin-top: 8px;
		flex-wrap: wrap;
	}
	.j9passages {
		border: 0;
		padding: 0;
		margin: 0;
		display: grid;
		gap: 10px;
	}
	.j9passage {
		display: flex;
		gap: 8px;
		align-items: flex-start;
	}
	@media (max-width: 900px) {
		.j9flow {
			grid-template-columns: 1fr 1fr;
		}
		.j9flow li::after {
			display: none;
		}
		.j9own {
			grid-template-columns: 1fr;
		}
	}
	.j9floors {
		margin: 14px 0 0;
		padding: 8px 12px;
		border: 1px dashed var(--line);
		border-radius: var(--r);
		font-size: 12.5px;
		color: var(--muted);
	}
</style>
