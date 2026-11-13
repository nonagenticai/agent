<script lang="ts">
	// J2 Knowledge panel — "does the agent answer from your knowledge, and where is the knowledge missing?". Reads
	// knowledgeAnalytics(job, days): grounded-answer rate, uncited and "I don't know" rates, staff feedback with its
	// top reasons, the questions knowledge did not answer (each opens the stores searching for it) and the most- and
	// never-cited documents. Same look as the rest of J2: Kpi tiles, thin share bars, lists with a fix link.
	import { api } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import { fmt } from '$lib/format';
	import { when } from '$lib/runtime/labels';
	import { FEEDBACK_REASONS, type KnowledgeAnalytics } from '$lib/api/types/grounding';
	import { storePath, storesHref } from '$lib/grounding/grounding';
	import Card from '$lib/ui/Card.svelte';
	import Kpi from '$lib/ui/Kpi.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';

	let { job, days }: { job: string; days: number } = $props();

	const data = new Resource<KnowledgeAnalytics>(
		() => api().knowledgeAnalytics(job, days),
		(k) => k.knowledge_answers === 0 && k.feedback.up + k.feedback.down === 0,
	);
	$effect(() => {
		void job;
		void days;
		void data.refresh();
	});
	const reasonLabel = (r: string) => FEEDBACK_REASONS.find((x) => x.key === r)?.label ?? r;
	const tone = (rate: number | null) =>
		rate == null ? 'muted' : rate >= 0.85 ? 'ok' : rate >= 0.7 ? 'warn' : 'err';
</script>

<Card
	title="Does it answer from your knowledge?"
	sub="Answers that searched the knowledge stores, last {days} days. Grounded = the answer cites at least one source; only grounded FAQ answers reach customers unchanged."
>
	{#snippet actions()}
		<a class="btn sm" href="/admin/knowledge/stores">Knowledge stores</a>
		<a class="btn sm" href="/admin/knowledge/eval">Golden questions</a>
	{/snippet}
	<LoadState resource={data}>
		{#snippet empty()}
			This job has not answered from knowledge yet and nobody has rated an answer. Bind a knowledge store and
			ask a policy question in the <a href="/chat">test chat</a>.
		{/snippet}
		{#snippet children(k)}
			{@const fbTotal = k.feedback.up + k.feedback.down}
			{@const downs = k.feedback.down}
			<div class="kpis" data-testid="kn-kpis">
				<Kpi
					label="Grounded answers"
					value={fmt.pct(k.grounded_rate, 0)}
					note="{fmt.n(k.grounded)} of {fmt.n(k.knowledge_answers)} knowledge answers"
				/>
				<Kpi
					label="Answered without a source"
					value={fmt.pct(k.uncited_rate, 0)}
					note="{fmt.n(k.not_grounded)} not grounded"
				/>
				<Kpi
					label="“I don't know”"
					value={fmt.pct(k.dont_know_rate, 0)}
					note="{fmt.n(k.dont_know)} answers"
				/>
				<Kpi
					label="Staff feedback"
					value={fbTotal ? `${fmt.n(k.feedback.up)} up · ${fmt.n(downs)} down` : '—'}
					note={fbTotal ? `${fmt.pct(k.feedback.up / fbTotal, 0)} positive` : 'no ratings yet'}
				/>
			</div>

			{#if k.knowledge_answers}
				<div class="kn-share" data-testid="kn-share">
					<span
						class="kn-bar"
						role="img"
						aria-label="Grounded {fmt.pct(k.grounded_rate, 0)}, not grounded {fmt.pct(
							k.uncited_rate,
							0,
						)}, I don't know {fmt.pct(k.dont_know_rate, 0)}"
					>
						<i class="g" style:width="{(k.grounded_rate ?? 0) * 100}%"></i><i
							class="n"
							style:width="{(k.uncited_rate ?? 0) * 100}%"
						></i><i class="d" style:width="{(k.dont_know_rate ?? 0) * 100}%"></i>
					</span>
					<span class="kn-legend">
						<span><i class="g"></i>grounded {fmt.n(k.grounded)}</span>
						<span><i class="n"></i>not grounded {fmt.n(k.not_grounded)}</span>
						<span><i class="d"></i>I don't know {fmt.n(k.dont_know)}</span>
						<span class="s-{tone(k.grounded_rate)} kn-verdict"
							>{k.grounded_rate == null
								? ''
								: k.grounded_rate >= 0.85
									? 'Mostly grounded'
									: 'Too many answers without a source'}</span
						>
					</span>
				</div>
			{/if}

			<div class="grid g2 kn-grid">
				<section aria-labelledby="kn-unanswered">
					<h3 id="kn-unanswered">Questions knowledge did not answer</h3>
					{#if k.unanswered.length}
						<ul class="list-plain kn-list" data-testid="kn-unanswered">
							{#each k.unanswered as q (q.query)}
								<li>
									<span class="kn-main"
										><a href={storesHref(q.query)}>{q.query}</a><span class="cell-sub"
											>“{q.example}” · last {when(q.last_at)}</span
										></span
									>
									<span class="mono">{fmt.n(q.count)}×</span>
								</li>
							{/each}
						</ul>
						<p class="muted kn-foot">Open one to search the stores for it, then add the missing document.</p>
					{:else}
						<div class="empty">Every knowledge answer in this window cites a source.</div>
					{/if}
				</section>
				<section aria-labelledby="kn-reasons">
					<h3 id="kn-reasons">Why staff down-voted answers</h3>
					{#if k.feedback.reasons.length}
						<ul class="kn-reasons" data-testid="kn-reasons">
							{#each k.feedback.reasons as r (r.reason)}
								<li>
									<span>{reasonLabel(r.reason)}</span>
									<span class="mono">{fmt.n(r.count)}</span>
									<span class="kn-rbar" aria-hidden="true"
										><span style:width="{downs ? (r.count / downs) * 100 : 0}%"></span></span
									>
								</li>
							{/each}
						</ul>
						<p class="muted kn-foot">
							{fmt.n(downs - k.feedback.reasons.reduce((n, r) => n + r.count, 0))} down votes gave no reason. Promote
							a wrong answer to a golden question from the case or the test chat.
						</p>
					{:else}
						<div class="empty">{downs ? 'Down votes gave no reason.' : 'No down votes in this window.'}</div>
					{/if}
				</section>
				<section aria-labelledby="kn-cited">
					<h3 id="kn-cited">Most-cited documents</h3>
					{#if k.most_cited.length}
						{@const top = k.most_cited[0]!.citations}
						<ul class="kn-reasons" data-testid="kn-most-cited">
							{#each k.most_cited as d (d.document_id)}
								<li title={d.document_id}>
									<span class="kn-main"
										>{d.title}<span class="cell-sub mono">{storePath(d.store_path)}</span></span
									>
									<span class="mono">{fmt.n(d.citations)}</span>
									<span class="kn-rbar" aria-hidden="true"
										><span class="g" style:width="{(d.citations / top) * 100}%"></span></span
									>
								</li>
							{/each}
						</ul>
					{:else}
						<div class="empty">No answer cited a document in this window.</div>
					{/if}
				</section>
				<section aria-labelledby="kn-never">
					<h3 id="kn-never">
						Never cited <span class="muted"
							>· {fmt.n(k.never_cited_total)} of {fmt.n(k.documents_total)} documents</span
						>
					</h3>
					{#if k.never_cited.length}
						<ul class="list-plain kn-list" data-testid="kn-never-cited">
							{#each k.never_cited as d (d.document_id)}
								<li title={d.document_id}>
									<span class="kn-main">{d.title}</span>
									<span class="cell-sub mono">{storePath(d.store_path)}</span>
								</li>
							{/each}
						</ul>
						<p class="muted kn-foot">
							Curated policy documents first. Never cited can mean nobody asks — or the agent cannot find it.
						</p>
					{:else}
						<div class="empty">Every document was cited at least once.</div>
					{/if}
				</section>
			</div>
		{/snippet}
	</LoadState>
</Card>

<style>
	.kn-share {
		display: grid;
		gap: 6px;
		margin: 14px 0 4px;
	}
	.kn-bar {
		display: flex;
		height: 10px;
		border-radius: 999px;
		overflow: hidden;
		background: var(--sunk);
	}
	.kn-bar i {
		display: block;
		height: 100%;
	}
	.g {
		background: var(--ok);
	}
	.n {
		background: var(--err);
	}
	.d {
		background: var(--warn);
	}
	.kn-legend {
		display: flex;
		flex-wrap: wrap;
		gap: 4px 14px;
		font-size: 12px;
		color: var(--muted);
	}
	.kn-legend i {
		display: inline-block;
		width: 9px;
		height: 9px;
		border-radius: 2px;
		margin-right: 5px;
		vertical-align: -1px;
	}
	.kn-verdict {
		font-weight: 600;
	}
	.kn-grid {
		margin-top: 16px;
	}
	.kn-grid section {
		min-width: 0;
	}
	.kn-grid h3 {
		font-size: 13px;
		margin: 0 0 6px;
	}
	.kn-main {
		min-width: 0;
		overflow-wrap: anywhere;
	}
	.kn-list li {
		padding: 7px 0;
	}
	.kn-reasons {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 8px;
	}
	.kn-reasons li {
		display: grid;
		grid-template-columns: minmax(0, 1fr) auto;
		gap: 2px 10px;
		font-size: 13px;
	}
	.kn-rbar {
		grid-column: 1 / -1;
		height: 6px;
		border-radius: 999px;
		background: var(--sunk);
		overflow: hidden;
	}
	.kn-rbar span {
		display: block;
		height: 100%;
		border-radius: 999px;
		background: var(--err);
	}
	.kn-rbar span.g {
		background: var(--accent);
	}
	.kn-foot {
		font-size: 12px;
		margin: 8px 0 0;
	}
</style>
