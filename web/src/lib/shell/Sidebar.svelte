<script lang="ts">
	// The scoped sidebar: brand, Workspace + Agent switchers, the nav groups of the current scope (Advanced and More
	// are folded behind a toggle), and the data footer. Rendered in the ink sidebar on desktop and inside the drawer on a
	// phone (`inDrawer`: light panel colours, plus the user menu inline).
	import { page } from '$app/state';
	import type { NavGroupView } from '$lib/pages';
	import type { Badge } from '$lib/status/inbox';
	import { prefs } from '$lib/ui/prefs.svelte';
	import Icon from '$lib/ui/Icon.svelte';
	import ScopeSwitcher from './ScopeSwitcher.svelte';
	import type { Snippet } from 'svelte';

	let {
		nav,
		tenantLabel,
		inDrawer = false,
		showScope = true,
		footer,
		badges = {},
		onpickworkspace,
		onnavigate,
		extra,
	}: {
		nav: NavGroupView[];
		tenantLabel: string;
		inDrawer?: boolean;
		showScope?: boolean;
		footer?: string[];
		/** Counts of things that need a person, keyed by the href of the item they badge. */
		badges?: Record<string, Badge>;
		onpickworkspace: () => void;
		onnavigate?: () => void;
		extra?: Snippet;
	} = $props();

	let opened = $state<Record<string, boolean>>({});

	function isOn(href: string): boolean {
		if (href.includes('#')) return false;
		const path = page.url.pathname;
		// Section roots with sub-pages listed beside them match exactly.
		if (href === '/admin' || /^\/admin\/jobs\/[^/]+$/.test(href) || href === '/admin/jobs')
			return path === href;
		return path === href || path.startsWith(`${href}/`);
	}
	function isOpen(g: NavGroupView): boolean {
		if (!g.collapsed) return true;
		return opened[g.group] ?? g.sections.some((s) => s.items.some((i) => isOn(i.href)));
	}
</script>

<div class="sidebar" class:in-drawer={inDrawer} data-testid="sidebar">
	<div class="brand">
		<span class="brand-mark" aria-hidden="true"><i></i><i></i><i></i></span>
		<div><b>Agent</b><span>{tenantLabel}</span></div>
	</div>
	{#if showScope}<ScopeSwitcher {tenantLabel} {onpickworkspace} {onnavigate} />{/if}
	<nav class="nav" aria-label="Pages">
		{#each nav as g (g.group)}
			{@const open = isOpen(g)}
			<div class="nav-group" data-testid="nav-{g.group}">
				{#if g.collapsed}
					<button
						class="nav-fold"
						type="button"
						aria-expanded={open}
						aria-controls="nav-items-{g.group}"
						onclick={() => (opened[g.group] = !open)}
						><span>{g.label}</span><i aria-hidden="true" class:open></i></button
					>
				{:else}
					<span>{g.label}</span>
				{/if}
				{#if open}
					<div id="nav-items-{g.group}" class="nav-items">
						{#each g.sections as sec (sec.section)}
							{#if sec.section !== g.label}<span class="nav-sec">{sec.section}</span>{/if}
							{#each sec.items as item (item.href)}
								<a
									href={item.href}
									class:on={isOn(item.href)}
									aria-current={isOn(item.href) ? 'page' : undefined}
									onclick={() => onnavigate?.()}
									><Icon name={item.icon} /><span>{item.label}</span>{#if badges[item.href]}{@const b =
											badges[item.href]!}<span
											class="badge"
											class:warn={!b.urgent}
											data-testid="nav-badge"
											aria-label="{b.count} {b.count === 1 ? 'item needs' : 'items need'} you">{b.count}</span
										>{/if}{#if prefs.showMilestones && item.milestone !== 'M1'}<span class="badge ms"
											>{item.milestone}</span
										>{/if}</a
								>
							{/each}
						{/each}
					</div>
				{/if}
			</div>
		{/each}
	</nav>
	{@render extra?.()}
	{#if footer}
		<div class="side-foot">
			{#each footer as line, i (i)}{#if i === 0}<span>{line}</span>{:else}<code>{line}</code>{/if}{/each}
		</div>
	{/if}
</div>

<style>
	.sidebar {
		display: flex;
		flex-direction: column;
		min-height: 100%;
	}
	.nav-items {
		display: grid;
		gap: 1px;
	}
	.nav-sec {
		font: 500 11.5px/1 var(--sans);
		padding: 8px 10px 4px;
		color: var(--sb-muted, var(--side-muted));
	}
	.nav-fold {
		display: flex;
		align-items: center;
		justify-content: space-between;
		width: 100%;
		border: 0;
		background: none;
		cursor: pointer;
		text-align: left;
		font: 600 10.5px/1 var(--cond);
		letter-spacing: 0.1em;
		text-transform: uppercase;
		color: var(--side-muted);
		padding: 8px 10px 6px;
		border-radius: 6px;
	}
	.nav-fold:hover {
		background: var(--side-hi);
	}
	.nav-fold i {
		width: 6px;
		height: 6px;
		border-right: 1.5px solid currentColor;
		border-bottom: 1.5px solid currentColor;
		transform: rotate(-45deg);
		transition: transform 0.12s;
	}
	.nav-fold i.open {
		transform: rotate(45deg);
	}
	.badge.ms {
		background: color-mix(in srgb, var(--side-muted) 25%, transparent);
		color: var(--side-fg);
	}
	/* In the phone drawer the nav sits on a light panel, not the ink sidebar. */
	.in-drawer {
		--sb-fg: var(--fg);
		--sb-strong: var(--fg);
		--sb-muted: var(--muted);
		--sb-hi: var(--sunk);
	}
	.in-drawer :global(.nav a),
	.in-drawer .nav-fold {
		color: var(--fg);
	}
	.in-drawer :global(.nav a.on) {
		color: var(--fg);
		background: var(--sunk);
	}
	.in-drawer :global(.nav-group > span),
	.in-drawer .nav-sec,
	.in-drawer .nav-fold {
		color: var(--muted);
	}
	.in-drawer .nav-fold:hover {
		background: var(--sunk);
	}
	.in-drawer .badge.ms {
		color: var(--fg);
	}
	.in-drawer :global(.brand) {
		display: none;
	}
	.in-drawer :global(.side-foot) {
		border-top-color: var(--line);
		color: var(--muted);
	}
	.in-drawer :global(.side-foot code) {
		color: var(--fg);
	}
</style>
