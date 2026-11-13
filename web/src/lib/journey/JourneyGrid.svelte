<script lang="ts" module>
	import type {
		JobJourneyActivity,
		JourneyActivity,
		JourneyChannel,
		JourneyStage,
		JourneyTheme,
		StageKnowledgeKind,
		StageMessage,
		ValidationFinding,
	} from '$lib/api/types/journey';

	export type GridActivity = JourneyActivity &
		Partial<Pick<JobJourneyActivity, 'refinement' | 'base_label' | 'pending'>>;
	export type GridKind = StageKnowledgeKind & { present?: boolean; store?: string | null };

	type RowKey =
		| 'themes'
		| 'act_customer'
		| 'act_company'
		| 'msg_chat'
		| 'msg_email'
		| 'msg_form'
		| 'msg_oos'
		| 'objects'
		| 'kinds';
	interface Row {
		key: RowKey;
		label: string;
		sub: string;
	}
	const ROWS: Row[] = [
		{ key: 'themes', label: 'Themes', sub: 'customer state' },
		{ key: 'act_customer', label: 'Activities', sub: 'customer' },
		{ key: 'act_company', label: 'Activities', sub: 'company' },
		{ key: 'msg_chat', label: 'Messages · Chat', sub: 'v1 (Telegram, staff /chat)' },
		{ key: 'msg_email', label: 'Messages · Emails', sub: 'later' },
		{ key: 'msg_form', label: 'Messages · Contact forms', sub: 'later' },
		{ key: 'msg_oos', label: 'Phone · Visit · Signing', sub: 'out of scope' },
		{ key: 'objects', label: 'Business objects', sub: 'read or written' },
		{ key: 'kinds', label: 'Knowledge kinds', sub: 'strata_rag store' },
	];
</script>

<script lang="ts">
	// The owner's journey grid (D23, 11§4.3.1): stages as columns, swimlanes as rows. Used by the journey designer
	// (P8, editable) and the job's effective grid (J4, with tenant refinements). An empty cell of an in-scope stage is
	// validation red (rule 4, 11§7); an out-of-scope stage is greyed.
	import type { Snippet } from 'svelte';
	import { MediaQuery } from 'svelte/reactivity';
	import MatrixTable from '$lib/ui/MatrixTable.svelte';
	import { objectKey } from './catalogue';

	let {
		stages,
		themes,
		channels,
		activities,
		messages,
		kinds,
		findings = [],
		onStage,
		onActivity,
		onAdd,
		addLabel = '+ activity',
		addDisabled = false,
		addTitle,
		activityTone,
		activityMeta,
		stageMeta,
	}: {
		stages: JourneyStage[];
		themes: JourneyTheme[];
		channels: JourneyChannel[];
		activities: GridActivity[];
		messages: StageMessage[];
		kinds: GridKind[];
		findings?: ValidationFinding[];
		onStage?: (s: JourneyStage) => void;
		onActivity?: (a: GridActivity) => void;
		onAdd?: (s: JourneyStage) => void;
		addLabel?: string;
		addDisabled?: boolean;
		addTitle?: string;
		/** J4 lens: a tone per activity (`inherited` · `refined` · `ready` · `gap` · `idle`) that colours its card. */
		activityTone?: (a: GridActivity) => string | null;
		/** J4: extra line inside an activity card (readiness, live outcome). */
		activityMeta?: Snippet<[GridActivity]>;
		/** J4: extra content under a stage header (stage readiness). */
		stageMeta?: Snippet<[JourneyStage]>;
	} = $props();

	// Phone/tablet (≤860px): stages × swimlanes is 1000–1500px wide, so the grid turns into a stage accordion — one
	// card per stage with its swimlanes stacked inside. Same cells, same handlers; desktop keeps the matrix.
	// (No matchMedia in jsdom/SSR: the matrix is the fallback.)
	const narrow =
		typeof window !== 'undefined' && typeof window.matchMedia === 'function'
			? new MediaQuery('max-width: 860px')
			: null;
	const stacked = $derived(narrow?.current ?? false);
	/** Swimlanes shown inside a stage card: themes go in the card head; empty later-channel lanes are dropped. */
	function lanes(s: JourneyStage) {
		return ROWS.filter((r) => {
			if (r.key === 'themes' || r.key === 'msg_oos') return false;
			if (r.key === 'msg_email') return utter(s, 'email').length > 0;
			if (r.key === 'msg_form') return utter(s, 'contact_form').length > 0;
			return true;
		});
	}

	const themeLabel = $derived(Object.fromEntries(themes.map((t) => [t.key, t.label])));
	const later = $derived(channels.filter((c) => c.status === 'later').map((c) => c.label));
	const oos = $derived(channels.filter((c) => c.status === 'out_of_scope'));

	/** The person-side lane holds what a customer or a known employee asks; the company lane what the company starts. */
	function acts(s: JourneyStage, actor: 'customer' | 'company') {
		return activities.filter((a) => a.stage === s.key && (a.actor === 'company') === (actor === 'company'));
	}
	function enabled(s: JourneyStage) {
		return activities.filter((a) => a.stage === s.key && a.refinement !== 'hidden');
	}
	function utter(s: JourneyStage, ch: string) {
		return messages.filter((m) => m.stage === s.key && m.channel === ch).flatMap((m) => m.utterances);
	}
	function objects(s: JourneyStage) {
		const seen = new Map<string, number>();
		for (const a of enabled(s))
			for (const o of a.objects)
				seen.set(o.object, (seen.get(o.object) ?? 0) + (o.role === 'subject' ? 2 : 1));
		return [...seen.entries()].sort((a, b) => b[1] - a[1]).map(([k]) => k);
	}
	function redCount(s: JourneyStage) {
		return findings.filter((f) => f.stage === s.key && f.severity === 'red').length;
	}
	const label = (o: string) =>
		objectKey(o).replaceAll('_', ' ') + (o.includes('{') ? o.slice(o.indexOf('{')) : '');
</script>

{#snippet lane(r: Row, s: JourneyStage)}
	{#if r.key === 'themes'}
		<div class="chips">
			{#each s.themes as t, i (t)}<span class="chip accent">{themeLabel[t] ?? t}</span
				>{#if i < s.themes.length - 1}<span class="muted">→</span>{/if}{/each}
		</div>
	{:else if s.out_of_scope}
		<span class="muted oos-cell">Out of scope</span>
	{:else if r.key === 'act_customer' || r.key === 'act_company'}
		{@const list = acts(s, r.key === 'act_customer' ? 'customer' : 'company')}
		<div
			class="acts"
			class:red={r.key === 'act_customer' && !enabled(s).length}
			data-testid="cell-{r.key}-{s.key}"
		>
			{#each list as a (a.key)}
				<button
					type="button"
					class="act {a.actor}"
					class:hidden={a.refinement === 'hidden'}
					data-tone={activityTone?.(a) ?? undefined}
					onclick={() => onActivity?.(a)}
					disabled={!onActivity}
					title={a.key}
				>
					{#if a.refinement === 'hidden'}<s>{a.label}</s>{:else}{a.label}{/if}
					{#if a.refinement === 'extension'}<span class="chip cust">tenant</span>{/if}
					{#if a.refinement === 'relabelled'}<span class="chip work" title="Template label: {a.base_label}"
							>relabelled</span
						>{/if}
					{#if a.refinement === 'hidden'}<span class="chip dead">hidden</span>{/if}
					{#if a.pending}<span class="chip warn" title="Not in a release yet">unreleased</span>{/if}
					{#if activityMeta}<span class="act-meta">{@render activityMeta(a)}</span>{/if}
				</button>
			{:else}
				{#if r.key === 'act_customer' && !enabled(s).length}
					<span class="red-text">No activity (rule 4)</span>
				{:else}
					<span class="muted">—</span>
				{/if}
			{/each}
			{#if onAdd && r.key === 'act_customer'}
				<button
					type="button"
					class="btn sm add"
					onclick={() => onAdd(s)}
					disabled={addDisabled}
					title={addTitle}>{addLabel}</button
				>
			{/if}
		</div>
	{:else if r.key === 'msg_chat'}
		{@const u = utter(s, 'chat')}
		<div class:red={!u.length}>
			{#each u as m (m)}<div class="utt">“{m}”</div>{:else}<span class="red-text"
					>No chat messages (rule 4)</span
				>{/each}
		</div>
	{:else if r.key === 'msg_email' || r.key === 'msg_form'}
		{@const u = utter(s, r.key === 'msg_email' ? 'email' : 'contact_form')}
		<div class="later">
			{#each u as m (m)}<div class="utt">“{m}”</div>{:else}<span class="muted">—</span>{/each}
		</div>
	{:else if r.key === 'msg_oos'}
		<span class="muted oos-cell" title={oos.map((c) => `${c.label}: ${c.note}`).join('\n')}>out of scope</span
		>
	{:else if r.key === 'objects'}
		{@const objs = objects(s)}
		<div class="chips" class:red={!objs.length}>
			{#each objs as o (o)}<span class="chip tool">{label(o)}</span>{:else}<span class="red-text"
					>No business objects (rule 4)</span
				>{/each}
		</div>
	{:else if r.key === 'kinds'}
		{@const ks = kinds.filter((k) => k.stage === s.key)}
		<div class="chips">
			{#each ks as k (k.kind)}
				<span
					class="chip {k.present === false ? 'warn' : 'muted'}"
					title="{k.description}{k.required ? ' · required' : ''}{k.present === false
						? ' · not in the bound store'
						: k.present
							? ` · in store ${k.store}`
							: ''}">{k.kind}{k.required ? '' : ' (opt.)'}{k.present === false ? ' · missing' : ''}</span
				>
			{:else}<span class="muted">—</span>{/each}
		</div>
	{/if}
{/snippet}

{#snippet stageCards()}
	<ol class="jg-stages" aria-label="Journey stages">
		{#each stages as s, i (s.key)}
			{@const n = enabled(s).length}
			{@const red = redCount(s)}
			<li class="jg-stage" class:oos={s.out_of_scope} data-testid="stage-card-{s.key}">
				<details open={i === 0 || red > 0}>
					<summary>
						<span class="jg-head">
							<span class="jg-chev" aria-hidden="true">▸</span>
							<b class="jg-title" title={s.synonyms.join(', ')}>{s.order}. {s.label}</b>
							{#if s.out_of_scope}<span class="chip muted">out of scope</span>
							{:else}<span class="chip muted">{n} activit{n === 1 ? 'y' : 'ies'}</span>{/if}
							{#if red}<span class="chip err" title="Red validation items in this stage">{red} red</span>{/if}
							{@render stageMeta?.(s)}
						</span>
						{#if s.themes.length}
							<span class="chips jg-themes">
								{#each s.themes as t, k (t)}<span class="chip accent">{themeLabel[t] ?? t}</span
									>{#if k < s.themes.length - 1}<span class="muted">→</span>{/if}{/each}
							</span>
						{/if}
					</summary>
					<div class="jg-body">
						{#if onStage || s.key === 'change' || s.key === 'exit'}
							<p class="jg-tools">
								{#if onStage}<button type="button" class="btn sm" onclick={() => onStage(s)}
										>Edit stage</button
									>{/if}
								{#if s.key === 'change' || s.key === 'exit'}<span
										class="veto"
										title="Change and Exit are unlabelled on the owner's slide; the labels are owner-vetoable (K48)"
										>label vetoable</span
									>{/if}
							</p>
						{/if}
						{#if s.out_of_scope}
							<p class="muted oos-cell">
								Out of scope for this journey — no activities, messages or objects.
							</p>
						{:else}
							{#each lanes(s) as r (r.key)}
								<section class="jg-lane">
									<h4>{r.label} <span>{r.sub}</span></h4>
									{@render lane(r, s)}
								</section>
							{/each}
						{/if}
					</div>
				</details>
			</li>
		{/each}
	</ol>
{/snippet}

<div class="journey-grid" class:stacked>
	{#if stacked}
		{@render stageCards()}
	{:else}
		<MatrixTable
			rows={ROWS}
			columns={stages}
			rowKey={(r) => r.key}
			colKey={(s) => s.key}
			caption="Journey grid: swimlanes by stage"
			corner="Swimlane"
		>
			{#snippet colHeader(s)}
				<div class="stage-head" class:oos={s.out_of_scope}>
					{#if onStage}
						<button type="button" class="linkish" onclick={() => onStage(s)} title={s.synonyms.join(', ')}
							>{s.order}. {s.label}</button
						>
					{:else}
						<b title={s.synonyms.join(', ')}>{s.order}. {s.label}</b>
					{/if}
					{#if s.out_of_scope}<span class="chip muted">out of scope</span>{/if}
					{#if redCount(s)}<span class="chip err" title="Red validation items in this stage"
							>{redCount(s)} red</span
						>{/if}
					{#if s.key === 'change' || s.key === 'exit'}<span
							class="veto"
							title="Change and Exit are unlabelled on the owner's slide; the labels are owner-vetoable (K48)"
							>label vetoable</span
						>{/if}
					{@render stageMeta?.(s)}
				</div>
			{/snippet}
			{#snippet rowHeader(r)}
				<span class="cell-title">{r.label}</span><span class="cell-sub">{r.sub}</span>
			{/snippet}
			{#snippet cell(r, s)}{@render lane(r, s)}{/snippet}
		</MatrixTable>
	{/if}
	<p class="muted legend">
		Channels: <span class="chip ok">Chat · v1</span>
		{#each later as l (l)}<span class="chip muted">{l} · later</span>{/each}
		{#each oos as c (c.key)}<span class="chip dead" title={c.note}>{c.label} · out of scope</span>{/each}
	</p>
</div>

<style>
	.journey-grid :global(.matrix td) {
		vertical-align: top;
		min-width: 168px;
		font-size: 12.5px;
	}
	.journey-grid :global(.matrix th.row-head) {
		min-width: 150px;
	}
	/* Row heads are <th>: match the body-cell rule colour and drop it on the last row like the <td>s. */
	.journey-grid :global(.matrix tbody th) {
		border-bottom-color: var(--line-2);
	}
	.journey-grid :global(.matrix tbody tr:last-child th) {
		border-bottom: 0;
	}
	/* The grid is wider than the card: keep the horizontal scrollbar visible (macOS overlays hide it). */
	.journey-grid :global(.matrix-wrap) {
		padding-bottom: 4px;
	}
	.journey-grid :global(.matrix-wrap::-webkit-scrollbar) {
		height: 10px;
	}
	.journey-grid :global(.matrix-wrap::-webkit-scrollbar-track) {
		background: var(--sunk);
		border-radius: 5px;
	}
	.journey-grid :global(.matrix-wrap::-webkit-scrollbar-thumb) {
		background: var(--faint);
		border-radius: 5px;
	}
	.stage-head {
		display: grid;
		gap: 4px;
		justify-items: start;
	}
	.stage-head.oos {
		opacity: 0.7;
	}
	.linkish {
		background: none;
		border: 0;
		padding: 0;
		font: 600 13px var(--sans);
		color: var(--accent);
		cursor: pointer;
		text-align: left;
	}
	.veto {
		font-size: 10.5px;
		color: var(--faint);
		border-bottom: 1px dotted var(--faint);
	}
	.acts {
		display: grid;
		gap: 4px;
		justify-items: start;
	}
	.act {
		text-align: left;
		border: 1px solid var(--line);
		border-left: 3px solid var(--cust);
		background: var(--panel);
		border-radius: 5px;
		padding: 3px 6px;
		font-size: 12px;
		cursor: pointer;
		display: flex;
		flex-wrap: wrap;
		gap: 4px;
		align-items: center;
	}
	.act.company {
		border-left-color: var(--work);
	}
	.act.hidden {
		opacity: 0.65;
	}
	.act[data-tone='inherited'] {
		background: var(--sunk);
		border-color: var(--line-2);
		color: var(--muted);
	}
	.act[data-tone='refined'] {
		background: color-mix(in srgb, var(--accent) 9%, var(--panel));
		border-color: color-mix(in srgb, var(--accent) 45%, var(--line));
	}
	.act[data-tone='ready'] {
		border-left-color: var(--ok);
	}
	.act[data-tone='gap'] {
		border-left-color: var(--warn);
		border-color: color-mix(in srgb, var(--warn) 55%, var(--line));
		background: color-mix(in srgb, var(--warn) 9%, var(--panel));
	}
	.act[data-tone='idle'] {
		border-left-color: var(--line);
		color: var(--muted);
	}
	.act-meta {
		flex-basis: 100%;
		display: flex;
		flex-wrap: wrap;
		gap: 4px;
		align-items: center;
		font-size: 11px;
		color: var(--muted);
	}
	.act:disabled {
		cursor: default;
	}
	.act .chip {
		font-size: 10px;
		padding: 1px 5px;
	}
	.red {
		outline: 1.5px dashed var(--err);
		outline-offset: 2px;
		border-radius: 4px;
		padding: 2px;
		background: color-mix(in srgb, var(--err) 6%, transparent);
	}
	.red-text {
		color: var(--err);
		font-weight: 600;
		font-size: 12px;
	}
	.utt {
		font-style: italic;
		color: var(--fg);
	}
	.later .utt {
		color: var(--muted);
	}
	.oos-cell {
		font-style: italic;
	}
	.add {
		margin-top: 2px;
	}
	.jg-stages {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 8px;
	}
	.jg-stage {
		border: 1px solid var(--line);
		border-radius: 8px;
		background: var(--panel);
		min-width: 0;
	}
	.jg-stage.oos {
		opacity: 0.75;
	}
	.jg-stage summary {
		display: grid;
		gap: 6px;
		padding: 10px 12px;
		cursor: pointer;
		list-style: none;
	}
	.jg-stage summary::-webkit-details-marker {
		display: none;
	}
	.jg-chev {
		color: var(--faint);
		font-size: 12px;
		transition: transform 0.15s;
	}
	details[open] .jg-chev {
		transform: rotate(90deg);
	}
	.jg-head {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		align-items: center;
	}
	.jg-title {
		font-size: 14px;
		margin-right: 2px;
	}
	.jg-themes {
		display: flex;
		flex-wrap: wrap;
		gap: 4px;
		align-items: center;
	}
	.jg-body {
		display: grid;
		gap: 12px;
		padding: 4px 12px 12px;
		border-top: 1px solid var(--line-2);
		font-size: 13px;
		min-width: 0;
	}
	.jg-tools {
		display: flex;
		gap: 10px;
		align-items: center;
		margin: 6px 0 0;
	}
	.jg-lane {
		display: grid;
		gap: 5px;
		min-width: 0;
	}
	.jg-lane h4 {
		margin: 0;
		font: 600 11px/1.3 var(--cond);
		letter-spacing: 0.07em;
		text-transform: uppercase;
		color: var(--muted);
	}
	.jg-lane h4 span {
		font-weight: 400;
		text-transform: none;
		letter-spacing: 0;
		color: var(--faint);
		margin-left: 4px;
	}
	.jg-lane .acts {
		justify-items: stretch;
		grid-template-columns: repeat(auto-fill, minmax(min(100%, 230px), 1fr));
	}
	.jg-lane .act {
		font-size: 13px;
		padding: 6px 8px;
	}
	.journey-grid {
		min-width: 0;
	}
	.legend {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		align-items: center;
		font-size: 12px;
		margin: 8px 0 0;
	}
</style>
