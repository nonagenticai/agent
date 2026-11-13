<script lang="ts">
	// "Routing" (SW routing): how queued handovers are handed out. Off, or Round-robin to the staff who are online on
	// /support, with how many minutes an auto-assigned item may sit untouched (while its assignee is away) before it
	// moves on to the next person. Everyone on the workspace reads it; the tenant admin and the Support manager change
	// it (the control stays visible, disabled with the reason, for the others).
	import { api } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { writeHint } from '$lib/auth/roles';
	import type { RoutingMode, SupportRouting } from '$lib/api/types/support';

	let cur = $state<SupportRouting | null>(null);
	let open = $state(false);
	let mode = $state<RoutingMode>('round_robin');
	let minutes = $state(5);
	let busy = $state(false);
	let error = $state<string | null>(null);

	const canEdit = $derived(session.member && (session.has('tenant_admin') || session.has('support_manager')));
	// SW-7: the platform flag. Off: nothing is auto-assigned, whatever the saved mode says.
	const summary = $derived(cur ? (!cur.enabled || cur.mode === 'off' ? 'Off' : 'Round-robin') : '…');
	const valid = $derived(Number.isInteger(minutes) && minutes >= 1 && minutes <= 1440);
	const dirty = $derived(
		!!cur && (mode !== cur.mode || (mode === 'round_robin' && minutes * 60 !== cur.reassign_after_secs)),
	);

	function adopt(r: SupportRouting) {
		cur = r;
		mode = r.mode;
		minutes = Math.round(r.reassign_after_secs / 60);
	}

	$effect(() => {
		let gone = false;
		api()
			.supportRouting()
			.then((r) => {
				if (!gone) adopt(r);
			})
			.catch((e) => {
				if (!gone) error = e instanceof Error ? e.message : 'Could not load the routing setting.';
			});
		return () => (gone = true);
	});

	async function save() {
		if (!canEdit || !valid || busy) return;
		busy = true;
		error = null;
		try {
			adopt(await api().setSupportRouting({ mode, reassign_after_secs: minutes * 60 }));
			open = false;
		} catch (e) {
			error = e instanceof Error ? e.message : 'Could not save the routing setting.';
		} finally {
			busy = false;
		}
	}
</script>

<div class="rc">
	<button
		class="btn sm"
		type="button"
		aria-expanded={open}
		aria-haspopup="true"
		onclick={() => (open = !open)}
	>
		Routing · {summary}
	</button>
	{#if open}
		<div class="pop" role="group" aria-label="Routing">
			{#if cur && !cur.enabled}
				<p class="note" data-testid="routing-flag-off">
					Automatic assignment is not switched on for this workspace, so handovers wait in the queue until
					someone takes them. The platform team switches it on. The setting below applies once it is.
				</p>
			{/if}
			<fieldset title={canEdit ? undefined : writeHint('support_manager')}>
				<legend>New handovers</legend>
				<label class="opt"
					><input
						type="radio"
						name="routing-mode"
						value="off"
						bind:group={mode}
						disabled={!canEdit || busy}
					/> Off — wait in the queue</label
				>
				<label class="opt"
					><input
						type="radio"
						name="routing-mode"
						value="round_robin"
						bind:group={mode}
						disabled={!canEdit || busy}
					/> Round-robin — to whoever is online</label
				>
				<label class="mins"
					>Reassign after
					<input
						type="number"
						min="1"
						max="1440"
						step="1"
						aria-label="Reassign after minutes"
						bind:value={minutes}
						disabled={!canEdit || busy || mode !== 'round_robin'}
					/>
					min</label
				>
				<p class="note">
					Only people on the Support workspace count as online. An item nobody opened is moved on when its
					assignee goes away. Something you took or assigned yourself never moves.
				</p>
			</fieldset>
			{#if !canEdit}<p class="note">{writeHint('support_manager')}</p>{/if}
			{#if error}<p class="err" role="alert">{error}</p>{/if}
			<div class="row">
				<button class="btn sm" type="button" onclick={() => (open = false)}>Close</button>
				<button
					class="btn sm primary"
					type="button"
					disabled={!canEdit || !valid || !dirty || busy}
					onclick={save}>Save</button
				>
			</div>
		</div>
	{/if}
</div>

<style>
	.rc {
		position: relative;
	}
	.pop {
		position: absolute;
		left: 0;
		top: calc(100% + 4px);
		z-index: 10;
		display: grid;
		gap: 8px;
		min-width: 260px;
		padding: 10px;
		border: 1px solid var(--line);
		border-radius: 8px;
		background: var(--panel);
		box-shadow: 0 6px 24px rgb(0 0 0 / 0.14);
	}
	fieldset {
		display: grid;
		gap: 6px;
		margin: 0;
		padding: 0;
		border: 0;
	}
	legend {
		font-size: 12px;
		color: var(--muted);
		padding: 0;
		margin-bottom: 4px;
	}
	.opt,
	.mins {
		display: flex;
		align-items: center;
		gap: 6px;
		font-size: 13px;
	}
	.mins input {
		width: 64px;
	}
	.note {
		margin: 0;
		font-size: 12px;
		color: var(--muted);
	}
	.err {
		margin: 0;
		font-size: 12px;
		color: var(--danger, #b00020);
	}
	.row {
		display: flex;
		justify-content: flex-end;
		gap: 6px;
	}
</style>
