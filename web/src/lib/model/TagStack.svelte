<script lang="ts">
	// P7 composition stack: a job composes base → industry → department → job, later layers overriding earlier ones.
	// Drawn top-down (job on top) with this layer marked, plus the layers that build on top of it.
	import type { TagStackLayer } from '$lib/api/types/model';
	import { tagHref } from './logic';
	import { SCOPE_LABEL } from './ui';

	let { stack, above }: { stack: TagStackLayer[]; above: string[] } = $props();

	const top = $derived([...stack].reverse());
</script>

<div class="wrap">
	<ol class="stack-list" aria-label="Composition order, top overrides bottom">
		{#each top as s (s.layer_key)}
			<li class="layer s-{s.scope}" class:here={s.here}>
				<span class="scope">{SCOPE_LABEL[s.scope]}</span>
				{#if s.here}
					<span class="key mono">{s.layer_key}</span>
					<span class="tag">this layer</span>
				{:else if s.latest !== null}
					<a class="key mono" href={tagHref(s.layer_key, s.latest)}>{s.layer_key}</a>
					<span class="tag mono">@v{s.latest}</span>
				{:else}
					<span class="key mono">{s.layer_key}</span>
					<span class="tag">untagged</span>
				{/if}
			</li>
		{/each}
	</ol>
	<p class="hint">
		↑ Upper layers patch or tombstone what lower layers add. A job pins one tag of each layer; the release cut
		compiles them in this order.
	</p>
	{#if above.length}
		<p class="hint">
			<b>{above.length} layer{above.length === 1 ? '' : 's'} build on this one</b> and inherit what it
			contributes:
			{#each above as a, i (a)}<span class="mono">{a}</span>{i < above.length - 1 ? ', ' : ''}{/each}.
		</p>
	{/if}
</div>

<style>
	.stack-list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 4px;
	}
	.layer {
		display: grid;
		grid-template-columns: 92px minmax(0, 1fr) auto;
		align-items: center;
		gap: 10px;
		padding: 8px 12px;
		border: 1px solid var(--line);
		border-radius: var(--r);
		background: var(--sunk);
		font-size: 13px;
	}
	/* Each scope sits a little narrower than the one under it, so the pile reads as a stack. */
	.s-job {
		margin: 0 18px;
	}
	.s-industry_dept {
		margin: 0 12px;
	}
	.s-industry {
		margin: 0 6px;
	}
	.here {
		background: color-mix(in srgb, var(--accent) 12%, var(--panel));
		border-color: var(--accent);
		font-weight: 600;
	}
	.scope {
		font: 600 11px/1.2 var(--cond);
		letter-spacing: 0.06em;
		text-transform: uppercase;
		color: var(--muted);
	}
	.key {
		overflow-wrap: anywhere;
	}
	.tag {
		font-size: 12px;
		color: var(--muted);
	}
	.here .tag {
		color: var(--accent);
	}
	.hint {
		margin: 10px 0 0;
		font-size: 12px;
		color: var(--muted);
	}
	.hint b {
		color: var(--fg);
		font-weight: 600;
	}
</style>
