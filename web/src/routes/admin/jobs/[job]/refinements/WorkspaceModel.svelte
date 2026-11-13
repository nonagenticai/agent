<script lang="ts">
	// J13 hero: what this job is made of (template base → your refinements → your draft) and the one path every
	// change takes to customers (Draft → Test → Publish, D54).
	import type { PathStep } from '$lib/release/J13-workspace';

	let {
		layers,
		liveRelease,
		noRelease = false,
		refinements,
		drafts,
		draftRev,
		steps,
	}: {
		/** Template layers in the live release, bottom-up; null while loading. */
		layers: { tag: string; scope: string }[] | null;
		liveRelease: string | null;
		/** The job is loaded and has no current release (e.g. still being generated). */
		noRelease?: boolean;
		/** Tenant refinement rows (incl. settings overrides); null while loading. */
		refinements: number | null;
		drafts: number | null;
		draftRev: number | null;
		steps: PathStep[];
	} = $props();

	const STATUS: Record<PathStep['status'], string> = {
		done: 'Up to date',
		current: 'You are here',
		todo: 'Next',
	};
</script>

<section class="ws" aria-label="How this job is built" data-testid="j13-model">
	<div class="ws-stack">
		<h2 class="ws-h">What your agent runs</h2>
		<p class="muted ws-lead">Read from the top: a layer overrides the ones below it.</p>
		<ol class="stack" reversed>
			<li class="lay draft" class:empty={!drafts}>
				<div class="lay-head">
					<b>Your draft</b>
					<span class="chip {drafts ? 'accent' : 'muted'}"
						>{drafts === null ? '…' : drafts ? `${drafts} unpublished` : 'empty'}</span
					>
				</div>
				<p class="muted">
					Every save lands here first{draftRev !== null ? ` · draft rev ${draftRev}` : ''}. Only the test chat
					sees it.
				</p>
			</li>
			<li class="lay mine">
				<div class="lay-head">
					<b>Your refinements</b>
					<span class="chip ok">{refinements === null ? '…' : `${refinements} rows`}</span>
				</div>
				<p class="muted">Hide, relabel, extend and settings on top of the template — this tenant only.</p>
			</li>
			<li class="lay base">
				<div class="lay-head">
					<b>Template base</b>
					<span class="chip muted">read-only</span>
				</div>
				<p class="muted">
					{#if liveRelease}From release <span class="mono">{liveRelease}</span> — changes only when you publish
						a newer template.{:else if noRelease}No release published yet: the first publish sets the base
						this job runs on.{:else}Loading the live release…{/if}
				</p>
				{#if layers?.length}
					<ul class="pins" aria-label="Template layers">
						{#each [...layers].reverse() as l (l.tag)}
							<li><span class="pin-scope">{l.scope}</span><span class="mono">{l.tag}</span></li>
						{/each}
					</ul>
				{/if}
			</li>
		</ol>
	</div>

	<div class="ws-path">
		<h2 class="ws-h">How a change reaches customers</h2>
		<p class="muted ws-lead">Nothing you save here is live. Each change takes the same three steps.</p>
		<ol class="path" aria-label="Draft, test, publish">
			{#each steps as s, i (s.key)}
				<li
					class="step {s.status}"
					data-step={s.key}
					aria-current={s.status === 'current' ? 'step' : undefined}
				>
					<span class="num" aria-hidden="true">{i + 1}</span>
					<div>
						<div class="lay-head">
							<b>{s.label}</b>
							<span class="chip {s.status === 'current' ? 'accent' : s.status === 'done' ? 'ok' : 'muted'}"
								>{STATUS[s.status]}</span
							>
						</div>
						<p class="muted">{s.detail}</p>
						{#if s.links.length}
							<p class="step-links">
								{#each s.links as l, j (l.href)}
									<a href={l.href} class:btn={s.key === 'publish'} class:sm={s.key === 'publish'}>{l.label}</a
									>{#if j < s.links.length - 1}<span class="muted" aria-hidden="true"> · </span>{/if}
								{/each}
							</p>
						{/if}
					</div>
				</li>
			{/each}
		</ol>
	</div>
</section>

<style>
	.ws {
		display: grid;
		grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
		gap: 16px;
		margin: 0 0 16px;
	}
	@media (max-width: 860px) {
		.ws {
			grid-template-columns: minmax(0, 1fr);
		}
	}
	.ws > div {
		border: 1px solid var(--line);
		background: var(--panel);
		border-radius: var(--r);
		padding: 14px 16px;
		min-width: 0;
	}
	.ws-h {
		font-size: 15px;
		margin: 0;
	}
	.ws-lead {
		margin: 2px 0 12px;
		font-size: 13px;
	}
	.stack,
	.path {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 6px;
	}
	.lay {
		border: 1px solid var(--line);
		border-left: 4px solid var(--line);
		border-radius: var(--r);
		padding: 8px 12px;
		background: var(--sunk);
	}
	.lay p {
		margin: 3px 0 0;
		font-size: 13px;
	}
	.lay.draft {
		border-style: dashed;
		border-left-style: solid;
		border-left-color: var(--accent);
		background: var(--panel);
	}
	.lay.draft.empty {
		border-left-color: var(--line);
	}
	.lay.mine {
		border-left-color: var(--ok);
		background: var(--panel);
	}
	.lay.base {
		border-left-color: var(--muted);
	}
	.lay-head {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 8px;
	}
	.pins {
		list-style: none;
		margin: 8px 0 0;
		padding: 0;
		display: grid;
		gap: 3px;
		font-size: 12px;
	}
	.pins li {
		display: flex;
		gap: 8px;
		align-items: baseline;
		min-width: 0;
		overflow-wrap: anywhere;
	}
	.pin-scope {
		min-width: 92px;
		color: var(--muted);
		text-transform: uppercase;
		letter-spacing: 0.04em;
		font-size: 11px;
	}
	.step {
		display: grid;
		grid-template-columns: 28px minmax(0, 1fr);
		gap: 10px;
		padding: 8px 0;
		border-bottom: 1px solid var(--line-2);
	}
	.step:last-child {
		border-bottom: 0;
	}
	.step p {
		margin: 3px 0 0;
		font-size: 13px;
	}
	.num {
		display: inline-grid;
		place-items: center;
		width: 26px;
		height: 26px;
		border-radius: 50%;
		border: 1.5px solid var(--line);
		font: 600 12px var(--mono);
		color: var(--muted);
	}
	.current .num {
		border-color: var(--accent);
		color: var(--accent);
	}
	.done .num {
		border-color: var(--ok);
		color: var(--ok);
	}
	.step-links {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 4px;
		margin-top: 6px !important;
	}
</style>
