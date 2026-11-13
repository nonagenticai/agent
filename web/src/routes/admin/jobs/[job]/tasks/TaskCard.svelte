<script lang="ts">
	// One MCP task: status, its open questions as forms generated from each question's JSON Schema, decline and
	// cancel, and the result once it ended. Text from the task or its questions is always rendered as text.
	import { api } from '$lib/app.svelte';
	import { ApiError } from '$lib/api/errors';
	import { toast } from '$lib/ui/toaster.svelte';
	import {
		answerContent,
		answerProblem,
		escalationLabel,
		isTaskTerminal,
		questionFields,
		taskStatusLabel,
		taskStatusWord,
		validateAnswer,
		when,
	} from '$lib/mcp-tasks/tasks';
	import type { McpTaskQuestion, McpTaskView } from '$lib/api/types/mcp-tasks';
	import Callout from '$lib/ui/Callout.svelte';
	import Status from '$lib/ui/Status.svelte';

	let {
		task,
		canWrite = false,
		onrefresh,
	}: {
		task: McpTaskView;
		canWrite?: boolean;
		/** Called when the list should be read again (after an answer or cancel, or when the task changed elsewhere). */
		onrefresh?: () => void;
	} = $props();

	const terminal = $derived(isTaskTerminal(task.status));
	let drafts = $state<Record<string, Record<string, string>>>({});
	let errors = $state<Record<string, string>>({});
	let busy = $state('');
	let notice = $state('');
	let cancelError = $state('');

	const draft = (key: string) => (drafts[key] ??= {});
	const paragraphs = (m: string) => m.split(/\n\s*\n/).filter((p) => p.trim());
	const words = (e: unknown) =>
		e instanceof ApiError ? answerProblem(e.code, e.message) : e instanceof Error ? e.message : String(e);

	async function send(q: McpTaskQuestion, action: 'accept' | 'decline') {
		if (!canWrite || busy) return;
		errors[q.key] = '';
		notice = '';
		let content: Record<string, string> | undefined;
		if (action === 'accept') {
			const bad = validateAnswer(q.requested_schema, draft(q.key));
			if (bad) {
				errors[q.key] = bad;
				return;
			}
			content = answerContent(draft(q.key));
		}
		busy = q.key;
		try {
			const r = await api().answerMcpTask(task.task_id, {
				key: q.key,
				action,
				...(content ? { content } : {}),
			});
			toast(r.already_answered ? 'Already answered.' : 'Answer sent.');
			onrefresh?.();
		} catch (e) {
			if (e instanceof ApiError && e.code === 'answered_elsewhere') notice = words(e);
			else errors[q.key] = words(e);
			if (e instanceof ApiError && e.status === 409) onrefresh?.();
		} finally {
			busy = '';
		}
	}

	async function cancel() {
		if (!canWrite || busy || task.cancel_requested) return;
		busy = 'cancel';
		cancelError = '';
		try {
			await api().cancelMcpTask(task.task_id);
			toast('Cancel requested.');
			onrefresh?.();
		} catch (e) {
			cancelError = words(e);
			if (e instanceof ApiError && e.status === 409) onrefresh?.();
		} finally {
			busy = '';
		}
	}
</script>

<article class="task" aria-label={`Task ${task.tool}`}>
	<header class="task-head">
		<strong class="mono">{task.tool}</strong>
		<Status word={taskStatusWord(task.status)} label={taskStatusLabel(task.status)} />
		{#if task.cancel_requested && !terminal}<span class="muted">cancel requested</span>{/if}
		{#if task.escalated_at}<Status word="open" label="Needs staff" />{/if}
		<span class="muted task-when">Started {when(task.created_at)}</span>
	</header>
	{#if task.escalated_at}<p class="task-msg">{escalationLabel(task.escalation_reason)}</p>{/if}
	{#if task.status_message}<p class="task-msg">{task.status_message}</p>{/if}
	<p class="muted task-meta">
		<span class="mono">{task.task_id}</span>
		{#if task.terminal_at}· ended {when(task.terminal_at)}{/if}
	</p>

	{#if notice}<Callout kind="info" title="Already answered">{notice}</Callout>{/if}

	{#if !terminal}
		{#each task.questions as q (q.key)}
			{@const fields = questionFields(q.requested_schema)}
			<form
				class="task-q"
				aria-label={`Question ${q.key}`}
				onsubmit={(e) => {
					e.preventDefault();
					void send(q, 'accept');
				}}
			>
				<fieldset disabled={!canWrite || busy !== ''}>
					{#each paragraphs(q.message) as p, i (i)}<p class="task-para">{p}</p>{/each}
					{#each fields as f (f.name)}
						{#if f.kind === 'choice'}
							<label class="field"
								><span class="label">{f.name}{f.required ? ' *' : ''}</span>
								<select
									class="input"
									value={draft(q.key)[f.name] ?? ''}
									onchange={(e) => (draft(q.key)[f.name] = e.currentTarget.value)}
								>
									<option value="">Choose…</option>
									{#each f.options as o (o)}<option value={o}>{o}</option>{/each}
								</select>
							</label>
						{:else}
							<label class="field"
								><span class="label">{f.name}{f.required ? ' *' : ''}</span>
								<textarea
									class="input"
									rows="3"
									value={draft(q.key)[f.name] ?? ''}
									oninput={(e) => (draft(q.key)[f.name] = e.currentTarget.value)}></textarea>
							</label>
						{/if}
					{/each}
					{#if errors[q.key]}<span class="field-err" role="alert">{errors[q.key]}</span>{/if}
					<div class="actions">
						<button class="btn primary" type="submit">{busy === q.key ? 'Sending…' : 'Submit answer'}</button>
						<button class="btn" type="button" onclick={() => send(q, 'decline')}>Decline</button>
					</div>
				</fieldset>
			</form>
		{/each}
	{/if}

	{#if task.status === 'completed' && task.result !== null && task.result !== undefined}
		<div class="task-result">
			<span class="label">Result</span>
			<pre class="mono task-json">{typeof task.result === 'string'
					? task.result
					: JSON.stringify(task.result, null, 2)}</pre>
		</div>
	{/if}

	<div class="actions">
		<button
			class="btn"
			type="button"
			disabled={terminal || !canWrite || busy !== '' || task.cancel_requested}
			onclick={cancel}
			>{task.cancel_requested && !terminal
				? 'Cancel requested'
				: busy === 'cancel'
					? 'Cancelling…'
					: 'Cancel task'}</button
		>
	</div>
	{#if cancelError}<span class="field-err" role="alert">{cancelError}</span>{/if}
</article>

<style>
	.task {
		display: grid;
		gap: 8px;
		padding: 12px 16px;
		border-bottom: 1px solid var(--border, #ddd);
	}
	.task-head {
		display: flex;
		flex-wrap: wrap;
		gap: 12px;
		align-items: center;
	}
	.task-when {
		margin-left: auto;
	}
	.task-msg,
	.task-meta,
	.task-para {
		margin: 0;
	}
	.task-meta {
		font-size: 0.8em;
	}
	.task-para {
		white-space: pre-wrap;
	}
	.task-q fieldset {
		display: grid;
		gap: 8px;
		border: 0;
		padding: 0;
		margin: 0;
	}
	.task-json {
		max-height: 240px;
		overflow: auto;
		white-space: pre-wrap;
	}
</style>
