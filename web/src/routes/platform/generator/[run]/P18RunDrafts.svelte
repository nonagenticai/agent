<script lang="ts">
	// P18 drafts of one step: each LLM draft with its confidence, example data and the human decision
	// (accept / rename / merge / drop; "new object" → proposal to the super_admin).
	import { api } from '$lib/app.svelte';
	import type { GeneratorDraftItem, GeneratorRun } from '$lib/api/types/journey';
	import JsonView from '$lib/ui/JsonView.svelte';
	import P18OperationDrawer from './P18OperationDrawer.svelte';

	let {
		run: r,
		items,
		open,
		act,
		decide = (id, itemId, d) => api().decideGeneratorItem(id, itemId, d),
	}: {
		run: GeneratorRun;
		items: GeneratorDraftItem[];
		open: boolean;
		act: (fn: () => Promise<unknown>, ok: string) => Promise<void>;
		/** The decision call: platform routes by default, the D97 job route on a tenant run. */
		decide?: (
			id: number,
			itemId: string,
			d: { decision: GeneratorDraftItem['decision']; note?: string },
		) => Promise<unknown>;
	} = $props();

	// F-23: a platform run's Operations draft can be proposed as a new operation (super_admin).
	let opItem = $state<GeneratorDraftItem | null>(null);
	let opOpen = $state(false);

	const DECISION: Record<GeneratorDraftItem['decision'], string> = {
		pending: 'warn',
		accepted: 'ok',
		merged: 'accent',
		renamed: 'accent',
		dropped: 'dead',
	};
	const DECISION_TEXT: Record<GeneratorDraftItem['decision'], string> = {
		pending: 'to decide',
		accepted: 'accepted',
		merged: 'merged',
		renamed: 'renamed',
		dropped: 'dropped',
	};

	let renaming = $state<string | null>(null);
	let renameTo = $state('');
	let merging = $state<string | null>(null);
	let mergeInto = $state('');

	// Pending first: the page's job is to get every draft decided.
	const ordered = $derived(
		[...items].sort((a, b) => Number(b.decision === 'pending') - Number(a.decision === 'pending')),
	);
</script>

<ul class="p18-drafts" data-testid="drafts">
	{#each ordered as it (it.id)}
		<li class="p18-draft {it.decision}">
			<div class="p18-dhead">
				<span class="p18-dtitle mono">{it.title}</span>
				<span class="chip {DECISION[it.decision]}">{DECISION_TEXT[it.decision]}</span>
			</div>
			<div class="p18-dmeta">
				<span class="p18-kind">{it.kind.replaceAll('_', ' ')}</span>
				<span class="p18-conf" title="LLM confidence">
					<span class="p18-confbar" aria-hidden="true"
						><span style:width="{Math.round(it.llm_confidence * 100)}%"></span></span
					>
					{Math.round(it.llm_confidence * 100)}% sure
				</span>
			</div>
			<div class="cell-sub">{it.detail}</div>
			{#if it.decision_note}<div class="cell-sub p18-note">{it.decision_note}</div>{/if}
			{#if it.new_object}
				<div class="row p18-newobj">
					<span class="chip proposed">new object: {it.new_object}</span>
					<span
						class="chip {it.proposal === 'approved'
							? 'ok'
							: it.proposal === 'rejected'
								? 'err'
								: it.proposal === 'proposed'
									? 'warn'
									: 'muted'}"
						>{it.proposal === 'none' ? 'not proposed (rule 1)' : `proposal ${it.proposal}`}</span
					>
				</div>
			{/if}
			{#if it.new_operation}
				<div class="row p18-newobj">
					<span class="chip proposed"
						>new operation: {String(it.new_operation.object ?? '')}.{String(
							it.new_operation.verb ?? '',
						)}</span
					>
					<span class="chip {it.proposal === 'approved' ? 'ok' : it.proposal === 'rejected' ? 'err' : 'warn'}"
						>proposal {it.proposal}</span
					>
				</div>
			{/if}
			{#if it.example}
				<details>
					<summary class="p18-small">Example data</summary>
					<JsonView value={it.example} />
				</details>
			{/if}
			{#if open}
				<div class="row p18-dactions">
					{#if renaming === it.id}
						<input class="input" aria-label="New name" bind:value={renameTo} />
						<button
							class="btn sm primary"
							type="button"
							onclick={async () => {
								await act(() => decide(r.id, it.id, { decision: 'renamed', note: renameTo }), 'Renamed.');
								renaming = null;
							}}>Save</button
						>
						<button class="btn sm" type="button" onclick={() => (renaming = null)}>Cancel</button>
					{:else if merging === it.id}
						<select class="input" aria-label="Merge into" bind:value={mergeInto}>
							{#each items.filter((x) => x.id !== it.id) as x (x.id)}<option value={x.id}>{x.title}</option
								>{/each}
						</select>
						<button
							class="btn sm primary"
							type="button"
							onclick={async () => {
								await act(() => decide(r.id, it.id, { decision: 'merged', note: mergeInto }), 'Merged.');
								merging = null;
							}}>Merge</button
						>
						<button class="btn sm" type="button" onclick={() => (merging = null)}>Cancel</button>
					{:else if it.decision === 'pending'}
						<button
							class="btn sm primary"
							type="button"
							onclick={() => act(() => decide(r.id, it.id, { decision: 'accepted' }), 'Accepted.')}
							>Accept</button
						>
						<button
							class="btn sm"
							type="button"
							onclick={() => {
								renaming = it.id;
								renameTo = it.title;
							}}>Rename</button
						>
						<button
							class="btn sm"
							type="button"
							disabled={items.length < 2}
							onclick={() => {
								merging = it.id;
								mergeInto = items.find((x) => x.id !== it.id)?.id ?? '';
							}}>Merge</button
						>
						<button
							class="btn sm danger"
							type="button"
							onclick={() => act(() => decide(r.id, it.id, { decision: 'dropped' }), 'Dropped.')}>Drop</button
						>
					{:else}
						<button
							class="btn sm"
							type="button"
							onclick={() => act(() => decide(r.id, it.id, { decision: 'pending' }), 'Decision undone.')}
							>Undo</button
						>
					{/if}
					{#if r.scope === 'platform' && it.kind === 'operation_expectation' && it.proposal === 'none' && it.decision !== 'dropped'}
						<button
							class="btn sm"
							type="button"
							onclick={() => {
								opItem = it;
								opOpen = true;
							}}>Propose new operation</button
						>
					{:else if it.kind === 'operation_expectation' && it.new_operation}
						<button
							class="btn sm"
							type="button"
							disabled={it.proposal !== 'proposed'}
							onclick={() =>
								act(() => api().resolveGeneratorProposal(r.id, it.id, true), 'Operation approved.')}
							>Approve operation</button
						>
						<button
							class="btn sm"
							type="button"
							disabled={it.proposal !== 'proposed'}
							onclick={() =>
								act(() => api().resolveGeneratorProposal(r.id, it.id, false), 'Operation rejected.')}
							>Reject operation</button
						>
					{/if}
					{#if it.new_object && it.proposal === 'none'}
						<button
							class="btn sm"
							type="button"
							onclick={() =>
								act(
									() => api().proposeGeneratorObject(r.id, it.id),
									`Proposed ${it.new_object} to the super_admin.`,
								)}>Propose new object</button
						>
					{:else if it.new_object && it.proposal === 'proposed'}
						<button
							class="btn sm"
							type="button"
							onclick={() =>
								act(() => api().resolveGeneratorProposal(r.id, it.id, true), `${it.new_object} approved.`)}
							>Approve object</button
						>
						<button
							class="btn sm"
							type="button"
							onclick={() =>
								act(() => api().resolveGeneratorProposal(r.id, it.id, false), `${it.new_object} rejected.`)}
							>Reject</button
						>
					{/if}
				</div>
			{/if}
		</li>
	{/each}
</ul>

<P18OperationDrawer
	run={r}
	item={opItem}
	bind:open={opOpen}
	onproposed={() => act(async () => {}, 'Operation proposal recorded.')}
/>

<style>
	.p18-drafts {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 8px;
	}
	.p18-draft {
		border: 1px solid var(--line-2);
		border-left: 3px solid var(--line);
		border-radius: var(--r);
		padding: 10px 12px;
		display: grid;
		gap: 4px;
		min-width: 0;
	}
	.p18-draft.pending {
		border-left-color: var(--warn);
		background: color-mix(in srgb, var(--warn) 4%, transparent);
	}
	.p18-draft.accepted {
		border-left-color: var(--ok);
	}
	.p18-draft.renamed,
	.p18-draft.merged {
		border-left-color: var(--accent);
	}
	.p18-draft.dropped {
		border-left-color: var(--dead);
		opacity: 0.75;
	}
	.p18-draft.dropped .p18-dtitle {
		text-decoration: line-through;
	}
	.p18-dhead {
		display: flex;
		gap: 8px;
		align-items: center;
		justify-content: space-between;
		flex-wrap: wrap;
	}
	.p18-dtitle {
		font-weight: 600;
		font-size: 13px;
		overflow-wrap: anywhere;
	}
	.p18-dmeta {
		display: flex;
		gap: 12px;
		align-items: center;
		font-size: 11.5px;
		color: var(--muted);
	}
	.p18-kind {
		font-family: var(--mono);
	}
	.p18-conf {
		display: inline-flex;
		align-items: center;
		gap: 5px;
	}
	.p18-confbar {
		display: inline-block;
		width: 44px;
		height: 4px;
		border-radius: 2px;
		background: var(--line-2);
		overflow: hidden;
	}
	.p18-confbar span {
		display: block;
		height: 100%;
		background: var(--muted);
	}
	.p18-note {
		font-style: italic;
	}
	.p18-newobj {
		gap: 6px;
	}
	.p18-dactions {
		gap: 6px;
		margin-top: 4px;
	}
	.p18-dactions .input {
		max-width: 320px;
		flex: 1 1 180px;
	}
	.p18-small {
		font-size: 12px;
		cursor: pointer;
	}
</style>
