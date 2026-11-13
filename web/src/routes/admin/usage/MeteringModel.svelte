<script lang="ts">
	// T5: the metering model at a glance — one turn → one qualifier LLM call → one usage_ledger row → a funding lane.
	// Generator runs share the ledger and lanes (D56); knowledge and ingest are metered elsewhere — the side note says so.
	const STEPS: { n: string; title: string; text: string }[] = [
		{ n: '1', title: 'A turn', text: 'A customer message (Telegram) or a staff test chat.' },
		{
			n: '2',
			title: 'Qualifier call',
			text: 'One LLM call reads the turn and picks the activity (e.g. Start a return).',
		},
		{
			n: '3',
			title: 'Ledger row',
			text: 'usage_ledger counts turns + tokens per job · person/channel · project · provider·model.',
		},
		{
			n: '4',
			title: 'Funding lane',
			text: 'The provider decides who pays: platform GPU, platform external or your key.',
		},
	];
</script>

<section class="t5-model" aria-label="How usage is metered">
	<ol class="t5-flow">
		{#each STEPS as s, i (s.n)}
			<li class="t5-step">
				<span class="t5-n">{s.n}</span>
				<div>
					<strong>{s.title}</strong>
					<p>{s.text}</p>
				</div>
				{#if i < STEPS.length - 1}<span class="t5-arrow" aria-hidden="true">→</span>{/if}
			</li>
		{/each}
	</ol>
	<p class="t5-aside">
		Generator runs you start are LLM usage too: same ledger, same funding lanes, under the generator activity
		— not a separate bill. Counted outside the LLM ledger: knowledge searches (strata_rag) and document ingest
		(strata_ingest).
	</p>
</section>

<style>
	.t5-model {
		border: 1px solid var(--line);
		border-radius: var(--r);
		background: var(--panel);
		padding: 14px 16px;
		margin-bottom: 16px;
	}
	.t5-flow {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		grid-template-columns: repeat(4, minmax(0, 1fr));
		gap: 10px;
	}
	.t5-step {
		position: relative;
		display: flex;
		gap: 10px;
		align-items: flex-start;
		padding: 10px 12px;
		background: var(--sunk);
		border-radius: var(--r);
	}
	.t5-step strong {
		font-size: 13px;
	}
	.t5-step p {
		margin: 2px 0 0;
		font-size: 12px;
		color: var(--muted);
		line-height: 1.4;
	}
	.t5-n {
		flex: none;
		width: 22px;
		height: 22px;
		border-radius: 50%;
		display: grid;
		place-items: center;
		font: 600 12px/1 var(--mono);
		color: var(--accent);
		border: 1px solid var(--accent);
	}
	.t5-arrow {
		position: absolute;
		right: -10px;
		top: 50%;
		transform: translateY(-50%);
		color: var(--faint);
		font-size: 13px;
		z-index: 1;
	}
	.t5-aside {
		margin: 10px 0 0;
		font-size: 12px;
		color: var(--muted);
	}
	@media (max-width: 900px) {
		.t5-flow {
			grid-template-columns: repeat(2, minmax(0, 1fr));
		}
		.t5-arrow {
			display: none;
		}
	}
	@media (max-width: 520px) {
		.t5-flow {
			grid-template-columns: 1fr;
		}
	}
</style>
