<script lang="ts">
	// "Should the agent have handled this?" outside Resolve: any working role (Support agent, Support manager,
	// Operator, Job designer), owner or not, once per handover item. When feedback already exists (on the item, or
	// the route answers 409 `feedback_exists`), the dialog shows it instead of the form.
	import Drawer from '$lib/ui/Drawer.svelte';
	import { api } from '$lib/app.svelte';
	import type { HandoverFeedback } from '$lib/api/types/journey';
	import { clockOf } from '$lib/support/time';

	type Verdict = 'agent_should_handle' | 'right_to_hand_over';

	let {
		open = $bindable(false),
		job,
		itemId,
		onDone,
	}: {
		open?: boolean;
		job: string;
		itemId: string;
		onDone?: () => void;
	} = $props();

	let existing = $state<HandoverFeedback | null>(null);
	let verdict = $state<Verdict | ''>('');
	let why = $state('');
	let busy = $state(false);
	let loading = $state(false);
	let error = $state<string | null>(null);

	const VERDICT: Record<Verdict, string> = {
		agent_should_handle: 'The agent should have handled this',
		right_to_hand_over: 'It was right to hand over',
	};

	$effect(() => {
		if (!open) return;
		const j = job;
		const id = itemId;
		verdict = '';
		why = '';
		error = null;
		existing = null;
		loading = true;
		let live = true;
		api()
			.jobHandovers(j)
			.then((q) => {
				if (live) existing = q.items.find((h) => h.id === id)?.feedback ?? null;
			})
			.catch(() => {
				/* the form still works; the route refuses a second verdict */
			})
			.finally(() => {
				if (live) loading = false;
			});
		return () => {
			live = false;
		};
	});

	async function submit(e: SubmitEvent) {
		e.preventDefault();
		if (!verdict) {
			error = 'Choose an answer';
			return;
		}
		busy = true;
		error = null;
		try {
			const h = await api().jobHandoverFeedback(job, itemId, verdict, why.trim());
			existing = h.feedback ?? null;
			onDone?.();
		} catch (err) {
			const x = err as { status?: number; code?: string; details?: { feedback?: HandoverFeedback } };
			if (x?.status === 409 && (x.code === 'feedback_exists' || x.code === 'proposal_drafted')) {
				existing = x.details?.feedback ?? existing;
				if (!existing) error = 'Feedback on this handover was already given.';
			} else error = err instanceof Error ? err.message : 'Could not save the feedback.';
		} finally {
			busy = false;
		}
	}
</script>

<Drawer bind:open title="Give feedback">
	{#if loading}
		<p class="muted">Loading…</p>
	{:else if existing}
		<div class="done" data-testid="feedback-given">
			<p><strong>Feedback given</strong></p>
			<p>{VERDICT[existing.verdict as Verdict] ?? 'Recorded'}{existing.note ? ` — ${existing.note}` : ''}</p>
			<p class="muted">{existing.by_name} · {clockOf(existing.at)}</p>
		</div>
	{:else}
		<form class="fd" aria-label="Give feedback on this handover" onsubmit={submit}>
			<fieldset>
				<legend>Should the agent have handled this?</legend>
				<label class="opt">
					<input type="radio" name="fb-verdict" value="agent_should_handle" bind:group={verdict} />
					<span>Yes, the agent should have handled this</span>
				</label>
				<label class="opt">
					<input type="radio" name="fb-verdict" value="right_to_hand_over" bind:group={verdict} />
					<span>No, it was right to hand over</span>
				</label>
			</fieldset>
			<label class="field">
				<span>Why (optional)</span>
				<textarea rows="3" aria-label="Why (optional)" bind:value={why}></textarea>
			</label>
			{#if error}<p class="err" role="alert">{error}</p>{/if}
			<div class="actions">
				<button class="btn" type="button" onclick={() => (open = false)}>Cancel</button>
				<button class="btn primary" type="submit" disabled={busy}>Send feedback</button>
			</div>
		</form>
	{/if}
	{#if existing && error}<p class="err" role="alert">{error}</p>{/if}
</Drawer>

<style>
	.fd,
	.done {
		display: grid;
		gap: 12px;
	}
	.done p {
		margin: 0;
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
	.field {
		display: grid;
		gap: 4px;
	}
	.muted {
		color: var(--muted);
	}
	.err {
		color: var(--err);
		margin: 0;
	}
	.actions {
		display: flex;
		justify-content: flex-end;
		gap: 8px;
	}
</style>
