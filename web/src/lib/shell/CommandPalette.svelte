<script lang="ts">
	// ⌘K / Ctrl+K search: a top-bar trigger plus a modal list of pages (settings are pages) and, from two letters on,
	// the cases of every agent (`GET /api/search`, debounced). A failed case search leaves the pages alone.
	import { tick } from 'svelte';
	import { goto } from '$app/navigation';
	import type { NavSession } from '$lib/pages';
	import Icon from '$lib/ui/Icon.svelte';
	import { api } from '$lib/app.svelte';
	import type { CaseHit } from '$lib/api/types/runtime';
	import { paletteRows, searchPages } from './search';

	let { session, job }: { session: NavSession; job: string } = $props();

	let open = $state(false);
	let q = $state('');
	let active = $state(0);
	let input = $state<HTMLInputElement>();
	let trigger = $state<HTMLButtonElement>();

	let hits = $state<CaseHit[]>([]);
	let seq = 0;
	const pages = $derived(searchPages(q, session, job));
	const results = $derived(paletteRows(pages, hits));
	const caseStart = $derived(results.findIndex((r) => r.kind === 'case'));

	// Debounced case search; a late answer for an older query is dropped.
	$effect(() => {
		const text = q.trim();
		const mine = ++seq;
		if (!open || text.length < 2) {
			hits = [];
			return;
		}
		const timer = setTimeout(() => {
			void Promise.resolve()
				.then(() => api().searchCases(text))
				.then((h) => {
					if (mine === seq) hits = h;
				})
				.catch(() => {
					if (mine === seq) hits = [];
				});
		}, 200);
		return () => clearTimeout(timer);
	});
	const isMac = typeof navigator !== 'undefined' && /mac|iphone|ipad/i.test(navigator.platform);

	async function show() {
		q = '';
		hits = [];
		active = 0;
		open = true;
		await tick();
		input?.focus();
	}
	function close() {
		open = false;
		void tick().then(() => trigger?.focus());
	}
	function pick(i: number) {
		const item = results[i];
		if (!item) return;
		open = false;
		void goto(item.href);
	}
	function onWindowKey(e: KeyboardEvent) {
		if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k') {
			e.preventDefault();
			if (open) close();
			else void show();
		}
	}
	function onKey(e: KeyboardEvent) {
		if (e.key === 'Escape') {
			e.preventDefault();
			close();
		} else if (e.key === 'ArrowDown') {
			e.preventDefault();
			active = results.length ? (active + 1) % results.length : 0;
		} else if (e.key === 'ArrowUp') {
			e.preventDefault();
			active = results.length ? (active - 1 + results.length) % results.length : 0;
		} else if (e.key === 'Enter') {
			e.preventDefault();
			pick(active);
		}
	}
</script>

<svelte:window onkeydown={onWindowKey} />

<button
	bind:this={trigger}
	class="palette-trigger"
	type="button"
	aria-haspopup="dialog"
	aria-label="Search pages"
	data-testid="palette-trigger"
	onclick={() => void show()}
	><Icon name="search" /><span>Search pages</span><kbd>{isMac ? '⌘K' : 'Ctrl K'}</kbd></button
>

{#if open}
	<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
	<div class="palette-scrim" onclick={close}>
		<div
			class="palette"
			role="dialog"
			aria-modal="true"
			aria-label="Search"
			tabindex="-1"
			onclick={(e) => e.stopPropagation()}
		>
			<input
				bind:this={input}
				bind:value={q}
				role="combobox"
				aria-expanded="true"
				aria-controls="palette-results"
				aria-activedescendant={results.length ? `palette-opt-${active}` : undefined}
				aria-label="Search"
				placeholder="Search pages and cases"
				autocomplete="off"
				spellcheck="false"
				oninput={() => (active = 0)}
				onkeydown={onKey}
			/>
			<ul id="palette-results" role="listbox" aria-label="Results">
				{#each results as r, i (r.href)}
					{#if i === 0 && r.kind === 'page'}<li class="group" role="presentation">Pages</li>{/if}
					{#if i === caseStart}<li class="group" role="presentation">Cases</li>{/if}
					<li
						id="palette-opt-{i}"
						role="option"
						aria-selected={i === active}
						class:on={i === active}
						onmouseenter={() => (active = i)}
						onclick={() => pick(i)}
						onkeydown={null}
					>
						{#if r.kind === 'page'}<Icon name={r.icon} /><span>{r.label}</span>{:else}<Icon
								name="search"
							/><span>{r.label}<small>{r.detail}</small></span>{/if}
					</li>
				{/each}
			</ul>
			{#if q.trim() && !results.length}<p class="muted" role="status">Nothing matches “{q.trim()}”.</p>{/if}
		</div>
	</div>
{/if}

<style>
	.palette-trigger {
		display: inline-flex;
		align-items: center;
		gap: 8px;
		height: 34px;
		padding: 0 10px;
		border: 1px solid var(--line);
		border-radius: 6px;
		background: var(--panel);
		color: var(--muted);
		font: 400 13px/1 var(--sans);
		cursor: pointer;
	}
	.palette-trigger:hover {
		color: var(--fg);
		border-color: var(--muted);
	}
	kbd {
		font: 500 11px/1 var(--mono, ui-monospace, monospace);
		padding: 2px 5px;
		border: 1px solid var(--line);
		border-radius: 4px;
	}
	.palette-scrim {
		position: fixed;
		inset: 0;
		z-index: 60;
		display: flex;
		justify-content: center;
		align-items: flex-start;
		padding: 12vh 16px 0;
		background: color-mix(in srgb, var(--fg) 40%, transparent);
	}
	.palette {
		width: min(560px, 100%);
		background: var(--panel);
		color: var(--fg);
		border: 1px solid var(--line);
		border-radius: 10px;
		box-shadow: 0 12px 40px rgb(0 0 0 / 0.3);
		overflow: hidden;
	}
	input {
		width: 100%;
		box-sizing: border-box;
		padding: 14px 16px;
		border: 0;
		border-bottom: 1px solid var(--line);
		background: transparent;
		color: inherit;
		font: 400 15px/1.2 var(--sans);
	}
	input:focus-visible {
		outline: 2px solid var(--accent, currentColor);
		outline-offset: -2px;
	}
	ul {
		list-style: none;
		margin: 0;
		padding: 6px;
		max-height: 50vh;
		overflow: auto;
	}
	li {
		display: flex;
		align-items: center;
		gap: 10px;
		padding: 9px 10px;
		border-radius: 6px;
		cursor: pointer;
	}
	li.group {
		padding: 6px 10px 2px;
		font: 600 11px/1 var(--sans);
		text-transform: uppercase;
		color: var(--muted);
		cursor: default;
	}
	small {
		display: block;
		color: var(--muted);
		font-size: 12px;
	}
	li.on {
		background: var(--sunk);
	}
	p {
		margin: 0;
		padding: 12px 16px;
	}
	@media (max-width: 860px) {
		.palette-trigger span,
		kbd {
			display: none;
		}
		.palette-trigger {
			width: 34px;
			justify-content: center;
			padding: 0;
		}
	}
</style>
