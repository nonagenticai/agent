<script lang="ts">
	// Assign… (Support manager): give the conversation to an active Support agent of the tenant. The list is the
	// handover queue's assignees (a super_admin who is not a Support agent member is never offered).
	import Drawer from '$lib/ui/Drawer.svelte';
	import { api } from '$lib/app.svelte';
	import type { HandoverAssignee } from '$lib/api/types/journey';

	let {
		open = $bindable(false),
		job,
		current,
		onSubmit,
	}: {
		open?: boolean;
		job: string;
		/** Sub of whoever has it now (left out of the list). */
		current: string | null;
		onSubmit: (sub: string) => Promise<void>;
	} = $props();

	let people = $state<HandoverAssignee[]>([]);
	let pick = $state('');
	let busy = $state(false);
	let error = $state<string | null>(null);

	$effect(() => {
		if (!open) return;
		pick = '';
		error = null;
		api()
			.jobHandovers(job)
			.then((q) => (people = q.assignees.filter((a) => a.sub !== current)))
			.catch((e) => (error = e instanceof Error ? e.message : 'Could not load the team.'));
	});

	async function submit(e: SubmitEvent) {
		e.preventDefault();
		if (!pick) {
			error = 'Choose who should have it';
			return;
		}
		busy = true;
		error = null;
		try {
			await onSubmit(pick);
			open = false;
		} catch (err) {
			error = err instanceof Error ? err.message : 'Could not assign.';
		} finally {
			busy = false;
		}
	}
</script>

<Drawer bind:open title="Assign">
	<form class="ad" aria-label="Assign this conversation" onsubmit={submit}>
		<label class="field">
			<span>Give it to</span>
			<select bind:value={pick} aria-label="Support agent">
				<option value="">Choose a Support agent…</option>
				{#each people as p (p.sub)}<option value={p.sub}>{p.name}</option>{/each}
			</select>
		</label>
		<p class="muted">They become the owner and can reply at once.</p>
		{#if error}<p class="err" role="alert">{error}</p>{/if}
		<div class="actions">
			<button class="btn" type="button" onclick={() => (open = false)}>Cancel</button>
			<button class="btn primary" type="submit" disabled={busy}>Assign</button>
		</div>
	</form>
</Drawer>

<style>
	.ad {
		display: grid;
		gap: 12px;
	}
	.field {
		display: grid;
		gap: 4px;
	}
	.muted {
		color: var(--muted);
		margin: 0;
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
