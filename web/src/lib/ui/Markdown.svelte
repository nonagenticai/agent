<script lang="ts">
	// A chat message's text as the safe markdown subset of $lib/markdown: elements and text interpolation only
	// (no {@html}), so HTML in a message stays text. Links are https only and open in a new tab.
	import { parseMarkdown, type MdInline } from '$lib/markdown';

	let { text, cls = '' }: { text: string; cls?: string } = $props();
	const blocks = $derived(parseMarkdown(text ?? ''));
</script>

{#snippet inline(
	nodes: MdInline[],
)}{#each nodes as n, i (i)}{#if n.t === 'text'}{n.v}{:else if n.t === 'strong'}<strong
				>{@render inline(n.c)}</strong
			>{:else if n.t === 'em'}<em>{@render inline(n.c)}</em>{:else if n.t === 'code'}<code>{n.v}</code
			>{:else}<a href={n.href} target="_blank" rel="noopener noreferrer">{@render inline(n.c)}</a
			>{/if}{/each}{/snippet}

<div class="md {cls}">
	{#each blocks as b, i (i)}
		{#if b.t === 'p'}
			<p>
				{#each b.lines as l, j (j)}{#if j > 0}<br />{/if}{@render inline(l)}{/each}
			</p>
		{:else if b.t === 'ul'}
			<ul>
				{#each b.items as it, j (j)}<li>{@render inline(it)}</li>{/each}
			</ul>
		{:else}
			<ol start={b.start}>
				{#each b.items as it, j (j)}<li>{@render inline(it)}</li>{/each}
			</ol>
		{/if}
	{/each}
</div>

<style>
	.md :global(p),
	.md :global(ul),
	.md :global(ol) {
		margin: 0;
	}
	.md :global(* + p),
	.md :global(* + ul),
	.md :global(* + ol) {
		margin-top: 0.4em;
	}
	.md :global(ul),
	.md :global(ol) {
		padding-left: 1.3em;
	}
	.md :global(code) {
		font-size: 0.92em;
		padding: 0 0.25em;
		border-radius: 4px;
		background: color-mix(in srgb, currentColor 10%, transparent);
	}
</style>
