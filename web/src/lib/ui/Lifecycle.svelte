<script lang="ts">
	// The agent's lifecycle on one line: Build, Test, Release, Live. Each stage says where the agent stands and
	// links to the page where that stage is worked on. The Live stage is the one StatusChip.
	import type { AgentStatus } from '$lib/api/types/status';
	import type { JobKey } from '$lib/api/types/core';
	import { jobPath } from '$lib/scope';
	import { when } from '$lib/runtime/labels';
	import { L, releaseLabel } from './labels';
	import Chip from './Chip.svelte';
	import StatusChip from './StatusChip.svelte';

	let { status, job }: { status: AgentStatus; job: JobKey } = $props();

	const next = $derived(status.next_release);
	const tests = $derived(status.tests);
	// What the Test stage says, in plain words: [headline, detail]
	const testWords = $derived.by((): [string, string] => {
		const t = tests;
		const of = `${t.passed} of ${t.questions} passing`;
		switch (t.state) {
			case 'running':
				return ['Running', 'checking the agent now'];
			case 'passing':
				return [of, t.last_run_at ? `last run ${when(t.last_run_at)}` : 'all passing'];
			case 'failing':
				return [
					t.failed > 0 ? `${plural(t.failed, 'test')} failing` : 'Last run failed',
					t.questions > 0 ? of : 'run it again',
				];
			case 'not_run':
				return ['Not run', 'run it before each release'];
			default:
				return ['No tests yet', 'add questions to check the agent'];
		}
	});
	const testVariant = $derived(
		tests.state === 'passing' ? 'ok' : tests.state === 'failing' ? 'err' : 'muted',
	);
	const plural = (n: number, w: string) => `${n} ${w}${n === 1 ? '' : 's'}`;
</script>

<ol class="life" aria-label="Agent lifecycle" data-testid="lifecycle">
	<li>
		<a class="stage" href={jobPath(job, '/release')} data-stage="build">
			<span class="k">Build</span>
			<b>{next && next.changes > 0 ? plural(next.changes, 'change') : 'No changes'}</b>
			<span class="s"
				>{next && next.changes > 0 ? 'not in a release yet' : 'everything is in the live release'}</span
			>
		</a>
	</li>
	<li>
		<a class="stage" href={jobPath(job, '/validation')} data-stage="test">
			<span class="k">Test</span>
			<b><Chip text={testWords[0]} variant={testVariant} /></b>
			<span class="s">{L.validation_grid}: {testWords[1]}</span>
		</a>
	</li>
	<li>
		<a class="stage" href={jobPath(job, '/readiness')} data-stage="release">
			<span class="k">Release</span>
			<b>
				{#if next && next.blockers > 0}
					<Chip text={plural(next.blockers, 'blocker')} variant="err" />
				{:else if next}
					<Chip text="Ready" variant="ok" />
				{:else}
					<Chip text="Up to date" variant="muted" />
				{/if}
			</b>
			<span class="s"
				>{next && next.blockers > 0
					? "next release can't go live yet"
					: next
						? 'next release can go live'
						: 'nothing to release'}</span
			>
		</a>
	</li>
	<li>
		<a class="stage cur" href={jobPath(job)} data-stage="live" aria-current="step">
			<span class="k">Live</span>
			<b><StatusChip {status} /></b>
			<span class="s"
				>{status.live_release
					? `${releaseLabel(status.live_release.id)}, live since ${when(status.live_release.since)}`
					: L.not_live_for_customers}</span
			>
		</a>
	</li>
</ol>

<style>
	.life {
		list-style: none;
		margin: 0 0 16px;
		padding: 0;
		display: grid;
		grid-template-columns: repeat(4, minmax(0, 1fr));
		gap: 8px;
	}
	.life > li {
		min-width: 0;
	}
	.stage {
		display: grid;
		gap: 4px;
		min-width: 0;
		overflow-wrap: anywhere;
		height: 100%;
		padding: 12px 14px;
		border: 1px solid var(--line);
		border-radius: var(--r);
		background: var(--panel);
		color: var(--fg);
		text-decoration: none;
	}
	.stage :global(.chip) {
		max-width: 100%;
		white-space: normal;
		overflow-wrap: anywhere;
	}
	.stage:hover {
		border-color: var(--accent);
	}
	.stage.cur {
		border-color: var(--accent);
	}
	.k {
		font: 600 11px/1 var(--cond);
		letter-spacing: 0.1em;
		text-transform: uppercase;
		color: var(--muted);
	}
	.s {
		font-size: 12.5px;
		color: var(--muted);
	}
	@media (max-width: 860px) {
		.life {
			grid-template-columns: repeat(2, minmax(0, 1fr));
		}
	}
	@media (max-width: 520px) {
		.life {
			grid-template-columns: minmax(0, 1fr);
		}
	}
</style>
