<script lang="ts">
	// D68 per-language knowledge coverage: every knowledge kind × every language the job serves (D59 job setting),
	// counted as the documents of the kind's bound store in that language (each document carries the language of
	// the source that produced it). A gap is a warning — the agent can still answer from another language's passage
	// or hand over — never a block on save, test or publish. Fix a gap by adding a source in that language.
	import { api } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import { jobPath } from '$lib/scope';
	import { fmt } from '$lib/format';
	import { LANGUAGE_LABEL } from '$lib/api/types/core';
	import type { KnowledgeCoverage } from '$lib/api/types/tenant';
	import { STAGES } from '$lib/ontology/catalogue';
	import { gapsByLanguage } from '$lib/tenant/coverage-D68';
	import Callout from '$lib/ui/Callout.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';

	let { job, tick }: { job: string; tick?: unknown } = $props();
	const cov = new Resource<KnowledgeCoverage>(
		() => api().knowledgeCoverage(job),
		() => false,
	);
	$effect(() => {
		void job;
		void tick;
		void cov.refresh();
	});
	const stageLabel = (s: string) => STAGES.find(([k]) => k === s)?.[1] ?? s;
	const label = (l: string) => LANGUAGE_LABEL[l] ?? l;
	const sourcesHref = $derived(jobPath(job, '/sources'));
	const languagesHref = $derived(jobPath(job, '/refinements'));
	const order = (c: KnowledgeCoverage) =>
		[...c.rows].sort(
			(a, b) =>
				STAGES.findIndex(([k]) => k === a.stage) - STAGES.findIndex(([k]) => k === b.stage) ||
				Number(b.required) - Number(a.required) ||
				a.kind.localeCompare(b.kind),
		);
</script>

<section class="card cov" aria-label="Knowledge coverage per language" data-testid="coverage-matrix">
	<header class="cov-head">
		<h2>Coverage per language</h2>
		<p class="muted">
			Documents in each kind's store, per language this job serves (<a href={languagesHref}
				>languages served</a
			>). Each document has the language of the source that produced it. A gap is a warning, not a block: the
			agent answers from a passage in another language, or hands over.
		</p>
	</header>
	{#if cov.value.state === 'unavailable'}
		<Unavailable reason={cov.value.reason} retry={cov.retry} />
	{:else if cov.value.state === 'ok'}
		{@const c = cov.value.data}
		{@const byLang = gapsByLanguage(c)}
		{#if c.languages.length === 0}
			<p class="muted">This job serves no language yet; set its languages first.</p>
		{:else}
			{#if c.gaps}
				<Callout kind="gap" title={`${c.gaps} missing language${c.gaps === 1 ? '' : 's'} on required kinds`}>
					<span data-testid="coverage-gaps"
						>{c.languages
							.filter((l) => byLang[l])
							.map(
								(l) =>
									`${label(l)}: ${byLang[l]} required kind${byLang[l] === 1 ? '' : 's'} without a document`,
							)
							.join(' · ')}. Add a source in that language on <a href={sourcesHref}>Sources</a> (its language is
						set per source).</span
					>
				</Callout>
			{:else}
				<Callout kind="info" title="Every required kind has documents in every language"
					><span>Nothing to add.</span></Callout
				>
			{/if}
			<div class="tbl-wrap">
				<table class="tbl dense cov-tbl">
					<thead>
						<tr>
							<th>Knowledge kind</th>
							{#each c.languages as l, i (l)}
								<th class="r" title={label(l)}
									>{l}{#if i === 0}<span class="cell-sub">default</span>{/if}</th
								>
							{/each}
						</tr>
					</thead>
					<tbody>
						{#each order(c) as r (`${r.stage}|${r.kind}`)}
							<tr data-kind={r.kind}>
								<td>
									<span class="mono kind">{r.kind}</span>
									{#if r.required}<Chip text="required" variant="accent" />{/if}
									<span class="cell-sub">{stageLabel(r.stage)} · {r.store ? r.store : 'no store bound'}</span>
								</td>
								{#each c.languages as l (l)}
									{@const n = r.documents[l] ?? 0}
									<td
										class="r cell"
										class:gap={n === 0 && r.required}
										class:soft={n === 0 && !r.required}
										data-lang={l}
										title={n === 0
											? `No ${label(l)} document${r.store ? ` in ${r.store}` : ': no store bound'}${r.required ? ' (required kind)' : ''}`
											: `${n} ${label(l)} document${n === 1 ? '' : 's'}`}>{n === 0 ? 'gap' : fmt.n(n)}</td
									>
								{/each}
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
			{#if Object.keys(c.unserved).length}
				<p class="muted cov-note">
					Also in the stores, in languages this job does not serve: {Object.entries(c.unserved)
						.map(([l, n]) => `${n} ${label(l)}`)
						.join(', ')}. They still answer, but count for no language here.
				</p>
			{/if}
		{/if}
	{:else}
		<p class="muted">Loading…</p>
	{/if}
</section>

<style>
	.cov {
		margin-top: 16px;
		padding: 14px 16px;
		display: flex;
		flex-direction: column;
		gap: 10px;
		min-width: 0;
	}
	.cov-head h2 {
		margin: 0;
		font: 600 15px/1.3 var(--cond);
	}
	.cov-head p {
		margin: 2px 0 0;
		font-size: 12.5px;
	}
	.kind {
		margin-right: 4px;
		overflow-wrap: anywhere;
	}
	.cell {
		font-variant-numeric: tabular-nums;
		width: 64px;
	}
	.cell.gap {
		background: color-mix(in srgb, var(--warn) 16%, transparent);
		color: var(--warn);
		font-weight: 600;
	}
	.cell.soft {
		color: var(--muted);
		font-style: italic;
	}
	.cov-note {
		margin: 0;
		font-size: 12.5px;
	}
</style>
