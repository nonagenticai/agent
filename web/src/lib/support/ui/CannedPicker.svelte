<script lang="ts">
	// Saved replies, opened by `/` in the composer. The composer owns the keys (Arrow, Enter, Escape on its textarea),
	// so no global binding is made here; clicking an option picks it too.
	import type { CannedReply } from '$lib/api/types/support';

	let {
		replies,
		query = '',
		active = 0,
		onPick,
	}: {
		replies: CannedReply[];
		query?: string;
		active?: number;
		onPick: (r: CannedReply) => void;
	} = $props();
</script>

<ul class="cp" role="listbox" aria-label="Saved replies">
	{#each replies as r, i (r.id)}
		<li role="option" aria-selected={i === active} class:on={i === active}>
			<button type="button" tabindex="-1" onclick={() => onPick(r)}>
				<strong>{r.title}</strong>
				<span>{r.body}</span>
			</button>
		</li>
	{:else}
		<li class="none" role="presentation">
			{query ? `No saved reply matches “${query}”` : 'No saved replies yet'}
		</li>
	{/each}
</ul>

<style>
	.cp {
		list-style: none;
		margin: 0 0 6px;
		padding: 4px;
		max-height: 200px;
		overflow-y: auto;
		border: 1px solid var(--line);
		border-radius: var(--r);
		background: var(--panel);
		box-shadow: var(--shadow);
	}
	li.on button {
		background: var(--sunk);
	}
	button {
		display: grid;
		gap: 1px;
		width: 100%;
		text-align: left;
		border: 0;
		background: none;
		padding: 5px 8px;
		border-radius: 4px;
		cursor: pointer;
	}
	span {
		color: var(--muted);
		font-size: 12px;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.none {
		padding: 6px 8px;
		color: var(--muted);
		font-size: 12px;
	}
</style>
