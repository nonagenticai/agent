<script lang="ts">
	// J13: every unpublished change of the job, across all design pages (D54 `draftDiff`) — where it was made, what
	// Production runs there now (live) next to what the test chat runs (draft), who and when. Discard one or all.
	import { api, app, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { writeAttrs } from '$lib/auth/roles';
	import type { DraftChange } from '$lib/api/types/release';
	import { areaLabel, pageLink } from '$lib/release/J13-workspace';

	let {
		changes,
		liveRelease,
		onchange,
	}: {
		/** Null while loading. */
		changes: DraftChange[] | null;
		liveRelease: string | null;
		/** After a discard: the page re-reads the draft, the tables and the job. */
		onchange: () => void;
	} = $props();

	const wa = $derived(writeAttrs(session, 'job_designer'));
	let busy = $state('');
	let confirmAll = $state(false);

	const when = (iso: string) =>
		new Date(iso).toLocaleString('en-GB', {
			day: 'numeric',
			month: 'short',
			hour: '2-digit',
			minute: '2-digit',
			timeZone: 'UTC',
		});

	async function discard(id: string, what: string) {
		if (busy) return;
		busy = id;
		const ok = await guard(
			() => api().discardDraft(id),
			id === 'all'
				? 'Discarded every unpublished change — the draft is back to what Production runs.'
				: `Discarded: ${what}. The live value is back in the draft.`,
			'Not discarded',
		);
		busy = '';
		confirmAll = false;
		if (ok) onchange();
	}
</script>

<section class="dc" aria-labelledby="dc-h" data-testid="j13-draft">
	<header class="dc-head">
		<div>
			<h2 id="dc-h">
				Unpublished changes
				{#if changes}<span class="chip {changes.length ? 'accent' : 'muted'}">{changes.length}</span>{/if}
			</h2>
			<p class="muted">
				Everything you and your team saved on the design pages since the last publish. Production still runs
				the live value; the test chat runs the draft.
			</p>
		</div>
		{#if changes?.length}
			<div class="dc-all">
				{#if confirmAll}
					<span class="muted">Throw away all {changes.length}?</span>
					<button
						class="btn sm danger"
						type="button"
						disabled={wa.disabled || busy !== ''}
						title={wa.title}
						onclick={() => discard('all', '')}>Yes, discard all</button
					>
					<button class="btn sm" type="button" onclick={() => (confirmAll = false)}>Keep</button>
				{:else}
					<button
						class="btn sm"
						type="button"
						disabled={wa.disabled || busy !== ''}
						title={wa.title}
						onclick={() => (confirmAll = true)}>Discard all</button
					>
				{/if}
			</div>
		{/if}
	</header>

	{#if changes === null}
		<p class="muted">Loading the draft…</p>
	{:else if changes.length === 0}
		<div class="dc-empty">
			<b>No unpublished changes.</b>
			<span class="muted"
				>Production runs {liveRelease ?? 'the published release'} exactly as designed. Your next save on any design
				page starts a draft.</span
			>
		</div>
	{:else}
		<ul class="dc-list">
			{#each changes as c (c.id)}
				{@const link = pageLink(c.page, app.job)}
				<li class="dc-row" data-draft={c.key}>
					<div class="dc-where">
						{#if areaLabel(c.area) !== link.title}<span class="chip muted">{areaLabel(c.area)}</span>{/if}
						{#if link.href}<a href={link.href}>{link.title}</a>{:else}<span>{link.title}</span>{/if}
						{#if c.in_release}<span class="chip ok" title="Ships when {c.in_release} is applied"
								>in {c.in_release}</span
							>{/if}
					</div>
					<div class="dc-what">{c.label}</div>
					<div class="dc-diff">
						<span class="dc-side"
							><span class="dc-tag">Live</span><span class="mono">{c.live ?? 'not set'}</span></span
						>
						<span class="dc-side draft"
							><span class="dc-tag">Draft</span><span class="mono">{c.draft ?? 'removed'}</span></span
						>
					</div>
					<div class="dc-meta">
						<span class="muted">{c.by_name} · {when(c.at)}</span>
						<button
							class="btn sm"
							type="button"
							aria-label="Discard {c.label}"
							disabled={wa.disabled || busy !== ''}
							title={wa.title ?? 'Throw this change away; the live value comes back'}
							onclick={() => discard(c.id, c.label)}>{busy === c.id ? 'Discarding…' : 'Discard'}</button
						>
					</div>
				</li>
			{/each}
		</ul>
	{/if}
</section>

<style>
	.dc {
		border: 1px solid var(--line);
		background: var(--panel);
		border-radius: var(--r);
		padding: 14px 16px;
		margin: 0 0 16px;
	}
	.dc-head {
		display: flex;
		flex-wrap: wrap;
		justify-content: space-between;
		gap: 8px 16px;
		align-items: flex-start;
	}
	.dc-head h2 {
		font-size: 15px;
		margin: 0;
		display: flex;
		gap: 8px;
		align-items: center;
	}
	.dc-head p {
		margin: 2px 0 10px;
		font-size: 13px;
		max-width: 70ch;
	}
	.dc-all {
		display: flex;
		gap: 6px;
		align-items: center;
		flex-wrap: wrap;
		font-size: 13px;
	}
	.dc-empty {
		display: grid;
		gap: 2px;
		padding: 10px 12px;
		border: 1px dashed var(--line);
		border-radius: var(--r);
		font-size: 13px;
	}
	.dc-list {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.dc-row {
		display: grid;
		grid-template-columns: minmax(150px, 0.8fr) minmax(0, 1.6fr) minmax(0, 1.6fr) auto;
		gap: 6px 14px;
		align-items: center;
		padding: 10px 0;
		border-top: 1px solid var(--line-2);
		font-size: 13px;
	}
	@media (max-width: 1100px) {
		.dc-row {
			grid-template-columns: minmax(0, 1fr) auto;
		}
		.dc-what,
		.dc-diff {
			grid-column: 1 / -1;
		}
		.dc-where {
			grid-row: 1;
		}
		.dc-meta {
			grid-row: 1;
			grid-column: 2;
		}
	}
	.dc-where {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		align-items: center;
	}
	.dc-what {
		overflow-wrap: anywhere;
	}
	.dc-diff {
		display: grid;
		gap: 3px;
		min-width: 0;
		justify-items: start;
	}
	.dc-side {
		display: inline-flex;
		gap: 6px;
		align-items: baseline;
		min-width: 0;
		max-width: 100%;
		overflow-wrap: anywhere;
		padding: 2px 6px;
		border-radius: 4px;
		background: var(--sunk);
		font-size: 12px;
	}
	.dc-side.draft {
		outline: 1px dashed var(--accent);
		background: var(--panel);
	}
	.dc-tag {
		width: 3.4em;
		font-size: 10px;
		text-transform: uppercase;
		letter-spacing: 0.05em;
		color: var(--muted);
		flex: none;
	}
	.dc-side.draft .dc-tag {
		color: var(--accent);
	}
	.dc-meta {
		display: flex;
		gap: 8px;
		align-items: center;
		justify-content: flex-end;
		white-space: nowrap;
	}
</style>
