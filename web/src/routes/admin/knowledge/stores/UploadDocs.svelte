<script lang="ts">
	// T7 "Add documents": drop or pick files for one leaf store. They go up as a one-off strata_ingest job without a
	// source (`source_kind: 'file'`), listed on Ingest jobs. Limits are checked here first and again by the route.
	import { api } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { canWrite, writeAttrs, writeHint } from '$lib/auth/roles';
	import { toast } from '$lib/ui/toaster.svelte';
	import type { KnowledgeStore } from '$lib/api/types/tenant';
	import type { IngestJob } from '$lib/api/types/tenant';
	import { errText } from '$lib/tenant/ui';
	import {
		UPLOAD_ACCEPT,
		UPLOAD_MAX_BYTES,
		UPLOAD_MAX_FILES,
		UPLOAD_TYPES,
		checkUpload,
		fmtBytes,
	} from '$lib/tenant/retrieval';
	import SupersedesInput from '$lib/pk/SupersedesInput.svelte';

	let { store, ondone }: { store: KnowledgeStore; ondone?: (job: IngestJob) => void } = $props();

	let files = $state<File[]>([]);
	let over = $state(false);
	let busy = $state(false);
	let refused = $state('');
	let done = $state<IngestJob | null>(null);
	let input = $state<HTMLInputElement | undefined>();
	/** PK-8: platform topic keys these documents replace in searches (sent with the upload; none = not sent). */
	let supersedes = $state<string[]>([]);

	const problems = $derived(files.length ? checkUpload(files) : []);
	const total = $derived(files.reduce((a, f) => a + f.size, 0));
	const bound = $derived(!!store.strata_store_id);
	const allowed = $derived(canWrite(session, 'knowledge_manager') && bound);

	function add(list: FileList | File[] | null | undefined) {
		if (!list) return;
		const next = [...files];
		for (const f of Array.from(list))
			if (!next.some((x) => x.name === f.name && x.size === f.size)) next.push(f);
		files = next;
		refused = '';
		done = null;
	}
	function drop(e: DragEvent) {
		e.preventDefault();
		over = false;
		if (allowed) add(e.dataTransfer?.files);
	}
	function remove(i: number) {
		files = files.filter((_, j) => j !== i);
	}
	async function upload() {
		if (problems.length || !files.length) return;
		busy = true;
		refused = '';
		try {
			const job = await api().uploadToStore(
				store.id,
				files,
				undefined,
				supersedes.length ? supersedes : undefined,
			);
			done = job;
			files = [];
			if (input) input.value = '';
			toast(`${job.source_label} queued for ${store.key}.`);
			ondone?.(job);
		} catch (e) {
			refused = errText(e);
		}
		busy = false;
	}
</script>

<details class="ud" data-testid="t7-upload">
	<summary>Upload files</summary>
	{#if !bound}
		<p class="muted ud-small">strata_rag has not created this store yet, so it cannot take files.</p>
	{/if}
	<div
		class="ud-drop"
		class:over
		class:off={!allowed}
		role="group"
		aria-label="Drop files here"
		ondragover={(e) => {
			e.preventDefault();
			over = allowed;
		}}
		ondragleave={() => (over = false)}
		ondrop={drop}
	>
		<span>Drop files here or</span>
		<label
			class="btn sm"
			title={canWrite(session, 'knowledge_manager') ? undefined : writeHint('knowledge_manager')}
			>Choose files<input
				bind:this={input}
				class="ud-input"
				type="file"
				multiple
				accept={UPLOAD_ACCEPT}
				disabled={!allowed}
				aria-label="Choose files"
				onchange={(e) => add((e.currentTarget as HTMLInputElement).files)}
			/></label
		>
		<span class="muted ud-small"
			>Up to {UPLOAD_MAX_FILES} files, {fmtBytes(UPLOAD_MAX_BYTES)} in total · {UPLOAD_TYPES.join(', ')}</span
		>
	</div>
	{#if files.length}
		<ul class="list-plain ud-files" data-testid="t7-upload-files">
			{#each files as f, i (f.name + f.size)}
				<li>
					<span class="mono">{f.name}</span>
					<span class="muted mono">{fmtBytes(f.size)}</span>
					<button class="btn sm" type="button" onclick={() => remove(i)} aria-label={`Remove ${f.name}`}
						>Remove</button
					>
				</li>
			{/each}
		</ul>
		<p class="muted ud-small">{files.length} file{files.length === 1 ? '' : 's'} · {fmtBytes(total)}</p>
	{/if}
	<SupersedesInput bind:value={supersedes} disabled={!allowed} id="upload-supersedes-{store.id}" />
	{#if problems.length}
		<ul class="ud-problems" data-testid="t7-upload-problems">
			{#each problems as p (p.code + (p.file ?? ''))}<li>{p.message}</li>{/each}
		</ul>
	{/if}
	{#if refused}<p class="ud-problems" role="alert">Not uploaded: {refused}</p>{/if}
	{#if done}
		<p class="ud-small">
			Queued as ingest job <a href={`/admin/knowledge/jobs?id=${encodeURIComponent(done.id)}`}>{done.id}</a>;
			documents appear here once it completes.
		</p>
	{/if}
	<div class="row">
		<button
			class="btn sm primary"
			type="button"
			{...writeAttrs(session, 'knowledge_manager')}
			disabled={!allowed || busy || !files.length || problems.length > 0}
			onclick={upload}>{busy ? 'Uploading…' : 'Upload'}</button
		>
		{#if files.length}<button class="btn sm" type="button" onclick={() => (files = [])}>Clear</button>{/if}
	</div>
</details>

<style>
	.ud {
		margin-top: 10px;
		border-top: 1px solid var(--line-2);
		padding-top: 10px;
	}
	.ud summary {
		cursor: pointer;
		font-weight: 600;
	}
	.ud-drop {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
		align-items: center;
		justify-content: center;
		text-align: center;
		margin: 10px 0;
		padding: 16px 12px;
		border: 1.5px dashed var(--line);
		border-radius: 8px;
		background: var(--bg);
	}
	.ud-drop.over {
		border-color: var(--accent);
		background: var(--sunk);
	}
	.ud-drop.off {
		opacity: 0.6;
	}
	.ud-input {
		position: absolute;
		width: 1px;
		height: 1px;
		opacity: 0;
		overflow: hidden;
	}
	.ud-drop label {
		position: relative;
	}
	.ud-small {
		font-size: 12px;
	}
	.ud-files li {
		display: flex;
		gap: 10px;
		align-items: center;
		justify-content: space-between;
		flex-wrap: wrap;
		overflow-wrap: anywhere;
	}
	.ud-problems {
		color: var(--err);
		font-size: 12.5px;
		margin: 6px 0;
		padding-left: 18px;
	}
</style>
