<script lang="ts">
	// The tenant's own text for the prompt slots a job designer may refine (KH T4, D108): tone, escalation, format.
	// `PUT /api/jobs/{job}/instructions/{slot}` `{text}` saves to the draft; "Back to inherited" is
	// `POST /api/jobs/{job}/refinements/revert` `{object: 'instruction', key}`. The inherited text is the base layer
	// of `GET /api/jobs/{job}/instruction` (the release's folded slot); the job's own text is the tenant layer.
	import { onMount } from 'svelte';
	import { api } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { writeAttrs } from '$lib/auth/roles';
	import { ApiError } from '$lib/api/errors';
	import { Resource } from '$lib/load.svelte';
	import {
		INSTRUCTION_SLOTS,
		MAX_INSTRUCTION_CHARS,
		type InstructionSlot,
		type OverlayLayers,
	} from '$lib/api/types/release';
	import { toast } from '$lib/ui/toaster.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';

	let { onchange }: { onchange?: () => void } = $props();

	const LABEL: Record<InstructionSlot, [string, string]> = {
		tone: ['Tone', 'How the agent sounds to your customers.'],
		escalation: ['Escalation', 'When and how the agent hands over to a person.'],
		format: ['Format', 'How replies are laid out: length, lists, links.'],
	};

	const layers = new Resource<OverlayLayers>(
		() => api().refinementLayers('instruction'),
		() => false,
	);
	onMount(() => void layers.refresh());

	const attrs = $derived(writeAttrs(session, 'job_designer'));
	const textOf = (v: unknown) => (typeof v === 'string' ? v : '');

	/** Unsaved edits per slot; a slot absent here shows the saved text. */
	let edits = $state<Record<string, string>>({});
	let busy = $state<string | null>(null);
	let errors = $state<Record<string, string>>({});

	function row(l: OverlayLayers, slot: string) {
		const own = l.tenant.find((t) => t.key === slot) ?? null;
		const base = l.base.find((b) => b.key === slot) ?? null;
		return {
			own,
			base,
			hidden: own?.kind === 'tombstone',
			saved: own?.kind === 'tombstone' ? '' : textOf(own?.item?.text),
		};
	}
	const value = (l: OverlayLayers, slot: string) => edits[slot] ?? row(l, slot).saved;
	const problem = (text: string) =>
		!text.trim()
			? 'Write the instruction, or use Back to inherited.'
			: text.length > MAX_INSTRUCTION_CHARS
				? `At most ${MAX_INSTRUCTION_CHARS} characters.`
				: '';

	async function run(slot: string, act: () => Promise<unknown>, ok: string) {
		if (busy) return;
		busy = slot;
		errors[slot] = '';
		try {
			await act();
			toast(ok);
			delete edits[slot];
			await layers.refresh();
			onchange?.();
		} catch (e) {
			errors[slot] = e instanceof ApiError ? e.message : 'The change could not be saved.';
		} finally {
			busy = null;
		}
	}
	const save = (slot: string, text: string) =>
		run(
			slot,
			() => api().saveInstruction(slot, { text }),
			`Saved your ${slot} instruction to draft — live when a release publishes it.`,
		);
	const revert = (slot: string) =>
		run(
			slot,
			() => api().refine('revert', 'instruction', slot),
			`The ${slot} instruction is back to the inherited text.`,
		);
</script>

<section class="stack" data-testid="instruction-editor" aria-labelledby="ins-h">
	<h3 id="ins-h">Instructions</h3>
	<p class="muted">
		Your own wording for the agent's tone, escalation and reply format. It replaces the inherited text for
		this job once a release publishes it (saved to draft first).
	</p>
	<LoadState resource={layers} empty="No instructions.">
		{#snippet children(l)}
			{#each INSTRUCTION_SLOTS as slot (slot)}
				{@const r = row(l, slot)}
				{@const text = value(l, slot)}
				{@const bad = problem(text)}
				<form
					class="ins"
					novalidate
					data-testid="instruction-{slot}"
					onsubmit={(e) => {
						e.preventDefault();
						if (!bad && text.trim() !== r.saved.trim()) save(slot, text);
					}}
				>
					<label for="ins-{slot}"><b>{LABEL[slot][0]}</b></label>
					<span class="muted small">{LABEL[slot][1]}</span>
					{#if r.hidden}
						<p class="muted">Hidden for this job. Unhide it in the refinements first.</p>
					{:else}
						<textarea
							id="ins-{slot}"
							class="input"
							rows="4"
							value={text}
							oninput={(e) => (edits[slot] = e.currentTarget.value)}
							disabled={attrs.disabled || busy !== null}
							title={attrs.title}
							placeholder={r.base ? 'Using the inherited text' : 'Nothing inherited — write your own'}
						></textarea>
						<div class="muted small" data-testid="instruction-{slot}-inherited">
							{#if r.base}
								<b>Inherited:</b> <span class="inh">{textOf(r.base.text)}</span>
							{:else}
								Nothing inherited for this slot.
							{/if}
						</div>
						{#if errors[slot]}
							<div class="callout err" role="alert">
								<strong>Not saved</strong><span>{errors[slot]}</span>
							</div>
						{/if}
						<div class="row">
							<button
								type="submit"
								class="btn primary"
								disabled={attrs.disabled || busy !== null || !!bad || text.trim() === r.saved.trim()}
								title={attrs.title}>{busy === slot ? 'Saving…' : 'Save'}</button
							>
							{#if r.own}
								<button
									type="button"
									class="btn"
									disabled={attrs.disabled || busy !== null}
									title={attrs.title}
									onclick={() => revert(slot)}>{r.base ? 'Back to inherited' : 'Remove my text'}</button
								>
							{/if}
						</div>
					{/if}
				</form>
			{/each}
		{/snippet}
	</LoadState>
</section>

<style>
	.ins {
		display: grid;
		gap: 6px;
		min-width: 0;
		padding: 10px 0;
		border-top: 1px solid var(--line);
	}
	.ins textarea {
		width: 100%;
		max-width: 100%;
		box-sizing: border-box;
		resize: vertical;
	}
	.inh {
		white-space: pre-wrap;
		overflow-wrap: anywhere;
	}
	.small {
		font-size: 12px;
	}
	.row {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
	}
</style>
