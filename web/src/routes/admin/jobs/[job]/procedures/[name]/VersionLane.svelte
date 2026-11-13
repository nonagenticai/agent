<script lang="ts">
	// J12 detail: every version of the procedure on one line, oldest → newest. Each says where it stands for
	// customers: runs now, in the live release, shadowed by a tenant version, in review (n of 2), draft, retired.
	// Clicking one selects it for the flow below.
	import type { ProcedureView } from '$lib/api/types/ontology';
	import { STATUS_LABEL, liveOf, pendingOf } from '../view';

	let {
		rows,
		current,
		release,
		onpick,
	}: {
		rows: ProcedureView[];
		current: number;
		release: string | null;
		onpick: (version: number) => void;
	} = $props();
	const live = $derived(liveOf(rows, release));
	const pending = $derived(pendingOf(rows, release));
	const ordered = $derived([...rows].sort((a, b) => a.version - b.version));
</script>

<ol class="vl" aria-label="Versions, oldest first" data-testid="procedure-versions">
	{#each ordered as v (v.version)}
		<li>
			<button
				type="button"
				class="vl-node s-{v.status.toLowerCase()}"
				class:run={v.version === live?.version}
				class:next={v.version === pending?.version}
				class:cur={v.version === current}
				aria-pressed={v.version === current}
				onclick={() => onpick(v.version)}
			>
				<span class="vl-top">
					<b class="mono">{v.version}</b>
					<span class="vl-origin">{v.origin === 'tenant' ? 'Tenant' : 'Base'}</span>
				</span>
				<span class="vl-status">
					{#if v.version === live?.version}Live{:else if v.version === pending?.version}Next release{:else if v.shadowed}Shadowed{:else if v.status === 'CANARY'}In
						review · {v.reviewers.length} approved{:else}{STATUS_LABEL[v.status]}{/if}
				</span>
				<span class="vl-sub">
					{#if v.released_in && v.released_in === release}in live release{:else if v.released_in}last in
						{v.released_in}{:else if v.version === pending?.version}published, not live yet{:else if v.status === 'DRAFT' || v.status === 'CANARY'}{#if v.outside_grant.length}<span
								class="vl-bad">{v.outside_grant.length} outside grant</span
							>{:else}draft{v.test_runs?.length
								? ` · ${v.test_runs.length} test run${v.test_runs.length === 1 ? '' : 's'}`
								: ''}{/if}{:else if v.outside_grant.length}<span class="vl-bad"
							>{v.outside_grant.length} outside grant</span
						>{:else if v.test_runs?.length}{v.test_runs.length} test run{v.test_runs.length === 1
							? ''
							: 's'}{:else}&nbsp;{/if}
				</span>
			</button>
		</li>
	{/each}
</ol>

<style>
	.vl {
		display: flex;
		flex-wrap: wrap;
		gap: 0;
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.vl li {
		display: flex;
		align-items: center;
	}
	.vl li + li::before {
		content: '';
		width: 14px;
		height: 2px;
		background: var(--line);
	}
	.vl-node {
		display: grid;
		gap: 2px;
		text-align: left;
		min-width: 128px;
		padding: 7px 10px;
		border-radius: var(--r);
		border: 1px solid var(--line);
		background: var(--panel);
		color: var(--fg);
		cursor: pointer;
		font: inherit;
	}
	.vl-node:hover {
		border-color: var(--accent);
	}
	.vl-node.cur {
		outline: 2px solid var(--accent);
		outline-offset: 1px;
	}
	.vl-node.run {
		border-color: color-mix(in srgb, var(--ok) 60%, transparent);
		background: color-mix(in srgb, var(--ok) 10%, var(--panel));
	}
	.vl-node.s-draft {
		border-style: dashed;
	}
	.vl-node.s-archived,
	.vl-node.s-discarded {
		opacity: 0.75;
	}
	.vl-top {
		display: flex;
		gap: 8px;
		align-items: baseline;
		justify-content: space-between;
	}
	.vl-top b {
		font-size: 13.5px;
	}
	.vl-origin {
		font-size: 11px;
		color: var(--muted);
	}
	.vl-status {
		font-size: 12px;
		font-weight: 600;
	}
	.vl-node.next {
		border-color: color-mix(in srgb, var(--accent) 60%, transparent);
	}
	.next .vl-status {
		color: var(--accent);
	}
	.run .vl-status {
		color: var(--ok);
	}
	.s-canary .vl-status {
		color: var(--warn);
	}
	.s-discarded .vl-status {
		color: var(--err);
	}
	.s-draft .vl-status,
	.s-archived .vl-status {
		color: var(--muted);
	}
	.vl-sub {
		font-size: 11px;
		color: var(--faint);
	}
	.vl-bad {
		color: var(--err);
		font-weight: 600;
	}
	@media (max-width: 600px) {
		.vl {
			display: grid;
			grid-template-columns: 1fr 1fr;
			gap: 6px;
		}
		.vl li + li::before {
			display: none;
		}
		.vl-node {
			width: 100%;
			min-width: 0;
		}
	}
</style>
