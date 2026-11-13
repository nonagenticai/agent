<script lang="ts">
	// L1: one turn of a conversation — the customer's message and what each agent did with it, in order:
	// qualifier (P1/P2 gates + qualification) → procedure (operations, ids + outcome codes only, K50; knowledge) →
	// post-gate (R1 fact check of the reply) → the reply. Staff messages and state changes sit under the turn.
	import type { Turn } from '$lib/live/console/L1-operator';
	import { hhmmss, notable, stepOf } from '$lib/live/console/L1-operator';
	import AnswerSources from '$lib/grounding/AnswerSources.svelte';

	let { turn, working = false }: { turn: Turn; working?: boolean } = $props();

	const step = $derived(working ? stepOf(turn) : null);
	const okOp = (o: string, state: string) =>
		state === 'ok' && !o.startsWith('invalid') && !o.startsWith('not_');
	const opTone = (o: { state: string; outcome: string }) =>
		o.state === 'unknown_outcome' || o.outcome === 'unknown_outcome'
			? 'err'
			: o.state === 'error' || o.state === 'denied'
				? 'err'
				: okOp(o.outcome, o.state)
					? 'ok'
					: 'warn';
	const gateTone = (v: string) => (v === 'pass' ? 'ok' : v === 'ask' ? 'warn' : 'err');
	const hasQualifier = $derived(turn.qualifier.gates.length > 0 || !!turn.qualifier.qualified);
	const hasProcedure = $derived(turn.procedure.ops.length > 0 || turn.procedure.knowledge.length > 0);
	const slots = $derived(
		turn.qualifier.qualified
			? Object.entries(turn.qualifier.qualified.slots).filter(([, v]) => v !== null && v !== '')
			: [],
	);
</script>

<li class="l1-turn" data-testid="console-turn">
	<div class="l1-turn-head">
		<span class="l1-turn-n">Turn {turn.n}</span>
		<span class="l1-time">{hhmmss(turn.at)}</span>
	</div>
	{#if turn.customer}
		<p class="l1-said l1-said-customer">
			<span class="l1-who">{turn.customer.author}</span>{turn.customer.text}
		</p>
	{/if}

	<ol class="l1-steps" aria-label="What the agents did">
		{#if hasQualifier || step === 'qualifier'}
			<li class="l1-step" data-agent="qualifier">
				<span class="l1-agent">Qualifier</span>
				<div class="l1-step-body">
					{#each turn.qualifier.gates as g (g.at + g.point)}
						<span class="l1-v {gateTone(g.verdict)}" title="{g.point} gate · {g.latency_ms} ms"
							>{g.point} {g.verdict}</span
						>
						{#each notable(g) as v (v.key)}
							<span class="l1-v warn" title="Below threshold: asked again or fell back"
								>{v.label}: {v.value} ({Math.round(v.confidence * 100)}%)</span
							>
						{/each}
						{#if g.note}<span class="l1-note">{g.note}</span>{/if}
					{/each}
					{#if turn.qualifier.qualified}
						{@const q = turn.qualifier.qualified}
						<span class="l1-mono">{q.activity}</span>
						<span class="l1-dim">{q.object} · {q.modality}</span>
						{#each slots as [k, v] (k)}<span class="l1-slot">{k}={v}</span>{/each}
					{/if}
					{#if step === 'qualifier'}<span class="l1-working">working…</span>{/if}
				</div>
			</li>
		{/if}
		{#if hasProcedure || step === 'procedure'}
			<li class="l1-step" data-agent="procedure">
				<span class="l1-agent">Procedure</span>
				<div class="l1-step-body">
					{#each turn.procedure.ops as o (o.call_id)}
						<span class="l1-op {opTone(o)}" title={o.detail ?? `${o.route} · ${o.latency_ms ?? '—'} ms`}
							><span class="l1-mono">{o.op}</span> → {o.outcome}{#if o.instance}<span class="l1-dim">
									@{o.instance}</span
								>{/if}</span
						>
					{/each}
					{#each turn.procedure.knowledge as k (k.at + k.query)}
						<span class="l1-op ok" title="knowledge.search “{k.query}” in {k.store}"
							><span class="l1-mono">knowledge.search</span> → {k.passages.length} passage{k.passages
								.length === 1
								? ''
								: 's'}</span
						>
					{/each}
					{#if step === 'procedure'}<span class="l1-working">working…</span>{/if}
				</div>
			</li>
		{/if}
		{#if turn.postGate.length || step === 'post_gate'}
			<li class="l1-step" data-agent="post_gate">
				<span class="l1-agent">Post-gate</span>
				<div class="l1-step-body">
					{#each turn.postGate as g (g.at)}
						<span class="l1-v {gateTone(g.verdict)}">R1 {g.verdict}</span>
						{#each g.verdicts.filter((v) => v.value !== 'yes' || v.below_threshold) as v (v.key)}
							<span class="l1-v warn">{v.label}: {v.value}</span>
						{/each}
						<span class="l1-dim">{g.verdicts.map((v) => v.key).join(' · ')} checked</span>
						{#if g.note}<span class="l1-note">{g.note}</span>{/if}
					{/each}
					{#if step === 'post_gate' && !turn.postGate.length}<span class="l1-working"
							>checking the reply…</span
						>{/if}
				</div>
			</li>
		{/if}
	</ol>

	{#each turn.replies as r (r.at)}
		<p class="l1-said l1-said-agent">
			<span class="l1-who">Agent</span>{r.text}
			{#if r.used_knowledge === undefined && r.cites?.length}<span class="l1-cites"
					>cites {r.cites.join(', ')}</span
				>{/if}
		</p>
		{#if r.used_knowledge}<div class="l1-sources"><AnswerSources g={r} /></div>{/if}
	{/each}
	{#each turn.staff as m (m.at)}
		<p class="l1-said l1-said-staff"><span class="l1-who">{m.author} · staff</span>{m.text}</p>
	{/each}
	{#each turn.alerts as a (a.at)}
		<p class="l1-event err"><b>{a.code.replaceAll('_', ' ')}</b> · {a.text}</p>
	{/each}
	{#each turn.states as s (s.at + s.note)}
		<p class="l1-event"><b>{s.status.replaceAll('_', ' ')}</b> · {s.note}</p>
	{/each}
</li>
