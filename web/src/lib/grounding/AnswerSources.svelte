<script lang="ts">
	// The sources under one agent answer: a grounding chip (grounded / not grounded / I don't know) and the cited
	// chunks — title and page, the store path, the chunk id on hover and when expanded. Used by the test chat, the
	// console and case transcripts. Renders nothing for an answer that used no knowledge.
	import type { AnswerGrounding } from '$lib/api/types/grounding';
	import { GROUNDING_LABEL, groundingOf, platformCite, sourceLabel, storePath } from './grounding';

	let { g, compact = false }: { g: AnswerGrounding; compact?: boolean } = $props();
	const verdict = $derived(groundingOf(g));
	const label = $derived(GROUNDING_LABEL[verdict]);
	const sources = $derived(g.sources ?? []);
</script>

{#if verdict !== 'no_knowledge'}
	<div class="as" class:compact data-testid="answer-sources" data-grounding={verdict}>
		{#if verdict !== 'grounded'}
			<span class="chip {label.variant} sm as-flag" title={label.title} data-testid="grounding-flag"
				>{verdict === 'not_grounded' ? '⚠ ' : ''}{label.text}</span
			>
		{/if}
		{#if sources.length}
			<details class="as-list">
				<summary>
					<span class="chip ok sm" title={label.title}>grounded</span>
					{sources.length} source{sources.length === 1 ? '' : 's'}:
					<span class="as-titles">{sources.map((s) => sourceLabel(s)).join(' · ')}</span>
				</summary>
				<ol>
					{#each sources as s (s.chunk_id)}
						{@const pc = platformCite(s)}
						<li title="chunk {s.chunk_id} · document {s.document_id}">
							<b>{sourceLabel(s)}</b>
							{#if pc}
								<span class="as-layer" data-testid="citation-platform"
									><span class="chip accent sm">{pc.text}</span>{#if pc.href}
										<a href={pc.href} target="_blank" rel="noopener noreferrer">source</a
										>{/if}{#if pc.licence}
										<span class="as-ids">{pc.licence}</span>{/if}</span
								>
							{:else}
								<span class="as-path mono">{storePath(s.store_path)}</span>
							{/if}
							<span class="as-excerpt">{s.excerpt}</span>
							<span class="as-ids mono">chunk {s.chunk_id} · {s.document_id}</span>
						</li>
					{/each}
				</ol>
			</details>
		{:else if !compact}
			<span class="as-none"
				>{verdict === 'dont_know'
					? 'The knowledge search found nothing to cite.'
					: 'Knowledge was searched, but the answer cites no source.'}</span
			>
		{/if}
	</div>
{/if}

<style>
	.as {
		display: flex;
		flex-wrap: wrap;
		align-items: baseline;
		gap: 4px 8px;
		margin-top: 4px;
		font-size: 12px;
		color: var(--muted);
		min-width: 0;
		max-width: 100%;
	}
	.chip.sm {
		font-size: 10.5px;
		padding: 2px 6px;
	}
	.as-list {
		min-width: 0;
		max-width: 100%;
	}
	.as-list summary {
		cursor: pointer;
		overflow-wrap: anywhere;
	}
	.as-titles {
		color: var(--fg);
	}
	.as-list ol {
		margin: 6px 0 2px;
		padding-left: 18px;
		display: grid;
		gap: 6px;
	}
	.as-list li {
		display: grid;
		gap: 1px;
		overflow-wrap: anywhere;
	}
	.as-list li b {
		color: var(--fg);
		font-weight: 600;
	}
	.as-path,
	.as-ids {
		font-size: 11px;
		color: var(--faint);
	}
	.as-excerpt {
		color: var(--muted);
	}
	.as-layer {
		display: inline-flex;
		flex-wrap: wrap;
		gap: 4px 6px;
		align-items: baseline;
		font-size: 11px;
	}
	.as-none {
		font-style: italic;
	}
	.compact .as-excerpt {
		display: none;
	}
</style>
