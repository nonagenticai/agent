<script lang="ts">
	// From cb:web/src/routes/console/Inspector.svelte@45ee128: one stream entry (summary, payload, raw) or one
	// agent's work. Op envelopes carry ids + outcome codes only (K50), and the inspector says so.
	import type { BusEvent } from '$lib/api/types/live';
	import type { RouteHint } from '$lib/api/types/runtime';
	import { direction, laneOf } from '$lib/live/console/store.svelte';
	import {
		clock,
		eventOf,
		links,
		plain,
		summary,
		type AgentCard,
		type WorkItem,
	} from '$lib/live/console/bus';
	import JsonView from '$lib/ui/JsonView.svelte';

	let {
		event,
		agent,
		ack,
	}: { event: BusEvent | null; agent: AgentCard | null; ack: 'acked' | 'pending' | null } = $props();

	const entry = $derived.by(() => {
		if (!event) return null;
		return {
			stream: event.stream,
			entry_id: event.entry_id,
			consumer_group: event.consumer_group,
			ack,
			direction: direction(event.envelope.topic) === 'down' ? 'forward' : 'return path',
		};
	});
	type View = 'summary' | 'payload' | 'raw';
	const VIEWS: { id: View; label: string }[] = [
		{ id: 'summary', label: 'Summary' },
		{ id: 'payload', label: 'Payload' },
		{ id: 'raw', label: 'Raw' },
	];
	let view = $state<View>('summary');
	const isOp = $derived(event ? event.envelope.topic.startsWith('op.') : false);
	/** A knowledge.search result mirrors faq_ask's `citations` + `sources` (ids only, K50). */
	const faq = $derived.by(() => {
		const p = event?.envelope.payload;
		if (!p || !Array.isArray(p.citations)) return null;
		const sources = Array.isArray(p.sources)
			? (p.sources as { chunk_id: string; document_id: string; store_path?: string[] }[])
			: [];
		return { citations: p.citations as string[], sources };
	});
	/** F-19 (T9b): a qualified request carries the P1 gate's route hint (null until B5); other entries none. */
	const hint = $derived.by(() => {
		const p = event?.envelope.payload;
		if (!p || !('route_hint' in p)) return undefined;
		const h = p.route_hint as RouteHint | null;
		return h && typeof h.request_type === 'string' ? h : null;
	});
	const found = $derived(event ? links(event) : []);
	const outstanding = $derived(agent ? agent.items.filter((w) => w.status === 'run') : []);
	const finished = $derived(agent ? agent.items.filter((w) => w.status !== 'run') : []);
	const STATUS = { run: 'outstanding', ok: 'succeeded', fail: 'failed' } as const;
	const label = (w: WorkItem) =>
		`${eventOf(w.event.envelope.topic) ?? w.event.envelope.topic} · ${w.event.envelope.correlation_id}`;
</script>

<aside class="panel inspector" aria-label="Inspector">
	<header>
		{#if event}
			<span class="label">Stream entry on {laneOf(event.stream)} lane</span>
			<h3>{event.envelope.topic} · {event.entry_id}</h3>
		{:else if agent}
			<span class="label">Agent · inferred from bus events and consumer-group metrics</span>
			<h3>{agent.name}</h3>
		{:else}
			<span class="label">Stream entry</span>
			<h3>Pick an entry or an agent</h3>
		{/if}
	</header>
	{#if event}
		<div class="vtabs" role="tablist" aria-label="Inspector view">
			{#each VIEWS as v (v.id)}
				<button
					type="button"
					role="tab"
					id="itab-{v.id}"
					aria-selected={view === v.id}
					aria-controls="ipanel"
					onclick={() => (view = v.id)}>{v.label}</button
				>
			{/each}
		</div>
	{/if}
	<div
		class="ibody"
		id="ipanel"
		role={event ? 'tabpanel' : undefined}
		aria-labelledby={event ? `itab-${view}` : undefined}
	>
		{#if event && entry}
			{#if view === 'summary'}
				<p class="plain">{plain(event) || 'No plain-language summary for this topic.'}</p>
				{#if summary(event)}<p>{summary(event)}</p>{/if}
				<p>
					{event.envelope.from} → {event.consumer_group ?? 'no consumer group'} · {entry.direction} · {clock(
						event.envelope.published_at_ms,
					)}{ack ? ` · ${ack}` : ''}
				</p>
				{#if hint !== undefined}
					<p data-testid="inspector-route-hint">
						Likely request type:
						{#if hint}<b>{hint.request_type}</b> · {Math.round(hint.confidence * 100)}%{#if hint.activity}
								· {hint.activity}{/if}
						{:else}none (the gate gave no hint; the qualifier decided){/if}
					</p>
				{/if}
				{#if faq}
					<div class="faq" data-testid="inspector-citations">
						<h4>
							knowledge cited ({faq.citations.length})
							{#if !faq.citations.length}<span
									class="chip err"
									data-testid="grounding-flag"
									title="The search returned no citation: an answer built on it is not grounded (an answer counts only with ≥1 citation)."
									>not grounded</span
								>{/if}
						</h4>
						{#if faq.sources.length}
							<ul class="wl">
								{#each faq.sources as s (s.chunk_id)}
									<li title="chunk {s.chunk_id}">
										<span class="mono">{s.document_id}</span>
										{#if s.store_path?.length}· {s.store_path.join(' / ')}{/if}<br /><span class="mono"
											>chunk {s.chunk_id}</span
										>
									</li>
								{/each}
							</ul>
						{:else}
							<p>No source matched: the agent must say it does not know or hand over.</p>
						{/if}
					</div>
				{/if}
				{#if isOp}<p class="k50">
						Operation envelopes are mirrored here as ids and outcome codes only (K50).
					</p>{/if}
				{#if found.length}
					<div class="ilinks">
						{#each found as l (l.kind + l.label)}
							<a href={l.href}>{l.kind}: {l.label}</a>
						{/each}
					</div>
				{:else}
					<p>This entry names no case, operation or system.</p>
				{/if}
			{:else if view === 'payload'}
				<div>
					<h4>payload</h4>
					<JsonView value={event.envelope.payload} />
				</div>
			{:else}
				<div>
					<h4>XADD {event.stream}</h4>
					<JsonView value={entry} />
				</div>
				<div>
					<h4>field "envelope"</h4>
					<JsonView value={event.envelope} />
				</div>
			{/if}
		{:else if agent}
			<p>Status: <b>{agent.status}</b> · {agent.line}</p>
			<p>Working and waiting are inferred from unanswered entries seen here, not reported by the agent.</p>
			{#if agent.view}
				<div>
					<h4>consumer group</h4>
					<JsonView
						value={{
							group: agent.view.group,
							consumers: agent.view.consumers,
							pending: agent.view.pending,
							lag: agent.view.lag,
							status: agent.view.status,
						}}
					/>
				</div>
			{:else}
				<p>No consumer-group metrics yet; runtime state is unknown.</p>
			{/if}
			{#each [{ title: 'outstanding-work', list: outstanding }, { title: 'finished-work', list: finished }] as sec (sec.title)}
				<div>
					<h4>{sec.title} ({sec.list.length})</h4>
					{#if sec.list.length}
						<ul class="wl">
							{#each [...sec.list].reverse().slice(0, 30) as w (w.key)}
								<li>
									<span class="st {w.status}">{STATUS[w.status]}</span>{label(w)}<br />{summary(w.event)}
								</li>
							{/each}
						</ul>
					{:else}
						<p>None seen in this window.</p>
					{/if}
				</div>
			{/each}
		{:else}
			<p>Click any entry to see its stream entry and envelope, or an agent to see its work.</p>
		{/if}
	</div>
</aside>
