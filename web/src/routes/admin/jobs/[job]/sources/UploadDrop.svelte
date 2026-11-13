<script lang="ts">
	// The files of a file-source re-sync: drop them here or pick them; checked against the re-sync limits (20 files,
	// 50 MB in all) before anything is sent. Agent does not keep them: each re-sync uploads the full set again.
	import { checkUploads, fileSize, INGESTIBLE, MAX_UPLOAD_FILES, mergeFiles } from '$lib/tenant/ingest-files';

	let {
		files = $bindable([]),
		disabled = false,
		why = '',
	}: { files?: File[]; disabled?: boolean; why?: string } = $props();

	const check = $derived(checkUploads(files));
	let over = $state(false);
	let input = $state<HTMLInputElement>();
	const accept = INGESTIBLE.map((e) => `.${e}`).join(',');

	function add(list: FileList | File[] | null | undefined) {
		if (disabled || !list) return;
		files = mergeFiles(files, [...list]);
		if (input) input.value = '';
	}
	function drop(e: DragEvent) {
		e.preventDefault();
		over = false;
		add(e.dataTransfer?.files);
	}
</script>

<div
	class="drop"
	class:over
	class:disabled
	role="group"
	aria-label="Files to ingest"
	data-testid="upload-drop"
	title={why || undefined}
	ondragover={(e) => {
		e.preventDefault();
		if (!disabled) over = true;
	}}
	ondragleave={() => (over = false)}
	ondrop={drop}
>
	<label class="pick">
		<span><strong>Drop files here</strong> or <span class="link">choose files</span></span>
		<input
			bind:this={input}
			type="file"
			multiple
			{accept}
			{disabled}
			aria-label="Choose files to ingest"
			onchange={(e) => add(e.currentTarget.files)}
		/>
	</label>
	<span class="cell-sub"
		>Up to {MAX_UPLOAD_FILES} files, 50 MB in all · PDF, DOCX, PPTX, ODT, RTF, HTML, TXT, MD · the whole set replaces
		the previous run's documents</span
	>
	{#if files.length}
		<ul class="files" aria-label="Chosen files">
			{#each files as f (f.name)}
				<li class:bad={check.unsupported.includes(f.name)}>
					<span class="mono name">{f.name}</span>
					<span class="cell-sub">{fileSize(f.size)}</span>
					<button
						class="btn sm"
						type="button"
						aria-label={`Remove ${f.name}`}
						{disabled}
						onclick={() => (files = files.filter((x) => x !== f))}>Remove</button
					>
				</li>
			{/each}
		</ul>
		<span class="cell-sub" data-testid="upload-total"
			>{files.length} file{files.length === 1 ? '' : 's'} · {fileSize(check.total)} of 50 MB</span
		>
	{/if}
	{#each check.errors as e (e)}<span class="err-text" role="alert">{e}</span>{/each}
	{#if check.unsupported.length}
		<span class="warn-text"
			>strata_ingest does not read {check.unsupported.join(', ')}: {check.unsupported.length === 1
				? 'it is'
				: 'they are'} reported as refused and the run fails, keeping the earlier documents.</span
		>
	{/if}
</div>

<style>
	.drop {
		display: flex;
		flex-direction: column;
		gap: 6px;
		padding: 12px;
		border: 1.5px dashed var(--line);
		border-radius: 8px;
		background: var(--panel);
		min-width: 0;
	}
	.drop.over {
		border-color: var(--accent);
		background: color-mix(in srgb, var(--accent) 7%, var(--panel));
	}
	.drop.disabled {
		opacity: 0.65;
	}
	.pick {
		position: relative;
		cursor: pointer;
	}
	.pick input {
		position: absolute;
		inset: 0;
		opacity: 0;
		cursor: pointer;
		width: 100%;
	}
	.link {
		color: var(--accent);
		text-decoration: underline;
	}
	.files {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 4px;
	}
	.files li {
		display: flex;
		gap: 8px;
		align-items: center;
		min-width: 0;
	}
	.files li.bad .name {
		color: var(--warn);
	}
	.name {
		flex: 1;
		min-width: 0;
		overflow-wrap: anywhere;
		font-size: 12.5px;
	}
	.err-text {
		color: var(--err);
		font-size: 12.5px;
	}
	.warn-text {
		color: var(--warn);
		font-size: 12.5px;
	}
</style>
