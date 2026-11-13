<script lang="ts">
	// J17 knowledge answers per release cut: the BMO validation grid as a product (10§6.2, r7 R3). Per release cut,
	// every knowledge question is answered and graded: question, expected answer, reference citation, response,
	// verdict. Answers with a missing or wrong citation are rejected. Secondary to the draft test (J17DraftTest).
	import { onDestroy, onMount } from 'svelte';
	import { api, app, guard } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { writeHint, canWrite, writeAttrs } from '$lib/auth/roles';
	import type { RejectReason, ValidationCut, ValidationRow } from '$lib/api/types/journey';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Card from '$lib/ui/Card.svelte';
	import LiveNote from '$lib/release/LiveNote.svelte';
	import Tabs from '$lib/ui/Tabs.svelte';
	import Kpi from '$lib/ui/Kpi.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import { journeyUi, when } from '$lib/journey/format';
	import J17QuestionEditors from './J17QuestionEditors.svelte';

	const res = new Resource(() => api().validationCuts());
	let active = $state('');
	onMount(async () => {
		await res.refresh();
		const v = res.value;
		if (v.state === 'ok') active = v.data.items[0]!.release.id;
	});
	const base = $derived(`/admin/jobs/${app.job}`);

	const REASON: Record<Exclude<RejectReason, null>, string> = {
		missing_citation: 'missing citation',
		wrong_citation: 'wrong citation',
		incorrect: 'incorrect answer',
	};
	const STATE: Record<ValidationCut['state'], string> = {
		current: 'current',
		approved: 'approved, not applied',
		applied_earlier: 'applied earlier',
	};

	let running = $state('');
	let timer: ReturnType<typeof setTimeout> | undefined;
	onDestroy(() => clearTimeout(timer));
	function runCut(id: string) {
		running = id;
		timer = setTimeout(async () => {
			await guard(() => api().runValidationCut(id), `Graded every question on ${id}.`, 'Not run');
			running = '';
			await res.refresh();
		}, journeyUi.draftMs);
	}

	let rejecting = $state('');
	let reason = $state<Exclude<RejectReason, null>>('missing_citation');
	async function verdict(cut: string, row: string, v: 'pass' | 'rejected') {
		const ok = await guard(
			() => api().setValidationVerdict(cut, row, v, v === 'rejected' ? reason : null),
			v === 'pass' ? 'Marked pass.' : `Rejected: ${REASON[reason]}.`,
		);
		if (ok) {
			rejecting = '';
			await res.refresh();
		}
	}

	let addOpen = $state(false);
	let q = $state('');
	let exp = $state('');
	let ref = $state('');
	let actv = $state('orientation:ask_policy_question');
	async function addQuestion() {
		if (
			await guard(
				() => api().addValidationQuestion({ question: q, expected: exp, reference: ref, activity: actv }),
				'Question added to the open cuts.',
			)
		) {
			addOpen = false;
			q = exp = ref = '';
			await res.refresh();
		}
	}

	// ---- F-50 (B8 T12a): edit / delete a question, import a CSV ----
	let editing = $state<ValidationRow | null>(null);
	let importOpen = $state(false);
	let deleting = $state('');
	async function removeQuestion(r: ValidationRow) {
		if (
			await guard(
				() => api().deleteValidationQuestion(r.id),
				'Question deleted. Earlier cuts keep it.',
				'Not deleted',
			)
		) {
			deleting = '';
			await res.refresh();
		}
	}
	const count = (c: ValidationCut, v: string) => c.rows.filter((r) => r.verdict === v).length;
</script>

<section class="kq" aria-labelledby="kq-h">
	<div class="kq-head">
		<div>
			<h2 id="kq-h">Knowledge answers per release</h2>
			<p class="muted kq-sub">
				Each release cut answers the same knowledge questions your SMEs signed off. A reply is rejected when
				its citation is missing or wrong (post-gate F2), or the answer is incorrect.
			</p>
		</div>
		<div class="row">
			<button
				class="btn"
				type="button"
				{...writeAttrs(session, 'job_designer')}
				onclick={() => (importOpen = true)}>Import CSV</button
			>
			<button
				class="btn"
				type="button"
				{...writeAttrs(session, 'job_designer')}
				onclick={() => (addOpen = true)}>Add question</button
			>
		</div>
	</div>

	<LiveNote
		what="Validation questions, gradings and verdicts are test data: they measure a cut, they do not change it."
	/>

	<LoadState resource={res}>
		{#snippet empty()}
			<div data-testid="no-release">
				No release yet. Approve one on the <a href="{base}/release">Release</a> page; its cut is graded here.
			</div>
		{/snippet}
		{#snippet children(page)}
			{@const cut = page.items.find((c) => c.release.id === active) ?? page.items[0]!}
			<Tabs
				items={page.items.map(
					(c) => [c.release.id, `${c.release.id} · ${STATE[c.state]}`] as [string, string],
				)}
				bind:active
				label="Release cuts"
			/>
			<div class="stack top">
				<div class="grid g4">
					<Kpi label="Pass" value={String(count(cut, 'pass'))} note="of {cut.rows.length} questions" />
					<Kpi
						label="Rejected"
						value={String(count(cut, 'rejected'))}
						note="{cut.rows.filter((r) => r.reason === 'missing_citation' || r.reason === 'wrong_citation')
							.length} for citations"
					/>
					<Kpi label="Not graded" value={String(count(cut, 'pending'))} />
					<Kpi
						label="Last run"
						value={cut.ran_at ? when(cut.ran_at).slice(5) : null}
						note={cut.ran_by ? `by ${cut.ran_by}` : ''}
						unavailable={cut.ran_at ? '' : 'never run on this cut'}
					/>
				</div>
				<Card cls="flush" title="Cut {cut.release.id}" sub="bundle {cut.release.hash}">
					{#snippet actions()}
						<button
							class="btn primary"
							type="button"
							disabled={!!running || !canWrite(session, 'release_manager')}
							title={canWrite(session, 'release_manager') ? undefined : writeHint('release_manager')}
							onclick={() => runCut(cut.release.id)}
						>
							{#if running === cut.release.id}<span class="spinner" aria-hidden="true"></span> Running…{:else}Run
								on cut{/if}
						</button>
					{/snippet}
					{#if running === cut.release.id}
						<p class="running" role="status">
							Answering {cut.rows.length} questions against {cut.release.id}; LLM-as-judge grades citations…
						</p>
					{/if}
					{#if !cut.rows.length}
						<div class="empty">No questions on this cut yet. Add the questions your SMEs sign off on.</div>
					{:else}
						<div class="tbl-wrap">
							<table class="tbl grid-tbl" data-testid="validation-rows">
								<thead>
									<tr>
										<th>Question</th>
										<th>Expected answer</th>
										<th>Reference</th>
										<th>Response</th>
										<th>Verdict</th>
										<th></th>
									</tr>
								</thead>
								<tbody>
									{#each cut.rows as r (r.id)}
										<tr class:rej={r.verdict === 'rejected'}>
											<td
												><div class="cell-title">{r.question}</div>
												<div class="cell-sub mono">{r.activity}</div></td
											>
											<td>{r.expected}</td>
											<td class="mono small">{r.reference}</td>
											<td>
												{#if r.response}
													{r.response}
													<div class="chips cited">
														{#each r.cited as c (c)}<span class="chip {c === r.reference ? 'ok' : 'warn'}"
																>{c}</span
															>{:else}<span class="chip err">no citation</span>{/each}
													</div>
												{:else}<span class="muted">Not run on this cut</span>{/if}
											</td>
											<td>
												<span
													class="chip {r.verdict === 'pass'
														? 'ok'
														: r.verdict === 'rejected'
															? 'err'
															: 'muted'}">{r.verdict}</span
												>
												{#if r.reason}<div class="cell-sub err-text">{REASON[r.reason]}</div>{/if}
												{#if r.judged_by}<div class="cell-sub">
														by {r.judged_by === 'judge' ? 'LLM judge' : r.judged_by}
													</div>{/if}
											</td>
											<td>
												<div class="row nowrap q-actions" data-testid="question-actions-{r.id}">
													{#if deleting === r.id}
														<button
															class="btn sm danger"
															type="button"
															aria-label="Confirm delete of question {r.question}"
															onclick={() => removeQuestion(r)}>Delete question</button
														>
														<button class="btn sm" type="button" onclick={() => (deleting = '')}>Keep</button>
													{:else}
														<button
															class="btn sm"
															type="button"
															aria-label="Edit question {r.question}"
															{...writeAttrs(session, 'job_designer')}
															onclick={() => (editing = r)}>Edit</button
														>
														<button
															class="btn sm"
															type="button"
															aria-label="Delete question {r.question}"
															{...writeAttrs(session, 'job_designer')}
															onclick={() => (deleting = r.id)}>Delete</button
														>
													{/if}
												</div>
												{#if r.response}
													{#if rejecting === r.id}
														<div class="stack tight">
															<select class="input" aria-label="Reject reason" bind:value={reason}>
																{#each Object.entries(REASON) as [k, l] (k)}<option value={k}>{l}</option
																	>{/each}
															</select>
															<div class="row">
																<button
																	class="btn sm danger"
																	type="button"
																	onclick={() => verdict(cut.release.id, r.id, 'rejected')}>Reject</button
																>
																<button class="btn sm" type="button" onclick={() => (rejecting = '')}
																	>Cancel</button
																>
															</div>
														</div>
													{:else}
														<div class="row nowrap">
															<button
																class="btn sm"
																type="button"
																{...writeAttrs(session, 'release_manager')}
																onclick={() => verdict(cut.release.id, r.id, 'pass')}>Pass</button
															>
															<button
																class="btn sm"
																type="button"
																{...writeAttrs(session, 'release_manager')}
																onclick={() => (rejecting = r.id)}>Reject…</button
															>
														</div>
													{/if}
												{/if}
											</td>
										</tr>
									{/each}
								</tbody>
							</table>
						</div>
					{/if}
				</Card>
			</div>
		{/snippet}
	</LoadState>
</section>

<J17QuestionEditors bind:editing bind:importOpen onchange={() => res.refresh()} />

<Drawer bind:open={addOpen} title="Add validation question">
	<div class="stack">
		<div class="field"><label for="vq">Question</label><input id="vq" class="input" bind:value={q} /></div>
		<div class="field">
			<label for="ve">Expected answer</label><textarea id="ve" class="input" rows="3" bind:value={exp}
			></textarea>
		</div>
		<div class="field">
			<label for="vr">Reference citation</label><input
				id="vr"
				class="input"
				placeholder="policy.returns §2"
				bind:value={ref}
			/>
		</div>
		<div class="field">
			<label for="va">Activity</label><input id="va" class="input mono" bind:value={actv} />
		</div>
		<p class="note">Added to the current and approved cuts; earlier cuts keep their graded set.</p>
		<div class="row">
			<button class="btn primary" type="button" {...writeAttrs(session, 'job_designer')} onclick={addQuestion}
				>Add question</button
			>
			<button class="btn" type="button" onclick={() => (addOpen = false)}>Cancel</button>
		</div>
	</div>
</Drawer>

<style>
	.kq {
		margin-top: 28px;
	}
	.kq-head {
		display: flex;
		flex-wrap: wrap;
		gap: 8px 16px;
		align-items: flex-start;
		justify-content: space-between;
		margin-bottom: 8px;
	}
	.kq-head h2 {
		font-size: 17px;
		margin: 0 0 2px;
	}
	.kq-sub {
		margin: 0;
		max-width: 70ch;
		font-size: 13px;
	}
	.top {
		margin-top: 12px;
	}
	.small {
		font-size: 12px;
	}
	.grid-tbl td {
		vertical-align: top;
		font-size: 13px;
	}
	tr.rej td {
		background: color-mix(in srgb, var(--err) 5%, transparent);
	}
	.cited {
		margin-top: 4px;
	}
	.err-text {
		color: var(--err);
	}
	.q-actions {
		margin-bottom: 6px;
	}
	.nowrap {
		flex-wrap: nowrap;
	}
	.tight {
		gap: 6px;
	}
	.running {
		color: var(--accent);
		margin: 0 16px 8px;
	}
	.spinner {
		display: inline-block;
		width: 12px;
		height: 12px;
		border: 2px solid color-mix(in srgb, currentColor 30%, transparent);
		border-top-color: currentColor;
		border-radius: 50%;
		animation: spin 0.8s linear infinite;
		vertical-align: -2px;
	}
	@keyframes spin {
		to {
			transform: rotate(360deg);
		}
	}
</style>
