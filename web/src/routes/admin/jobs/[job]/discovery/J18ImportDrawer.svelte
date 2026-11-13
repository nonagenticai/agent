<script lang="ts">
	// J18 "Upload export" (F-52, B8 T15a): the file is previewed first (its columns and five scrubbed sample rows; nothing
	// is stored and no model is called), then the person picks which column holds the ticket text and which one a
	// count, and only then is it uploaded and classified. CSV, JSON and XLSX (first sheet).
	import { api, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { writeAttrs } from '$lib/auth/roles';
	import type { DiscoveryPreview } from '$lib/api/types/journey';
	import { fmt } from '$lib/format';
	import Drawer from '$lib/ui/Drawer.svelte';
	import { IMPORT_ACCEPT, importFileProblem, initialColumns, uploadInput } from './import';

	let { open = $bindable(false), onuploaded }: { open?: boolean; onuploaded: () => void | Promise<void> } =
		$props();

	let file = $state<File | null>(null);
	let preview = $state<DiscoveryPreview | null>(null);
	let problem = $state('');
	let busy = $state(false);
	let text = $state('');
	let count = $state('');
	$effect(() => {
		if (open) {
			file = null;
			preview = null;
			problem = '';
		}
	});

	async function pick(e: Event) {
		const f = (e.currentTarget as HTMLInputElement).files?.[0] ?? null;
		file = f;
		preview = null;
		problem = '';
		if (!f) return;
		busy = true;
		try {
			const p = await api().previewDiscoveryImport(f);
			preview = p;
			({ text, count } = initialColumns(p));
		} catch (err) {
			problem = importFileProblem(err);
		} finally {
			busy = false;
		}
	}
	async function upload() {
		if (!file || !preview || busy) return;
		busy = true;
		const f = file;
		const input = uploadInput(f, preview, { text, count });
		const ok = await guard(
			() => api().uploadDiscoveryImport(input),
			`${f.name} uploaded — classifying ${fmt.n(preview.rows)} rows.`,
			'Not uploaded',
		);
		busy = false;
		if (ok) {
			open = false;
			await onuploaded();
		}
	}
</script>

<Drawer bind:open title="Upload a ticket export" wide>
	<div class="stack">
		<p class="note">
			A CSV, JSON or XLSX export, one ticket per row. You see its columns first; nothing is stored until you
			upload.
		</p>
		<div class="field">
			<label for="disc-file">Export file</label>
			<input id="disc-file" class="input" type="file" accept={IMPORT_ACCEPT} onchange={pick} />
		</div>
		{#if busy && !preview}<p class="muted" role="status">Reading the file…</p>{/if}
		{#if problem}<p class="callout err" role="alert" data-testid="import-problem">{problem}</p>{/if}
		{#if preview}
			<div data-testid="import-preview">
				<p class="small muted">
					<b>{preview.format.toUpperCase()}</b> · {fmt.n(preview.rows)} rows · {preview.columns.length} columns.
					Cells below are scrubbed excerpts.
				</p>
				<div class="tbl-wrap">
					<table class="tbl dense">
						<thead
							><tr
								>{#each preview.columns as c (c)}<th>{c}</th>{/each}</tr
							></thead
						>
						<tbody>
							{#each preview.sample_rows as r, i (i)}
								<tr
									>{#each r as cell, j (j)}<td>{cell}</td>{/each}</tr
								>
							{/each}
						</tbody>
					</table>
				</div>
				<div class="row">
					<div class="field">
						<label for="disc-text">Text column</label>
						<select id="disc-text" class="input" bind:value={text}>
							{#each preview.columns as c (c)}<option value={c}>{c}</option>{/each}
						</select>
					</div>
					<div class="field">
						<label for="disc-count">Count column (optional)</label>
						<select id="disc-count" class="input" bind:value={count}>
							<option value="">Each row is one ticket</option>
							{#each preview.columns as c (c)}<option value={c}>{c}</option>{/each}
						</select>
					</div>
				</div>
			</div>
		{/if}
		<div class="row">
			<button
				class="btn primary"
				type="button"
				{...writeAttrs(session, 'job_designer')}
				disabled={!preview || !text || busy || writeAttrs(session, 'job_designer').disabled}
				onclick={upload}>{busy && preview ? 'Uploading…' : 'Upload and classify'}</button
			>
			<button class="btn" type="button" onclick={() => (open = false)}>Cancel</button>
		</div>
	</div>
</Drawer>
