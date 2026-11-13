<script lang="ts">
	// Retrieval eval (T7's second route). Design intent: the tenant_admin answers "does search find the right passage
	// for the questions customers actually ask, and did my last change make it better or worse?". A golden question
	// says what a good result is — a document, the text a passage must contain, or document / chunk ids (any one
	// matching is a hit) — and optionally the leaf store it lives in (leaf accuracy only). "Run eval" proxies strata_rag
	// `POST /api/eval {source:"tenant", modes}` (at most 200 questions): recall@1/5/10, leaf accuracy and laya counts
	// per mode, a PASS/FAIL verdict (routed modes vs flat) and every question's rank. strata_rag checks leaves and
	// documents only when a run starts (409 `eval_not_ready`, listed here). Golden questions are tenant data (written
	// straight, not D54 draft config); eval runs are operational records. `?add=<question>` opens the form prefilled.
	// Runs are asynchronous (strata_rag ASYNC RUNS): the start answers 202 with the run `running`, and the page polls
	// `GET /api/eval/runs/{id}` every 2 s for its progress until it completes or fails. A running run stays in the
	// list, so leaving the page and coming back resumes the polling; one run per tenant at a time (409 `eval_running`).
	import '$lib/phone-stack.css';
	import { onDestroy, onMount, untrack } from 'svelte';
	import { api, guard, guardOutcome } from '$lib/app.svelte';
	import { toast } from '$lib/ui/toaster.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { canWrite as canWriteFor, writeAttrs } from '$lib/auth/roles';
	import { Resource } from '$lib/load.svelte';
	import { fmt } from '$lib/format';
	import { ApiError } from '$lib/api/errors';
	import type { Paged } from '$lib/api/types/core';
	import type { KnowledgeStore } from '$lib/api/types/tenant';
	import type {
		CompletedEvalRun,
		EvalMode,
		EvalRunDetail,
		EvalRunQuestion,
		EvalRunSummary,
		GoldenQuestion,
		GoldenQuestionInput,
	} from '$lib/api/types/retrieval';
	import { EVAL_DEFAULT_MODES, EVAL_MODES, isCompletedRun } from '$lib/api/types/retrieval';
	import { when } from '$lib/tenant/ui';
	import {
		EVAL_MAX_QUESTIONS,
		EVAL_POLL_MAX_FAILS,
		EVAL_POLL_MS,
		pollDelay,
		headline,
		progressText,
		notReadyLines,
		parseIds,
		questionKey,
		questionPatch,
		questionProblems,
	} from '$lib/tenant/retrieval';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LiveNote from '$lib/release/LiveNote.svelte';
	import Freshness from '$lib/ui/Freshness.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Kpi from '$lib/ui/Kpi.svelte';
	import Meter from '$lib/ui/Meter.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';
	import EvalReports from './EvalReports.svelte';
	import RunDiff from './RunDiff.svelte';
	import CsvImport from './CsvImport.svelte';
	import RunQuestions from './RunQuestions.svelte';

	const questions = new Resource<Paged<GoldenQuestion>>(() => api().goldenQuestions());
	const runs = new Resource<Paged<EvalRunSummary>>(() => api().evalRuns());
	const stores = new Resource<Paged<KnowledgeStore>>(() => api().knowledgeStores());
	const canWrite = $derived(canWriteFor(session, 'knowledge_manager'));

	const qs = $derived(questions.value.state === 'ok' ? questions.value.data.items : []);
	/** Older pages of the run history ("Load older runs"); cleared whenever the first page is reloaded. */
	let older = $state<EvalRunSummary[]>([]);
	let olderNext = $state<string | null>(null);
	const firstNext = $derived(runs.value.state === 'ok' ? runs.value.data.next : null);
	const nextCursor = $derived(older.length ? olderNext : firstNext);
	/** The polled run's latest read: it overlays that run's (older) summary in the list. */
	let live = $state<EvalRunSummary | null>(null);
	const runList = $derived(
		[...(runs.value.state === 'ok' ? runs.value.data.items : []), ...older]
			.filter((r) => !gone.includes(r.id))
			.map((r) => (live && r.id === live.id ? live : r)),
	);
	/** Runs that finished scoring: the only ones with numbers to show, compare and diff. */
	const doneRuns = $derived(runList.filter((r): r is CompletedEvalRun => isCompletedRun(r)));
	/** The tenant's running run, if any (strata_rag runs one at a time). */
	const runningRun = $derived(runList.find((r) => r.status === 'running') ?? null);
	/** The latest started run, when it failed. */
	const lastFailed = $derived(runList[0]?.status === 'failed' ? runList[0] : null);
	/** D91: with more than one store tree the run names its root (else 422 `root_required`). */
	const roots = $derived(
		stores.value.state === 'ok' ? stores.value.data.items.filter((s) => !s.parent_id) : [],
	);
	let evalRoot = $state('');
	const needRoot = $derived(roots.length > 1);
	const leaves = $derived(
		stores.value.state === 'ok' ? stores.value.data.items.filter((s) => s.kind === 'leaf') : [],
	);

	let shownId = $state<string | null>(null);
	const shown = $derived(runList.find((r) => r.id === shownId) ?? doneRuns[0] ?? runList[0] ?? null);
	const shownDone = $derived(shown && isCompletedRun(shown) ? shown : null);
	const prevOf = (r: CompletedEvalRun | null) => (r ? (doneRuns[doneRuns.indexOf(r) + 1] ?? null) : null);
	const latest = $derived(doneRuns[0] ?? null);
	const latestHead = $derived(headline(latest));
	const prevHead = $derived(headline(prevOf(latest)));
	/**
	 * Run snapshots by id (`GET /api/eval/runs/{id}`: questions + per-question results; the list carries summaries).
	 * A finished run never changes, so each is read once (only finished runs are read through here; a running one is
	 * polled); a failed read is forgotten so the next look retries.
	 */
	const details = new Map<string, Promise<EvalRunDetail>>();
	function detailOf(id: string): Promise<EvalRunDetail> {
		let p = details.get(id);
		if (!p) {
			p = api().evalRun(id);
			details.set(id, p);
			p.catch(() => details.delete(id));
		}
		return p;
	}
	/** The latest run's question snapshot, for the "set changed" note. */
	let latestQs = $state<{ id: string; questions: EvalRunQuestion[] } | null>(null);
	$effect(() => {
		const id = latest?.id;
		if (!id || latestQs?.id === id) return;
		detailOf(id)
			.then((d) => {
				if (latest?.id === id) latestQs = { id, questions: d.questions };
			})
			.catch(() => {});
	});
	/** The set changed since the latest run: its numbers describe other questions. */
	const stale = $derived.by(() => {
		if (!latest || latestQs?.id !== latest.id) return false;
		const a = new Set(latestQs.questions.map(questionKey));
		return qs.length !== a.size || qs.some((q) => !a.has(questionKey(q)));
	});

	async function loadOlder() {
		if (!nextCursor) return;
		try {
			const page = await api().evalRuns({ cursor: nextCursor });
			older = [...older, ...page.items];
			olderNext = page.next;
		} catch (e) {
			toast(`Older runs not loaded: ${(e as Error).message}`, 'err');
		}
	}
	async function refreshRuns() {
		older = [];
		olderNext = null;
		await runs.refresh();
	}

	// ---- add / edit ----
	interface Form {
		query: string;
		expected_leaf: string;
		expected_document: string;
		expected_text: string;
		document_ids: string;
		chunk_ids: string;
	}
	const blank = (): Form => ({
		query: '',
		expected_leaf: '',
		expected_document: '',
		expected_text: '',
		document_ids: '',
		chunk_ids: '',
	});
	const formOf = (q: Partial<GoldenQuestionInput>): Form => ({
		query: q.query ?? '',
		expected_leaf: q.expected_leaf ?? '',
		expected_document: q.expected_document ?? '',
		expected_text: q.expected_text ?? '',
		document_ids: (q.expected_document_ids ?? []).join('; '),
		chunk_ids: (q.expected_chunk_ids ?? []).join('; '),
	});
	const inputOf = (f: Form): GoldenQuestionInput => ({
		query: f.query,
		expected_leaf: f.expected_leaf,
		expected_document: f.expected_document,
		expected_text: f.expected_text,
		expected_document_ids: parseIds(f.document_ids),
		expected_chunk_ids: parseIds(f.chunk_ids),
	});
	let form = $state<Form>(blank());
	let editing = $state<GoldenQuestion | null>(null);
	let formOpen = $state(false);
	let moreOpen = $state(false);
	let confirmDel = $state<GoldenQuestion | null>(null);
	let docTitles = $state<string[]>([]);
	const touched = $derived(Object.values(form).some((v) => v.trim()));
	/** strata_rag's validation (422 `invalid_input`) plus its 409 `question_exists`, before anything is sent. */
	const formProblems = $derived.by(() => {
		const p = questionProblems(inputOf(form));
		const q = form.query.trim().toLowerCase();
		if (q && qs.some((x) => x.id !== editing?.id && x.query.toLowerCase() === q))
			p.push('this question is already in the set');
		return p;
	});
	const full = $derived(qs.length >= EVAL_MAX_QUESTIONS);

	/** Suggest the chosen leaf's documents for "expected document". */
	async function loadDocs(leafKey: string) {
		const s = leaves.find((x) => x.key === leafKey);
		docTitles = [];
		if (!s) return;
		try {
			docTitles = (await api().knowledgeDocuments(s.id)).items.map((d) => d.title);
		} catch {
			docTitles = [];
		}
	}
	function openForm(prefill: Partial<GoldenQuestionInput> = {}, row: GoldenQuestion | null = null) {
		form = formOf(prefill);
		editing = row;
		formOpen = true;
		moreOpen = !!(form.document_ids || form.chunk_ids);
		if (form.expected_leaf) void loadDocs(form.expected_leaf);
		requestAnimationFrame(() => document.getElementById('ev-form')?.scrollIntoView?.({ block: 'center' }));
	}
	function closeForm() {
		form = blank();
		editing = null;
		formOpen = false;
	}
	async function save(e: SubmitEvent) {
		e.preventDefault();
		const input = inputOf(form);
		const row = editing;
		if (row) {
			// PATCH semantics: send only what changed; a cleared field goes as null (an id list as []).
			const patch = questionPatch(row, input);
			if (!Object.keys(patch).length) {
				closeForm();
				return;
			}
			if (
				await guard(() => api().updateGoldenQuestion(row.id, patch), 'Golden question saved.', 'Not saved')
			) {
				closeForm();
				await questions.refresh();
			}
			return;
		}
		const ok = await guard(
			() => api().createGoldenQuestion(input),
			'Added to the golden set. Run the eval to include it.',
			'Not added',
		);
		if (ok) {
			closeForm();
			await questions.refresh();
		}
	}
	async function remove(q: GoldenQuestion) {
		confirmDel = null;
		if (await guard(() => api().deleteGoldenQuestion(q.id), 'Golden question deleted.', 'Not deleted'))
			await questions.refresh();
	}

	// ---- run ----
	let modes = $state<EvalMode[]>([...EVAL_DEFAULT_MODES]);
	const toggleMode = (m: EvalMode, on: boolean) => {
		modes = EVAL_MODES.filter((x) => (x === m ? on : modes.includes(x)));
	};
	/** The start request is in flight. */
	let starting = $state(false);
	const busy = $derived(starting || runningRun != null);
	const RUN_BUSY_REASON = 'An eval is running: strata_rag runs one eval per tenant at a time.';
	/** The lines of strata_rag's last 409 `eval_not_ready`: what is missing before the set can run. */
	let notReady = $state<string[]>([]);
	async function runEval() {
		starting = true;
		notReady = [];
		goneNote = null;
		const box: { run: EvalRunSummary | null } = { run: null };
		const out = await guardOutcome(
			async () => {
				box.run = await api().runEval({
					modes: [...modes],
					...(evalRoot ? { root_id: evalRoot } : {}),
				});
			},
			() => `Eval started over ${box.run?.question_count ?? 0} questions; progress shows here.`,
			'Eval not started',
		);
		if (out.kind === 'ok') {
			if (box.run) {
				live = box.run;
				shownId = box.run.id;
			}
			await refreshRuns();
		} else if (out.error instanceof ApiError && out.error.code === 'eval_not_ready') {
			// strata_rag's problem lines (agent-api `upstream::refusal` → `problems`); agent's own refusal is one line
			const problems = out.error.details.problems;
			notReady =
				Array.isArray(problems) && problems.length
					? problems.filter((x): x is string => typeof x === 'string')
					: notReadyLines(out.error.message);
		} else if (out.error instanceof ApiError && out.error.code === 'eval_running') {
			// Started elsewhere (another tab or admin): show it and follow its progress.
			await refreshRuns();
		}
		starting = false;
	}

	// ---- poll a running run (every EVAL_POLL_MS until it completes or fails) ----
	const summaryOfDetail = ({ questions, results: _r, ...head }: EvalRunDetail): EvalRunSummary => ({
		...head,
		question_count: questions.length,
	});
	let polling: string | null = null;
	let pollTimer: ReturnType<typeof setTimeout> | undefined;
	let destroyed = false;
	/** The last poll failed (a transient read error): the run goes on server-side, the page retries with backoff. */
	let pollError = $state<string | null>(null);
	/** Consecutive failed polls of the watched run; at EVAL_POLL_MAX_FAILS the page stops and offers a retry. */
	let pollFails = $state(0);
	/** Polling stopped after too many failed reads: the id of the run to resume on "Retry". */
	let pollStopped = $state<string | null>(null);
	/** Runs a poll answered 404 for (deleted, or another strata_rag): never polled again, hidden from the list. */
	let gone = $state<string[]>([]);
	/** The run that just turned out gone, for the "no longer exists" note (cleared when a new run starts). */
	let goneNote = $state<string | null>(null);
	function watch(id: string) {
		if (polling === id || gone.includes(id) || pollStopped === id) return;
		polling = id;
		pollFails = 0;
		pollError = null;
		clearTimeout(pollTimer);
		const tick = async () => {
			if (destroyed || polling !== id) return;
			try {
				const d = await api().evalRun(id);
				if (destroyed || polling !== id) return;
				pollError = null;
				live = summaryOfDetail(d);
				if (d.status === 'running') {
					pollTimer = setTimeout(tick, EVAL_POLL_MS);
					return;
				}
				polling = null;
				pollFails = 0;
				if (d.status === 'completed') {
					details.set(id, Promise.resolve(d));
					toast(
						`Eval done (${d.verdict}): hybrid recall@5 ${headline(d)?.toFixed(2) ?? '—'} over ${d.questions.length} questions.`,
					);
				} else {
					toast(`Eval failed: ${d.error ?? 'no reason given'}`, 'err');
				}
				shownId = id;
				await refreshRuns();
				live = null;
			} catch (e) {
				if (destroyed || polling !== id) return;
				if (e instanceof ApiError && e.status === 404) {
					// The run no longer exists: polling it again can only 404 again.
					polling = null;
					pollError = null;
					gone = [...gone, id];
					goneNote = id;
					if (live?.id === id) live = null;
					await refreshRuns();
					return;
				}
				pollError = (e as Error).message;
				pollFails += 1;
				if (pollFails >= EVAL_POLL_MAX_FAILS) {
					polling = null;
					pollStopped = id;
					return;
				}
				pollTimer = setTimeout(tick, pollDelay(pollFails));
			}
		};
		pollTimer = setTimeout(tick, EVAL_POLL_MS);
	}
	/** "Retry" after polling stopped: read the run again now, with a fresh failure budget. */
	function resumePolling() {
		const id = pollStopped;
		if (!id) return;
		pollStopped = null;
		watch(id);
	}
	$effect(() => {
		const id = runningRun?.id;
		if (id) untrack(() => watch(id));
	});
	onDestroy(() => {
		destroyed = true;
		clearTimeout(pollTimer);
	});
	const runLabel = (r: EvalRunSummary) =>
		r.status === 'completed'
			? (r.verdict ?? '')
			: r.status === 'running'
				? `running ${r.progress.done}/${r.progress.total}`
				: 'failed';

	/** One line of what a question counts as a hit. */
	function expects(q: GoldenQuestion): string {
		const parts: string[] = [];
		if (q.expected_document) parts.push(q.expected_document);
		if (q.expected_text) parts.push(`text “${q.expected_text}”`);
		if (q.expected_document_ids.length)
			parts.push(
				`${q.expected_document_ids.length} document id${q.expected_document_ids.length === 1 ? '' : 's'}`,
			);
		if (q.expected_chunk_ids.length)
			parts.push(`${q.expected_chunk_ids.length} chunk id${q.expected_chunk_ids.length === 1 ? '' : 's'}`);
		return parts.join(' · ');
	}

	onMount(async () => {
		await Promise.all([questions.refresh(), runs.refresh(), stores.refresh()]);
		const add = new URLSearchParams(window.location.search).get('add');
		if (add?.trim()) openForm({ query: add.trim() });
	});

	const playHref = (q: string) => `/admin/knowledge/stores?q=${encodeURIComponent(q)}`;
	const delta = (a: number | null, b: number | null) => (a == null || b == null ? null : a - b);
	const MODE_LABEL: Record<EvalMode, string> = {
		embed: 'embed',
		laya: 'laya (paid calls)',
		hybrid: 'hybrid',
		flat: 'flat (baseline)',
	};
</script>

<PageHead
	id="T7"
	title="Retrieval eval"
	sub="Does search find the right passage? Keep a set of golden questions with what a good result is, run them through the routing modes, and compare runs."
>
	{#snippet actions()}
		<Freshness resource={runs} />
		<a class="btn" href="/admin/knowledge/stores#t7-playground">Search preview</a>
		{#if needRoot}
			<select class="input" aria-label="Store tree to evaluate" bind:value={evalRoot}>
				<option value="">Pick a store tree…</option>
				{#each roots as r (r.id)}<option value={r.id}>{r.label} ({r.key})</option>{/each}
			</select>
		{/if}
		<button
			class="btn primary"
			type="button"
			{...writeAttrs(session, 'knowledge_manager')}
			disabled={!canWrite || busy || qs.length === 0 || modes.length === 0 || (needRoot && !evalRoot)}
			title={runningRun ? RUN_BUSY_REASON : undefined}
			aria-describedby={runningRun ? 'ev-run-reason' : undefined}
			data-testid="ev-run"
			onclick={runEval}>{starting ? 'Starting…' : runningRun ? 'Running…' : 'Run eval'}</button
		>
	{/snippet}
</PageHead>

<LiveNote what="Golden questions and eval runs measure the agent; they do not change it." />

{#if questions.value.state === 'unavailable'}
	<Unavailable reason={questions.value.reason} retry={questions.retry} />
{:else}
	<div class="kpis ev-kpis" data-testid="ev-kpis">
		<Kpi
			label="recall@5 · hybrid"
			value={latestHead == null ? null : latestHead.toFixed(2)}
			delta={latestHead != null && prevHead ? delta(latestHead, prevHead)! / prevHead : null}
			note={latest ? `last run ${when(latest.started_at)} · ${latest.verdict}` : 'no run yet'}
		/>
		<Kpi
			label="Golden questions"
			value={fmt.n(qs.length)}
			note={`at most ${EVAL_MAX_QUESTIONS} per tenant`}
		/>
		<Kpi
			label="Runs"
			value={nextCursor ? `${fmt.n(runList.length)}+` : fmt.n(runList.length)}
			note={latest ? `latest by ${latest.started_by ?? '—'}` : ''}
		/>
	</div>

	<fieldset class="ev-modes" data-testid="ev-modes">
		<legend class="label">Modes to score</legend>
		{#each EVAL_MODES as m (m)}
			<label class="ev-mode"
				><input
					type="checkbox"
					checked={modes.includes(m)}
					disabled={!canWrite || busy}
					onchange={(e) => toggleMode(m, (e.currentTarget as HTMLInputElement).checked)}
				/> <span class="mono">{MODE_LABEL[m]}</span></label
			>
		{/each}
		<span class="muted ev-small">PASS = every routed mode's recall@5 is at least flat's.</span>
	</fieldset>

	{#if runningRun}
		<div class="callout ev-running" role="status" aria-live="polite" data-testid="ev-running">
			<strong>Eval running · <span data-testid="ev-progress">{progressText(runningRun)}</span></strong>
			<Meter value={runningRun.progress.total ? runningRun.progress.done / runningRun.progress.total : 0} />
			<span class="muted ev-small" id="ev-run-reason"
				>Started {when(runningRun.started_at)} by {runningRun.started_by ?? '—'}. {RUN_BUSY_REASON} It keeps running
				if you leave this page.</span
			>
			{#if pollStopped === runningRun.id}<span class="ev-small ev-warn" data-testid="ev-poll-stopped"
					>Progress not read {EVAL_POLL_MAX_FAILS} times in a row ({pollError}); stopped checking. The run
					goes on on the server.
					<button class="btn sm" type="button" data-testid="ev-poll-retry" onclick={resumePolling}
						>Retry</button
					></span
				>{:else if pollError}<span class="ev-small ev-warn" data-testid="ev-poll-error"
					>Progress not read ({pollError}); retrying in {Math.round(pollDelay(pollFails) / 1000)} s.</span
				>{/if}
		</div>
	{:else if lastFailed}
		<div class="callout err ev-failed" role="alert" data-testid="ev-failed">
			<strong>The last eval failed{lastFailed.error ? `: ${lastFailed.error}` : '.'}</strong>
			<span class="muted ev-small"
				>Started {when(lastFailed.started_at)} · stopped after {progressText(lastFailed)}. Nothing was scored;
				run it again.</span
			>
		</div>
	{/if}

	{#if goneNote && !runningRun}
		<div class="callout err" role="alert" data-testid="ev-gone">
			<strong>This run no longer exists.</strong>
			<span class="muted ev-small"
				>Run {goneNote} was removed while this page followed it; it has no results.</span
			>
		</div>
	{/if}

	{#if notReady.length}
		<div class="callout err ev-notready" role="alert" data-testid="ev-not-ready">
			<strong>strata_rag cannot run the set yet: fix these, then run again.</strong>
			<ul>
				{#each notReady.slice(0, 20) as line (line)}<li>{line}</li>{/each}
				{#if notReady.length > 20}<li>… and {notReady.length - 20} more</li>{/if}
			</ul>
		</div>
	{/if}

	{#if stale && qs.length}
		<p class="note ev-stale">
			The golden set changed since the last run: run the eval again to score the current questions.
		</p>
	{/if}

	<Card
		title="Results by mode"
		sub="recall@k: share of questions with a matching passage in the first k results. Leaf accuracy: over the questions that name a leaf, the first passage comes from it. Laya counts are questions, not rates."
	>
		{#snippet actions()}
			{#if runList.length > 1}
				<label class="ev-pick"
					><span class="label">Run</span>
					<!-- Bound to the shown run, so the default (newest completed run) is the selected option, never index -1. -->
					<select
						class="input"
						value={shown?.id ?? ''}
						onchange={(e) => (shownId = e.currentTarget.value)}
						data-testid="ev-run-pick"
					>
						{#each runList as r, i (r.id)}<option value={r.id}
								>{when(r.started_at)} · {r.question_count} q · {runLabel(r)}{i === 0
									? ' (latest)'
									: ''}</option
							>{/each}
					</select></label
				>
			{/if}
		{/snippet}
		{#if runs.value.state === 'unavailable'}
			<Unavailable reason={runs.value.reason} retry={runs.retry} />
		{:else if runs.value.state === 'loading'}
			<p class="muted">Loading…</p>
		{:else if !shown}
			<div class="empty">
				No eval has run yet. Add a few golden questions, then “Run eval” to see how well search finds them.
			</div>
		{:else if shownDone}
			<EvalReports run={shownDone} prev={prevOf(shownDone)} />
			<RunQuestions run={shownDone} detail={detailOf} />
		{:else if shown.status === 'running'}
			<div class="empty" data-testid="ev-shown-status">
				Run {shown.id} is running: {progressText(shown)}. Its results show here when it completes.
			</div>
		{:else}
			<div class="empty" data-testid="ev-shown-status">
				Run {shown.id} failed after {progressText(shown)}: {shown.error ?? 'no reason given'}. It has no
				results.
			</div>
		{/if}
	</Card>

	<div class="grid ev-main">
		<Card
			title={`Golden questions · ${qs.length} of ${EVAL_MAX_QUESTIONS}`}
			sub="Customer questions and what a good result is: a document, text a passage must contain, or document / chunk ids — any one matching is a hit. Tenant data: saved straight, no release."
		>
			{#snippet actions()}
				<button
					class="btn sm"
					type="button"
					{...writeAttrs(session, 'knowledge_manager')}
					disabled={!canWrite || full}
					title={full ? `The set holds at most ${EVAL_MAX_QUESTIONS} questions` : undefined}
					onclick={() => openForm()}>Add question</button
				>
			{/snippet}
			{#if formOpen}
				<form class="ev-form" id="ev-form" onsubmit={save} data-testid="ev-form">
					<label class="field ev-wide"
						><span class="label">Question</span><input
							class="input"
							bind:value={form.query}
							placeholder="How long does a refund take?"
						/></label
					>
					<label class="field"
						><span class="label">Expected document</span><input
							class="input"
							list="ev-docs"
							bind:value={form.expected_document}
							placeholder="Document title"
						/></label
					>
					<datalist id="ev-docs"
						>{#each docTitles as t (t)}<option value={t}></option>{/each}</datalist
					>
					<label class="field"
						><span class="label">Expected text</span><input
							class="input"
							bind:value={form.expected_text}
							placeholder="Words a good passage contains"
						/></label
					>
					<label class="field"
						><span class="label">Leaf store (optional)</span>
						<select
							class="input mono"
							bind:value={form.expected_leaf}
							onchange={() => loadDocs(form.expected_leaf)}
						>
							<option value="">Any store</option>
							{#if form.expected_leaf && !leaves.some((s) => s.key === form.expected_leaf)}
								<option value={form.expected_leaf}>{form.expected_leaf}</option>
							{/if}
							{#each leaves as s (s.id)}<option value={s.key}>{s.key}</option>{/each}
						</select></label
					>
					<details class="ev-wide ev-more" bind:open={moreOpen}>
						<summary>Document or chunk ids</summary>
						<div class="ev-ids">
							<label class="field"
								><span class="label">Expected document ids</span><input
									class="input mono"
									bind:value={form.document_ids}
									placeholder="id; id"
								/></label
							>
							<label class="field"
								><span class="label">Expected chunk ids</span><input
									class="input mono"
									bind:value={form.chunk_ids}
									placeholder="id; id"
								/></label
							>
						</div>
					</details>
					{#if touched && formProblems.length}
						<p class="ev-problems ev-wide" data-testid="ev-form-problems">{formProblems.join('; ')}.</p>
					{/if}
					<div class="row ev-wide">
						<button
							class="btn primary sm"
							type="submit"
							{...writeAttrs(session, 'knowledge_manager')}
							disabled={!canWrite || formProblems.length > 0}>{editing ? 'Save' : 'Add to golden set'}</button
						>
						<button class="btn sm" type="button" onclick={closeForm}>Cancel</button>
					</div>
				</form>
			{/if}
			{#if confirmDel}
				<div class="callout err" role="alertdialog" aria-label="Delete golden question">
					<strong>Delete “{confirmDel.query}”?</strong>
					<span>Past runs keep it; the next run will not ask it.</span>
					<span
						><button class="btn sm danger" type="button" onclick={() => remove(confirmDel!)}>Delete</button>
						<button class="btn sm" type="button" onclick={() => (confirmDel = null)}>Keep it</button></span
					>
				</div>
			{/if}
			{#if questions.value.state === 'loading'}
				<p class="muted">Loading…</p>
			{:else if qs.length === 0}
				<div class="empty">
					No golden questions yet. Add the questions customers ask most, or import a CSV. Any question from
					the
					<a href="/admin/knowledge/stores#t7-playground">search preview</a> can be added from there.
				</div>
			{:else}
				<div class="tbl-wrap ph-stack">
					<table class="tbl" data-testid="ev-questions">
						<thead>
							<tr><th>Question</th><th>Expected</th><th>Leaf</th><th></th></tr>
						</thead>
						<tbody>
							{#each qs as q (q.id)}
								<tr>
									<td><a href={playHref(q.query)} title="Try it in the search preview">{q.query}</a></td>
									<td data-label="Expected">{expects(q)}</td>
									<td class="mono" data-label="Leaf">{q.expected_leaf ?? '—'}</td>
									<td class="ev-act">
										<button
											class="btn sm"
											type="button"
											{...writeAttrs(session, 'knowledge_manager')}
											onclick={() => openForm(q, q)}
											aria-label={`Edit ${q.query}`}>Edit</button
										>
										<button
											class="btn sm"
											type="button"
											{...writeAttrs(session, 'knowledge_manager')}
											onclick={() => (confirmDel = q)}
											aria-label={`Delete ${q.query}`}>Delete</button
										>
									</td>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
			{/if}
			<CsvImport
				existing={qs.map((q) => q.query)}
				disabled={!canWrite}
				onimported={() => questions.refresh()}
			/>
		</Card>

		<Card
			title="Run history"
			sub="Pick two runs to see what changed: metrics per mode, and the questions added or dropped."
		>
			{#if doneRuns.length < 2}
				<div class="empty">Two completed runs are needed for a comparison.</div>
			{:else}
				<RunDiff runs={doneRuns} detail={detailOf} />
			{/if}
			{#if nextCursor}
				<button class="btn sm ev-older" type="button" onclick={loadOlder}>Load older runs</button>
			{/if}
		</Card>
	</div>
{/if}

<style>
	.ev-kpis {
		margin-bottom: 12px;
	}
	.ev-stale {
		margin: 0 0 12px;
		color: var(--warn);
	}
	.ev-main {
		grid-template-columns: minmax(0, 3fr) minmax(0, 2fr);
		margin-top: 16px;
	}
	.ev-pick {
		display: inline-flex;
		gap: 6px;
		align-items: center;
	}
	.ev-pick select {
		max-width: 230px;
	}
	.ev-form {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(min(200px, 100%), 1fr));
		gap: 10px;
		margin-bottom: 12px;
		padding: 10px;
		border: 1px solid var(--line-2);
		border-radius: 8px;
		background: var(--bg);
	}
	.ev-wide {
		grid-column: 1 / -1;
	}
	.ev-problems {
		margin: 0;
		color: var(--err);
		font-size: 12.5px;
	}
	.ev-modes {
		display: flex;
		flex-wrap: wrap;
		gap: 6px 14px;
		align-items: center;
		margin: 0 0 12px;
		padding: 0;
		border: 0;
	}
	.ev-modes legend {
		float: left;
		margin-right: 6px;
	}
	.ev-mode {
		display: inline-flex;
		gap: 4px;
		align-items: center;
		min-height: 32px;
	}
	.ev-small {
		font-size: 12px;
	}
	.ev-warn {
		color: var(--warn);
	}
	.ev-running,
	.ev-failed {
		display: flex;
		flex-direction: column;
		gap: 6px;
		margin-bottom: 12px;
		overflow-wrap: anywhere;
	}
	.ev-notready {
		margin-bottom: 12px;
		overflow-wrap: anywhere;
	}
	.ev-notready ul {
		margin: 4px 0 0;
		padding-left: 18px;
		font-size: 12.5px;
	}
	.ev-more summary {
		cursor: pointer;
		font-size: 12.5px;
	}
	.ev-ids {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(min(200px, 100%), 1fr));
		gap: 10px;
		margin-top: 8px;
	}
	.ev-older {
		margin-top: 10px;
	}
	.ev-act {
		white-space: nowrap;
		text-align: right;
	}
	@media (max-width: 900px) {
		.ev-main {
			grid-template-columns: minmax(0, 1fr);
		}
		.ev-act {
			text-align: left;
		}
	}
</style>
