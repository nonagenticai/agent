<script lang="ts">
	// Resolve: how it ended (resolved / not resolvable / duplicate), an optional closing message (previewed as the
	// customer will see it, within the channel's limit), and the one-time "Should the agent have handled this?"
	// feedback, and (SW-6) "Ask the customer for a rating", default ON. The server's own default is on too but off after
	// a complaint, which the page cannot see, so `ask_rating` is sent only when the person unticks the box; left alone
	// it is omitted and the server decides. The box is hidden on a channel nobody reads (api, mcp, voice, staff).
	// Submits `SupportApi.resolve` through `onSubmit`.
	import Drawer from '$lib/ui/Drawer.svelte';
	import { limitFor } from '$lib/support/limits';
	import type { Resolve, ResolveOutcome } from '$lib/api/types/support';

	let {
		open = $bindable(false),
		channel,
		customer,
		onSubmit,
	}: {
		open?: boolean;
		channel: string;
		customer: string;
		/** Resolves when done; throws with the message to show. */
		onSubmit: (input: Resolve) => Promise<void>;
	} = $props();

	const OUTCOMES: { id: ResolveOutcome; label: string; hint: string }[] = [
		{ id: 'resolved', label: 'Resolved', hint: 'The customer has what they needed.' },
		{ id: 'not_resolvable', label: 'Not resolvable', hint: 'Nothing more can be done for this request.' },
		{ id: 'duplicate', label: 'Duplicate', hint: 'Handled in another conversation or case.' },
	];
	type Verdict = 'agent_should_handle' | 'right_to_hand_over' | '';

	let outcome = $state<ResolveOutcome>('resolved');
	let closing = $state('');
	let askRating = $state(true);
	let verdict = $state<Verdict>('');
	let why = $state('');
	let busy = $state(false);
	let error = $state<string | null>(null);

	const asks = $derived(!['api', 'mcp', 'voice', 'staff'].includes(channel));
	const limit = $derived(limitFor(channel));
	const over = $derived(closing.trim().length > limit);
	const dirty = $derived(!askRating || closing.trim() !== '' || why.trim() !== '' || verdict !== '');

	$effect(() => {
		if (open) {
			outcome = 'resolved';
			closing = '';
			askRating = true;
			verdict = '';
			why = '';
			error = null;
		}
	});

	async function submit(e: SubmitEvent) {
		e.preventDefault();
		if (over || busy) return;
		busy = true;
		error = null;
		const input: Resolve = { outcome };
		if (closing.trim()) input.closing_message = closing.trim();
		if (asks && !askRating) input.ask_rating = false;
		if (verdict) input.feedback = { verdict, ...(why.trim() ? { note: why.trim() } : {}) };
		try {
			await onSubmit(input);
			open = false;
		} catch (err) {
			error = err instanceof Error ? err.message : 'Could not resolve.';
		} finally {
			busy = false;
		}
	}
</script>

<Drawer bind:open title="Resolve" {dirty}>
	<form class="rd" aria-label="Resolve this conversation" onsubmit={submit}>
		<fieldset>
			<legend>Outcome</legend>
			{#each OUTCOMES as o (o.id)}
				<div class="opt">
					<input
						type="radio"
						id="outcome-{o.id}"
						name="outcome"
						value={o.id}
						aria-describedby="outcome-{o.id}-hint"
						bind:group={outcome}
					/>
					<label for="outcome-{o.id}"><strong>{o.label}</strong></label>
					<span class="muted" id="outcome-{o.id}-hint">{o.hint}</span>
				</div>
			{/each}
		</fieldset>

		<label class="field">
			<span>Closing message (optional)</span>
			<textarea
				rows="3"
				aria-label="Closing message (optional)"
				placeholder="Sent to the customer before the conversation goes back to the agent"
				bind:value={closing}
			></textarea>
			<span class="count" class:over>{closing.trim().length} / {limit}</span>
		</label>
		{#if closing.trim()}
			<div class="preview" data-testid="closing-preview">
				<span class="muted">{customer} will see:</span>
				<p class="bubble">{closing.trim()}</p>
			</div>
		{/if}

		{#if asks}
			<label class="opt rate">
				<input type="checkbox" bind:checked={askRating} aria-describedby="ask-rating-hint" />
				<span><strong>Ask the customer for a rating</strong></span>
			</label>
			<span class="muted hint" id="ask-rating-hint">Not asked by default after a complaint.</span>
		{/if}

		<fieldset>
			<legend>Should the agent have handled this? (optional)</legend>
			<label class="opt">
				<input type="radio" name="verdict" value="agent_should_handle" bind:group={verdict} />
				<span>Yes, the agent should have handled this</span>
			</label>
			<label class="opt">
				<input type="radio" name="verdict" value="right_to_hand_over" bind:group={verdict} />
				<span>No, it was right to hand over</span>
			</label>
			<label class="opt">
				<input type="radio" name="verdict" value="" bind:group={verdict} />
				<span>Skip</span>
			</label>
			{#if verdict}
				<label class="field">
					<span>Why (optional)</span>
					<textarea rows="2" aria-label="Why (optional)" bind:value={why}></textarea>
				</label>
			{/if}
		</fieldset>

		{#if error}<p class="err" role="alert">{error}</p>{/if}
		<div class="actions">
			<button class="btn" type="button" onclick={() => (open = false)}>Cancel</button>
			<button
				class="btn primary"
				type="submit"
				disabled={busy || over}
				title={over ? `At most ${limit} characters` : undefined}>Resolve</button
			>
		</div>
	</form>
</Drawer>

<style>
	.rd {
		display: grid;
		gap: 14px;
	}
	fieldset {
		border: 1px solid var(--line);
		border-radius: 8px;
		padding: 8px 12px 10px;
		display: grid;
		gap: 6px;
		margin: 0;
	}
	legend {
		font-weight: 600;
		padding: 0 4px;
	}
	.opt {
		display: flex;
		gap: 8px;
		align-items: baseline;
	}
	.hint {
		font-size: 12px;
		margin-top: -8px;
	}
	.field {
		display: grid;
		gap: 4px;
	}
	.muted {
		color: var(--muted);
	}
	.count {
		font-size: 12px;
		color: var(--muted);
		justify-self: end;
	}
	.count.over,
	.err {
		color: var(--err);
	}
	.preview {
		display: grid;
		gap: 4px;
	}
	.bubble {
		margin: 0;
		justify-self: end;
		max-width: 72%;
		padding: 8px 11px;
		border-radius: 10px;
		background: color-mix(in srgb, var(--accent) 14%, var(--panel));
		white-space: pre-wrap;
		overflow-wrap: anywhere;
	}
	.actions {
		display: flex;
		justify-content: flex-end;
		gap: 8px;
	}
</style>
