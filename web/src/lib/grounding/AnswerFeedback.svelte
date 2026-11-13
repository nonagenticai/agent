<script lang="ts">
	// Thumbs up / down on one agent answer, with an optional reason (wrong / incomplete / not grounded / tone /
	// other + text) on a down vote. A down-voted answer offers "Promote to golden question" (the eval page's `?add=`).
	// Feedback is operational data: it is stored at once (`submitAnswerFeedback`) and never enters the draft.
	import { api, guard } from '$lib/app.svelte';
	import {
		FEEDBACK_REASONS,
		type AnswerFeedback,
		type AnswerFeedbackInput,
		type FeedbackReason,
		type FeedbackVote,
	} from '$lib/api/types/grounding';
	import { promoteHref } from './grounding';

	let {
		answer,
		initial = null,
		onsaved,
	}: {
		/** Which answer is rated and where (vote, reason and note are filled here). */
		answer: Omit<AnswerFeedbackInput, 'vote' | 'reason' | 'note'>;
		/** The caller's earlier rating of this answer, if any. */
		initial?: AnswerFeedback | null;
		onsaved?: (f: AnswerFeedback) => void;
	} = $props();

	let saved = $state<AnswerFeedback | null>(null);
	let open = $state(false);
	let reason = $state<FeedbackReason | null>(null);
	let note = $state('');
	let busy = $state(false);
	const current = $derived(saved ?? initial);
	const uid = $props.id();

	async function submit(vote: FeedbackVote) {
		busy = true;
		let row: AnswerFeedback | null = null;
		const ok = await guard(
			async () =>
				(row = await api().submitAnswerFeedback({
					...answer,
					vote,
					reason: vote === 'down' ? reason : null,
					note: vote === 'down' ? note.trim() || null : null,
				})),
			vote === 'up'
				? 'Saved — data, not design (D69): marked as a good answer.'
				: 'Saved — data, not design (D69): feedback recorded.',
			'Feedback not saved',
		);
		busy = false;
		if (ok && row) {
			saved = row;
			open = false;
			onsaved?.(row);
		}
	}
	function down() {
		reason = current?.vote === 'down' ? current.reason : null;
		note = current?.vote === 'down' ? (current.note ?? '') : '';
		open = !open;
	}
	const reasonLabel = (r: FeedbackReason | null) => FEEDBACK_REASONS.find((x) => x.key === r)?.label ?? '';
	const THUMB = 'M7 10v11H3V10zM7 10l4-8a3 3 0 0 1 3 3v4h5.5a2 2 0 0 1 2 2.3l-1.3 8A2 2 0 0 1 18.2 21H7';
</script>

<div class="af" data-testid="answer-feedback" data-vote={current?.vote ?? ''}>
	<div class="af-row">
		<span class="af-q">Was this answer right?</span>
		<button
			type="button"
			class="af-btn"
			class:on={current?.vote === 'up'}
			aria-pressed={current?.vote === 'up'}
			aria-label="Good answer"
			title="Good answer"
			disabled={busy}
			onclick={() => submit('up')}
			><svg viewBox="0 0 24 24" aria-hidden="true"><path d={THUMB} /></svg></button
		>
		<button
			type="button"
			class="af-btn down"
			class:on={current?.vote === 'down'}
			aria-pressed={current?.vote === 'down'}
			aria-expanded={open}
			aria-controls="{uid}-form"
			aria-label="Bad answer"
			title="Bad answer — say why"
			disabled={busy}
			onclick={down}><svg viewBox="0 0 24 24" aria-hidden="true"><path d={THUMB} /></svg></button
		>
		{#if current?.vote === 'down' && !open}
			{#if current.reason}<span class="chip err sm">{reasonLabel(current.reason)}</span>{/if}
			<a class="af-promote" href={promoteHref(answer.question)} data-testid="promote-golden"
				>Promote to golden question →</a
			>
		{/if}
	</div>
	{#if open}
		<form
			class="af-form"
			id="{uid}-form"
			onsubmit={(e) => {
				e.preventDefault();
				void submit('down');
			}}
		>
			<fieldset>
				<legend>What is wrong with it? <span class="muted">(optional)</span></legend>
				<div class="af-reasons">
					{#each FEEDBACK_REASONS as r (r.key)}
						<label class="af-reason" class:on={reason === r.key}>
							<input type="radio" name="{uid}-reason" value={r.key} bind:group={reason} />
							{r.label}
						</label>
					{/each}
				</div>
			</fieldset>
			<label class="af-note">
				<span class="sr-only">Note</span>
				<input
					class="input"
					bind:value={note}
					maxlength="500"
					placeholder={reason === 'other' ? 'What is wrong?' : 'Anything to add? (optional)'}
				/>
			</label>
			<div class="af-actions">
				<button class="btn sm primary" type="submit" disabled={busy}>Send feedback</button>
				<button class="btn sm" type="button" onclick={() => (open = false)}>Cancel</button>
			</div>
		</form>
	{/if}
</div>

<style>
	.af {
		margin-top: 4px;
		font-size: 12px;
		color: var(--muted);
		min-width: 0;
	}
	.af-row {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 4px 6px;
	}
	.af-btn {
		display: inline-grid;
		place-items: center;
		width: 28px;
		height: 28px;
		padding: 0;
		border: 1px solid var(--line);
		border-radius: 6px;
		background: var(--panel);
		color: var(--muted);
		cursor: pointer;
	}
	.af-btn svg {
		width: 15px;
		height: 15px;
		fill: none;
		stroke: currentColor;
		stroke-width: 1.8;
		stroke-linejoin: round;
	}
	.af-btn.down svg {
		transform: rotate(180deg);
	}
	.af-btn.on {
		color: var(--ok);
		border-color: var(--ok);
		background: color-mix(in srgb, var(--ok) 12%, transparent);
	}
	.af-btn.down.on {
		color: var(--err);
		border-color: var(--err);
		background: color-mix(in srgb, var(--err) 12%, transparent);
	}
	.af-btn:focus-visible {
		outline: 2px solid var(--accent);
		outline-offset: 1px;
	}
	.chip.sm {
		font-size: 10.5px;
		padding: 2px 6px;
	}
	.af-promote {
		font-weight: 500;
	}
	.af-form {
		display: grid;
		gap: 6px;
		margin-top: 6px;
		padding: 8px;
		border: 1px solid var(--line);
		border-radius: 6px;
		background: var(--sunk);
	}
	.af-form fieldset {
		border: 0;
		margin: 0;
		padding: 0;
		min-width: 0;
	}
	.af-form legend {
		padding: 0;
		margin-bottom: 4px;
		color: var(--fg);
	}
	.af-reasons {
		display: flex;
		flex-wrap: wrap;
		gap: 4px;
	}
	.af-reason {
		display: inline-flex;
		align-items: center;
		gap: 4px;
		padding: 3px 8px;
		border: 1px solid var(--line);
		border-radius: 999px;
		background: var(--panel);
		cursor: pointer;
	}
	.af-reason.on {
		border-color: var(--accent);
		color: var(--accent);
	}
	.af-reason input {
		margin: 0;
	}
	.af-note .input {
		width: 100%;
	}
	.af-actions {
		display: flex;
		gap: 6px;
		flex-wrap: wrap;
	}
	.sr-only {
		position: absolute;
		width: 1px;
		height: 1px;
		overflow: hidden;
		clip: rect(0 0 0 0);
		white-space: nowrap;
	}
</style>
