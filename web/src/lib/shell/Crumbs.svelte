<script lang="ts">
	import type { Crumb } from './crumbs';
	let { crumbs }: { crumbs: Crumb[] } = $props();
</script>

{#if crumbs.length}
	<nav class="crumbs" aria-label="Breadcrumb" data-testid="crumbs">
		<ol>
			{#each crumbs as c, i (i)}
				<li>
					{#if c.href}<a href={c.href}>{c.label}</a>{:else}<span
							aria-current={i === crumbs.length - 1 ? 'page' : undefined}>{c.label}</span
						>{/if}
				</li>
			{/each}
		</ol>
	</nav>
{/if}

<style>
	.crumbs {
		min-width: 0;
		flex: 1;
	}
	ol {
		display: flex;
		align-items: center;
		flex-wrap: nowrap;
		gap: 0;
		list-style: none;
		margin: 0;
		padding: 0;
		min-width: 0;
		font-size: 13.5px;
	}
	li {
		display: flex;
		align-items: center;
		min-width: 0;
		white-space: nowrap;
	}
	li + li::before {
		content: '/';
		color: var(--faint);
		padding: 0 8px;
	}
	li a,
	li span {
		overflow: hidden;
		text-overflow: ellipsis;
		max-width: 28ch;
		color: var(--muted);
		text-decoration: none;
	}
	li a:hover {
		color: var(--fg);
		text-decoration: underline;
	}
	li:last-child span {
		color: var(--fg);
		font-weight: 600;
	}
	@media (max-width: 860px) {
		li:not(:last-child):not(:first-child) {
			display: none;
		}
		li:first-child:not(:last-child) {
			display: none;
		}
		li:last-child::before {
			content: none;
		}
	}
</style>
