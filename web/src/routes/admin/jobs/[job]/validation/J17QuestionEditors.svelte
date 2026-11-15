<script lang="ts">
	// J17 question editors (F-50, B8 T12a): "Edit" a question and "Import CSV" of questions. Both are job_designer
	// writes on live test data; an edit applies to the open cuts (earlier cuts keep the old text), and an import is all
	// or nothing: one bad row, a file over 4 MiB or more than 500 questions rejects the whole file.
	import { api, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { writeAttrs } from '$lib/auth/roles';
	import type { ValidationRow } from '$lib/api/types/journey';
	import Drawer from '$lib/ui/Drawer.svelte';
	import { toast } from '$lib/ui/toaster.svelte';
	import { changedFields, draftOf, draftProblem, importProblem, type QuestionDraft } from './questions';

	let {
		editing = $bindable(null),
		importOpen = $bindable(false),
		onchange,
	}: {
		editing?: ValidationRow | null;
		importOpen?: boolean;
		onchange: () => void | Promise<void>;
	} = $props();

	// ---- edit ----
	let editOpen = $state(false);
	let from = $state<QuestionDraft | null>(null);
	let to = $state<QuestionDraft>({ question: '', expected: '', reference: '', activity: '' });
	let editId = $state('');
	$effect(() => {
		if (editing) {
			editId = editing.id;
			from = draftOf(editing);
			to = draftOf(editing);
			editOpen = true;
			editing = null;
		}
	});
	const problem = $derived(draftProblem(to));
	const patch = $derived(from ? changedFields(from, to) : {});
	async function save() {
		if (problem || !Object.keys(patch).length) return;
		const ok = await guard(
			() => api().updateValidationQuestion(editId, patch),
			'Question saved. Earlier cuts keep the old text.',
			'Not saved',
		);
		if (ok) {
			editOpen = false;
			await onchange();
		}
	}

	// ---- import ----
	let file = $state<File | null>(null);
	let refusal = $state('');
	let busy = $state(false);
	$effect(() => {
		if (importOpen) {
			file = null;
			refusal = '';
		}
	});
	function pick(e: Event) {
		file = (e.currentTarget as HTMLInputElement).files?.[0] ?? null;
		refusal = '';
	}
	async function doImport() {
		if (!file || busy) return;
		busy = true;
		refusal = '';
		try {
			const r = await api().importValidationQuestions(file);
			toast(`Imported ${r.imported} question${r.imported === 1 ? '' : 's'} from ${file.name}.`);
			importOpen = false;
			await onchange();
		} catch (e) {
			refusal = importProblem(e);
		} finally {
			busy = false;
		}
	}
</script>

<Drawer bind:open={editOpen} title="Edit validation question">
	<div class="stack">
		<div class="field">
			<label for="vq-e-q">Question</label><input id="vq-e-q" class="input" bind:value={to.question} />
		</div>
		<div class="field">
			<label for="vq-e-e">Expected answer</label><textarea
				id="vq-e-e"
				class="input"
				rows="3"
				bind:value={to.expected}></textarea>
		</div>
		<div class="field">
			<label for="vq-e-r">Reference citation</label><input
				id="vq-e-r"
				class="input"
				bind:value={to.reference}
			/>
		</div>
		<div class="field">
			<label for="vq-e-a">Activity</label><input id="vq-e-a" class="input mono" bind:value={to.activity} />
		</div>
		{#if problem}<p class="note err-text" role="alert" data-testid="edit-problem">{problem}</p>{/if}
		<p class="note">
			Applies to the current and approved cuts; earlier cuts keep the text they were graded on.
		</p>
		<div class="row">
			<button
				class="btn primary"
				type="button"
				{...writeAttrs(session, 'job_designer')}
				disabled={!!problem || !Object.keys(patch).length || writeAttrs(session, 'job_designer').disabled}
				onclick={save}>Save question</button
			>
			<button class="btn" type="button" onclick={() => (editOpen = false)}>Cancel</button>
		</div>
	</div>
</Drawer>

<Drawer bind:open={importOpen} title="Import questions from CSV">
	<div class="stack">
		<p class="note">
			A CSV with the columns <span class="mono">question</span>, <span class="mono">expected</span>,
			<span class="mono">reference</span> and optionally <span class="mono">activity</span> (up to 4 MiB). It is
			all or nothing: if any row is not valid, or the job would hold more than 500 questions, the whole file is
			rejected.
		</p>
		<div class="field">
			<label for="vq-import">CSV file</label>
			<input id="vq-import" class="input" type="file" accept=".csv,text/csv" onchange={pick} />
		</div>
		{#if refusal}<p class="callout err" role="alert" data-testid="import-refusal">{refusal}</p>{/if}
		<div class="row">
			<button
				class="btn primary"
				type="button"
				{...writeAttrs(session, 'job_designer')}
				disabled={!file || busy || writeAttrs(session, 'job_designer').disabled}
				onclick={doImport}>{busy ? 'Importing…' : 'Import questions'}</button
			>
			<button class="btn" type="button" onclick={() => (importOpen = false)}>Cancel</button>
		</div>
	</div>
</Drawer>

<style>
	.err-text {
		color: var(--err);
	}
</style>
