<script lang="ts">
	// T2 "Calendar" (F-14, B4 T16a): the tenant's holiday calendar — one for every job. A holiday is closed all day for
	// a job whose business hours use the tenant calendar (J21 Hours). D54/D69: a save is a tenant-wide draft change,
	// recorded in every job's draft; each job's Production gets it when that job publishes (the draft badge names the
	// jobs still on an older calendar). Saving needs the Support manager role; every member reads it.
	import { onMount } from 'svelte';
	import { api, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { canWrite, writeAttrs, writeHint } from '$lib/auth/roles';
	import { Resource } from '$lib/load.svelte';
	import type { TenantCalendar, TenantCalendarView } from '$lib/api/types/tenant';
	import {
		MAX_HOLIDAYS,
		MAX_LABEL,
		calendarError,
		calendarText,
		knownTimezone,
		normalizeCalendar,
	} from '$lib/journey/J21Hours';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';

	const cal = new Resource<TenantCalendarView>(
		() => api().tenantCalendar(),
		() => false,
	);
	onMount(() => void cal.refresh());

	/** The draft being edited; null until the calendar loads. */
	let edit = $state<TenantCalendar | null>(null);
	let saving = $state(false);
	$effect(() => {
		const v = cal.value;
		if (v.state === 'ok' && edit === null)
			edit = structuredClone(
				$state.snapshot(v.data.draft ?? v.data.live) ?? { timezone: 'Europe/Amsterdam', holidays: [] },
			);
	});
	const manager = $derived(canWrite(session, 'support_manager'));
	const attrs = $derived(writeAttrs(session, 'support_manager'));
	const error = $derived(
		edit ? (calendarError(edit) ?? (knownTimezone(edit.timezone.trim()) ? null : 'unknown timezone')) : null,
	);

	function addHoliday() {
		if (!edit || edit.holidays.length >= MAX_HOLIDAYS) return;
		edit.holidays.push({ date: '', label: '', closed: true });
	}
	async function save() {
		if (!edit || error) return;
		const body = normalizeCalendar($state.snapshot(edit) as TenantCalendar);
		saving = true;
		const ok = await guard(
			() => api().saveTenantCalendar(body),
			'Calendar saved to the draft of every job. Publish each job to make it live.',
		);
		saving = false;
		if (ok) {
			edit = body;
			await cal.refresh();
		}
	}
</script>

<Card
	title="Calendar"
	sub="Holidays every job is closed on. A job uses them when its business hours say so (Handover, Hours)."
>
	{#snippet actions()}
		{#if cal.value.state === 'ok' && cal.value.data.drafted.length}
			<span data-testid="calendar-drafted"
				><Chip
					text="draft · {cal.value.data.drafted.length} job{cal.value.data.drafted.length === 1
						? ''
						: 's'} to publish"
					variant="warn"
				/></span
			>
		{/if}
	{/snippet}
	<LoadState resource={cal}>
		{#snippet children(v)}
			<div data-testid="tenant-calendar">
				<p class="small" data-testid="calendar-live"><b>Live:</b> {calendarText(v.live)}</p>
				{#if v.drafted.length}
					<p class="muted small">Unpublished in: {v.drafted.join(', ')}.</p>
				{/if}
				{#if edit}
					<label class="field cal-tz">
						<span class="label">Timezone</span>
						<input
							class="input"
							data-testid="calendar-timezone"
							bind:value={edit.timezone}
							maxlength="64"
							{...attrs}
						/>
					</label>
					{#if edit.holidays.length}
						<ul class="list-plain cal-days" data-testid="calendar-holidays">
							{#each edit.holidays as h, i (i)}
								<li>
									<input
										class="input cal-date"
										type="date"
										aria-label="Holiday {i + 1} date"
										bind:value={h.date}
										{...attrs}
									/>
									<input
										class="input"
										aria-label="Holiday {i + 1} name"
										placeholder="e.g. King's Day"
										maxlength={MAX_LABEL}
										bind:value={h.label}
										{...attrs}
									/>
									<button
										class="btn sm"
										type="button"
										aria-label="Remove holiday {i + 1}"
										onclick={() => edit?.holidays.splice(i, 1)}
										{...attrs}>×</button
									>
								</li>
							{/each}
						</ul>
					{:else}
						<p class="muted small">No holidays.</p>
					{/if}
					<div class="row">
						<button class="btn sm" type="button" data-testid="calendar-add" onclick={addHoliday} {...attrs}
							>+ Holiday</button
						>
						<button
							class="btn primary sm"
							type="button"
							data-testid="calendar-save"
							disabled={!manager || !!error || saving}
							title={!manager ? writeHint('support_manager') : (error ?? undefined)}
							onclick={save}>Save to draft</button
						>
						{#if error && manager}<span class="muted small" data-testid="calendar-error">{error}</span>{/if}
					</div>
				{/if}
			</div>
		{/snippet}
	</LoadState>
</Card>

<style>
	.cal-tz {
		max-width: 22em;
	}
	.cal-days {
		display: grid;
		gap: 6px;
		margin: 8px 0;
	}
	.cal-days li {
		display: grid;
		grid-template-columns: 11em minmax(0, 1fr) auto;
		gap: 6px;
		align-items: center;
	}
	.cal-date {
		width: 100%;
	}
</style>
