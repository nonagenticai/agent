<script lang="ts">
	// J20 Case detail — one case end to end, as the agent experienced it. Turn by turn: what the customer said, the
	// gate verdicts (P1/P2/R1), what the agent understood (activity · object · modality), the procedure steps and
	// operation calls it ran (side-effect class, system, canonical outcome, confirm-back), the passages it cited, the
	// objects it changed, and how the case ended. Which release / refinement rev / routing rev it ran on, and — for an
	// `unknown_outcome` — exactly what a person must check before anything is sent again (12 §3.3 step 10).
	// Started from the cb trace drawer (cb:web/src/routes/admin/jobs/[job]/cases/+page.svelte@45ee128).
	import { onMount } from 'svelte';
	import { page } from '$app/state';
	import { api, app, guard } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { canWrite, writeAttrs } from '$lib/auth/roles';
	import { OPERATOR_NOTE, SUPPORT_ROLES_NOTE } from '$lib/runtime/support-D55';
	import { jobPath } from '$lib/scope';
	import { fmt } from '$lib/format';
	import { CHANNEL_LABEL, clock, stamp } from '$lib/conversations/labels';
	import { zoneLabel } from '$lib/support/time';
	import { watchConversation } from '$lib/conversations/feed';
	import { workspaceHref } from '$lib/support/links';
	import ConversationPane from '$lib/support/ui/ConversationPane.svelte';
	import { CALL_STATE_LABEL, STATUS_LABEL, STATUS_WORD, activityLabel } from '$lib/runtime/labels';
	import {
		FAMILY_LABEL,
		SIDE_LABEL,
		confirmMessage,
		endingOf,
		objectChangesOf,
		outcomeClass,
		proceduresOf,
		sideEffectOf,
		turnsOf,
		unknownOutcomeTask,
		type OutcomeFamily,
	} from '$lib/runtime/J20-case-story';
	import { GATE_POINT, offset } from '../lanes';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Status from '$lib/ui/Status.svelte';
	import JsonView from '$lib/ui/JsonView.svelte';
	import Meter from '$lib/ui/Meter.svelte';
	import MilestoneChip from '$lib/ui/MilestoneChip.svelte';
	import AnswerSources from '$lib/grounding/AnswerSources.svelte';
	import AnswerFeedback from '$lib/grounding/AnswerFeedback.svelte';
	import { groundingOf, replyAnswerId } from '$lib/grounding/grounding';

	const id = $derived(page.params.id ?? '');
	const detail = new Resource(
		() => api().caseDetail(id),
		() => false,
	);
	const overview = new Resource(
		() => api().jobOverview(),
		() => false,
	);
	/** The ratings of this case's answers (operational feedback); the caller's own show as their current vote. */
	const feedback = new Resource(
		() => api().answerFeedback({ case_id: id }),
		() => false,
	);
	const mine = $derived(
		new Map(
			(feedback.value.state === 'ok' ? feedback.value.data : [])
				.filter((f) => f.by === session.me?.sub)
				.map((f) => [f.answer_id, f]),
		),
	);
	onMount(() => {
		void detail.refresh();
		void overview.refresh();
		void feedback.refresh();
	});
	const liveRelease = $derived(
		overview.value.state === 'ok' ? (overview.value.data.current_release?.id ?? null) : null,
	);

	const IDENTITY = ['0 · anonymous', '1 · stated', '2 · verified', '3 · step-up'];
	const FAMILY_TONE: Record<OutcomeFamily, 'ok' | 'muted' | 'warn' | 'err'> = {
		ok: 'ok',
		user: 'muted',
		gate: 'warn',
		system: 'err',
		unknown: 'err',
	};
	const SYSTEM: Record<string, string> = {
		'shopify-prod': 'Shopify',
		'shopify-test': 'Shopify (test)',
		'frappe-helpdesk-prod': 'Frappe Helpdesk',
		'stripe-checkout-prod': 'Stripe',
		'stripe-checkout-test': 'Stripe (test)',
		'demo-retail-stub': 'Demo stub',
		agent_cases: 'Agent case queue',
		strata_rag: 'Knowledge store',
		builtin: 'Built into the agent',
	};
	const systemOf = (inst: string | null) => (inst ? (SYSTEM[inst] ?? inst) : '—');

	let handoverOpen = $state(false);
	let reason = $state('');
	let checkNote = $state('');
	let busy = $state(false);
	const admin = $derived(canWrite(session, 'support_agent'));
	/** Recording an unknown outcome is the Operator's (§R.1). */
	const operator = $derived(canWrite(session, 'operator'));
	const confClass = (c: number) => (c >= 0.8 ? 's-ok' : c >= 0.6 ? 's-warn' : 's-err');

	/** The pane's live feed: any frame of the conversation re-reads its transcript. */
	const framesOf = (id: string) => (on: (f: { event: string; data: unknown }) => void) =>
		watchConversation(api().conversationEvents, id, {
			onFrame: () => on({ event: 'resync', data: { conversation_id: id } }),
		});

	async function handOver() {
		busy = true;
		const ok = await guard(
			() => api().handOverCase(id, reason),
			`${id} handed over to staff.`,
			'Not handed over',
		);
		busy = false;
		if (ok) {
			handoverOpen = false;
			reason = '';
			await detail.refresh();
		}
	}
	// Same call (and same result) as J22 "Needs a person": the check is recorded on the case timeline, the alert
	// clears, and nothing is ever re-sent automatically.
	async function record(callId: string, op: string, verdict: 'landed' | 'not_landed') {
		busy = true;
		const ok = await guard(
			() => api().checkUnknownOutcome(callId, verdict, checkNote),
			verdict === 'landed'
				? `Recorded: ${op} went through. Case ${id} updated.`
				: `Recorded: ${op} did not go through. Case ${id} updated.`,
			'Not recorded',
		);
		busy = false;
		if (ok) {
			checkNote = '';
			await detail.refresh();
		}
	}
</script>

<PageHead
	id="J20"
	title="Case {id}"
	sub="One case end to end, as the agent experienced it: what the customer said, what the agent checked and did, and how it ended. All times in {zoneLabel()}."
>
	{#snippet actions()}
		<a class="btn" href={jobPath(app.job, '/cases')}>All cases</a>
		<a class="btn" href={jobPath(app.job, `/operation-calls?case_id=${encodeURIComponent(id)}`)}
			>Operation calls</a
		>
	{/snippet}
</PageHead>

<LoadState resource={detail}>
	{#snippet children(d)}
		{@const c = d.case}
		{@const turns = turnsOf(d)}
		{@const ending = endingOf(d)}
		{@const task = unknownOutcomeTask(d)}
		{@const procs = proceduresOf(d)}
		{@const changes = objectChangesOf(d)}
		{@const passages = d.timeline.flatMap((e) => (e.kind === 'knowledge' ? e.passages : []))}

		<!-- 1. The story in one band: who, what, how it ended, what it ran on. -->
		<section class="story end-{ending.kind}" data-testid="case-story">
			<div class="story-main">
				<div class="story-who">
					<Status word={STATUS_WORD[c.status]} label={STATUS_LABEL[c.status]} />
					<span class="muted"
						>{c.customer_name} · {CHANNEL_LABEL[c.channel] ?? c.channel} · identity {IDENTITY[
							c.identity_level
						] ?? c.identity_level} · {stamp(c.started_at)}</span
					>
				</div>
				<h2 class="story-title">{c.title}</h2>
				<p class="story-end"><b>{ending.headline}.</b> {ending.why}</p>
				<ol class="path" aria-label="Turns">
					{#each turns as t (t.n)}
						<li class="path-step fam-{t.worst ?? 'none'}">
							<a href="#turn-{t.n}">
								<span class="path-n">{t.n}</span>
								<span class="path-txt"
									>{t.qualified
										? (t.qualified.activity.split(':')[1] ?? t.qualified.activity).replaceAll('_', ' ')
										: t.opener
											? 'follow-up'
											: 'start'}</span
								>
							</a>
						</li>
					{/each}
					<li class="path-step path-end end-{ending.kind}"><span>{STATUS_LABEL[c.status]}</span></li>
				</ol>
			</div>
			<aside class="ranon" data-testid="ran-on">
				<span class="label">Ran on</span>
				<div class="ranon-env">
					<Chip
						text={c.env === 'prod' ? 'Production' : 'Test env'}
						variant={c.env === 'prod' ? 'ok' : 'warn'}
					/>
					<span class="muted">{c.env === 'prod' ? 'published release' : 'draft + test routes'}</span>
				</div>
				{#if c.release_id}<a
						class="mono ranon-rel"
						href={jobPath(app.job, `/release/${encodeURIComponent(c.release_id)}`)}>{c.release_id}</a
					>{:else}<span class="mono ranon-rel">draft</span>{/if}
				{#if c.env === 'prod' && liveRelease}
					<span class="cell-sub"
						>{liveRelease === c.release_id
							? 'still the live release'
							: `superseded — live now: ${liveRelease}`}</span
					>
				{/if}
				{#if c.draft_rev != null}<div class="revs">
						<span title="The draft revision this test-environment case ran on (D63)"
							>draft rev <b>{c.draft_rev}</b></span
						>
					</div>{/if}
				<p class="cell-sub">
					{c.env === 'prod'
						? 'Changes saved to draft never reach production cases until they are published.'
						: 'Test cases run the unpublished draft against test systems.'}
				</p>
			</aside>
		</section>

		<!-- 2. The decision a person owes this case (only when a write never answered). -->
		{#if task}
			{@const sys = systemOf(task.call.instance)}
			<section class="todo" data-testid="unknown-outcome" aria-labelledby="todo-h">
				<div class="todo-head">
					<h3 id="todo-h">A person must check {sys} before anything is sent again</h3>
					<Chip text="unknown_outcome" variant="err" />
					<MilestoneChip milestone="M4" note="ops M4" />
				</div>
				<div class="todo-grid">
					<div>
						<span class="label">What happened</span>
						<p>
							<b class="mono">{task.call.op}</b>
							({SIDE_LABEL[task.side]}, class {task.side}) was sent to {sys}
							<span class="mono">({task.call.instance})</span>
							{#if task.confirmedBy}after the customer confirmed “{task.confirmedBy.text}”{/if} at {clock(
								task.call.at,
							)}. No answer within {fmt.ms(task.call.latency_ms ?? 0)}, and the verify read got no answer
							either.
							{#if task.change}<b>{task.change.object} {task.change.ref}</b> is now
								<i>{task.change.from} → ?</i>{/if}
						</p>
						<span class="label">Why the agent did not retry</span>
						<p>
							The request may have gone through. Retrying an irreversible write that already landed would do
							it twice — so the runner never retries <span class="mono">unknown_outcome</span>; it stops,
							alerts and tells the customer honestly.
						</p>
					</div>
					<div>
						<span class="label">What you do</span>
						<ol class="todo-steps">
							<li>
								Open {sys} and look for the {task.change?.object.toLowerCase() ?? 'record'}
								{#if task.change}<span class="mono">{task.change.ref}</span>{/if}
								{#if task.key}— search idempotency key <span class="mono">{task.key}</span>{/if}.
							</li>
							<li>
								Record what you found. The case is updated and its alert cleared; nothing is sent again
								automatically — if it did not go through, re-sending is your separate decision.
							</li>
							<li>Tell the customer — the agent promised an answer today.</li>
						</ol>
						<label class="field">
							<span class="label">Note (optional)</span>
							<input class="input" bind:value={checkNote} placeholder="e.g. refund re_3Q… seen in Shopify" />
						</label>
						<div class="actions">
							<button
								class="btn primary"
								type="button"
								{...writeAttrs(session, 'operator')}
								disabled={!operator || busy}
								onclick={() => record(task.call.call_id, task.call.op, 'landed')}>It went through</button
							>
							<button
								class="btn"
								type="button"
								{...writeAttrs(session, 'operator')}
								disabled={!operator || busy}
								onclick={() => record(task.call.call_id, task.call.op, 'not_landed')}
								>It did not go through</button
							>
						</div>
						{#if !operator}<span class="cell-sub" data-testid="support-roles-note">{OPERATOR_NOTE}</span>{/if}
					</div>
				</div>
			</section>
		{/if}

		<div class="layout">
			<div class="main-col">
				<!-- 3a. The conversation as the staff workspace shows it, read only. -->
				{#if c.conversation_id}
					<Card title="Conversation" sub="As staff see it in the workspace">
						{#snippet actions()}
							<a
								class="btn sm primary"
								href={workspaceHref(app.job, c.conversation_id)}
								data-testid="open-workspace">Open in workspace</a
							>
						{/snippet}
						<div class="conv-pane">
							<ConversationPane
								readonly
								job={app.job}
								conversationId={c.conversation_id}
								frames={framesOf(c.conversation_id)}
							/>
						</div>
					</Card>
				{/if}
				<!-- 3b. The same conversation turn by turn, with the machinery between the messages. -->
				<Card
					title="Turn by turn"
					sub="{turns.length} turns · {d.timeline.length} steps · times from the first message"
				>
					<ol class="turns" data-testid="timeline">
						{#each turns as t (t.n)}
							<li class="turn" id="turn-{t.n}">
								<div class="turn-head">
									<span class="turn-n">Turn {t.n}</span>
									{#if t.opener}<span class="mono muted"
											>{offset(c.started_at, t.opener.at)} · {clock(t.opener.at)}</span
										>{/if}
									{#if t.qualified}
										<Chip text={t.qualified.activity} variant="accent" title="Qualified activity" />
										<span class="cell-sub mono">{t.qualified.object} · {t.qualified.modality}</span>
									{/if}
									{#if t.steps.some((e) => e.kind === 'gate' && e.verdict === 'handover')}<Chip
											text="gate → handover"
											variant="warn"
										/>{/if}
									{#if t.worst && t.worst !== 'ok'}<Chip
											text={FAMILY_LABEL[t.worst]}
											variant={FAMILY_TONE[t.worst]}
										/>{/if}
								</div>
								<ul class="steps">
									<!-- the messages themselves are the conversation card's (one transcript) -->
									{#if t.opener?.msg_id && d.timeline.some((e) => e.kind === 'operation' && e.confirm_ref === t.opener?.msg_id)}
										<li class="tl-row msg" data-kind="message">
											<span class="cell-sub"
												>The customer’s message was used as confirm-back (<span class="mono"
													>{t.opener.msg_id}</span
												>)</span
											>
										</li>
									{/if}
									{#each t.steps as e, i (i)}
										{#if e.kind === 'gate'}
											{@const low = [...e.verdicts].sort((a, b) => a.confidence - b.confidence)[0]}
											<li class="tl-row mech" data-kind="gate">
												<details>
													<summary>
														<span class="k">Gate</span>
														<b>{GATE_POINT[e.point]}</b>
														<Status
															word={e.verdict === 'pass' ? 'ok' : e.verdict === 'block' ? 'err' : 'warn'}
															label={e.verdict}
														/>
														{#if low}<span class="cell-sub"
																>lowest {low.label} {low.confidence.toFixed(2)}</span
															>{/if}
													</summary>
													<div class="tbl-wrap verdicts-wrap">
														<table class="tbl dense verdicts">
															<thead
																><tr><th>Q</th><th>Question</th><th>Verdict</th><th>Confidence</th></tr
																></thead
															>
															<tbody>
																{#each e.verdicts as v (v.key)}
																	<tr>
																		<td class="mono">{v.key}</td>
																		<td>{v.label}</td>
																		<td class="mono"
																			>{v.value}{#if v.below_threshold}
																				<Chip text="below threshold" variant="warn" />{/if}</td
																		>
																		<td
																			><span class="conf"
																				><Meter value={v.confidence} cls={confClass(v.confidence)} /><span
																					class="mono">{v.confidence.toFixed(2)}</span
																				></span
																			></td
																		>
																	</tr>
																{/each}
															</tbody>
														</table>
													</div>
													<span class="cell-sub mono">{fmt.ms(e.latency_ms)}</span>
												</details>
												{#if e.note}<p class="note">{e.note}</p>{/if}
											</li>
										{:else if e.kind === 'qualified'}
											<li class="tl-row mech" data-kind="qualified">
												<span class="k">Understood</span>
												<span
													><b>{activityLabel(e.activity)}</b> on <b>{e.object}</b> ({e.modality})
													<span class="cell-sub mono">· space {e.context_space}</span></span
												>
												<div class="chips">
													{#each Object.entries(e.slots) as [k, v] (k)}<Chip text="{k}: {v ?? '—'}" />{/each}
												</div>
											</li>
										{:else if e.kind === 'knowledge'}
											<li class="tl-row mech" data-kind="knowledge">
												<span class="k">Looked up</span>
												<span
													>“{e.query}” in <span class="mono">{e.store}</span> → {e.passages.length} passage{e
														.passages.length === 1
														? ''
														: 's'}</span
												>
												<ol class="passages">
													{#each e.passages as p (p.title)}
														<li>
															<b>{p.title}</b>
															<Chip text={p.kind} variant="work" />
															<span class="mono muted">score {p.score.toFixed(2)}</span>
															<p>{p.excerpt}</p>
														</li>
													{/each}
												</ol>
											</li>
										{:else if e.kind === 'operation'}
											{@const oc = outcomeClass(e.outcome)}
											{@const fam = e.state === 'unknown_outcome' ? 'unknown' : oc.family}
											{@const side = sideEffectOf(e.op)}
											{@const conf = confirmMessage(d, e.confirm_ref)}
											<li class="tl-row mech op fam-{fam}" data-kind="operation">
												<div class="op-line">
													<span class="side side-{side}" title={SIDE_LABEL[side]}>{side}</span>
													<b class="mono">{e.op}</b>
													<span class="muted">→ {systemOf(e.instance)}</span>
													{#if e.route === 'fallback'}<Chip text="fallback route" variant="warn" />{/if}
													{#if e.fallback_from}<Chip
															text="fallback from {e.fallback_from}"
															variant="warn"
														/>{/if}
													<Chip
														text={FAMILY_LABEL[fam]}
														variant={FAMILY_TONE[fam]}
														title={CALL_STATE_LABEL[e.state]}
													/>
												</div>
												<div class="op-facts">
													{#if e.step}<span
															>step {e.step.n} of
															<span class="mono">{e.step.procedure}@{e.step.version}</span>
															· {e.step.name}</span
														>{/if}
													<span
														>outcome <span class="mono">{e.outcome}</span
														>{#if fam !== 'ok'}{#if oc.code !== e.outcome}
																→ <span class="mono">{oc.code}</span>{/if} — {oc.meaning}{/if}</span
													>
													{#if conf}<span class="ok-txt">✓ confirm-back: customer said “{conf.text}”</span>
													{:else if e.confirm_ref}<span
															>confirm_ref <span class="mono">{e.confirm_ref}</span></span
														>{/if}
													{#if e.object_change}<span
															><b>{e.object_change.object} {e.object_change.ref}</b>: {e.object_change.from ??
																'new'} → {e.object_change.to ?? '? (unknown)'}</span
														>{/if}
													{#if e.detail}<span class="cell-sub">{e.detail}</span>{/if}
												</div>
												<details class="op-more">
													<summary class="cell-sub">call details</summary>
													<div class="op-grid">
														<span class="label">Route</span><span class="mono"
															>{e.route} → {e.instance ?? '—'}</span
														>
														<span class="label">Template</span><span class="mono">{e.template ?? '—'}</span>
														<span class="label">Latency</span><span class="mono">{fmt.ms(e.latency_ms)}</span>
														<span class="label">Call</span><span class="mono">{e.call_id}</span>
													</div>
												</details>
											</li>
										{:else if e.kind === 'reply'}
											{@const aid = replyAnswerId(c.case_id, e.at)}
											<li class="tl-row msg agent" data-kind="reply">
												<span class="who-lbl">Agent replied</span>
												{#if e.used_knowledge}
													<AnswerSources g={e} />
												{:else if e.cites?.length}
													<div class="chips">
														{#each e.cites as k (k)}<Chip text="cites {k}" variant="work" />{/each}
													</div>
												{/if}
												<AnswerFeedback
													answer={{
														surface: 'case',
														conversation_id: c.conversation_id,
														answer_id: aid,
														case_id: c.case_id,
														question: t.opener?.text ?? '',
														answer: e.text,
														grounding: groundingOf(e),
													}}
													initial={mine.get(aid) ?? null}
												/>
											</li>
										{:else if e.kind === 'state'}
											<li class="tl-row mech" data-kind="state">
												<span class="k">Case</span>
												<span
													><b>→ {STATUS_LABEL[e.status]}</b>
													{e.note}{#if e.by}<span class="muted"> · {e.by}</span>{/if}</span
												>
											</li>
										{:else if e.kind === 'alert'}
											<li class="tl-row mech alert" data-kind="alert">
												<span class="k err-txt">Alert</span>
												<span><b class="mono">{e.code}</b> — {e.text}</span>
											</li>
										{/if}
									{/each}
								</ul>
							</li>
						{/each}
					</ol>
				</Card>
			</div>

			<div class="side-col">
				<Card title="How it ended" sub={ending.headline}>
					<p class="small">{ending.why}</p>
					<dl class="facts">
						<dt>Outcome</dt>
						<dd class="mono">{c.outcome ?? '— (open)'}</dd>
						<dt>Escalation</dt>
						<dd>{c.escalation_reason?.replaceAll('_', ' ') ?? 'none'}</dd>
						<dt>Last update</dt>
						<dd>{stamp(c.updated_at)}</dd>
					</dl>
					<h4>Staff</h4>
					{#if c.status === 'handover' || (c.owner && c.owner.owner !== 'agent')}
						<p class="small">
							{c.owner?.owner.startsWith('user:') || (!c.owner && c.assignee)
								? `Held by ${c.owner?.name ?? c.assignee_name ?? c.assignee}.`
								: 'Nobody has taken it yet.'}
							{#if c.sla}<span class="muted">SLA {c.sla.hours} h · due {stamp(c.sla.due_at)}</span>{/if}
						</p>
						{#if c.conversation_id}<a class="btn primary" href={workspaceHref(app.job, c.conversation_id)}
								>Work it in the workspace</a
							>{/if}
					{:else if ['resolved', 'not_eligible', 'cancelled_offboard'].includes(c.status)}
						<p class="muted small">The case is closed; nothing to hand over.</p>
					{:else if handoverOpen}
						<label class="field">
							<span class="label">Why hand over?</span>
							<input class="input" bind:value={reason} placeholder="e.g. customer prefers a phone call" />
						</label>
						<div class="actions">
							<button class="btn primary" type="button" disabled={busy || !admin} onclick={handOver}
								>Hand over to staff</button
							>
							<button class="btn" type="button" onclick={() => (handoverOpen = false)}>Cancel</button>
						</div>
					{:else}
						<button
							class="btn"
							type="button"
							{...writeAttrs(session, 'support_agent')}
							onclick={() => (handoverOpen = true)}>Hand over…</button
						>
						{#if !admin}<span class="cell-sub" data-testid="support-roles-note">{SUPPORT_ROLES_NOTE}</span
							>{/if}
					{/if}
				</Card>

				<Card title="Procedure run" sub="The steps the orchestrator ran for this case">
					{#if procs.length}
						<ul class="plain">
							{#each procs as p (p.procedure + p.version)}
								<li>
									<a class="mono" href={jobPath(app.job, `/procedures/${encodeURIComponent(p.procedure)}`)}
										>{p.procedure}@{p.version}</a
									>
									<span class="cell-sub">steps {p.steps.join(', ')}</span>
								</li>
							{/each}
						</ul>
					{:else}
						<p class="muted small">
							{d.timeline.some((e) => e.kind === 'operation')
								? 'Steps are not recorded for this case.'
								: 'No procedure ran — answered from knowledge or the conversation alone.'}
						</p>
					{/if}
				</Card>

				<Card title="What changed in your systems" sub="Object state changes caused by this case">
					{#if changes.length}
						<ul class="plain" data-testid="object-changes">
							{#each changes as ch, i (i)}
								<li class:unknown={ch.to === null}>
									<b>{ch.object}</b> <span class="mono">{ch.ref}</span><br />
									<span class="mono">{ch.from ?? 'new'} → {ch.to ?? '? unknown'}</span>
									<span class="cell-sub">via {ch.op}</span>
								</li>
							{/each}
						</ul>
					{:else}
						<p class="muted small">
							{d.timeline.some((e) => e.kind === 'operation' && sideEffectOf(e.op) !== 'R')
								? 'Writes ran but their state changes are not recorded for this case.'
								: 'Reads only — nothing was changed.'}
						</p>
					{/if}
				</Card>

				<Card title="Knowledge cited" sub="Passages the agent read before answering">
					{#if passages.length}
						<ul class="plain">
							{#each passages as p (p.title)}
								<li>{p.title} <span class="cell-sub mono">{p.source}</span></li>
							{/each}
						</ul>
					{:else}
						<p class="muted small">No knowledge lookups — facts came from operations.</p>
					{/if}
				</Card>

				<Card title="Raw envelope" sub="Ids and codes only — no message text or personal data (K50)">
					<details>
						<summary>Show JSON</summary>
						<JsonView value={d.envelope} />
					</details>
				</Card>
			</div>
		</div>
	{/snippet}
</LoadState>

<style>
	.story {
		display: grid;
		grid-template-columns: minmax(0, 1fr) 280px;
		gap: 16px;
		padding: 16px;
		margin-bottom: 14px;
		background: var(--panel);
		border: 1px solid var(--line);
		border-left: 4px solid var(--muted);
		border-radius: var(--r);
	}
	.story.end-contained {
		border-left-color: var(--ok);
	}
	.story.end-handover,
	.story.end-waiting {
		border-left-color: var(--warn);
	}
	.story.end-alert {
		border-left-color: var(--err);
	}
	.story-main {
		display: grid;
		gap: 8px;
		min-width: 0;
	}
	.story-who {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
		align-items: center;
		font-size: 13px;
	}
	.story-title {
		margin: 0;
		font-size: 20px;
	}
	.story-end {
		margin: 0;
		max-width: 70ch;
	}
	.path {
		list-style: none;
		margin: 4px 0 0;
		padding: 0;
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		align-items: center;
	}
	.path-step a,
	.path-step > span {
		display: inline-flex;
		gap: 6px;
		align-items: center;
		padding: 3px 10px 3px 4px;
		border: 1px solid var(--line);
		border-radius: 999px;
		font-size: 12px;
		color: var(--fg);
		text-decoration: none;
		background: var(--sunk);
	}
	.path-step:not(:last-child)::after {
		content: '→';
		margin-left: 6px;
		color: var(--muted);
	}
	.path-n {
		display: inline-grid;
		place-items: center;
		width: 20px;
		height: 20px;
		border-radius: 50%;
		background: var(--panel);
		border: 2px solid var(--ok);
		font-family: var(--mono);
		font-size: 11px;
	}
	.path-step.fam-none .path-n,
	.path-step.fam-user .path-n {
		border-color: var(--line);
	}
	.path-step.fam-gate .path-n {
		border-color: var(--warn);
	}
	.path-step.fam-system .path-n,
	.path-step.fam-unknown .path-n {
		border-color: var(--err);
	}
	.path-end > span {
		padding-left: 10px;
		font-weight: 600;
	}
	.path-end.end-alert > span {
		border-color: var(--err);
		color: var(--err);
	}
	.path-end.end-contained > span {
		border-color: var(--ok);
		color: var(--ok);
	}
	.path-end.end-handover > span {
		border-color: var(--warn);
		color: var(--warn);
	}
	.ranon {
		display: grid;
		gap: 6px;
		align-content: start;
		padding-left: 16px;
		border-left: 1px solid var(--line);
		min-width: 0;
	}
	.ranon-env {
		display: flex;
		gap: 6px;
		align-items: center;
		font-size: 12px;
	}
	.ranon-rel {
		font-weight: 600;
		overflow-wrap: anywhere;
	}
	.revs {
		display: flex;
		flex-wrap: wrap;
		gap: 4px 12px;
		font-size: 12px;
	}
	.ranon p {
		margin: 0;
	}
	.todo {
		margin-bottom: 14px;
		padding: 14px 16px;
		border: 1px solid var(--err);
		border-radius: var(--r);
		background: color-mix(in srgb, var(--err) 6%, var(--panel));
	}
	.todo-head {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
		align-items: center;
	}
	.todo-head h3 {
		margin: 0;
		font-size: 16px;
	}
	.todo-grid {
		display: grid;
		grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
		gap: 20px;
		margin-top: 10px;
	}
	.todo-grid p {
		margin: 2px 0 10px;
	}
	.todo-steps {
		margin: 4px 0 10px;
		padding-left: 18px;
		display: grid;
		gap: 4px;
	}
	.layout {
		display: grid;
		grid-template-columns: minmax(0, 1fr) 300px;
		gap: 14px;
		align-items: start;
	}
	.main-col {
		display: grid;
		gap: 14px;
		min-width: 0;
	}
	.conv-pane {
		height: 420px;
	}
	.side-col {
		display: grid;
		gap: 14px;
		min-width: 0;
	}
	.turns {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 18px;
	}
	.turn-head {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
		align-items: center;
		padding-bottom: 6px;
		border-bottom: 1px solid var(--line-2);
	}
	.turn-n {
		font-weight: 700;
	}
	.steps {
		list-style: none;
		margin: 8px 0 0;
		padding: 0;
		display: grid;
		gap: 8px;
	}
	.tl-row.msg {
		display: grid;
		gap: 2px;
		justify-items: start;
	}
	.tl-row.msg.agent {
		justify-items: end;
		text-align: left;
	}
	.who-lbl {
		font-size: 11px;
		color: var(--muted);
	}
	.tl-row.mech {
		margin-left: 18px;
		padding: 6px 10px;
		border-left: 3px solid var(--line);
		background: var(--sunk);
		border-radius: 0 var(--r) var(--r) 0;
		display: grid;
		gap: 4px;
		font-size: 13px;
		min-width: 0;
	}
	.tl-row.mech[data-kind='gate'] {
		border-left-color: var(--accent);
	}
	.tl-row.mech[data-kind='knowledge'] {
		border-left-color: var(--work);
	}
	.tl-row.op {
		border-left-color: var(--tool);
	}
	.tl-row.op.fam-unknown,
	.tl-row.op.fam-system,
	.tl-row.alert {
		border-left-color: var(--err);
		background: color-mix(in srgb, var(--err) 6%, var(--sunk));
	}
	.tl-row.op.fam-gate {
		border-left-color: var(--warn);
	}
	.k {
		font-size: 11px;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		color: var(--muted);
		margin-right: 4px;
	}
	summary {
		cursor: pointer;
	}
	.mech summary {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		align-items: center;
	}
	.note {
		margin: 0;
		font-size: 12px;
		color: var(--muted);
	}
	.op-line {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		align-items: center;
	}
	.side {
		display: inline-grid;
		place-items: center;
		min-width: 20px;
		height: 20px;
		padding: 0 4px;
		border-radius: 4px;
		font-family: var(--mono);
		font-size: 11px;
		font-weight: 700;
		border: 1px solid var(--line);
		background: var(--panel);
	}
	.side-W {
		border-color: var(--warn);
		color: var(--warn);
	}
	.side-X,
	.side-P {
		border-color: var(--err);
		color: var(--err);
	}
	.op-facts {
		display: grid;
		gap: 2px;
		padding-left: 26px;
		overflow-wrap: anywhere;
	}
	.op-more {
		padding-left: 26px;
	}
	.ok-txt {
		color: var(--ok);
	}
	.err-txt {
		color: var(--err);
	}
	.verdicts-wrap {
		max-width: 640px;
		margin-top: 6px;
	}
	.verdicts td {
		overflow-wrap: anywhere;
	}
	.conf {
		display: inline-flex;
		gap: 8px;
		align-items: center;
		min-width: 90px;
	}
	.passages {
		margin: 0;
		padding-left: 18px;
		display: grid;
		gap: 6px;
	}
	.passages p {
		margin: 2px 0;
	}
	.op-grid {
		display: grid;
		grid-template-columns: max-content minmax(0, 1fr);
		gap: 2px 12px;
		align-items: baseline;
		margin-top: 4px;
	}
	.facts {
		display: grid;
		grid-template-columns: max-content minmax(0, 1fr);
		gap: 2px 10px;
		margin: 6px 0 10px;
		font-size: 13px;
	}
	.facts dt {
		color: var(--muted);
	}
	.facts dd {
		margin: 0;
	}
	h4 {
		margin: 8px 0 4px;
		font-size: 13px;
	}
	.small {
		font-size: 13px;
		margin: 0 0 6px;
	}
	.plain {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 8px;
		font-size: 13px;
	}
	.plain li.unknown {
		color: var(--err);
	}
	@media (max-width: 1100px) {
		.layout {
			grid-template-columns: 1fr;
		}
	}
	@media (max-width: 760px) {
		.story,
		.todo-grid {
			grid-template-columns: 1fr;
		}
		.ranon {
			padding-left: 0;
			border-left: 0;
			border-top: 1px solid var(--line);
			padding-top: 10px;
		}
		.tl-row.mech {
			margin-left: 0;
		}
	}
</style>
