<script lang="ts">
	// Everything that used to crowd the top bar: who you are, theme, technical details, Platform studio, sign out, and
	// (super admin) a Developer submenu with the build-team tools. A popover on desktop; `inline` lays the same content
	// out inside the phone drawer.
	import { tick } from 'svelte';
	import { logout, session } from '$lib/auth/session.svelte';
	import { devPages } from '$lib/pages';
	import { prefs, setShowMilestones, setShowTech } from '$lib/ui/prefs.svelte';
	import Icon from '$lib/ui/Icon.svelte';

	let {
		inline = false,
		displayName,
		roleLabel,
		dark,
		ontheme,
		onviewas,
		onleaveviewas,
		onnavigate,
	}: {
		inline?: boolean;
		displayName: string;
		roleLabel: string;
		dark: boolean;
		ontheme: () => void;
		onviewas: () => void;
		onleaveviewas: () => void;
		onnavigate?: () => void;
	} = $props();

	let open = $state(false);
	let devOpen = $state(false);
	let trigger = $state<HTMLButtonElement>();
	let root = $state<HTMLElement>();
	const shown = $derived(inline || open);
	const initials = $derived(
		displayName
			.split(/\s+/)
			.filter(Boolean)
			.slice(0, 2)
			.map((w) => w[0]!.toUpperCase())
			.join('') || '?',
	);
	const dev = $derived(devPages(session));

	async function close(refocus: boolean) {
		open = false;
		devOpen = false;
		if (refocus) {
			await tick();
			trigger?.focus();
		}
	}
	function leave(fn: () => void) {
		void close(false);
		onnavigate?.();
		fn();
	}
	function onKey(e: KeyboardEvent) {
		if (e.key === 'Escape' && open) {
			e.preventDefault();
			void close(true);
		}
	}
	function onDocDown(e: MouseEvent) {
		if (open && root && !root.contains(e.target as Node)) void close(false);
	}
	function onFocusOut(e: FocusEvent) {
		if (open && root && e.relatedTarget instanceof Node && !root.contains(e.relatedTarget)) void close(false);
	}
</script>

<svelte:document onmousedown={onDocDown} />

<div class="um" class:inline bind:this={root} onkeydown={onKey} onfocusout={onFocusOut} role="presentation">
	{#if inline}
		<p class="um-who">
			<strong>{displayName}</strong>
			<span>{roleLabel}</span>
		</p>
	{:else}
		<button
			bind:this={trigger}
			class="um-trigger"
			type="button"
			data-testid="user-menu"
			aria-expanded={open}
			aria-controls="user-menu-panel"
			aria-label="Account menu: {displayName}, {roleLabel}"
			onclick={() => (open = !open)}
			><span class="avatar" aria-hidden="true">{initials}</span><span class="um-name">{displayName}</span
			><span class="um-role">{roleLabel}</span></button
		>
	{/if}
	{#if shown}
		<div id="user-menu-panel" class="um-panel" data-testid="user-menu-panel">
			{#if !inline}
				<p class="um-who">
					<strong>{displayName}</strong>
					<span>{roleLabel}</span>
				</p>
			{/if}
			<button class="um-item" type="button" data-testid="theme-toggle" onclick={ontheme}
				><Icon name={dark ? 'sun' : 'moon'} size={16} /><span
					>{dark ? 'Switch to light mode' : 'Switch to dark mode'}</span
				></button
			>
			<label class="um-item um-check"
				><input
					type="checkbox"
					checked={prefs.showTech}
					onchange={(e) => setShowTech(e.currentTarget.checked)}
				/><span>Show technical details</span></label
			>
			{#if session.superAdmin}
				<a class="um-item" href="/platform" data-testid="platform-studio" onclick={() => leave(() => {})}
					><Icon name="layers" size={16} /><span>Platform studio</span></a
				>
			{/if}
			{#if session.member && !session.superAdmin}
				<button
					class="um-item"
					type="button"
					data-testid="view-as-open"
					title="Preview the app with fewer of your roles"
					onclick={() => leave(onviewas)}><span>View as…</span></button
				>
				{#if session.viewAs}<button
						class="um-item"
						type="button"
						data-testid="view-as-leave"
						onclick={() => leave(onleaveviewas)}><span>Exit view-as</span></button
					>{/if}
			{/if}
			{#if session.superAdmin}
				<button
					class="um-item um-sub"
					type="button"
					data-testid="developer-toggle"
					aria-expanded={devOpen}
					aria-controls="user-menu-dev"
					onclick={() => (devOpen = !devOpen)}
					><span>Developer</span><i aria-hidden="true" class:open={devOpen}></i></button
				>
				{#if devOpen}
					<div id="user-menu-dev" class="um-dev" data-testid="developer-menu">
						<label class="um-item um-check"
							><input
								type="checkbox"
								checked={prefs.showMilestones}
								onchange={(e) => setShowMilestones(e.currentTarget.checked)}
							/><span>Show build milestones</span></label
						>
						{#if session.member}
							<button
								class="um-item"
								type="button"
								data-testid="view-as-open"
								title="Preview the app with fewer of your roles"
								onclick={() => leave(onviewas)}><span>View as…</span></button
							>
						{/if}
						{#if session.viewAs}<button
								class="um-item"
								type="button"
								data-testid="view-as-leave"
								onclick={() => leave(onleaveviewas)}><span>Exit view-as</span></button
							>{/if}
						{#each dev as d (d.id)}
							<a class="um-item" href={d.href} onclick={() => leave(() => {})}
								><Icon name={d.icon} size={16} /><span>{d.label}</span></a
							>
						{/each}
					</div>
				{/if}
			{/if}
			<button class="um-item" type="button" data-testid="sign-out" onclick={() => leave(() => void logout())}
				><span>Sign out</span></button
			>
		</div>
	{/if}
</div>

<style>
	.um {
		position: relative;
		margin-left: auto;
	}
	.um.inline {
		margin: 0;
		border-top: 1px solid var(--line);
		padding: 10px 10px 14px;
	}
	.um-trigger {
		display: flex;
		align-items: center;
		gap: 8px;
		border: 1px solid var(--line);
		background: var(--panel);
		color: var(--fg);
		border-radius: 999px;
		padding: 3px 12px 3px 3px;
		font: inherit;
		font-size: 12.5px;
		cursor: pointer;
	}
	.um-trigger:hover {
		border-color: var(--muted);
	}
	.um-name {
		font-weight: 600;
		max-width: 22ch;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.um-role {
		color: var(--muted);
		white-space: nowrap;
	}
	.um-who {
		margin: 0;
		padding: 8px 10px;
		display: grid;
		gap: 2px;
	}
	.um-who span {
		color: var(--muted);
		font-size: 13px;
	}
	.um-panel {
		display: grid;
		gap: 1px;
		min-width: 250px;
		padding: 6px;
	}
	.um:not(.inline) .um-panel {
		position: absolute;
		right: 0;
		top: calc(100% + 6px);
		z-index: 30;
		background: var(--panel);
		border: 1px solid var(--line);
		border-radius: 8px;
		box-shadow: var(--shadow);
		max-height: calc(100vh - 80px);
		overflow: auto;
	}
	.um-item {
		display: flex;
		align-items: center;
		gap: 10px;
		width: 100%;
		padding: 8px 10px;
		border: 0;
		border-radius: 6px;
		background: none;
		color: var(--fg);
		font: inherit;
		font-size: 13.5px;
		text-align: left;
		text-decoration: none;
		cursor: pointer;
	}
	.um-item:hover {
		background: var(--sunk);
	}
	.um-check input {
		margin: 0;
	}
	.um-sub {
		justify-content: space-between;
	}
	.um-sub i {
		width: 6px;
		height: 6px;
		border-right: 1.5px solid currentColor;
		border-bottom: 1.5px solid currentColor;
		transform: rotate(-45deg);
		transition: transform 0.12s;
	}
	.um-sub i.open {
		transform: rotate(45deg);
	}
	.um-dev {
		display: grid;
		gap: 1px;
		margin-left: 10px;
		padding-left: 6px;
		border-left: 2px solid var(--line);
	}
</style>
