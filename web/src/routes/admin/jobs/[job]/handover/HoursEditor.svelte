<script lang="ts">
	// J21 "Hours" (F-14, B4 T16a): the job's business hours, saved with the SLA policy to the draft (D54). `value` is
	// the draft being edited (null = always staffed); `live` is what Production runs. Out of hours a case still
	// enters the queue: its SLA clocks start at the next opening and the customer is told when to expect a reply
	// (a call-back or a reply). A holiday of the tenant calendar (/admin, Calendar) is closed all day when "Use tenant
	// calendar" is on.
	import { WEEKDAYS, type HandoverHours, type Weekday } from '$lib/api/types/journey';
	import {
		MAX_WINDOWS,
		OUT_OF_HOURS_LABEL,
		WEEKDAY_LABEL,
		defaultHours,
		hoursText,
	} from '$lib/journey/J21Hours';

	let {
		value = $bindable(),
		live,
		attrs,
		error,
	}: {
		value: HandoverHours | null;
		live: HandoverHours | null;
		/** The Support manager's write attributes (`writeAttrs`): disabled with the hint for everyone else. */
		attrs: { disabled: boolean; title: string | undefined };
		error: string | null;
	} = $props();

	function setStaffed(always: boolean) {
		value = always ? null : defaultHours(live?.timezone ?? 'Europe/Amsterdam');
	}
	function addWindow(d: Weekday) {
		if (!value) return;
		const last = value.weekly[d].at(-1);
		value.weekly[d].push(last ? { from: last.to, to: '24:00' } : { from: '09:00', to: '17:00' });
	}
	function removeWindow(d: Weekday, i: number) {
		if (value) value.weekly[d].splice(i, 1);
	}
</script>

<div class="j21-hours" data-testid="hours-section">
	<h3>Hours</h3>
	<p class="muted small">
		When people answer handovers. Out of hours a case still enters the queue: its SLA clocks start at the next
		opening and the customer is told when to expect a colleague.
	</p>
	<p class="small" data-testid="hours-live"><b>Live:</b> {hoursText(live)}</p>
	<label class="j21-check">
		<input
			type="checkbox"
			data-testid="hours-always"
			checked={value === null}
			onchange={(e) => setStaffed(e.currentTarget.checked)}
			{...attrs}
		/>
		Always staffed (no business hours)
	</label>
	{#if value}
		<label class="field j21-tz">
			<span class="label">Timezone</span>
			<input
				class="input"
				data-testid="hours-timezone"
				bind:value={value.timezone}
				placeholder="Europe/Amsterdam"
				maxlength="64"
				{...attrs}
			/>
		</label>
		<div class="tbl-wrap">
			<table class="tbl" data-testid="hours-week">
				<thead><tr><th>Day</th><th>Open</th><th></th></tr></thead>
				<tbody>
					{#each WEEKDAYS as d (d)}
						<tr data-testid="hours-day-{d}">
							<td>{WEEKDAY_LABEL[d]}</td>
							<td>
								{#each value.weekly[d] as w, i (i)}
									<span class="j21-win">
										<input
											class="input j21-time"
											aria-label="{WEEKDAY_LABEL[d]} window {i + 1} from"
											bind:value={w.from}
											placeholder="HH:MM"
											maxlength="5"
											{...attrs}
										/>–<input
											class="input j21-time"
											aria-label="{WEEKDAY_LABEL[d]} window {i + 1} to"
											bind:value={w.to}
											placeholder="HH:MM"
											maxlength="5"
											{...attrs}
										/>
										<button
											class="btn sm"
											type="button"
											aria-label="Remove {WEEKDAY_LABEL[d]} window {i + 1}"
											onclick={() => removeWindow(d, i)}
											{...attrs}>×</button
										>
									</span>
								{:else}
									<span class="muted">closed</span>
								{/each}
							</td>
							<td>
								<button
									class="btn sm"
									type="button"
									aria-label="Add a {WEEKDAY_LABEL[d]} window"
									onclick={() => addWindow(d)}
									disabled={attrs.disabled || value.weekly[d].length >= MAX_WINDOWS}
									title={attrs.title}>+ window</button
								>
							</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
		<label class="j21-check">
			<input
				type="checkbox"
				data-testid="hours-calendar"
				bind:checked={value.use_tenant_calendar}
				{...attrs}
			/>
			Use tenant calendar (its holidays are closed all day — <a href="/admin">Calendar</a>)
		</label>
		<label class="field j21-tz">
			<span class="label">Out of hours</span>
			<select class="input" data-testid="hours-out" bind:value={value.out_of_hours} {...attrs}>
				{#each Object.entries(OUT_OF_HOURS_LABEL) as [k, label] (k)}
					<option value={k}>{label}</option>
				{/each}
			</select>
		</label>
	{/if}
	{#if error}<p class="muted small" data-testid="hours-error">{error}</p>{/if}
</div>

<style>
	.j21-hours {
		margin-top: 14px;
		display: grid;
		gap: 8px;
	}
	.j21-hours h3 {
		font-size: 14px;
		margin: 0;
	}
	.j21-check {
		display: flex;
		gap: 6px;
		align-items: center;
		font-size: 13px;
	}
	.j21-tz {
		max-width: 22em;
	}
	.j21-win {
		display: inline-flex;
		gap: 4px;
		align-items: center;
		margin-right: 8px;
		flex-wrap: wrap;
	}
	.j21-time {
		width: 6.5em;
	}
</style>
