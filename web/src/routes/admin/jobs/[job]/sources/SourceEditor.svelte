<script lang="ts">
	// One source's settings: type, where it reads from, credentials (write-only), target store, schedule and the
	// field → ontology mapping. Test and Preview never save. Adapted from care-bus-agents SourceEditor.svelte@45ee128.
	import { onMount, tick } from 'svelte';
	import { api, guard } from '$lib/app.svelte';
	import { writeHint } from '$lib/auth/roles';
	import { ApiError } from '$lib/api/errors';
	import { LANGUAGE_LABEL } from '$lib/api/types/core';
	import { extOf, fileSize } from '$lib/tenant/ingest-files';
	import { toast } from '$lib/ui/toaster.svelte';
	import type {
		ColumnPreview,
		KnowledgeStore,
		MappingTarget,
		Source,
		SourceMapping,
		SourceSample,
	} from '$lib/api/types/tenant';
	import {
		draftDirty,
		draftErrors,
		draftOf,
		inputOf,
		leafStores,
		SOURCE_TYPES,
		structured,
		WEBSITE_MODES,
	} from '$lib/tenant/sources';
	import { when } from '$lib/tenant/ui';
	import Callout from '$lib/ui/Callout.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import JsonView from '$lib/ui/JsonView.svelte';
	import MappingEditor from './MappingEditor.svelte';
	import SupersedesInput from '$lib/pk/SupersedesInput.svelte';

	let {
		source,
		stores,
		targets,
		canWrite,
		onsaved,
		dirty = $bindable(false),
		editedMapping = $bindable(null),
	}: {
		source: Source | null;
		stores: KnowledgeStore[];
		targets: MappingTarget[];
		canWrite: boolean;
		onsaved: (s: Source | null) => void;
		dirty?: boolean;
		/** The form's mapping when it differs from the stored mapping (for "Re-sync with the edited mapping"). */
		editedMapping?: SourceMapping[] | null;
	} = $props();

	let draft = $state(draftOf(null));
	/** PK-8: platform topic keys this source's documents replace (always sent on save, so `[]` clears). */
	let supersedes = $state<string[]>([]);
	let sample = $state<SourceSample | null>(null);
	let preview = $state<ColumnPreview | null>(null);
	let saving = $state(false);
	let testing = $state(false);
	let failure = $state<{ code: string; message: string } | null>(null);
	let confirmDelete = $state(false);
	let purge = $state(false);
	/** The local CSV/XLSX the columns were last previewed from (never stored). */
	let previewFile = $state<File | null>(null);
	let previewing = $state(false);
	/** D68: the languages the job serves (draft, D59); a source's documents are in one of them. */
	let languages = $state<string[]>([]);
	onMount(async () => {
		try {
			languages = (await api().jobLanguages()).draft;
		} catch {
			languages = [];
		}
	});
	const shownLanguage = $derived(draft.language || languages[0] || '');
	const languageOptions = $derived(
		draft.language && !languages.includes(draft.language) ? [...languages, draft.language] : languages,
	);
	$effect(() => {
		draft = draftOf(source);
		supersedes = [...(source?.supersedes ?? [])];
		sample = null;
		preview = null;
		failure = null;
		confirmDelete = false;
		purge = false;
		previewFile = null;
	});
	const errors = $derived(draftErrors(draft));
	$effect(() => {
		dirty =
			(source ? draftDirty(draft, source) : JSON.stringify(draft) !== JSON.stringify(draftOf(null))) ||
			JSON.stringify(supersedes) !== JSON.stringify(source?.supersedes ?? []);
		editedMapping =
			source && structured(source.type) && JSON.stringify(draft.mapping) !== JSON.stringify(source.mapping)
				? draft.mapping.filter((m) => m.source_field.trim())
				: null;
	});
	const uses = $derived(structured(draft.type) && draft.authType !== 'none');
	const fields = $derived(preview?.columns ?? sample?.fields ?? []);
	const why = $derived(canWrite ? '' : writeHint('knowledge_manager'));
	const storeOptions = $derived(leafStores(stores));

	const asFailure = (e: unknown) =>
		e instanceof ApiError ? { code: e.code, message: e.message } : { code: 'error', message: String(e) };

	async function save() {
		if (saving || !canWrite || errors.length) return;
		saving = true;
		failure = null;
		const input = { ...inputOf(draft, source?.has_credentials ?? false), supersedes };
		try {
			const s = source ? await api().saveSource(source.id, input) : await api().createSource(input);
			toast(source ? 'Source saved.' : 'Source added.');
			onsaved(s);
			await tick();
		} catch (e) {
			failure = asFailure(e);
		} finally {
			saving = false;
		}
	}
	async function test() {
		testing = true;
		sample = null;
		failure = null;
		try {
			sample = await api().testSource(source?.id ?? null, inputOf(draft, source?.has_credentials ?? false));
		} catch (e) {
			failure = asFailure(e);
		} finally {
			testing = false;
		}
	}
	/** Columns of the table URL, or of `file` when given (a local sample of the feed). Sheet and header row come
	 *  from the draft, so what is previewed is what a re-sync reads once saved. */
	async function previewColumns(file: File | null = previewFile) {
		previewing = true;
		failure = null;
		try {
			const opts = {
				...(draft.sheet ? { sheet: draft.sheet } : {}),
				...(draft.headerRow ? {} : { header_row: false }),
			};
			const p = await api().previewSourceColumns(
				inputOf(draft, source?.has_credentials ?? false),
				file ?? undefined,
				opts,
			);
			preview = p;
			previewFile = file;
			toast('Columns read. Nothing was stored.');
		} catch (e) {
			preview = null;
			failure = asFailure(e);
		} finally {
			previewing = false;
		}
	}
	function pickPreviewFile(list: FileList | null) {
		const f = list?.[0];
		if (!f) return;
		if (extOf(f.name) === 'xlsx' && draft.format !== 'xlsx') draft.format = 'xlsx';
		if (extOf(f.name) === 'csv' && draft.format !== 'csv') draft.format = 'csv';
		draft.sheet = '';
		void previewColumns(f);
	}
	function pickSheet(name: string) {
		draft.sheet = preview?.sheets[0] === name ? '' : name;
		void previewColumns();
	}
	function toggleHeader(on: boolean) {
		draft.headerRow = on;
		void previewColumns();
	}
	async function remove() {
		if (!source) return;
		const s = source;
		const box: { v: { purged: boolean; documents_deleted: number; documents_kept: number } | null } = {
			v: null,
		};
		const ok = await guard(
			async () => {
				box.v = await api().deleteSource(s.id, purge);
			},
			() =>
				box.v?.purged
					? `Source deleted with its ${box.v.documents_deleted.toLocaleString('en-GB')} documents.`
					: `Source deleted. ${box.v?.documents_kept.toLocaleString('en-GB') ?? 0} documents kept in the store.`,
			'Not deleted',
		);
		if (ok) onsaved(null);
	}
</script>

{#if !canWrite}
	<Callout kind="info" title="Read only"
		><span>You can view this source. Editing needs the tenant_admin role.</span></Callout
	>
{/if}
<fieldset class="editor" disabled={!canWrite || saving}>
	<div class="form-grid">
		<label class="field"><span class="label">Name</span><input class="input" bind:value={draft.name} /></label
		>
		<fieldset class="field span" disabled={!!source}>
			<legend class="label">Type</legend>
			<div class="types">
				{#each SOURCE_TYPES as [t, label, hint] (t)}
					<label class="type" class:on={draft.type === t}>
						<input type="radio" name="source-type" value={t} bind:group={draft.type} />
						<span>{label}</span><span class="cell-sub">{hint}</span>
					</label>
				{/each}
			</div>
			{#if source}<span class="cell-sub">The type is fixed once the source is saved.</span>{/if}
		</fieldset>
		{#if draft.type === 'website'}
			<fieldset class="field span">
				<legend class="label">Pages</legend>
				<div class="types">
					{#each WEBSITE_MODES as [m, label, hint] (m)}
						<label class="type" class:on={draft.mode === m}>
							<input type="radio" name="website-mode" value={m} bind:group={draft.mode} />
							<span>{label}</span><span class="cell-sub">{hint}</span>
						</label>
					{/each}
				</div>
			</fieldset>
			{#if draft.mode === 'urls'}
				<label class="field span"
					><span class="label">URLs, one per line</span><textarea
						class="input mono"
						rows="4"
						bind:value={draft.urls}
					></textarea></label
				>
			{:else}
				<label class="field span"
					><span class="label">{draft.mode === 'crawl' ? 'Start page' : 'Sitemap URL'}</span><input
						class="input mono"
						bind:value={draft.url}
						placeholder="https://help.lumenloom.example/"
					/></label
				>
			{/if}
			{#if draft.mode === 'crawl'}
				<label class="field"
					><span class="label">Link depth (1–5)</span><input
						class="input"
						type="number"
						min="1"
						max="5"
						bind:value={draft.maxDepth}
					/></label
				>
				<label class="field"
					><span class="label">Max pages (1–500)</span><input
						class="input"
						type="number"
						min="1"
						max="500"
						bind:value={draft.maxPages}
					/></label
				>
				<label class="field"
					><span class="label">Allowed domains, one per line</span><textarea
						class="input mono"
						rows="2"
						bind:value={draft.allowedDomains}
					></textarea></label
				>
			{/if}
		{/if}
		{#if structured(draft.type)}
			<label class="field span"
				><span class="label">{draft.type === 'api' ? 'Endpoint URL' : 'Table URL'}</span><input
					class="input mono"
					bind:value={draft.url}
					placeholder="https://api.lumenloom.example/v1/promotions"
				/></label
			>
			<label class="field"
				><span class="label">Authentication</span>
				<select class="input" bind:value={draft.authType}>
					<option value="none">None</option>
					<option value="bearer">Bearer token</option>
					<option value="header">API key header</option>
				</select></label
			>
			{#if draft.authType === 'header'}
				<label class="field"
					><span class="label">Header name</span><input
						class="input mono"
						bind:value={draft.headerName}
						placeholder="X-Api-Key"
					/></label
				>
			{/if}
			{#if draft.type === 'datatable'}
				<label class="field"
					><span class="label">Format</span>
					<select class="input" bind:value={draft.format}>
						<option value="csv">CSV</option>
						<option value="xlsx">XLSX</option>
					</select></label
				>
			{/if}
		{/if}
		{#if draft.type === 'file'}
			<p class="muted span">
				{source
					? 'Upload the files under Runs below, with each re-sync (up to 20 files, 50 MB in all); Agent does not keep them.'
					: 'Add the source first; then upload its files under Runs, with each re-sync (up to 20 files, 50 MB in all).'}
			</p>
		{/if}
		{#if uses}
			<label class="field span">
				<span class="label"
					>Credentials {#if source?.has_credentials && !draft.clearSecret}<Chip
							text={`set · ${when(source.credentials_updated_at)}`}
							variant="ok"
						/>{/if}</span
				>
				<input
					class="input mono"
					type="password"
					autocomplete="new-password"
					aria-label="Credentials secret"
					bind:value={draft.secret}
					placeholder={source?.has_credentials
						? 'Leave empty to keep the stored secret'
						: 'Paste the token or key'}
				/>
			</label>
			{#if source?.has_credentials}
				<label class="check"
					><input type="checkbox" bind:checked={draft.clearSecret} /> Remove the stored credentials</label
				>
			{/if}
			<p class="cell-sub span">
				Write-only: stored encrypted and never shown again; sent only with a test and each re-sync.
			</p>
		{/if}
		<label class="field"
			><span class="label">Target store</span>
			<select class="input" bind:value={draft.targetStoreId}>
				<option value="" disabled>Pick a leaf store</option>
				{#each storeOptions as s (s.id)}<option value={s.id}
						>{s.key}{s.strata_store_id ? '' : ' (unbound)'}</option
					>{/each}
			</select></label
		>
		<label class="field"
			><span class="label">Language of its documents</span>
			<select
				class="input"
				value={shownLanguage}
				onchange={(e) => (draft.language = e.currentTarget.value)}
				aria-label="Language of its documents"
			>
				{#each languageOptions as l (l)}<option value={l}
						>{LANGUAGE_LABEL[l] ?? l} ({l}){l === languages[0] ? ' · job default' : ''}{languages.includes(l)
							? ''
							: ' · not served'}</option
					>{/each}
			</select>
			{#if draft.language && languages.length && !languages.includes(draft.language)}<span
					class="cell-sub warn-text"
					>The job no longer serves {draft.language}: its documents count for no language in coverage.</span
				>{/if}
		</label>
		<SupersedesInput bind:value={supersedes} id="source-supersedes" />
		<label class="field"
			><span class="label">Schedule (cron)</span><input
				class="input mono"
				bind:value={draft.schedule}
				placeholder="0 3 * * *"
				disabled
				aria-describedby="schedule-off"
			/><span class="cell-sub" id="schedule-off" data-testid="schedule-off"
				>Scheduled resync is switched off on this platform — use Resync now.</span
			></label
		>
		<label class="check"><input type="checkbox" bind:checked={draft.enabled} /> Enabled</label>
	</div>

	<div class="row">
		{#if draft.type !== 'file'}
			<button
				class="btn"
				type="button"
				disabled={!canWrite || testing}
				title={why || undefined}
				onclick={test}
				>{testing ? 'Testing…' : draft.type === 'website' ? 'Test start page' : 'Test connection'}</button
			>
		{/if}
		{#if draft.type === 'datatable'}
			<button
				class="btn"
				type="button"
				disabled={!canWrite || previewing}
				title={why || undefined}
				onclick={() => previewColumns(null)}>Preview columns</button
			>
			<label class="btn file-btn" class:disabled={!canWrite || previewing} title={why || undefined}
				>Preview from a local file
				<input
					type="file"
					accept=".csv,.xlsx"
					disabled={!canWrite || previewing}
					aria-label="Preview columns from a local CSV or XLSX"
					onchange={(e) => {
						pickPreviewFile(e.currentTarget.files);
						e.currentTarget.value = '';
					}}
				/></label
			>
		{/if}
		<span class="muted">Tests and previews never save.</span>
	</div>
	{#if sample}
		<div class="callout" class:ok-box={sample.ok} class:err={!sample.ok} role="status">
			<strong>{sample.ok ? 'Connection works' : 'Connection failed'}</strong>
			<span>HTTP {sample.status ?? '—'} · {sample.detail}</span>
			{#if sample.ok && sample.fields.length}<span class="muted"
					>Discovered fields: {sample.fields.join(', ')}</span
				>{/if}
		</div>
		{#if sample.sample != null}<JsonView value={sample.sample} />{/if}
	{/if}
	{#if preview}
		<div class="preview-opts" data-testid="preview-options">
			{#if preview.sheets.length > 1}
				<label class="field"
					><span class="label">Sheet</span>
					<select
						class="input"
						value={preview.sheet}
						disabled={!canWrite || previewing}
						onchange={(e) => pickSheet(e.currentTarget.value)}
					>
						{#each preview.sheets as sh (sh)}<option value={sh}>{sh}</option>{/each}
					</select></label
				>
			{/if}
			<label class="check"
				><input
					type="checkbox"
					checked={draft.headerRow}
					disabled={!canWrite || previewing}
					onchange={(e) => toggleHeader(e.currentTarget.checked)}
				/> First row names the columns</label
			>
		</div>
		<p class="muted">
			{preview.file}{previewFile ? ` (local, ${fileSize(previewFile.size)})` : ''}{preview.sheet
				? ` · sheet ${preview.sheet}`
				: ''} · {preview.row_count.toLocaleString('en-GB')} rows · columns: {preview.columns.join(', ')}
		</p>
		<div class="tbl-wrap">
			<table class="tbl dense">
				<thead
					><tr
						>{#each preview.columns as c (c)}<th>{c}</th>{/each}</tr
					></thead
				>
				<tbody>
					{#each preview.rows as r, i (i)}<tr
							>{#each preview.columns as c (c)}<td class="mono">{r[c] ?? ''}</td>{/each}</tr
						>{/each}
				</tbody>
			</table>
		</div>
	{/if}

	{#if structured(draft.type)}
		<h3>Field mapping</h3>
		<p class="muted mapping-hint">
			Each column becomes an attribute of an object in this job's ontology (for example
			<span class="mono">price_eur → Offering.price</span>), so the agent reads it as a fact, not as free
			text.
		</p>
		<MappingEditor bind:rows={draft.mapping} {targets} {fields} disabled={!canWrite} {why} />
	{/if}
	{#if errors.length}
		<Callout kind="info" title="Before you save"><span>{errors.join(' ')}</span></Callout>
	{/if}
</fieldset>
{#if failure}
	<Callout
		kind="err"
		title={failure.code === 'unknown_attribute'
			? '422 unknown_attribute — mapping rejected'
			: failure.code === 'invalid_schedule'
				? 'The stored schedule is not a valid cron'
				: 'Refused'}><span>{failure.message} ({failure.code})</span></Callout
	>
{/if}
<div class="actions">
	<button
		class="btn primary"
		type="button"
		disabled={!canWrite || saving || errors.length > 0}
		title={why || undefined}
		onclick={save}>{saving ? 'Saving…' : source ? 'Save' : 'Add source'}</button
	>
	{#if source}
		<button
			class="btn danger"
			type="button"
			disabled={!canWrite}
			title={why || undefined}
			onclick={() => (confirmDelete = true)}>Delete</button
		>
	{/if}
</div>
{#if confirmDelete && source}
	<div class="callout err" role="alertdialog" aria-label="Delete source">
		<strong>Delete {source.name}?</strong>
		<span
			>{purge
				? 'Its documents are deleted from strata_rag too.'
				: 'Its documents stay in the store; only the settings and run history go.'}</span
		>
		<label class="check"><input type="checkbox" bind:checked={purge} /> Also delete its documents</label>
		<span
			><button class="btn sm danger" type="button" onclick={remove}>Delete source</button>
			<button class="btn sm" type="button" onclick={() => (confirmDelete = false)}>Keep it</button></span
		>
	</div>
{/if}

<style>
	.preview-opts {
		display: flex;
		flex-wrap: wrap;
		gap: 8px 16px;
		align-items: flex-end;
	}
	.file-btn {
		position: relative;
		overflow: hidden;
	}
	.file-btn input {
		position: absolute;
		inset: 0;
		opacity: 0;
		cursor: pointer;
	}
	.file-btn.disabled {
		opacity: 0.55;
		pointer-events: none;
	}
	.warn-text {
		color: var(--warn);
	}
	.mapping-hint {
		margin: -4px 0 8px;
		font-size: 12.5px;
	}
	.editor {
		border: 0;
		padding: 0;
		margin: 0;
		min-width: 0;
		display: grid;
		gap: 12px;
	}
	.span {
		grid-column: 1 / -1;
	}
	.types {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(160px, 1fr));
		gap: 8px;
	}
	.type {
		display: grid;
		gap: 2px;
		padding: 8px;
		border: 1px solid var(--line);
		border-radius: 8px;
		cursor: pointer;
	}
	.type.on {
		border-color: var(--accent);
	}
	.check {
		display: flex;
		gap: 6px;
		align-items: center;
	}
	.ok-box {
		border-color: color-mix(in srgb, var(--ok) 45%, var(--line));
		background: color-mix(in srgb, var(--ok) 7%, var(--panel));
	}
	.ok-box strong {
		color: var(--ok);
	}
</style>
