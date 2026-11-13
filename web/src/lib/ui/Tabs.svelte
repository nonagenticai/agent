<script lang="ts">
	import type { Snippet } from 'svelte';
	let {
		items,
		active = $bindable(),
		children,
		label = 'Views',
	}: { items: [string, string, number?][]; active: string; children?: Snippet; label?: string } = $props();
	const uid = $props.id();
	// Narrow screens: the strip scrolls sideways; a fade marks the edge that hides more tabs, and the active tab is
	// scrolled into view so every tab stays reachable.
	let strip = $state<HTMLElement>();
	let fadeL = $state(false);
	let fadeR = $state(false);
	function edges() {
		if (!strip) return;
		fadeL = strip.scrollLeft > 2;
		fadeR = strip.scrollLeft + strip.clientWidth < strip.scrollWidth - 2;
	}
	$effect(() => {
		if (!strip || typeof ResizeObserver === 'undefined') return;
		const ro = new ResizeObserver(edges);
		ro.observe(strip);
		return () => ro.disconnect();
	});
	$effect(() => {
		const el = strip?.querySelector<HTMLElement>(`[aria-selected="true"]`);
		if (!strip || !el || active === undefined) return;
		const l = el.offsetLeft; // .tabs is positioned, so this is relative to the strip
		if (l < strip.scrollLeft || l + el.offsetWidth > strip.scrollLeft + strip.clientWidth)
			strip.scrollTo?.({ left: Math.max(0, l - 24) });
		edges();
	});
	function key(e: KeyboardEvent, i: number) {
		const target =
			e.key === 'ArrowRight'
				? (i + 1) % items.length
				: e.key === 'ArrowLeft'
					? (i + items.length - 1) % items.length
					: e.key === 'Home'
						? 0
						: e.key === 'End'
							? items.length - 1
							: -1;
		if (target < 0) return;
		e.preventDefault();
		active = items[target]![0];
		document.getElementById(`${uid}-tab-${active}`)?.focus();
	}
</script>

<div
	class="tabs"
	class:fade-l={fadeL}
	class:fade-r={fadeR}
	role="tablist"
	aria-label={label}
	bind:this={strip}
	onscroll={edges}
>
	{#each items as [id, label, count], i}
		<button
			type="button"
			role="tab"
			id="{uid}-tab-{id}"
			aria-controls={children ? `${uid}-panel` : undefined}
			tabindex={id === active ? 0 : -1}
			class="tab"
			class:on={id === active}
			aria-selected={id === active}
			onclick={() => (active = id)}
			onkeydown={(e) => key(e, i)}
		>
			{label}{#if count != null}<span class="count">{count}</span>{/if}
		</button>
	{/each}
</div>
{#if children}<div role="tabpanel" id="{uid}-panel" aria-labelledby="{uid}-tab-{active}" tabindex="0">
		{@render children()}
	</div>{/if}
