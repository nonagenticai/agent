<script lang="ts">
	// Reply / Note composer. Reply needs the conversation (a non-owner sees it disabled with an inline Take); a note
	// is staff-only and open to every support role. The draft lives here and survives an owner change.
	import { untrack } from 'svelte';
	import { api } from '$lib/app.svelte';
	import Icon from '$lib/ui/Icon.svelte';
	import { bindKeys } from '$lib/support/keys';
	import { limitFor, MAX_TEXT } from '$lib/support/limits';
	import type { AttachmentRef, CannedReply, SendKind } from '$lib/api/types/support';
	import { ACCEPT, MAX_FILES, noFilesReason, refusalText } from '$lib/support/attachments';
	import { KYC_HINT, asksForIdentifier } from '$lib/support/asks';
	import { getDraft, setDraft } from '$lib/support/drafts';
	import CannedPicker from './CannedPicker.svelte';

	let {
		job,
		conversationId = null,
		channel,
		canReply,
		canTake = true,
		takeTitle,
		answering = null,
		staffFiles,
		onTake,
		onSend,
	}: {
		job: string;
		/** The open conversation: its draft is kept while the workspace is open (switching panes, resizing). */
		conversationId?: string | null;
		channel: string;
		/** The admin's setting for this channel (`conversation.attachments.staff`); absent = on. */
		staffFiles?: boolean;
		/** The viewer owns the conversation, so a reply may go out. */
		canReply: boolean;
		canTake?: boolean;
		takeTitle?: string;
		/** A colleague owns the conversation: their name. No Take (it moves with Assign…), just who answers. */
		answering?: string | null;
		onTake: () => void;
		onSend: (kind: SendKind, text: string, attachments?: AttachmentRef[]) => void;
	} = $props();

	// Mounted per conversation (the caller keys it), so the first read is the right conversation's draft.
	const saved = untrack(() =>
		conversationId ? getDraft(job, conversationId) : { mode: 'reply' as SendKind, text: '' },
	);
	let mode = $state<SendKind>(saved.mode);
	let draft = $state(saved.text);
	let area = $state<HTMLTextAreaElement | null>(null);
	let canned = $state<CannedReply[] | null>(null);
	let active = $state(0);
	let dismissed = $state(false);

	const limit = $derived(mode === 'note' ? MAX_TEXT : limitFor(channel));
	const writable = $derived(mode === 'note' || canReply);

	// Attachments (SW-9): uploaded on pick, held as chips, carried by the next reply. Replies only; a note is text.
	type Chip = {
		key: string;
		name: string;
		state: 'uploading' | 'ready' | 'error';
		ref?: AttachmentRef;
		error?: string;
	};
	let chips = $state<Chip[]>([]);
	let picker = $state<HTMLInputElement | null>(null);
	let notice = $state('');
	const channelReason = $derived(noFilesReason(channel, staffFiles));
	const attachOff = $derived(
		!writable
			? 'Take this conversation to attach files.'
			: mode === 'note'
				? 'Notes are text only. Switch to Reply to attach a photo or PDF.'
				: channelReason,
	);
	const full = $derived(chips.filter((c) => c.state !== 'error').length >= MAX_FILES);
	const busy = $derived(chips.some((c) => c.state === 'uploading'));
	const refs = $derived(chips.flatMap((c) => (c.state === 'ready' && c.ref ? [c.ref] : [])));
	const noteWithFiles = $derived(mode === 'note' && chips.length > 0);

	async function onPick(e: Event) {
		const input = e.currentTarget as HTMLInputElement;
		const picked = [...(input.files ?? [])];
		input.value = '';
		if (!picked.length || attachOff) return;
		notice = '';
		const room = MAX_FILES - chips.filter((c) => c.state !== 'error').length;
		if (picked.length > room) notice = `You can attach up to ${MAX_FILES} files to one reply.`;
		for (const file of picked.slice(0, Math.max(room, 0))) {
			const chip: Chip = { key: crypto.randomUUID(), name: file.name, state: 'uploading' };
			chips.push(chip);
			const mine = chips[chips.length - 1]!;
			api()
				.uploadAttachment(job, conversationId ?? '', file)
				.then((ref) => {
					mine.ref = ref;
					mine.name = ref.display_name;
					mine.state = 'ready';
				})
				.catch((err) => {
					mine.state = 'error';
					mine.error = refusalText(err);
				});
		}
	}
	function drop(c: Chip) {
		chips = chips.filter((x) => x.key !== c.key);
		// An unsent upload is deleted; if that fails the server sweeps it within the hour.
		if (c.ref)
			api()
				.deleteAttachment(job, conversationId ?? '', c.ref.id)
				.catch(() => {});
	}

	const asksKey = $derived(mode === 'reply' && asksForIdentifier(draft));
	const over = $derived(draft.length > limit);
	const empty = $derived(draft.trim() === '' && !(mode === 'reply' && refs.length > 0));
	const blocked = $derived(busy || noteWithFiles);
	const picking = $derived(writable && mode === 'reply' && !dismissed && draft.startsWith('/'));
	const query = $derived(draft.slice(1).trim().toLowerCase());
	const shown = $derived(
		(canned ?? []).filter(
			(r) => !query || r.title.toLowerCase().includes(query) || r.body.toLowerCase().includes(query),
		),
	);

	$effect(() => {
		if (picking && canned === null) {
			canned = [];
			api()
				.canned(job)
				.then((l) => (canned = l))
				.catch(() => (canned = []));
		}
	});
	$effect(() => {
		query;
		active = 0;
	});
	$effect(() => {
		if (conversationId) setDraft(job, conversationId, { mode, text: draft });
	});
	$effect(() => {
		if (!area) return;
		return bindKeys(area, { 'Mod+Enter': send });
	});

	function send() {
		if (!writable || over || empty || blocked) return;
		if (mode === 'reply' && refs.length) onSend(mode, draft.trim(), refs);
		else onSend(mode, draft.trim());
		draft = '';
		chips = [];
		notice = '';
		mode = 'reply';
		dismissed = false;
	}
	function pick(r: CannedReply) {
		draft = r.body;
		dismissed = true;
		area?.focus();
	}
	function onInput() {
		dismissed = false;
	}
	function onKey(e: KeyboardEvent) {
		if (!picking) return;
		if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
			e.preventDefault();
			const n = shown.length;
			if (n) active = (active + (e.key === 'ArrowDown' ? 1 : n - 1)) % n;
		} else if (e.key === 'Enter' && !e.ctrlKey && !e.metaKey && shown[active]) {
			e.preventDefault();
			pick(shown[active]!);
		} else if (e.key === 'Escape') {
			e.preventDefault();
			dismissed = true;
		}
	}
</script>

<div class="cmp" class:is-note={mode === 'note'}>
	<div class="bar">
		<div class="tog" role="group" aria-label="Message type">
			<button
				type="button"
				class:on={mode === 'reply'}
				aria-pressed={mode === 'reply'}
				onclick={() => (mode = 'reply')}>Reply</button
			>
			<button
				type="button"
				class:on={mode === 'note'}
				aria-pressed={mode === 'note'}
				onclick={() => (mode = 'note')}>Note</button
			>
		</div>
		{#if mode === 'note'}
			<span class="only"><Icon name="lock" size={13} /> Visible to staff only</span>
		{/if}
		{#if mode === 'reply' && !canReply && !answering}
			<button class="btn sm primary take" type="button" disabled={!canTake} title={takeTitle} onclick={onTake}
				>Take</button
			>
		{/if}
	</div>
	{#if picking}<CannedPicker replies={shown} {query} {active} onPick={pick} />{/if}
	<textarea
		bind:this={area}
		bind:value={draft}
		rows="3"
		disabled={!writable}
		aria-label={mode === 'note' ? 'Internal note' : 'Reply to customer'}
		placeholder={!writable
			? answering
				? `${answering} is answering this conversation`
				: 'Take this conversation to reply'
			: mode === 'note'
				? 'Write a note for your team…'
				: 'Write a reply… ( / for saved replies )'}
		oninput={onInput}
		onkeydown={onKey}></textarea>
	{#if asksKey}<p class="hint" role="note" data-testid="kyc-hint">{KYC_HINT}</p>{/if}
	{#if chips.length}
		<ul class="chips" aria-label="Attached files" data-testid="chips">
			{#each chips as c (c.key)}
				<li class="chip" class:bad={c.state === 'error'}>
					<Icon name="file" size={13} />
					<span class="cn">{c.name}</span>
					{#if c.state === 'uploading'}
						<progress aria-label={`Uploading ${c.name}`}></progress>
						<span class="cs">Uploading…</span>
					{:else if c.state === 'error'}
						<span class="cs" role="alert">{c.error}</span>
					{/if}
					<button type="button" class="x" aria-label={`Remove ${c.name}`} onclick={() => drop(c)}
						><Icon name="x" size={12} /></button
					>
				</li>
			{/each}
		</ul>
	{/if}
	{#if notice}<p class="hint" role="status">{notice}</p>{/if}
	{#if noteWithFiles}<p class="hint" role="note">
			Notes are text only. Remove the attached files or switch to Reply.
		</p>{/if}
	<div class="foot">
		<input
			bind:this={picker}
			type="file"
			class="pick"
			accept={ACCEPT}
			multiple
			tabindex="-1"
			aria-label="Choose photos or PDFs"
			data-testid="attach-input"
			disabled={!!attachOff || full}
			onchange={onPick}
		/>
		<button
			type="button"
			class="btn sm clip"
			aria-label="Attach photos or PDFs"
			aria-describedby={attachOff ? 'attach-why' : undefined}
			title={attachOff ?? (full ? `Up to ${MAX_FILES} files per reply` : 'Attach photos or PDFs')}
			disabled={!!attachOff || full}
			onclick={() => picker?.click()}><Icon name="paperclip" size={15} /></button
		>
		{#if attachOff && writable}<span id="attach-why" class="why" title={attachOff} data-testid="attach-why"
				>{attachOff}</span
			>{/if}
		<span class="count" class:over aria-live="polite">{draft.length} / {limit}</span>
		<button class="btn primary" type="button" disabled={!writable || over || empty || blocked} onclick={send}>
			{mode === 'note' ? 'Add note' : 'Send'}
		</button>
	</div>
</div>

<style>
	.cmp {
		border-top: 1px solid var(--line);
		background: var(--panel);
		padding: 8px 12px 10px;
		display: grid;
		gap: 6px;
	}
	.cmp.is-note {
		background: var(--note);
		border-top-color: var(--note-line);
	}
	.bar {
		display: flex;
		align-items: center;
		gap: 10px;
	}
	.tog {
		display: inline-flex;
		border: 1px solid var(--line);
		border-radius: 6px;
		overflow: hidden;
	}
	.tog button {
		border: 0;
		background: var(--panel);
		padding: 3px 12px;
		cursor: pointer;
		font-size: 13px;
	}
	.tog button.on {
		background: var(--accent);
		color: var(--panel);
	}
	.only {
		display: inline-flex;
		gap: 4px;
		align-items: center;
		font-size: 12px;
		color: var(--note-fg);
	}
	.take {
		margin-left: auto;
	}
	textarea {
		width: 100%;
		resize: vertical;
		border: 1px solid var(--line);
		border-radius: var(--r);
		background: var(--bg);
		padding: 7px 9px;
		min-height: 64px;
	}
	.is-note textarea {
		border-color: var(--note-line);
		background: var(--panel);
	}
	textarea:disabled {
		opacity: 0.7;
		cursor: not-allowed;
	}
	.hint {
		margin: 0;
		font-size: 12px;
		color: var(--note-fg, var(--muted));
	}
	.foot {
		display: flex;
		align-items: center;
		gap: 8px;
	}
	.foot .count {
		margin-left: auto;
	}
	.pick {
		display: none;
	}
	.why {
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
		font-size: 12px;
		color: var(--muted);
		min-width: 0;
	}
	.chips {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
	}
	.chip {
		display: inline-flex;
		align-items: center;
		gap: 5px;
		max-width: 100%;
		border: 1px solid var(--line);
		border-radius: 999px;
		padding: 2px 4px 2px 9px;
		background: var(--bg);
		font-size: 12px;
	}
	.chip.bad {
		border-color: var(--err);
	}
	.cn {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		max-width: 180px;
	}
	.cs {
		color: var(--muted);
	}
	.chip.bad .cs {
		color: var(--err);
	}
	.chip progress {
		width: 48px;
		height: 6px;
	}
	.x {
		border: 0;
		background: transparent;
		cursor: pointer;
		display: grid;
		place-items: center;
		padding: 3px;
		border-radius: 50%;
	}
	.count {
		font: 12px var(--mono);
		color: var(--muted);
	}
	.count.over {
		color: var(--err);
		font-weight: 600;
	}
	/* Touch: 44 px targets, and a 16 px field so a phone does not zoom the page when it takes focus. */
	@media (max-width: 1199.98px) {
		.tog button,
		.foot .btn,
		.take {
			min-height: 44px;
			min-width: 44px;
		}
		.tog button {
			padding-inline: 16px;
		}
		textarea {
			font-size: 16px;
		}
	}
	@media (max-width: 767.98px) {
		.cmp {
			padding-bottom: calc(10px + env(safe-area-inset-bottom, 0px));
		}
		textarea {
			resize: none;
			min-height: 56px;
			max-height: 30dvh;
		}
	}
</style>
