<script lang="ts">
	// Reason, channel and priority filters of the inbox, in one popover. Options come from the reasons and channels
	// the inbox has seen; "Any" clears a filter.
	import { CHANNEL_LABEL } from '$lib/conversations/labels';
	import { ITEM_PRIORITIES } from '$lib/journey/J21Handover';
	import type { Filters } from '$lib/support/inbox.svelte';

	let {
		filters,
		reasons,
		channels,
		onChange,
	}: {
		filters: Filters;
		reasons: string[];
		channels: string[];
		onChange: (f: Filters) => void;
	} = $props();

	let open = $state(false);
	const active = $derived([filters.reason, filters.channel, filters.priority].filter((x) => !!x).length);
	const label = (c: string) => (CHANNEL_LABEL as Record<string, string>)[c] ?? c;
	const set = (k: keyof Filters, v: string) => onChange({ ...filters, [k]: v || undefined });
</script>

<div class="fp">
	<button
		class="btn sm"
		type="button"
		aria-expanded={open}
		aria-haspopup="true"
		onclick={() => (open = !open)}
	>
		Filters{active ? ` · ${active}` : ''}
	</button>
	{#if open}
		<div class="pop" role="group" aria-label="Inbox filters">
			<label
				>Reason
				<select value={filters.reason ?? ''} onchange={(e) => set('reason', e.currentTarget.value)}>
					<option value="">Any</option>
					{#each reasons as r (r)}<option value={r}>{r}</option>{/each}
				</select>
			</label>
			<label
				>Channel
				<select value={filters.channel ?? ''} onchange={(e) => set('channel', e.currentTarget.value)}>
					<option value="">Any</option>
					{#each channels as c (c)}<option value={c}>{label(c)}</option>{/each}
				</select>
			</label>
			<label
				>Priority
				<select value={filters.priority ?? ''} onchange={(e) => set('priority', e.currentTarget.value)}>
					<option value="">Any</option>
					{#each ITEM_PRIORITIES as p (p)}<option value={p}>{p[0]!.toUpperCase() + p.slice(1)}</option>{/each}
				</select>
			</label>
			<div class="row">
				<button class="btn sm" type="button" disabled={!active} onclick={() => onChange({})}>Clear</button>
				<button class="btn sm" type="button" onclick={() => (open = false)}>Done</button>
			</div>
		</div>
	{/if}
</div>

<style>
	.fp {
		position: relative;
	}
	.pop {
		position: absolute;
		right: 0;
		top: calc(100% + 4px);
		z-index: 10;
		display: grid;
		gap: 8px;
		min-width: 220px;
		padding: 10px;
		border: 1px solid var(--line);
		border-radius: 8px;
		background: var(--panel);
		box-shadow: 0 6px 24px rgb(0 0 0 / 0.14);
	}
	label {
		display: grid;
		gap: 3px;
		font-size: 12px;
		color: var(--muted);
	}
	.row {
		display: flex;
		justify-content: flex-end;
		gap: 6px;
	}
</style>
