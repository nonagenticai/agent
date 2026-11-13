<script lang="ts">
	// J21 Handover queue (D64). One queue per job: a case enters when the agent hands over (one of ten reasons) and
	// waits for a person. This page is the overview: the queue with its SLA state, the metrics, and the job's SLA
	// policy and business hours. Working a case (take, assign, reply, resolve, feedback) happens in the staff
	// workspace: a row opens `/support/{job}/c/{conversation_id}`. The SLA policy is job configuration, so it follows
	// D54: saved to the draft, tried in the test chat, live with the next release. Support manager role; without it the
	// editor shows disabled with the note. Times are local (`stamp`); the zone is named once, in the header.
	import { onDestroy, onMount } from 'svelte';
	import { api, app, guard } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import { toast } from '$lib/ui/toaster.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { writeHint, canWrite, writeAttrs, canEditStaffTeam } from '$lib/auth/roles';
	import { ApiError } from '$lib/api/errors';
	import type { JobRoutingMode, JobStaff } from '$lib/api/types/tenant';
	import { BINDING_KIND_LABEL } from '$lib/api/types/conversations';
	import type {
		HandoverItem,
		HandoverPriority,
		HandoverReason,
		HandoverHours,
		HandoverSlaTargets,
	} from '$lib/api/types/journey';
	import { hoursError, knownTimezone, normalizeHours } from '$lib/journey/J21Hours';
	import HoursEditor from './HoursEditor.svelte';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LiveNote from '$lib/release/LiveNote.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import Kpi from '$lib/ui/Kpi.svelte';
	import DraftBar from '$lib/release/DraftBar.svelte';
	import { clock } from '$lib/conversations/labels';
	import { zoneLabel } from '$lib/support/time';
	import { WORKSPACE_HOME, workspaceHref } from '$lib/support/links';
	import { watchJob } from '$lib/conversations/feed';
	import { customerLabel } from '$lib/api/types/conversations';
	import {
		PRIORITIES,
		PROPOSAL_KIND,
		REASON,
		REASONS,
		STATE,
		STATES,
		activeClock,
		clockText,
		dur,
		filterQueue,
		isClosed,
		policyError,
		queueOrder,
		slaOf,
		summarize,
		type BreachFilter,
		type StateFilter,
	} from '$lib/journey/J21Handover';

	const res = new Resource(() => api().handoverQueue({}));
	const pol = new Resource(
		() => api().handoverPolicy(),
		() => false,
	);
	onMount(() => {
		res.refresh();
		pol.refresh();
		staff.refresh();
		// the worker-side bindings are the integrations manager's to read (GET channel-bindings is
		// JobRequires<IntegrationsManager>): anyone else sees the staff team without them, and asks nothing it would be refused
		if (mayReadBindings) bindings.refresh();
		// A `handover` frame (an item queued, taken, assigned or closed) and the customer's rating (`csat`) re-read the
		// queue; frames of one change arrive together, so one re-read per burst.
		return watchJob((o) => api().jobEvents(o), {
			onFrame: (f) => {
				if (f.event !== 'handover' && f.event !== 'csat' && f.event !== 'owner') return;
				if (queueSoon) clearTimeout(queueSoon);
				queueSoon = setTimeout(() => {
					queueSoon = null;
					void res.refresh();
				}, 250);
			},
		});
	});
	let queueSoon: ReturnType<typeof setTimeout> | null = null;
	onDestroy(() => {
		if (queueSoon) clearTimeout(queueSoon);
	});
	/** §R.1 Support manager: the handover/SLA policy. */
	const manager = $derived(canWrite(session, 'support_manager'));

	// D125 Staff team: who works this job's handovers. Empty = every conversation worker of the company. A
	// tenant_admin or a support_manager edits it; it is live setting (not draft-gated): `PUT /api/jobs/{job}/staff`.
	const mayEditStaff = $derived(canEditStaffTeam(session));
	const mayReadBindings = $derived(canWrite(session, 'integrations_manager'));
	// D126.3: the staff side is one section: this team plus the job's worker-side channel bindings
	const bindings = new Resource(
		() => api().channelBindings(),
		() => false,
	);
	const workerBindings = $derived(
		bindings.value.state === 'ok' ? bindings.value.data.items.filter((b) => b.side === 'worker') : [],
	);
	const routedExternally = $derived(workerBindings.some((b) => b.enabled));
	const staff = new Resource<JobStaff>(
		() => api().jobStaff(app.job),
		() => false,
	);
	let members = $state<string[]>([]);
	let mode = $state<JobRoutingMode>('inherit');
	let secs = $state(300);
	let staffBusy = $state(false);
	let staffErr = $state<{ code: string; message: string } | null>(null);
	let adding = $state('');
	let staffLoaded: JobStaff | null = null;
	$effect(() => {
		if (staff.value.state === 'ok' && staff.value.data !== staffLoaded) {
			staffLoaded = staff.value.data;
			members = staffLoaded.members.map((m) => m.sub);
			mode = staffLoaded.routing.mode;
			secs = staffLoaded.routing.reassign_after_secs ?? 300;
		}
	});
	const MODE_LABEL: Record<JobRoutingMode, string> = {
		inherit: 'Follow the company setting',
		off: 'Off for this job (nobody is assigned automatically)',
		round_robin: 'Round-robin across the team',
	};
	const candidates = $derived(staff.value.state === 'ok' ? staff.value.data.candidates : []);
	const nameOf = (sub: string) => {
		const c = candidates.find((x) => x.sub === sub);
		return c?.name ?? c?.email ?? sub;
	};
	const secsOk = $derived(Number.isInteger(secs) && secs >= 60 && secs <= 86_400);
	const pickable = $derived(candidates.filter((c) => !members.includes(c.sub)));
	async function saveStaff() {
		if (staffBusy || (mode === 'round_robin' && !secsOk)) return;
		staffBusy = true;
		staffErr = null;
		try {
			await api().saveJobStaff(app.job, {
				members: [...members],
				routing: { mode, reassign_after_secs: mode === 'round_robin' ? secs : null },
			});
			toast('Staff team saved (audited).');
			staffLoaded = null;
			await staff.refresh();
		} catch (e) {
			staffErr =
				e instanceof ApiError ? { code: e.code, message: e.message } : { code: 'error', message: String(e) };
		} finally {
			staffBusy = false;
		}
	}
	const me = $derived(session.user?.sub ?? null);
	const job = $derived(`/admin/jobs/${app.job}`);
	/** A queue row opens its conversation in the workspace (the inbox when the row names none). */
	const rowHref = (h: HandoverItem) =>
		h.conversation_id ? workspaceHref(app.job, h.conversation_id) : WORKSPACE_HOME;

	// ---- filters ----
	let fState = $state<StateFilter>('open');
	let fAssignee = $state('');
	let fBreach = $state<BreachFilter>('any');
	let fReason = $state<HandoverReason | null>(null);
	let draftTick = $state(0);

	const PRIORITY: Record<HandoverPriority, string> = {
		urgent: 'err',
		high: 'err',
		normal: 'muted',
		low: 'muted',
	};

	// ---- SLA policy (draft) ----
	let edit = $state<HandoverSlaTargets | null>(null);
	const blank = (): HandoverSlaTargets => ({
		high: { first_response_min: 60, resolve_min: 240 },
		normal: { first_response_min: 120, resolve_min: 480 },
		low: { first_response_min: 240, resolve_min: 1440 },
	});
	/** F-14: the draft business hours being edited (null = always staffed; undefined until the policy loads). */
	let editHours = $state<HandoverHours | null | undefined>(undefined);
	/** The hours as loaded or last saved: unchanged hours are left out of the save (the API keeps them). */
	let hoursBase = '';
	$effect(() => {
		const v = pol.value;
		if (v.state === 'ok' && edit === null) edit = structuredClone($state.snapshot(v.data.draft) ?? blank());
		if (v.state === 'ok' && editHours === undefined) {
			editHours = structuredClone($state.snapshot(v.data.hours.draft));
			hoursBase = JSON.stringify(editHours);
		}
	});
	const editError = $derived(edit ? policyError(edit) : null);
	const hoursErr = $derived(
		editHours
			? (hoursError(editHours) ??
					(knownTimezone(editHours.timezone.trim()) ? null : 'hours: unknown timezone'))
			: null,
	);
	async function savePolicy() {
		if (!edit) return;
		const t = $state.snapshot(edit) as HandoverSlaTargets;
		const h = (editHours === undefined ? null : $state.snapshot(editHours)) as HandoverHours | null;
		const changed = editHours !== undefined && JSON.stringify(h) !== hoursBase;
		const hours = changed ? (h ? normalizeHours(h) : null) : undefined;
		if (await guard(() => api().saveHandoverPolicy(t, hours), 'Saved to draft. Test it, then publish.')) {
			if (changed) {
				editHours = hours ? structuredClone(hours) : null;
				hoursBase = JSON.stringify(editHours);
			}
			await pol.refresh();
			draftTick++;
		}
	}
	const POLICY_REASON =
		'The SLA policy is job configuration: a Support manager saves it to the draft; it is tested and published with a release (D54).';
</script>

<PageHead
	id="J21"
	sub="When the agent stops, a person takes over with everything the agent already did. This is the queue and its SLA; a row opens the conversation in the workspace."
>
	{#snippet actions()}
		<a class="btn primary" href={WORKSPACE_HOME} data-testid="workspace-link">Open the workspace</a>
		<a class="btn" href="{job}/conversations" data-testid="conversations-link">Conversations</a>
		<a class="btn" href="{job}/channels" data-testid="channels-link">Channel bindings</a>
		<a class="btn" href="{job}/cases">All cases</a>
	{/snippet}
</PageHead>

<LiveNote what="The queue shows live cases. The SLA policy below goes through the draft." />

<Callout kind="info" title="How the queue works (D64)">
	<span data-testid="d64-note">
		The agent hands a case over (<code>case.create{'{'}handover{'}'}</code> → <code>agent_cases</code>, K49)
		for one of ten reasons. It waits <b>queued</b> until a colleague takes it, is <b>assigned</b> to one
		person, may wait on the customer, and closes as <b>resolved</b> or <b>returned to the agent</b>. Every
		step records who and when. Work a case in the workspace; the SLA policy below is job configuration and
		goes through draft → test → release. Times are shown in {zoneLabel()}.
	</span>
</Callout>

<LoadState resource={res} empty="No handovers yet. When the agent hands a case to a person, it lands here.">
	{#snippet children(page)}
		{@const all = page.items}
		{@const now = page.now}
		{@const s = summarize(all, now)}
		{@const rows = queueOrder(
			filterQueue(all, { state: fState, assignee: fAssignee, breach: fBreach, reason: fReason }, now),
			now,
		)}
		{@const flagged = all.filter((h) => h.feedback?.verdict === 'agent_should_handle')}

		<div class="kpis j21-kpis">
			<Kpi label="Open" value={String(s.open)} note="not resolved or returned" />
			<Kpi label="Queued" value={String(s.queued)} note="waiting for someone to take it" />
			<Kpi label="SLA breached" value={String(s.breached)} note="open items past a due time" />
			<Kpi label="At risk" value={String(s.at_risk)} note="less than a quarter of the time left" />
			<Kpi
				label="Longest wait"
				value={s.oldest == null ? '—' : dur(s.oldest)}
				note="oldest open item, since it was queued"
			/>
		</div>

		<div class="j21-bar" data-testid="queue-bar">
			<label class="j21-f"
				>State
				<select class="input" bind:value={fState} data-testid="filter-state">
					<option value="open">Open · {s.open}</option>
					{#each STATES as st (st)}<option value={st}>{STATE[st].label}</option>{/each}
					<option value="all">All</option>
				</select>
			</label>
			<label class="j21-f"
				>Assignee
				<select class="input" bind:value={fAssignee} data-testid="filter-assignee">
					<option value="">Anyone</option>
					<option value="none">Unassigned</option>
					{#if me}<option value={me}>Me</option>{/if}
					{#each page.assignees.filter((a) => a.sub !== me) as a (a.sub)}
						<option value={a.sub}>{a.name}</option>
					{/each}
				</select>
			</label>
			<label class="j21-f"
				>SLA
				<select class="input" bind:value={fBreach} data-testid="filter-breach">
					<option value="any">Any</option>
					<option value="breached">Breached · {s.breached}</option>
					<option value="at_risk">At risk · {s.at_risk}</option>
				</select>
			</label>
			<label class="j21-f"
				>Reason
				<select
					class="input"
					data-testid="filter-reason"
					value={fReason ?? ''}
					onchange={(e) => (fReason = (e.currentTarget.value || null) as HandoverReason | null)}
				>
					<option value="">Any</option>
					{#each REASONS as r (r)}<option value={r}>{REASON[r].label} · {s.byReason[r].open} open</option
						>{/each}
				</select>
			</label>
		</div>

		<Card cls="flush">
			<div class="tbl-wrap">
				<table class="tbl clickable" data-testid="queue">
					<thead>
						<tr><th>Case</th><th>Why</th><th>Priority</th><th>State · assignee</th><th>Due</th></tr>
					</thead>
					<tbody>
						{#each rows as h (h.id)}
							{@const sla = slaOf(h, now)}
							{@const c = clockText(activeClock(h, now))}
							<tr class:j21-over={sla.breached && !isClosed(h.state)} data-testid="row-{h.id}">
								<td
									><a
										class="table-action mono"
										href={rowHref(h)}
										data-testid="open-{h.id}"
										title="Open in the workspace">{h.id}</a
									><span class="cell-title j21-subj">{h.subject}</span><span class="cell-sub"
										>{customerLabel(h)} · {h.queue}</span
									></td
								>
								<td><span class="chip {REASON[h.reason].chip}">{REASON[h.reason].label}</span></td>
								<td><span class="chip {PRIORITY[h.priority]}">{h.priority}</span></td>
								<td
									><span class="chip {STATE[h.state].chip}">{STATE[h.state].label}</span><span
										class="cell-sub">{h.assignee_name ?? 'unassigned'}</span
									></td
								>
								<td>
									<span class="chip {c.variant}" data-testid="sla-{h.id}">{c.text}</span>
									{#if sla.breached}<span class="chip err" data-testid="breach-{h.id}">SLA breached</span
										>{/if}
									<span class="cell-sub"
										>{h.first_response_at || isClosed(h.state) ? 'resolve' : 'first response'}
										{activeClock(h, now).due ? `by ${clock(activeClock(h, now).due!)}` : ''}</span
									>
								</td>
							</tr>
						{:else}
							<tr
								><td colspan="5" class="muted" data-testid="queue-empty">No cases match these filters.</td
								></tr
							>
						{/each}
					</tbody>
				</table>
			</div>
		</Card>

		{#if flagged.length}
			<section class="j21-feedback" data-testid="feedback-loop">
				<h2>Feedback into the design</h2>
				<p class="muted">
					Cases a person marked “the agent should have handled this” in the workspace. Each proposal is made
					on its own page.
				</p>
				<ul class="j21-props">
					{#each flagged as h (h.id)}
						{@const p = h.feedback!.proposal}
						<li>
							<a class="table-action mono" href={rowHref(h)}>{h.id}</a>
							<span class="j21-p-body">
								<b>{p?.label ?? 'No proposal'}</b>
								<span class="muted">{p ? PROPOSAL_KIND[p.kind] : ''} · flagged by {h.feedback!.by_name}</span>
							</span>
							{#if h.feedback!.drafted_at}<span class="chip accent">saved to draft</span>
							{:else if p}<a class="btn sm" href={p.href}>Open page</a>{/if}
						</li>
					{/each}
				</ul>
			</section>
		{/if}
	{/snippet}
</LoadState>

<section class="j21-policy" data-testid="sla-policy">
	<h2>SLA policy</h2>
	<p class="muted">
		Minutes from the moment a case is queued. Live cases keep the due times they were given when they entered
		the queue; a new policy applies to cases queued after it is published.
	</p>
	<DraftBar page="J21" tick={draftTick} />
	<LoadState resource={pol}>
		{#snippet children(p)}
			{#if !manager}
				<p class="muted small" data-testid="policy-disabled-reason">{POLICY_REASON}</p>
			{/if}
			{#if p.drafted}<span class="chip warn" data-testid="policy-drafted">draft differs from live</span>{/if}
			<div class="tbl-wrap">
				<table class="tbl" data-testid="policy-table">
					<thead>
						<tr
							><th>Priority</th><th>Live · first response</th><th>Live · resolve</th><th
								>Draft · first response (min)</th
							><th>Draft · resolve (min)</th></tr
						>
					</thead>
					<tbody>
						{#each PRIORITIES as pr (pr)}
							<tr>
								<td><span class="chip {PRIORITY[pr]}">{pr}</span></td>
								<td>{p.live ? dur(p.live[pr].first_response_min) : '—'}</td>
								<td>{p.live ? dur(p.live[pr].resolve_min) : '—'}</td>
								{#if edit}
									<td
										><input
											class="input j21-num"
											type="number"
											min="1"
											aria-label="Draft first response minutes, {pr}"
											bind:value={edit[pr].first_response_min}
											{...writeAttrs(session, 'support_manager')}
										/></td
									>
									<td
										><input
											class="input j21-num"
											type="number"
											min="1"
											aria-label="Draft resolve minutes, {pr}"
											bind:value={edit[pr].resolve_min}
											{...writeAttrs(session, 'support_manager')}
										/></td
									>
								{/if}
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
			{#if !p.live}<p class="muted">No live policy: queued cases carry no due times.</p>{/if}
			{#if editHours !== undefined}
				<HoursEditor
					bind:value={editHours}
					live={p.hours.live}
					attrs={writeAttrs(session, 'support_manager')}
					error={manager ? hoursErr : null}
				/>
			{/if}
			<div class="row">
				<button
					class="btn primary"
					type="button"
					data-testid="save-policy"
					disabled={!manager || !!editError || !!hoursErr}
					title={!manager ? writeHint('support_manager') : (editError ?? hoursErr ?? undefined)}
					onclick={savePolicy}>Save policy to draft</button
				>
				{#if editError && manager}<span class="muted small">{editError}</span>{/if}
				{#if p.drafted}<a class="btn" href="/chat">Test in test chat</a>
					<a class="btn" href="{job}/release">Review &amp; publish</a>{/if}
			</div>
		{/snippet}
	</LoadState>
</section>

<section class="j21-staff" data-testid="staff-team">
	<h2>Staff side</h2>
	<p class="muted" data-testid="staff-side-note">
		An enabled staff channel routes this job's handovers to that tool instead of the internal team.
	</p>
	<h3>Staff team</h3>
	<p class="muted">
		Who works this job's handovers. Support agents only see and take the conversations of jobs whose team is
		empty or names them; support managers see every job.
	</p>
	<LoadState resource={staff}>
		{#snippet children()}
			{#if !mayEditStaff}
				<p class="muted small" data-testid="staff-disabled-reason">
					{writeHint('tenant_admin')} or {writeHint('support_manager').replace('Needs the ', '')} to change the
					team.
				</p>
			{/if}
			{#if members.length === 0}
				<p data-testid="staff-empty">All support agents of the company handle this job</p>
			{:else}
				<ul class="j21-team" data-testid="staff-members">
					{#each members as m (m)}
						<li>
							<span>{nameOf(m)}</span>
							<button
								class="btn sm"
								type="button"
								disabled={!mayEditStaff}
								aria-label={`Remove ${nameOf(m)}`}
								onclick={() => (members = members.filter((x) => x !== m))}>Remove</button
							>
						</li>
					{/each}
				</ul>
			{/if}
			<div class="row">
				<select
					class="input"
					aria-label="Add a team member"
					data-testid="staff-add"
					bind:value={adding}
					disabled={!mayEditStaff || pickable.length === 0}
				>
					<option value="">Add a team member…</option>
					{#each pickable as c (c.sub)}<option value={c.sub}>{c.name ?? c.email ?? c.sub}</option>{/each}
				</select>
				<button
					class="btn"
					type="button"
					disabled={!mayEditStaff || !adding}
					onclick={() => {
						members = [...members, adding];
						adding = '';
					}}>Add</button
				>
			</div>
			<div class="row">
				<label class="j21-f"
					>Routing for this job
					<select class="input" data-testid="staff-mode" bind:value={mode} disabled={!mayEditStaff}>
						{#each Object.entries(MODE_LABEL) as [k, label] (k)}<option value={k}>{label}</option>{/each}
					</select></label
				>
				{#if mode === 'round_robin'}
					<label class="j21-f"
						>Reassign after (seconds, 60 to 86400)
						<input
							class="input j21-num"
							type="number"
							min="60"
							max="86400"
							data-testid="staff-secs"
							bind:value={secs}
							disabled={!mayEditStaff}
						/></label
					>
				{/if}
			</div>
			{#if mode === 'round_robin' && !secsOk}
				<p class="muted small" data-testid="staff-secs-error">Enter 60 to 86400 seconds.</p>
			{/if}
			{#if staffErr}
				<Callout kind="err" title="Team not saved"
					><span data-testid="staff-error"
						><span class="mono">{staffErr.code}</span> — {staffErr.message}</span
					></Callout
				>
			{/if}
			<div class="row">
				<button
					class="btn primary"
					type="button"
					data-testid="save-staff"
					disabled={!mayEditStaff || staffBusy || (mode === 'round_robin' && !secsOk)}
					onclick={saveStaff}>{staffBusy ? 'Saving…' : 'Save staff team'}</button
				>
			</div>
		{/snippet}
	</LoadState>

	<h3>Staff channels</h3>
	<LoadState resource={bindings}>
		{#snippet children()}
			{#if workerBindings.length === 0}
				<p class="muted" data-testid="staff-bindings-empty">
					No staff channel is bound: handovers go to the internal team above.
				</p>
			{:else}
				<ul class="j21-team" data-testid="staff-bindings">
					{#each workerBindings as b (b.id)}
						<li data-binding={b.id}>
							<span
								><b>{b.label}</b>
								<span class="muted">{BINDING_KIND_LABEL[b.kind] ?? b.kind}</span>
								<span class="chip {b.enabled ? 'ok' : 'muted'}">{b.enabled ? 'enabled' : 'disabled'}</span>
								<span class="chip {b.verified ? 'ok' : 'warn'}"
									>{b.verified ? 'verified' : 'not verified'}</span
								>
							</span>
							<a class="btn sm" href="{job}/channels" data-testid="staff-binding-edit">Edit</a>
						</li>
					{/each}
				</ul>
				{#if routedExternally}
					<p class="muted small" data-testid="staff-routed-externally">
						This job has an enabled staff channel: its handovers go to that tool, not to the internal team.
					</p>
				{/if}
			{/if}
			<a class="btn" href="{job}/channels" data-testid="staff-channels-link">Manage staff channels</a>
		{/snippet}
	</LoadState>
</section>

<style>
	.j21-kpis {
		margin: 14px 0;
	}
	.j21-bar {
		display: flex;
		flex-wrap: wrap;
		align-items: end;
		gap: 8px 14px;
		margin-bottom: 10px;
	}
	.j21-f {
		display: grid;
		gap: 2px;
		font-size: 12px;
		color: var(--muted);
	}
	.j21-f .input {
		min-width: 0;
	}
	.j21-subj {
		display: block;
	}
	tr.j21-over td:first-child {
		box-shadow: inset 3px 0 0 var(--err);
	}
	.j21-feedback,
	.j21-staff {
		margin-top: 24px;
	}
	.j21-team {
		list-style: none;
		padding: 0;
		margin: 0 0 10px;
		display: grid;
		gap: 6px;
		max-width: 420px;
	}
	.j21-team li {
		display: flex;
		justify-content: space-between;
		align-items: center;
		gap: 8px;
	}
	.j21-policy {
		margin-top: 22px;
	}
	.j21-feedback h2,
	.j21-policy h2 {
		font-size: 16px;
		margin: 0 0 4px;
	}
	.j21-num {
		width: 7em;
	}
	.j21-props {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 6px;
	}
	.j21-props li {
		display: grid;
		grid-template-columns: 80px minmax(0, 1fr) auto;
		gap: 10px;
		align-items: center;
		padding: 8px 12px;
		border: 1px solid var(--line);
		border-radius: 8px;
		background: var(--panel);
	}
	.j21-p-body {
		display: grid;
		gap: 2px;
		font-size: 13px;
	}
	@media (max-width: 560px) {
		.j21-kpis {
			grid-template-columns: 1fr 1fr;
		}
		.j21-props li {
			grid-template-columns: 1fr;
		}
	}
</style>
