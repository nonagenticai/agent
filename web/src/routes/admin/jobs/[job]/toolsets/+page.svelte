<script lang="ts">
	// J8 Toolsets (spec 12 §2): what each agent role may call in this job, per activity and env — computed, never
	// configured: N2 (the activity expects it) ∩ N3 (the role grants it, minus this tenant's narrowings) ∩ N4 (a route
	// exists), then every call is checked against N5 (identity level, confirm-back, hosted link). The tenant can only
	// narrow; a widening is refused (422). The page leads with the role × activity matrix, then explains one cell.
	import '$lib/phone-stack.css';
	import { onMount, tick } from 'svelte';
	import { page } from '$app/state';
	import { api, guardOutcome } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { canWrite as canWriteFn, isSuperAdmin, writeAttrs, writeHint } from '$lib/auth/roles';
	import { Resource } from '$lib/load.svelte';
	import { ApiError } from '$lib/api/errors';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Seg from '$lib/ui/Seg.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import type {
		AgentRoleKey,
		Env,
		Toolset,
		ToolsetMeta,
		ToolsetOverview,
		ToolsetRole,
		ToolsetRow,
	} from '$lib/api/types/ontology';
	import {
		MAX_TOOL_CALLS,
		SYSTEM_PROMPT_MAX,
		TURN_BUDGET_S_MAX,
		TURN_BUDGET_S_MIN,
		type RolePolicy,
	} from '$lib/api/types/ontology';
	import { opLabel } from '$lib/ontology/catalogue';
	import { SIDE_EFFECT_LABEL, SIDE_EFFECT_VARIANT, routeText, when } from '$lib/ontology/shared';
	import {
		CUT_LABEL,
		ROLE_LABEL,
		cutLink,
		perCall,
		procedureCheck,
		stepsUsing,
		type CutLink,
	} from '$lib/ontology/toolsetsJ8';
	import ChainJ8 from './ChainJ8.svelte';
	import DraftBar from '$lib/release/DraftBar.svelte';
	import MatrixJ8 from './MatrixJ8.svelte';
	import FunnelJ8 from './FunnelJ8.svelte';

	const job = $derived(page.params.job ?? 'customer_care_agent');
	const jobBase = $derived(`/admin/jobs/${encodeURIComponent(job)}`);
	let env = $state<string>('prod');
	let role = $state<AgentRoleKey>('procedure');
	let activity = $state('support:start_return');
	let widenError = $state<string | null>(null);
	let busy = $state(false);
	/** D54: draft keys of unpublished narrowings (`narrow:role:activity:op`). */
	let draftKeys = $state<Set<string>>(new Set());
	const canWrite = $derived(canWriteFn(session, 'job_designer'));
	const sa = $derived(isSuperAdmin(session));

	const res = new Resource(
		async () => {
			const [meta, ov] = await Promise.all([api().toolsetMeta(), api().toolsetOverview(env as Env)]);
			return { meta, ov } as { meta: ToolsetMeta; ov: ToolsetOverview };
		},
		(d) => d.meta.activities.length === 0,
	);

	onMount(async () => {
		await res.refresh();
		const v = res.value;
		if (v.state === 'ok' && !v.data.meta.activities.some((a) => a.key === activity))
			activity = v.data.meta.activities[0]?.key ?? '';
	});

	let lastEnv = 'prod';
	$effect(() => {
		if (env !== lastEnv) {
			lastEnv = env;
			widenError = null;
			res.refresh();
		}
	});

	async function pick(r: AgentRoleKey, a: string) {
		role = r;
		activity = a;
		widenError = null;
		if (typeof window !== 'undefined' && window.innerWidth < 900) {
			await tick();
			const el = document.getElementById('j8-detail');
			if (el && typeof el.scrollIntoView === 'function')
				el.scrollIntoView({ behavior: 'smooth', block: 'start' });
		}
	}

	async function narrow(r: ToolsetRow) {
		busy = true;
		widenError = null;
		const op = opLabel(r.op, r.variant);
		await guardOutcome(
			() => api().narrowToolsetGrant(role, activity, op),
			`${op} removed from ${ROLE_LABEL[role]} for ${activity}. Saved to draft — try it in the test chat; customers get it when you publish.`,
		);
		busy = false;
		await res.refresh();
	}
	async function restore(op: string, act = activity, rl: AgentRoleKey = role) {
		busy = true;
		widenError = null;
		try {
			await api().restoreToolsetGrant(rl, act, op);
		} catch (e) {
			widenError =
				e instanceof ApiError && e.status === 403 ? `${writeHint('job_designer')}.` : (e as Error).message;
		}
		busy = false;
		await res.refresh();
	}

	/** F-40 (B5 T11a): switch a configurable role (the assistant) for this job, or set its tool calls per turn. */
	async function saveRole(r: ToolsetRole, enabled: boolean, max: number | null, policy?: RolePolicy) {
		busy = true;
		await guardOutcome(
			() => api().setAgentRoleEnabled(r.role, enabled, max, policy),
			`${r.label} ${enabled ? 'on' : 'off'} for this job${max ? ` · up to ${max} tool calls per turn` : ''}. Saved to draft — try it in the test chat; customers get it when you publish.`,
		);
		busy = false;
		await res.refresh();
	}
	/** A5: the prompt being typed; null = show the stored one. */
	let promptDraft = $state<string | null>(null);
	async function savePrompt(r: ToolsetRole) {
		await saveRole(r, r.enabled_job, r.max_tool_calls, { systemPrompt: promptDraft ?? '' });
		promptDraft = null;
	}
	const CALL_CHOICES = Array.from({ length: MAX_TOOL_CALLS }, (_, i) => i + 1);

	const ORDER: (CutLink | null)[] = [null, 'narrowed', 'N4', 'N3', 'N2', 'role'];
	const sorted = (ts: Toolset) =>
		[...ts.rows].sort(
			(a, b) => ORDER.indexOf(cutLink(a, ts.role_enabled)) - ORDER.indexOf(cutLink(b, ts.role_enabled)),
		);

	type Pill = 'pass' | 'cut' | 'skip';
	const LINKS = ['N2', 'N3', 'N4'] as const;
	function pill(r: ToolsetRow, enabled: boolean, link: (typeof LINKS)[number]): Pill {
		const c = cutLink(r, enabled);
		if (c === 'role') return 'skip';
		if (c === null) return 'pass';
		const at = c === 'narrowed' ? 1 : LINKS.indexOf(c);
		const idx = LINKS.indexOf(link);
		return idx < at ? 'pass' : idx === at ? 'cut' : 'skip';
	}
	const PILL_GLYPH: Record<Pill, string> = { pass: '✓', cut: '✕', skip: '·' };
</script>

<PageHead
	id="J8"
	sub="The tools each agent role actually gets in this job. Nobody configures a toolset: it is what the activity expects, intersected with what the role may call and what you have routed. You can only take tools away."
/>

<ChainJ8 {job} superAdmin={sa} />

<DraftBar
	page="J8"
	tick={res.value}
	onload={(d) => (draftKeys = new Set(d.changes.filter((c) => c.page === 'J8').map((c) => c.key)))}
/>

<LoadState resource={res} empty="This job has no activities yet, so no toolset can be computed.">
	{#snippet children({ meta, ov })}
		{@const roleRow = meta.roles.find((r) => r.role === role)}
		{@const act = meta.activities.find((a) => a.key === activity)}
		{@const ts = ov.toolsets.find((t) => t.role === role && t.activity === activity)}
		{@const procTs = ov.toolsets.find((t) => t.role === 'procedure' && t.activity === activity)}
		{@const procs = ov.procedures.filter((p) => p.activity === activity)}

		<Card
			title="Who can call what, per activity"
			sub="Each cell is one computed toolset. Only the procedure role acts; the qualifier and post-gate only read."
			cls="flush"
		>
			{#snippet actions()}
				<span class="j8-envl">Env</span>
				<Seg
					label="Env"
					items={[
						['prod', 'prod'],
						['test', 'test'],
					]}
					bind:active={env}
				/>
			{/snippet}
			<MatrixJ8
				roles={meta.roles}
				activities={meta.activities}
				toolsets={ov.toolsets}
				procedures={ov.procedures}
				{role}
				{activity}
				onpick={pick}
			/>
		</Card>

		<section id="j8-detail" class="j8-detail" data-testid="toolset-funnel" aria-label="Selected toolset">
			<header class="j8-dh">
				<div>
					<p class="j8-eyebrow">Selected toolset · {ov.env}</p>
					<h2 class="j8-h">
						{ROLE_LABEL[role]} <span class="j8-x">in</span>
						{act?.label ?? activity}
					</h2>
					{#if roleRow}
						<p class="j8-rn">
							Used by {roleRow.used_by} · ceiling
							<Chip
								text={SIDE_EFFECT_LABEL[roleRow.max_side_effect]}
								variant={SIDE_EFFECT_VARIANT[roleRow.max_side_effect]}
							/> · {roleRow.note}
						</p>
					{/if}
				</div>
				<div class="j8-seg">
					<Seg
						label="Agent role"
						items={meta.roles.map((r) => [
							r.role,
							r.enabled ? ROLE_LABEL[r.role] : `${ROLE_LABEL[r.role]} (disabled)`,
						])}
						bind:active={role}
					/>
				</div>
			</header>

			{#if roleRow?.configurable}
				<div class="j8-role" data-testid="role-setting">
					<label class="j8-check">
						<input
							type="checkbox"
							checked={roleRow.enabled_job}
							{...writeAttrs(session, 'job_designer')}
							disabled={!canWrite || busy || (!roleRow.enabled_platform && !roleRow.enabled_job)}
							title={!canWrite
								? writeHint('job_designer')
								: roleRow.enabled_platform
									? undefined
									: 'Enabled by the platform first'}
							onchange={(e) => saveRole(roleRow, e.currentTarget.checked, roleRow.max_tool_calls)}
						/>
						Enable for this job
					</label>
					<label class="j8-calls">
						Max tool calls per turn
						<select
							value={roleRow.max_tool_calls === null ? '' : String(roleRow.max_tool_calls)}
							{...writeAttrs(session, 'job_designer')}
							disabled={!canWrite || busy}
							onchange={(e) => {
								const v = e.currentTarget.value;
								saveRole(roleRow, roleRow.enabled_job, v === '' ? null : Number(v));
							}}
						>
							<option value="">Default ({MAX_TOOL_CALLS})</option>
							{#each CALL_CHOICES as n (n)}
								<option value={String(n)}>{n}</option>
							{/each}
						</select>
					</label>
					<label class="j8-calls">
						Turn budget (seconds)
						<input
							type="number"
							min={TURN_BUDGET_S_MIN}
							max={TURN_BUDGET_S_MAX}
							placeholder="Default (60)"
							value={roleRow.turn_budget_s === null ? '' : String(roleRow.turn_budget_s)}
							{...writeAttrs(session, 'job_designer')}
							disabled={!canWrite || busy}
							onchange={(e) => {
								const v = e.currentTarget.value;
								saveRole(roleRow, roleRow.enabled_job, roleRow.max_tool_calls, {
									turnBudgetS: v === '' ? null : Number(v),
								});
							}}
						/>
					</label>
					<label class="j8-check">
						<input
							type="checkbox"
							checked={roleRow.case_per_thread}
							{...writeAttrs(session, 'job_designer')}
							disabled={!canWrite || busy}
							onchange={(e) =>
								saveRole(roleRow, roleRow.enabled_job, roleRow.max_tool_calls, {
									casePerThread: e.currentTarget.checked,
								})}
						/>
						One case per thread
					</label>
					<label class="j8-prompt">
						System prompt (blank = the default for this job's audience)
						<textarea
							rows="6"
							maxlength={SYSTEM_PROMPT_MAX}
							value={promptDraft ?? roleRow.system_prompt ?? ''}
							{...writeAttrs(session, 'job_designer')}
							disabled={!canWrite || busy}
							oninput={(e) => (promptDraft = e.currentTarget.value)}></textarea>
					</label>
					{#if promptDraft !== null}
						<button
							type="button"
							{...writeAttrs(session, 'job_designer')}
							disabled={!canWrite || busy}
							onclick={() => savePrompt(roleRow)}>Save prompt</button
						>
					{/if}
					{#if !roleRow.enabled_platform}
						<p class="j8-hint">
							Enabled by the platform first: {roleRow.label} is off on the platform, so no agent runs with it in
							this job and nothing below is callable.
						</p>
					{:else if !roleRow.enabled_job}
						<p class="j8-hint">
							Off for this job: nothing below is callable. Switch it on to let the assistant answer requests
							no procedure covers, with the tools of this activity's toolset.
						</p>
					{/if}
				</div>
			{:else if roleRow && !roleRow.enabled}
				<Callout kind="info" title="{roleRow.label} is disabled on the platform">
					The role is defined so its grants can be reviewed, but no agent runs with it: nothing below is
					callable until the platform enables it.
				</Callout>
			{/if}
			{#if act?.hidden}
				<Callout kind="gap" title="This activity is hidden for this job">
					The gate never routes to it, so this toolset is never used until the activity is unhidden on
					<a href="{jobBase}/refinements">Base &amp; refinements</a>.
				</Callout>
			{/if}
			{#if widenError}
				<div class="callout err" role="alert">
					<strong>Widening rejected</strong><span>{widenError}</span>
				</div>
			{/if}

			{#if ts}
				<FunnelJ8 {ts} />

				<Card
					title="{ts.rows.filter((r) => r.effective).length} of {ts.rows.length} operations callable"
					sub="Per operation: which link let it through or cut it, where it is routed, and what the runner still checks on every call."
					cls="flush j8-ops-card"
				>
					<ul class="j8o" aria-label="Operations">
						{#each sorted(ts) as r (opLabel(r.op, r.variant))}
							{@const c = cutLink(r, ts.role_enabled)}
							{@const pc = perCall(r.op, r.side_effect)}
							{@const used = role === 'procedure' ? stepsUsing(r, procs) : []}
							<li class="j8o-row" class:off={c !== null} class:nar={r.narrowed}>
								<div class="j8o-op">
									<span class="mono j8o-name">{opLabel(r.op, r.variant)}</span>
									<span class="j8o-tags">
										<Chip
											text={SIDE_EFFECT_LABEL[r.side_effect]}
											variant={SIDE_EFFECT_VARIANT[r.side_effect]}
										/>
										{#if r.expected}<Chip
												text={r.expected === 'R' ? 'required' : 'optional'}
												variant={r.expected === 'R' ? 'accent' : 'muted'}
											/>{/if}
									</span>
									{#if used.length}<span class="j8o-used">used by {used.join(', ')}</span>{/if}
								</div>
								<div class="j8o-chain" aria-label="Narrowing chain">
									{#each LINKS as l (l)}
										{@const p = pill(r, ts.role_enabled, l)}
										<span
											class="j8o-pill p-{p}"
											class:you={l === 'N3' && r.narrowed}
											title={p === 'cut' && c ? CUT_LABEL[c] : undefined}
											>{l}
											{l === 'N3' && r.narrowed ? '✂' : PILL_GLYPH[p]}</span
										>
									{/each}
								</div>
								<div class="j8o-route">
									<span class="j8o-k">route</span>
									<span class="mono">{routeText(r.route)}</span>
								</div>
								<div class="j8o-call">
									{#if c === null}
										<span class="j8o-k">per call</span>
										<span
											>needs <b>{pc.identity}</b>{[
												pc.confirm ? 'customer confirms first' : '',
												pc.hosted_link ? 'hosted payment link only' : '',
												pc.verify_read && !pc.hosted_link ? 'verified by a read after' : '',
											]
												.filter(Boolean)
												.map((x) => ` · ${x}`)
												.join('')}</span
										>
									{:else}
										<span class="j8o-k">{c ? CUT_LABEL[c] : ''}</span>
										<span class="j8o-why">{r.reason}</span>
										{#if used.length && c === 'narrowed'}
											<span class="j8o-warn"
												>That step is refused at run time (not_entitled) and the case escalates.</span
											>
										{/if}
									{/if}
								</div>
								<div class="j8o-act">
									{#if r.narrowed}
										<button
											type="button"
											class="btn sm"
											{...writeAttrs(session, 'job_designer')}
											disabled={!canWrite || busy}
											onclick={() => restore(opLabel(r.op, r.variant))}>Restore</button
										>
									{:else if r.granted && r.expected && ts.role_enabled}
										<button
											type="button"
											class="btn sm"
											{...writeAttrs(session, 'job_designer')}
											disabled={!canWrite || busy}
											title={canWrite
												? used.length
													? `Remove for this activity; ${used.join(', ')} would then be refused`
													: 'Remove this operation for this role and activity'
												: writeHint('job_designer')}
											onclick={() => narrow(r)}>Narrow</button
										>
									{:else if r.expected && !r.granted && ts.role_enabled}
										<button
											type="button"
											class="btn sm"
											{...writeAttrs(session, 'job_designer')}
											disabled={!canWrite || busy}
											title={canWrite
												? 'Try to add this to the role grant — tenants may only narrow, so this is refused'
												: writeHint('job_designer')}
											onclick={() => restore(opLabel(r.op, r.variant))}>Grant</button
										>
									{/if}
								</div>
							</li>
						{/each}
					</ul>
				</Card>
			{/if}

			{#if procTs}
				<Card
					title="What the procedure for this activity calls"
					sub="Procedures run as the procedure role. Every step must be in its toolset (checked again when a release is applied)."
					cls="flush"
				>
					{#if procs.length === 0}
						<p class="j8-empty">
							No published procedure serves this activity yet, so its toolset is unused. Author one on
							<a href="{jobBase}/procedures">Procedures</a>.
						</p>
					{:else}
						{#each procs as p (p.name + p.version)}
							{@const checks = procedureCheck(p, procTs)}
							{@const bad = checks.filter((x) => !x.callable).length}
							<div class="j8p">
								<p class="j8p-h">
									<a class="mono" href="{jobBase}/procedures/{encodeURIComponent(p.name)}">{p.name}</a>
									<span class="faint"
										>v{p.version} · {p.origin === 'tenant' ? 'your version' : 'template'} · space
										<span class="mono">{p.context_space}</span></span
									>
									{#if bad}<Chip text="{bad} step{bad > 1 ? 's' : ''} would be refused" variant="err" />
									{:else}<Chip text="every step callable" variant="ok" />{/if}
								</p>
								<ol class="j8p-steps">
									{#each checks as s (s.step_number + s.label)}
										<li class:bad={!s.callable}>
											<span class="j8p-n mono">{s.step_number}</span>
											<span class="j8p-name">{s.name}</span>
											<span class="mono j8p-op">{s.label}</span>
											{#if s.callable}<span class="j8p-ok">✓ in toolset</span>
											{:else}<span class="j8p-cut">✕ {s.cut ? CUT_LABEL[s.cut] : 'not in toolset'}</span>{/if}
										</li>
									{/each}
								</ol>
							</div>
						{/each}
					{/if}
				</Card>
			{/if}
		</section>

		<Card
			title="Your narrowings ({ov.narrowings.length})"
			sub="Saved to draft: the test chat runs them at once; customers get them when you publish a release."
			cls="flush"
		>
			{#snippet actions()}
				<a class="btn sm" href="{jobBase}/refinements">Refinements</a>
				<a class="btn sm" href="/chat">Test in test chat</a>
				<a class="btn sm" href="{jobBase}/release">Review &amp; publish</a>
			{/snippet}
			<div class="tbl-wrap ph-stack">
				<table class="tbl dense">
					<thead
						><tr
							><th>Role</th><th>Activity</th><th>Operation removed</th><th>State</th><th>By</th><th>When</th
							><th></th></tr
						></thead
					>
					<tbody>
						{#each ov.narrowings as n (n.role + n.activity + n.op)}
							<tr>
								<td>{ROLE_LABEL[n.role] ?? n.role}</td>
								<td class="mono" data-label="Activity">{n.activity}</td>
								<td class="mono" data-label="Removes">{n.op}</td>
								<td data-label="State">
									{#if draftKeys.has(`narrow:${n.role}:${n.activity}:${n.op}`)}<Chip
											text="draft"
											variant="warn"
											title="Live still has {n.op}; removed in the test chat"
										/>{:else}<Chip text="published" variant="ok" />{/if}
								</td>
								<td class="mono faint" data-label="By">{n.by}</td>
								<td class="faint" data-label="When">{when(n.at)}</td>
								<td class="r">
									<button
										type="button"
										class="btn sm"
										{...writeAttrs(session, 'job_designer')}
										disabled={!canWrite || busy}
										onclick={() => restore(n.op, n.activity, n.role)}>Restore</button
									>
								</td>
							</tr>
						{:else}
							<tr
								><td colspan="7" class="muted">No narrowings: every role has its full template grant.</td></tr
							>
						{/each}
					</tbody>
				</table>
			</div>
			<p class="j8-foot">
				Adding a tool is never a tenant change: a role grant or an activity's expected operations change only
				in the template{#if sa}
					(<a href="/platform/agent-roles">Agent roles</a>,
					<a href="/platform/operations">Operations catalogue</a>){/if}, and reach this job through a release.
				{#if !canWrite}{writeHint('job_designer')} to narrow or restore.{/if}
			</p>
		</Card>
	{/snippet}
</LoadState>

<style>
	.j8-role {
		display: flex;
		flex-wrap: wrap;
		gap: 12px 24px;
		align-items: center;
		margin: 0 0 12px;
		padding: 12px 14px;
		border: 1px solid var(--line, #e3e3e3);
		border-radius: 10px;
	}
	.j8-check,
	.j8-calls {
		display: inline-flex;
		align-items: center;
		gap: 8px;
	}
	.j8-prompt {
		display: grid;
		flex-basis: 100%;
		gap: 4px;
	}
	.j8-prompt textarea {
		width: 100%;
		font: inherit;
	}
	.j8-hint {
		flex-basis: 100%;
		margin: 0;
		color: var(--muted, #666);
	}
	.j8-envl {
		font: 600 11px/1.2 var(--cond);
		letter-spacing: 0.07em;
		text-transform: uppercase;
		color: var(--muted);
		margin-right: 6px;
	}
	.j8-detail {
		display: grid;
		gap: 12px;
		margin: 16px 0;
		padding-top: 4px;
		scroll-margin-top: 70px;
	}
	.j8-dh {
		display: flex;
		justify-content: space-between;
		align-items: flex-end;
		gap: 12px;
		flex-wrap: wrap;
	}
	.j8-eyebrow {
		margin: 0;
		font: 600 11px/1.2 var(--cond);
		letter-spacing: 0.07em;
		text-transform: uppercase;
		color: var(--accent);
	}
	.j8-h {
		margin: 2px 0 4px;
		font-size: 19px;
	}
	.j8-x {
		font-weight: 400;
		color: var(--faint);
	}
	.j8-rn {
		margin: 0;
		font-size: 12.5px;
		color: var(--muted);
	}
	.j8-seg {
		max-width: 100%;
		overflow-x: auto;
	}
	.j8o {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.j8o-row {
		display: grid;
		grid-template-columns: minmax(180px, 1.3fr) auto minmax(130px, 0.9fr) minmax(200px, 1.6fr) 84px;
		gap: 6px 16px;
		align-items: center;
		padding: 9px 14px;
		border-top: 1px solid var(--line-2);
	}
	.j8o-row.off .j8o-name,
	.j8o-row.off .j8o-route {
		color: var(--faint);
	}
	.j8o-row.off :global(.chip) {
		opacity: 0.6;
	}
	.j8o-row.nar {
		background: color-mix(in srgb, var(--cust) 5%, transparent);
	}
	.j8o-op {
		display: grid;
		gap: 3px;
		min-width: 0;
	}
	.j8o-name {
		font-weight: 600;
		overflow-wrap: anywhere;
	}
	.j8o-tags {
		display: flex;
		gap: 4px;
		flex-wrap: wrap;
	}
	.j8o-used {
		font-size: 11.5px;
		color: var(--muted);
	}
	.j8o-chain {
		display: flex;
		gap: 3px;
	}
	.j8o-pill {
		font: 600 11px/1 var(--mono);
		padding: 4px 6px;
		border-radius: 4px;
		border: 1px solid var(--line-2);
		white-space: nowrap;
	}
	.p-pass {
		color: var(--ok);
		background: color-mix(in srgb, var(--ok) 10%, transparent);
		border-color: transparent;
	}
	.p-cut {
		color: var(--warn);
		border-color: color-mix(in srgb, var(--warn) 55%, var(--line));
		background: color-mix(in srgb, var(--warn) 10%, transparent);
	}
	.p-cut.you {
		color: var(--cust);
		border-color: var(--cust);
		background: color-mix(in srgb, var(--cust) 10%, transparent);
	}
	.p-skip {
		color: var(--faint);
	}
	.j8o-route,
	.j8o-call {
		display: grid;
		gap: 1px;
		font-size: 12.5px;
		min-width: 0;
	}
	.j8o-route .mono {
		font-size: 12px;
		overflow-wrap: anywhere;
	}
	.j8o-k {
		font: 600 10.5px/1.2 var(--cond);
		letter-spacing: 0.06em;
		text-transform: uppercase;
		color: var(--faint);
	}
	.j8o-why {
		color: var(--muted);
	}
	.j8o-warn {
		color: var(--warn);
		font-size: 12px;
	}
	.j8o-act {
		text-align: right;
	}
	.j8p {
		padding: 10px 14px;
		border-top: 1px solid var(--line-2);
	}
	.j8p-h {
		display: flex;
		flex-wrap: wrap;
		gap: 6px 10px;
		align-items: center;
		margin: 0 0 6px;
		font-size: 13px;
	}
	.j8p-steps {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 2px;
	}
	.j8p-steps li {
		display: grid;
		grid-template-columns: 22px minmax(120px, 1fr) minmax(140px, 1fr) auto;
		gap: 10px;
		align-items: baseline;
		font-size: 12.5px;
		padding: 3px 0;
	}
	.j8p-steps li.bad {
		color: var(--warn);
	}
	.j8p-n {
		color: var(--faint);
	}
	.j8p-op {
		font-size: 12px;
		overflow-wrap: anywhere;
	}
	.j8p-ok {
		color: var(--ok);
		font-size: 12px;
	}
	.j8p-cut {
		color: var(--warn);
		font-size: 12px;
		font-weight: 600;
	}
	.j8-empty,
	.j8-foot {
		margin: 0;
		padding: 10px 14px;
		font-size: 12.5px;
		color: var(--muted);
	}
	.faint {
		color: var(--faint);
	}
	@media (max-width: 900px) {
		.j8o-row {
			grid-template-columns: minmax(0, 1fr) auto;
		}
		.j8o-op {
			grid-column: 1;
		}
		.j8o-chain {
			grid-column: 2;
			grid-row: 1;
		}
		.j8o-route,
		.j8o-call {
			grid-column: 1 / -1;
		}
		.j8o-act {
			grid-column: 1 / -1;
			text-align: left;
		}
		.j8o-act:empty {
			display: none;
		}
		.j8p-steps li {
			grid-template-columns: 22px minmax(0, 1fr);
		}
		.j8p-op,
		.j8p-ok,
		.j8p-cut {
			grid-column: 2;
		}
	}
</style>
