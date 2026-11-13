<script lang="ts">
	// Golden questions from a CSV (strata_rag's columns: `query` plus at least one of expected_document,
	// expected_document_ids, expected_chunk_ids, expected_text; optional expected_leaf; ids separated by `;`). Checked
	// line by line before anything is sent. strata_rag imports all or nothing (422 `invalid_input` for any bad row,
	// 409 `question_exists`, 422 `too_many_questions` past 200), so any problem blocks the import until it is fixed.
	import { api, guard } from '$lib/app.svelte';
	import { EVAL_MAX_QUESTIONS, parseGoldenCsv, type CsvImport } from '$lib/tenant/retrieval';

	let { existing, disabled, onimported }: { existing: string[]; disabled: boolean; onimported?: () => void } =
		$props();

	let open = $state(false);
	let text = $state('');
	let fileName = $state('');
	let busy = $state(false);
	const parsed = $derived<CsvImport | null>(text.trim() ? parseGoldenCsv(text, { existing }) : null);
	const blocked = $derived(!!parsed?.errors.length);

	async function pick(e: Event) {
		const f = (e.currentTarget as HTMLInputElement).files?.[0];
		if (!f) return;
		fileName = f.name;
		text = await f.text();
	}
	async function run() {
		if (!parsed || blocked || !parsed.rows.length) return;
		busy = true;
		const rows = parsed.rows;
		const ok = await guard(
			() => api().importGoldenQuestions(rows),
			`Imported ${rows.length} golden question${rows.length === 1 ? '' : 's'}.`,
			'Not imported',
		);
		busy = false;
		if (ok) {
			text = '';
			fileName = '';
			onimported?.();
		}
	}
	const SAMPLE =
		'query,expected_document,expected_leaf,expected_text\n"How fast do I get a refund?",Refund timing,policy.payment,\n"Does the Oslo sofa come in green?",,,moss green';
</script>

<details class="ci" bind:open data-testid="ev-import">
	<summary>Import CSV</summary>
	<p class="muted ci-small">
		Header with <span class="mono">query</span> and at least one of
		<span class="mono">expected_document, expected_text, expected_document_ids, expected_chunk_ids</span>;
		<span class="mono">expected_leaf</span> optional; ids separated by <span class="mono">;</span>. All lines
		import or none. The set holds at most {EVAL_MAX_QUESTIONS} questions; {Math.max(
			0,
			EVAL_MAX_QUESTIONS - existing.length,
		)} more fit.
	</p>
	<div class="row">
		<label class="btn sm"
			>Choose a CSV file<input
				class="ci-input"
				type="file"
				accept=".csv,text/csv"
				{disabled}
				aria-label="Choose a CSV file"
				onchange={pick}
			/></label
		>
		{#if fileName}<span class="mono muted ci-small">{fileName}</span>{/if}
		<button class="btn sm" type="button" {disabled} onclick={() => (text = SAMPLE)}>Paste an example</button>
	</div>
	<label class="field ci-text"
		><span class="label">Or paste it</span>
		<textarea class="input mono" rows="5" bind:value={text} {disabled} aria-label="CSV text"
		></textarea></label
	>
	{#if parsed}
		<p class="ci-small" data-testid="ev-import-summary">
			<b>{parsed.rows.length} question{parsed.rows.length === 1 ? '' : 's'} ready</b>{parsed.errors.length
				? ` · fix ${parsed.errors.length} problem${parsed.errors.length === 1 ? '' : 's'} first: nothing is imported while any line is bad`
				: ''}
		</p>
		{#if parsed.errors.length}
			<ul class="ci-errors" data-testid="ev-import-errors">
				{#each parsed.errors.slice(0, 20) as e (e.line + e.message)}
					<li>{e.line > 0 ? `Line ${e.line}: ` : ''}{e.message}</li>
				{/each}
				{#if parsed.errors.length > 20}<li>… and {parsed.errors.length - 20} more</li>{/if}
			</ul>
		{/if}
		<button
			class="btn sm primary"
			type="button"
			disabled={disabled || busy || blocked || !parsed.rows.length}
			onclick={run}>{busy ? 'Importing…' : `Import ${parsed.rows.length}`}</button
		>
	{/if}
</details>

<style>
	.ci {
		margin-top: 12px;
		border-top: 1px solid var(--line-2);
		padding-top: 10px;
	}
	.ci summary {
		cursor: pointer;
		font-weight: 600;
	}
	.ci-small {
		font-size: 12px;
	}
	.ci-text {
		margin: 8px 0;
	}
	.ci-text textarea {
		width: 100%;
		box-sizing: border-box;
	}
	.ci-input {
		position: absolute;
		width: 1px;
		height: 1px;
		opacity: 0;
		overflow: hidden;
	}
	label.btn {
		position: relative;
	}
	.ci-errors {
		color: var(--err);
		font-size: 12.5px;
		margin: 4px 0 8px;
		padding-left: 18px;
	}
</style>
