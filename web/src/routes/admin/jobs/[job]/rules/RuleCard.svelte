<script lang="ts">
	// J9 one rule: the sentence it enforces (with the tenant's values filled in), your values vs the template's
	// suggestion, the policy passage it cites, where it is evaluated, its outcomes, recent decisions, and a dry run.
	import type { BusinessRule } from '$lib/api/types/release';
	import { tagHref } from '$lib/model/logic';
	import Chip from '$lib/ui/Chip.svelte';
	import RuleTry from './RuleTry.svelte';
	import { outcomeVariant, parseLayer, statementParts, withUnit } from './J9-view';

	let {
		rule: r,
		base,
		superAdmin,
		wa,
		when,
		onvalues,
		onpassage,
		ondefinition,
	}: {
		rule: BusinessRule;
		/** `/admin/jobs/<job>` */
		base: string;
		superAdmin: boolean;
		wa: { disabled: boolean; title: string | undefined };
		when: (iso: string | null) => string;
		onvalues: (r: BusinessRule) => void;
		onpassage: (r: BusinessRule) => void;
		ondefinition?: (r: BusinessRule) => void;
	} = $props();

	const parts = $derived(statementParts(r));
	const layer = $derived(parseLayer(r.slot_layer));
	const differs = (v: BusinessRule['values'][number]) =>
		v.suggested != null &&
		v.value != null &&
		v.value.trim().toLowerCase() !== v.suggested.trim().toLowerCase();
</script>

<article class="j9rule" class:is-ph={r.placeholder} data-rule={r.id} aria-labelledby="rule-{r.id}">
	<header class="j9rule-head">
		<div class="j9rule-title">
			<h3 id="rule-{r.id}"><span class="mono j9rule-id">{r.id}</span> {r.label}</h3>
			{#if r.placeholder}<Chip text="Placeholder" variant="warn" />{:else}<Chip
					text="Values set"
					variant="ok"
				/>{/if}
		</div>
		<p class="j9rule-meta small muted">
			{r.tenant_authored ? 'Authored for this job ·' : 'Slot from'}
			{#if layer && superAdmin}<a class="mono" href={tagHref(layer.layer, layer.v)}>{r.slot_layer}</a
				>{:else}<span class="mono">{r.slot_layer ?? 'the job template'}</span>{/if}
			{#if r.slot_release}· shipped in <a class="mono" href="{base}/release/{r.slot_release}"
					>{r.slot_release}</a
				>{/if}
		</p>
	</header>

	{#if parts.length}
		<p class="j9rule-statement" data-testid="rule-statement">
			{#each parts as p, i (i)}{#if 'text' in p}{p.text}{:else if p.value === null}<mark
						class="j9-unset"
						title="{p.label}: not set">{p.label.toLowerCase()} — not set</mark
					>{:else}<mark class="j9-val" title={p.label}>{withUnit(p.value, p.unit)}</mark>{/if}{/each}
		</p>
	{/if}

	<div class="j9rule-body">
		<section class="j9rule-yours" aria-label="Your values">
			<h4>
				Yours <span class="muted small">— saved to draft, live once published</span>
				{#if r.live_values}<span class="chip warn">draft differs from live</span>{/if}
			</h4>
			<table class="tbl j9vals">
				<thead>
					<tr><th>Value</th><th>Your value</th><th>Template suggests</th></tr>
				</thead>
				<tbody>
					{#each r.values as v (v.key)}
						<tr>
							<td
								>{v.label}
								<div class="muted small">{v.hint}</div></td
							>
							<td>
								{#if v.value === null}<span class="j9-ph">not set</span>
								{:else}<b>{withUnit(v.value, v.unit)}</b>{/if}
								{#if differs(v)}<div class="small j9-own">your choice</div>{/if}
								{#if r.live_values && r.live_values[v.key] !== v.value}<div class="small muted">
										live: {r.live_values[v.key] == null ? 'not set' : withUnit(r.live_values[v.key]!, v.unit)}
									</div>{/if}
							</td>
							<td class="muted">{v.suggested == null ? '—' : withUnit(v.suggested, v.unit)}</td>
						</tr>
					{/each}
				</tbody>
			</table>
			<div class="j9rule-passage">
				<div class="small muted">Policy passage it cites</div>
				{#if r.passage}
					<b>{r.passage.title}</b> <span class="muted small mono">{r.passage.kind}</span>
					<blockquote>{r.passage.excerpt}</blockquote>
				{:else}<p class="j9-ph small">
						None linked — when the agent refuses under {r.id} it cannot cite your policy.
					</p>{/if}
			</div>
			<p class="small muted">
				{r.updated_by
					? `${r.live_values ? 'Saved to draft' : 'Last saved'} ${when(r.updated_at)} · ${r.updated_by_name ?? r.updated_by}`
					: 'Never set'}
			</p>
			<div class="j9row">
				{#if r.tenant_authored && ondefinition}<button
						class="btn sm"
						type="button"
						disabled={wa.disabled}
						title={wa.title}
						onclick={() => ondefinition?.(r)}>Edit definition</button
					>{/if}
				<button
					class="btn sm primary"
					type="button"
					disabled={wa.disabled || !r.values.length}
					title={wa.title}
					onclick={() => onvalues(r)}>Set values</button
				>
				<button
					class="btn sm"
					type="button"
					disabled={wa.disabled}
					title={wa.title}
					onclick={() => onpassage(r)}>{r.passage ? 'Change passage' : 'Link policy passage'}</button
				>
			</div>
		</section>
		{#if r.facts?.length}
			<RuleTry rule={r} />
		{/if}
	</div>

	<div class="j9rule-foot">
		<section aria-label="Where it is evaluated">
			<h4>Evaluated when</h4>
			{#if r.uses?.length}
				<ul class="j9list">
					{#each r.uses as u (u.activity + u.step)}
						<li>
							<a href="{base}/journey/{encodeURIComponent(u.activity)}">{u.activity_label}</a>
							{#if u.procedure}→ <a class="mono" href="{base}/procedures/{encodeURIComponent(u.procedure)}"
									>{u.procedure}</a
								>{/if}
							<div class="small muted">{u.step}</div>
						</li>
					{/each}
				</ul>
			{/if}
			<div class="small">
				{#each r.applies_to as op (op)}<Chip text={op} variant="tool" />{' '}{/each}
			</div>
			<div class="small muted">reads <span class="mono">{r.inputs.join(', ')}</span></div>
		</section>
		<section aria-label="Outcomes">
			<h4>Outcomes</h4>
			{#if r.outcomes?.length}
				<ul class="j9list">
					{#each r.outcomes as o (o.code)}
						<li>
							<Chip text={o.code} variant={outcomeVariant(o.code)} />
							<span class="small">{o.label}</span>
							<div class="small muted">{o.agent_does}</div>
						</li>
					{/each}
				</ul>
			{:else}<p class="small mono">{r.outcome}</p>{/if}
		</section>
		<section aria-label="Recent decisions">
			<h4>Recent decisions</h4>
			{#if r.decisions?.length}
				<ul class="j9list">
					{#each r.decisions as d (d.case_id)}
						<li>
							<a class="mono" href="{base}/cases/{encodeURIComponent(d.case_id)}">{d.case_id}</a>
							<Chip text={d.code} variant={outcomeVariant(d.code)} />
							<div class="small muted">{d.detail} · {when(d.at)}</div>
						</li>
					{/each}
				</ul>
			{:else}<p class="small muted">No case has evaluated {r.id} yet.</p>{/if}
			{#if r.history?.length}
				<details class="j9hist">
					<summary class="small">Change history ({r.history.length})</summary>
					<ul class="j9list">
						{#each r.history as h, i (i)}
							<li class="small">
								{h.change}
								<div class="muted">{h.by_name ?? h.by} · {when(h.at)}</div>
							</li>
						{/each}
					</ul>
				</details>
			{/if}
		</section>
	</div>
</article>

<style>
	.j9rule {
		background: var(--panel);
		border: 1px solid var(--line);
		border-radius: var(--r);
		padding: 16px;
		display: grid;
		gap: 12px;
		min-width: 0;
	}
	.j9rule.is-ph {
		border-color: var(--warn);
	}
	.j9rule-head {
		display: grid;
		gap: 2px;
	}
	.j9rule-title {
		display: flex;
		justify-content: space-between;
		gap: 8px;
		align-items: center;
		flex-wrap: wrap;
	}
	h3 {
		margin: 0;
		font-size: 16px;
	}
	h4 {
		margin: 0 0 6px;
		font-size: 13px;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		color: var(--muted);
	}
	.j9rule-id {
		font-size: 14px;
		color: var(--accent);
	}
	.j9rule-meta {
		margin: 0;
	}
	.small {
		font-size: 12px;
	}
	.j9rule-statement {
		margin: 0;
		font-size: 15px;
		line-height: 1.7;
	}
	mark {
		padding: 1px 6px;
		border-radius: 4px;
		font-weight: 600;
		color: var(--fg);
	}
	.j9-val {
		background: color-mix(in srgb, var(--accent) 14%, transparent);
	}
	.j9-unset {
		background: color-mix(in srgb, var(--warn) 18%, transparent);
		color: var(--warn);
	}
	.j9-ph {
		color: var(--warn);
		font-weight: 600;
		margin: 0;
	}
	.j9-own {
		color: var(--accent);
	}
	.j9rule-body {
		display: grid;
		grid-template-columns: minmax(0, 1.2fr) minmax(0, 1fr);
		gap: 16px;
		align-items: start;
	}
	.j9rule-yours {
		display: grid;
		gap: 10px;
		min-width: 0;
	}
	.j9vals {
		width: 100%;
	}
	.j9vals td {
		vertical-align: top;
	}
	.j9rule-passage blockquote {
		margin: 4px 0 0;
		padding-left: 10px;
		border-left: 3px solid var(--line-2);
		color: var(--muted);
		font-size: 13px;
	}
	.j9rule-yours > p {
		margin: 0;
	}
	.j9row {
		display: flex;
		gap: 8px;
		flex-wrap: wrap;
	}
	.j9rule-foot {
		display: grid;
		grid-template-columns: repeat(3, minmax(0, 1fr));
		gap: 16px;
		border-top: 1px solid var(--line-2);
		padding-top: 12px;
	}
	.j9rule-foot section {
		min-width: 0;
		display: grid;
		gap: 6px;
		align-content: start;
	}
	.j9list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 8px;
	}
	.j9hist summary {
		cursor: pointer;
	}
	@media (max-width: 900px) {
		.j9rule-body,
		.j9rule-foot {
			grid-template-columns: minmax(0, 1fr);
		}
	}
</style>
