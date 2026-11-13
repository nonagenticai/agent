<script lang="ts">
	// F-53 evidence: a ticket export (multipart upload) checked against this run's journey. Clusters the journey does
	// not cover come back as rule 7 information on Validate. Platform runs only; 413 / 422 / 409 refusals are shown
	// next to the file, never swallowed.
	import { api, guard } from '$lib/app.svelte';
	import type { GeneratorRun } from '$lib/api/types/journey';
	import Card from '$lib/ui/Card.svelte';
	import { countRows, uploadRefusal } from '$lib/journey/B8Generator';

	let { run: r, onuploaded }: { run: GeneratorRun; onuploaded: () => void | Promise<void> } = $props();

	let busy = $state(false);
	let error = $state('');
	let last = $state('');
	let input = $state<HTMLInputElement | null>(null);

	async function pick(e: Event) {
		const f = (e.currentTarget as HTMLInputElement).files?.[0];
		if (!f) return;
		busy = true;
		error = '';
		let rows = 0;
		try {
			rows = countRows(await f.text(), f.name);
		} catch {
			rows = 0;
		}
		const ok = await guard(
			async () => {
				try {
					await api().uploadGeneratorEvidence(r.id, { file: f, rows });
				} catch (err) {
					error = uploadRefusal(err);
					throw err;
				}
			},
			`${f.name} checked against the journey.`,
			'Not uploaded',
		);
		busy = false;
		if (input) input.value = '';
		if (ok) {
			last = f.name;
			await onuploaded();
		}
	}
</script>

<Card
	title="Evidence"
	sub="A ticket export checked against this run's journey; clusters it does not cover show up as rule 7 information."
>
	<div class="field">
		<label for="p18-evidence">Ticket export (CSV or JSON lines)</label>
		<input
			id="p18-evidence"
			class="input"
			type="file"
			accept=".csv,.tsv,.json,.jsonl,text/csv,application/json"
			disabled={busy}
			bind:this={input}
			onchange={pick}
		/>
	</div>
	{#if busy}<p class="muted" role="status"><span class="spinner" aria-hidden="true"></span> Checking…</p>{/if}
	{#if error}<p class="p18-ev-err" role="alert" data-testid="evidence-error">{error}</p>{/if}
	{#if last && !error}<p class="muted" data-testid="evidence-last">
			Last checked: <span class="mono">{last}</span>
		</p>{/if}
</Card>

<style>
	.p18-ev-err {
		color: var(--err);
		font-size: 13px;
		margin: 6px 0 0;
	}
</style>
