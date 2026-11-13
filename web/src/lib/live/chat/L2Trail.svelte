<script lang="ts">
	// L2: the reasoning trail of one agent reply — the gate verdicts, the activity it qualified, the procedure
	// steps and the operations on the env's systems, the rules with the values this design used, citations and
	// post-gate checks — led by where the OTHER design (live when testing the draft) would answer differently.
	import type { ChatEvent } from '$lib/api/types/live';
	import { formatDraftRev } from '$lib/format';
	import { platformCite } from '$lib/grounding/grounding';

	let { messages }: { messages: ChatEvent[] } = $props();

	const turns = $derived(
		messages.flatMap((m, i) =>
			m.role === 'agent' && m.trace
				? [
						{
							m,
							t: m.trace,
							ask:
								messages
									.slice(0, i)
									.filter((x) => x.role === 'customer')
									.at(-1)?.text ?? '',
						},
					]
				: [],
		),
	);
	let picked = $state<number | null>(null);
	// A new reply moves the view to it; a picked older turn stays picked until then.
	let seen = 0;
	$effect(() => {
		if (turns.length !== seen) {
			seen = turns.length;
			picked = null;
		}
	});
	const idx = $derived(picked ?? turns.length - 1);
	const cur = $derived(turns[idx]);
	const otherName = $derived(
		cur?.t.design === 'draft' ? `Live (${cur.t.release_id ?? 'published'})` : 'The draft',
	);
	const STATE_ICON = { done: '✓', failed: '✕', asked: '?', stopped: '■' } as const;
	const VERDICT_ICON = { pass: '✓', fail: '✕', skipped: '–' } as const;
</script>

<section class="l2t" data-testid="chat-trail" aria-label="How the agent answered">
	<header class="l2t-head">
		<h2>How the agent answered</h2>
		{#if turns.length > 1}
			<div class="l2t-turns" role="group" aria-label="Pick a reply">
				{#each turns as _, i (i)}
					<button
						type="button"
						class="l2t-turn"
						class:on={i === idx}
						aria-pressed={i === idx}
						onclick={() => (picked = i)}>{i + 1}</button
					>
				{/each}
			</div>
		{/if}
	</header>

	{#if !cur}
		<p class="muted l2t-empty">
			Send a message. Each reply shows the gates it passed, the activity and procedure it ran, the operations
			on the test systems, the rules with the values it used — and where Live would answer differently.
		</p>
	{:else}
		{@const t = cur.t}
		<p class="l2t-q">
			<span class="muted">Reply {idx + 1} to</span> “{cur.ask}”
			<span class="chip" class:accent={t.design === 'draft'} class:warn={t.design === 'live'}
				>{t.design === 'draft'
					? `draft ${formatDraftRev(t.draft_rev, t.draft_fp)}`
					: `live ${t.release_id}`}</span
			>
		</p>

		{#if t.differs.length}
			<div class="l2t-diff" data-testid="trail-differs">
				<b>{otherName} would differ</b>
				<ul>
					{#each t.differs as d, i (i)}
						<li>
							<span class="chip sm">{d.kind}</span>
							{d.summary}
							{#if d.other_text}<blockquote>{d.other_text}</blockquote>{/if}
						</li>
					{/each}
				</ul>
			</div>
		{:else if t.gates.length}
			<p class="l2t-same">✓ {otherName} would give the same answer.</p>
		{/if}

		{#if t.gates.length}
			<h3>Gates</h3>
			<ul class="l2t-rows">
				{#each t.gates as g (g.gate)}
					<li>
						<span class="mono l2t-key">{g.gate}</span>
						<span class="l2t-main">
							<b>{g.label}</b>
							{#if g.other_label}<span class="l2t-other"
									>{t.design === 'draft' ? 'live' : 'draft'}: {g.other_label}</span
								>{/if}
							<small class="muted">{g.question}</small>
						</span>
					</li>
				{/each}
			</ul>
		{/if}

		<h3>
			Procedure{#if t.activity}<span class="muted">
					· {t.activity.label} <span class="mono">{t.activity.key}</span></span
				>{/if}
		</h3>
		<ol class="l2t-steps">
			{#each t.steps as s, i (i)}
				<li class="st-{s.state}">
					<span class="l2t-dot" aria-hidden="true">{STATE_ICON[s.state]}</span>
					{s.label}{#if s.detail}<span class="muted"> · {s.detail}</span>{/if}
				</li>
			{/each}
		</ol>

		{#if t.ops.length}
			<h3>Operations <span class="muted">· {t.env} env</span></h3>
			<ul class="l2t-rows">
				{#each t.ops as o, i (i)}
					<li>
						<span class="l2t-dot" class:bad={o.state !== 'ok'} aria-hidden="true"
							>{o.state === 'ok' ? '✓' : '✕'}</span
						>
						<span class="l2t-main">
							<span class="mono">{o.operation}</span>
							<small class="muted">{o.instance_id} · {o.code} · {o.latency_ms} ms</small>
						</span>
					</li>
				{/each}
			</ul>
		{/if}

		{#if t.checks.length}
			<h3>Rules</h3>
			<ul class="l2t-rows">
				{#each t.checks as c (c.id)}
					<li>
						<span class="l2t-dot" class:bad={c.verdict === 'fail'} aria-hidden="true"
							>{VERDICT_ICON[c.verdict]}</span
						>
						<span class="l2t-main">
							<b>{c.id} {c.label}</b> <span class="muted">{c.verdict}</span>
							<small>{c.detail}</small>
							{#if c.other_detail}<span class="l2t-other"
									>{t.design === 'draft' ? 'live' : 'draft'}: {c.other_detail}</span
								>{/if}
						</span>
					</li>
				{/each}
			</ul>
		{/if}

		{#if t.cites.length}
			<h3>Cited</h3>
			<div class="chips">
				{#each t.cites as c (c)}<span class="chip mono">{c}</span>{/each}
			</div>
		{/if}
		{#each (cur.m.sources ?? []).filter((s) => s.layer === 'platform') as s (s.chunk_id)}
			{@const pc = platformCite(s)}
			{#if pc}
				<div class="chips l2t-platform" data-testid="trail-platform-cite">
					{#if pc.href}<a class="chip accent" href={pc.href} target="_blank" rel="noopener noreferrer"
							>{pc.text}</a
						>{:else}<span class="chip accent">{pc.text}</span>{/if}
					<small class="muted">{s.title}{pc.licence ? ` · ${pc.licence}` : ''}</small>
				</div>
			{/if}
		{/each}

		<h3>Post-gate checks</h3>
		<ul class="l2t-rows">
			{#each t.post_checks as p (p.key)}
				<li>
					<span class="l2t-dot" class:bad={p.verdict === 'fail'} aria-hidden="true"
						>{VERDICT_ICON[p.verdict]}</span
					>
					<span class="l2t-main">{p.label} <small class="muted">{p.detail}</small></span>
				</li>
			{/each}
		</ul>
	{/if}
</section>

<style>
	.l2t {
		border: 1px solid var(--line);
		border-radius: 10px;
		background: var(--panel);
		padding: 12px 14px;
		min-width: 0;
		font-size: 13px;
	}
	.l2t-head {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 8px;
	}
	.l2t-head h2 {
		margin: 0;
		font-size: 14px;
	}
	.l2t-turns {
		display: flex;
		flex-wrap: wrap;
		gap: 4px;
	}
	.l2t-turn {
		min-width: 26px;
		height: 24px;
		border: 1px solid var(--line);
		border-radius: 5px;
		background: var(--panel);
		color: var(--fg);
		font: 12px var(--mono);
		cursor: pointer;
	}
	.l2t-turn.on {
		border-color: var(--accent);
		color: var(--accent);
		font-weight: 600;
	}
	.l2t-empty {
		margin: 10px 0 0;
		line-height: 1.5;
	}
	.l2t-q {
		margin: 8px 0;
		line-height: 1.5;
	}
	.l2t-diff {
		border: 1px solid color-mix(in srgb, var(--warn) 45%, transparent);
		background: color-mix(in srgb, var(--warn) 8%, var(--panel));
		border-radius: 8px;
		padding: 8px 10px;
	}
	.l2t-diff ul {
		margin: 6px 0 0;
		padding-left: 16px;
		display: grid;
		gap: 6px;
	}
	.l2t-diff blockquote {
		margin: 4px 0 0;
		padding: 4px 8px;
		border-left: 3px solid var(--warn);
		background: var(--sunk);
		border-radius: 4px;
	}
	.l2t-same {
		margin: 0;
		color: var(--ok);
	}
	h3 {
		margin: 14px 0 6px;
		font-size: 11px;
		text-transform: uppercase;
		letter-spacing: 0.06em;
		color: var(--muted);
	}
	h3 .muted {
		text-transform: none;
		letter-spacing: 0;
	}
	.l2t-rows {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 6px;
	}
	.l2t-rows li {
		display: flex;
		gap: 8px;
		align-items: baseline;
		min-width: 0;
	}
	.l2t-key {
		flex: none;
		min-width: 26px;
		font-size: 11px;
		color: var(--accent);
	}
	.l2t-main {
		display: flex;
		flex-direction: column;
		min-width: 0;
		overflow-wrap: anywhere;
	}
	.l2t-other {
		font-size: 12px;
		color: var(--warn);
	}
	.l2t-steps {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 4px;
	}
	.l2t-dot {
		display: inline-grid;
		place-items: center;
		flex: none;
		width: 16px;
		height: 16px;
		margin-right: 4px;
		border-radius: 50%;
		font-size: 10px;
		background: color-mix(in srgb, var(--ok) 18%, transparent);
		color: var(--ok);
	}
	.l2t-dot.bad,
	.st-failed .l2t-dot {
		background: color-mix(in srgb, var(--err) 18%, transparent);
		color: var(--err);
	}
	.st-asked .l2t-dot {
		background: color-mix(in srgb, var(--accent) 18%, transparent);
		color: var(--accent);
	}
	.st-stopped .l2t-dot {
		background: color-mix(in srgb, var(--warn) 18%, transparent);
		color: var(--warn);
	}
</style>
