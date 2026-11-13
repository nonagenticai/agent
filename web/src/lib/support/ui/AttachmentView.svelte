<script lang="ts">
	// One attachment in a transcript row (SW-9). The bytes sit behind auth, so they are fetched with the bearer (the
	// API client) and shown from an object URL; no token ever goes into a URL. A photo loads when it scrolls into
	// view, shows as a thumbnail and opens larger on click; a PDF is a chip that downloads on click.
	import { onDestroy, tick } from 'svelte';
	import { api } from '$lib/app.svelte';
	import Icon from '$lib/ui/Icon.svelte';
	import type { AttachmentRef } from '$lib/api/types/support';
	import { isImage, sizeLabel } from '$lib/support/attachments';

	let { att, job, conversationId }: { att: AttachmentRef; job: string; conversationId: string } = $props();

	let url = $state<string | null>(null);
	let failed = $state(false);
	let big = $state(false);
	let box = $state<HTMLElement | null>(null);
	let closeBtn = $state<HTMLButtonElement | null>(null);
	let opener: HTMLElement | null = null;
	let loading: Promise<string | null> | null = null;

	function load(): Promise<string | null> {
		if (url) return Promise.resolve(url);
		loading ??= api()
			.attachmentBlob(job, conversationId, att.id)
			.then((b) => (url = URL.createObjectURL(b)))
			.catch(() => {
				failed = true;
				return null;
			})
			.finally(() => (loading = null));
		return loading;
	}

	// A photo is fetched when it comes into view (a long conversation does not fetch every photo up front).
	$effect(() => {
		if (!isImage(att) || !box) return;
		if (typeof IntersectionObserver === 'undefined') {
			load();
			return;
		}
		const io = new IntersectionObserver((es) => {
			if (es.some((e) => e.isIntersecting)) {
				io.disconnect();
				load();
			}
		});
		io.observe(box);
		return () => io.disconnect();
	});
	onDestroy(() => {
		if (url) URL.revokeObjectURL(url);
	});

	async function save() {
		const u = await load();
		if (!u) return;
		const a = document.createElement('a');
		a.href = u;
		a.download = att.display_name;
		a.click();
	}
	async function openBig(e: Event) {
		opener = e.currentTarget as HTMLElement;
		if (!(await load())) return;
		big = true;
		await tick();
		closeBtn?.focus();
	}
	function close() {
		big = false;
		opener?.focus();
	}
	function onKey(e: KeyboardEvent) {
		if (e.key === 'Escape') {
			e.stopPropagation();
			close();
		}
	}
</script>

{#if isImage(att)}
	<div class="ph" bind:this={box} data-testid="attachment-photo">
		<button
			type="button"
			class="thumb"
			aria-label={`Open photo ${att.display_name}`}
			title={att.display_name}
			onclick={openBig}
		>
			{#if url}
				<img src={url} alt={att.display_name} />
			{:else if failed}
				<span class="miss"><Icon name="warn" size={16} /> Photo unavailable</span>
			{:else}
				<span class="wait" aria-hidden="true"></span>
			{/if}
		</button>
		<span class="cap">{att.display_name} · {sizeLabel(att.byte_size)}</span>
	</div>
	{#if big && url}
		<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
		<div
			class="veil"
			role="dialog"
			aria-modal="true"
			aria-label={att.display_name}
			tabindex="-1"
			onkeydown={onKey}
		>
			<div class="modal">
				<img src={url} alt={att.display_name} />
				<div class="acts">
					<span class="nm">{att.display_name}</span>
					<a class="btn sm" href={url} download={att.display_name}
						><Icon name="download" size={14} /> Download</a
					>
					<button class="btn sm" type="button" bind:this={closeBtn} onclick={close}>Close</button>
				</div>
			</div>
		</div>
	{/if}
{:else}
	<button
		type="button"
		class="pdf"
		data-testid="attachment-pdf"
		aria-label={`Download ${att.display_name}, ${sizeLabel(att.byte_size)}`}
		onclick={save}
	>
		<Icon name="file" size={16} />
		<span class="nm">{att.display_name}</span>
		<span class="sz">{failed ? 'Unavailable' : sizeLabel(att.byte_size)}</span>
		<Icon name="download" size={14} />
	</button>
{/if}

<style>
	.ph {
		display: grid;
		gap: 2px;
		margin-top: 4px;
	}
	.thumb {
		border: 1px solid var(--line);
		border-radius: 8px;
		padding: 0;
		background: var(--sunk);
		cursor: zoom-in;
		overflow: hidden;
		width: 220px;
		max-width: 100%;
		height: 150px;
		display: grid;
		place-items: center;
	}
	.thumb img {
		width: 100%;
		height: 100%;
		object-fit: cover;
		display: block;
	}
	.wait {
		width: 100%;
		height: 100%;
		background: var(--sunk);
	}
	.miss {
		display: inline-flex;
		gap: 4px;
		align-items: center;
		color: var(--muted);
		font-size: 12px;
	}
	.cap {
		font-size: 11px;
		color: var(--muted);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.pdf {
		margin-top: 4px;
		display: inline-flex;
		align-items: center;
		gap: 6px;
		max-width: 100%;
		border: 1px solid var(--line);
		border-radius: 8px;
		background: var(--panel);
		padding: 5px 9px;
		cursor: pointer;
		font-size: 13px;
	}
	.nm {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		min-width: 0;
	}
	.sz {
		color: var(--muted);
		font-size: 12px;
		flex: none;
	}
	.veil {
		position: fixed;
		inset: 0;
		z-index: 50;
		background: rgb(0 0 0 / 0.6);
		display: grid;
		place-items: center;
		padding: 16px;
	}
	.modal {
		background: var(--panel);
		border-radius: 10px;
		padding: 10px;
		max-width: min(92vw, 1100px);
		max-height: 92vh;
		display: grid;
		gap: 8px;
		grid-template-rows: minmax(0, 1fr) auto;
	}
	.modal img {
		max-width: 100%;
		max-height: calc(92vh - 70px);
		object-fit: contain;
		justify-self: center;
		min-height: 0;
	}
	.acts {
		display: flex;
		align-items: center;
		gap: 8px;
	}
	.acts .nm {
		flex: 1;
		font-size: 13px;
	}
</style>
