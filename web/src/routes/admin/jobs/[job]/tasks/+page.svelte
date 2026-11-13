<script lang="ts">
	// J26 Tasks: the staff inbox of long-running MCP work the agent started. Open tasks are read again every few
	// seconds while the page is visible and any task is unfinished. Staff answer a task's questions and cancel
	// tasks; the agent never answers.
	import { onDestroy } from 'svelte';
	import { api } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { ApiError } from '$lib/api/errors';
	import { TASKS_POLL_MS, canAnswerTasks, isTaskTerminal } from '$lib/mcp-tasks/tasks';
	import type { McpTaskView } from '$lib/api/types/mcp-tasks';
	import PageHead from '$lib/ui/PageHead.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';
	import TaskCard from './TaskCard.svelte';

	const canWrite = $derived(canAnswerTasks(session));

	let view = $state<'open' | 'all'>('open');
	let tasks = $state<McpTaskView[]>([]);
	let phase = $state<'loading' | 'ok' | 'unavailable'>('loading');
	let loadError = $state('');
	let pollErr = $state('');
	let failures = 0;
	let gen = 0;
	let timer: ReturnType<typeof setTimeout> | undefined;

	const visible = () => typeof document === 'undefined' || document.visibilityState === 'visible';

	function schedule(g: number, delay: number) {
		clearTimeout(timer);
		timer = setTimeout(() => {
			// Hidden tab: skip the read, look again later.
			if (!visible()) schedule(g, TASKS_POLL_MS);
			else void read(g);
		}, delay);
	}

	async function read(g: number) {
		let items: McpTaskView[];
		try {
			items = (await api().mcpTasks(view === 'open')).tasks;
		} catch (e) {
			if (g !== gen) return;
			const msg = e instanceof Error ? e.message : String(e);
			if (phase !== 'ok') {
				loadError = msg;
				phase = 'unavailable';
				return;
			}
			failures += 1;
			if (e instanceof ApiError && (e.status === 401 || e.status === 403 || e.status === 404)) {
				pollErr = msg;
			} else if (failures >= 5) {
				pollErr = `Stopped refreshing after ${failures} failed reads: ${msg}`;
			} else schedule(g, TASKS_POLL_MS * 2 ** (failures - 1));
			return;
		}
		if (g !== gen) return;
		failures = 0;
		pollErr = '';
		tasks = items;
		phase = 'ok';
		if (items.some((t) => !isTaskTerminal(t.status))) schedule(g, TASKS_POLL_MS);
	}

	function reload() {
		clearTimeout(timer);
		const g = ++gen;
		failures = 0;
		void read(g);
	}

	$effect(() => {
		void view;
		reload();
		return () => {
			gen += 1;
			clearTimeout(timer);
		};
	});
	onDestroy(() => {
		gen += 1;
		clearTimeout(timer);
	});
</script>

<PageHead
	id="J26"
	sub="Long-running work the agent started. When a task needs a decision, a person answers here; the agent never answers."
/>

{#if !canWrite}
	<Callout kind="info" title="Read only"
		>Answering or cancelling a task needs a staff role in this job.</Callout
	>
{/if}
{#if pollErr}
	<Callout kind="err" title="Not refreshing"
		>{pollErr} <button class="btn" type="button" onclick={reload}>Retry</button></Callout
	>
{/if}

<Card title="Tasks" cls="flush">
	{#snippet actions()}
		<label class="field">
			<span class="label">Show</span>
			<select class="input" bind:value={view} aria-label="Task filter">
				<option value="open">Open</option>
				<option value="all">All</option>
			</select>
		</label>
	{/snippet}
	{#if phase === 'loading'}
		<p class="muted">Loading…</p>
	{:else if phase === 'unavailable'}
		<Unavailable reason={loadError} retry={reload} />
	{:else if tasks.length === 0}
		<div class="empty">{view === 'open' ? 'No open tasks.' : 'No tasks yet.'}</div>
	{:else}
		{#each tasks as t (t.task_id)}
			<TaskCard task={t} {canWrite} onrefresh={reload} />
		{/each}
	{/if}
</Card>
