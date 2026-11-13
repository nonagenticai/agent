<script lang="ts">
	// J5: one conversation of this activity, left to right — what the customer says, the gates that recognise it,
	// the procedure the orchestrator runs (each step an object operation routed to a system), and what changes.
	import type { ActivityDetail, ActivityUseCase } from '$lib/api/types/journey';
	import { ROUTE_CHIP, routeText } from '$lib/journey/J5Routes';

	let { d, u, base }: { d: ActivityDetail; u: ActivityUseCase; base: string } = $props();
	const slots = $derived(d.activity.objects.flatMap((o) => o.required_inputs));
	const writes = $derived(u.state_changes.length > 0);
</script>

<ol class="j5-flow" data-testid="use-case-flow">
	<li class="j5-col">
		<h3><span class="j5-n">1</span>Customer says</h3>
		<ul class="j5-quotes">
			{#each u.examples as e (e)}<li>“{e}”</li>{/each}
		</ul>
		<p class="j5-foot">
			{#if slots.length}
				Needs <b class="mono">{slots.join(', ')}</b> — asked for if the message does not contain it.
			{:else}
				No details needed before the agent can act.
			{/if}
		</p>
	</li>

	<li class="j5-col">
		<h3><span class="j5-n">2</span>Agent recognises it</h3>
		<ul class="j5-gates">
			{#each u.gates as g (g.key)}
				<li>
					<span class="chip tool" title="{g.point} · threshold {g.threshold}">{g.key}</span>
					<span class="j5-q">{g.question}</span>
					<span class="j5-a"
						>→ <b>{g.answer}</b>{#if g.verdicts_7d != null}<span class="muted">
								· {g.verdicts_7d}× in 7 d</span
							>{/if}</span
					>
				</li>
			{/each}
		</ul>
		<p class="j5-foot">
			Pre-gates answer these before any system is called. <a href="{base}/gates">Gates</a>
		</p>
	</li>

	<li class="j5-col">
		<h3><span class="j5-n">3</span>Runs the procedure</h3>
		{#if u.procedure}
			<ol class="j5-steps">
				{#each u.procedure.steps as s (s.n)}
					<li class:j5-write={s.side_effect !== 'R'}>
						<span class="j5-sn">{s.n}</span>
						<div>
							<div>{s.name}</div>
							<span class="mono">{s.op}</span>
							<div class="j5-tags">
								<span class="chip {ROUTE_CHIP[s.prod.kind]}" title="prod route">{routeText(s.prod)}</span>
								{#if s.confirm_back}<span class="chip warn">customer confirms first</span>{/if}
								{#if s.gate}<span class="chip muted">stops if not allowed</span>{/if}
								<span class="chip muted" title="identity level">id {s.identity_level}</span>
							</div>
						</div>
					</li>
				{/each}
			</ol>
			<p class="j5-foot">
				<a href="{base}/procedures/{u.procedure.name}">{u.procedure.name} v{u.procedure.version}</a>
				· {u.procedure.origin === 'tenant' ? 'your version' : 'template version'}
				{#if u.procedure.runs_7d}· {u.procedure.runs_7d} runs in 7 d{/if}
				{#if u.procedure.success_rate != null}· {Math.round(u.procedure.success_rate * 100)} % succeed{/if}
			</p>
		{:else if d.operations.length}
			<ol class="j5-steps">
				{#each d.operations as o, i (o.op + (o.variant ?? ''))}
					<li class:j5-write={o.side_effect !== 'R'}>
						<span class="j5-sn">{i + 1}</span>
						<div>
							<span class="mono">{o.op}{o.variant ? `{${o.variant}}` : ''}</span>
							<div class="j5-tags">
								<span class="chip {ROUTE_CHIP[o.routes[0]!.kind]}">{routeText(o.routes[0]!)}</span>
								{#if !o.required}<span class="chip muted">optional</span>{/if}
							</div>
						</div>
					</li>
				{/each}
			</ol>
			<p class="j5-foot">
				No procedure version yet: the release cut compiles one from these operations into
				<span class="mono">{d.procedure}</span>.
			</p>
		{:else}
			<p class="muted">No system is called: the agent answers from the knowledge store.</p>
		{/if}
	</li>

	<li class="j5-col">
		<h3><span class="j5-n">4</span>Outcome</h3>
		{#if writes}
			<ul class="j5-changes">
				{#each u.state_changes as c (c.verb + (c.to ?? ''))}
					<li>
						<span class="mono">{c.verb}</span>
						{#if c.to}
							<div class="j5-trans">
								<span class="j5-from"
									>{#each c.from as f (f)}<span class="chip muted">{f}</span>{/each}</span
								>
								<span aria-hidden="true">→</span>
								<span class="chip accent">{c.to}</span>
							</div>
							<span class="cell-sub">Any other status: refused, nothing changes.</span>
						{:else if c.from.length}
							<span class="cell-sub">Allowed while {c.object} is {c.from.join(' / ')}; status unchanged.</span
							>
						{:else}
							<span class="cell-sub">Creates or updates a {c.object} record.</span>
						{/if}
					</li>
				{/each}
			</ul>
		{:else}
			<p class="j5-readonly"><b>Reads only</b> — changes nothing in your systems.</p>
		{/if}
		<h4>Before the reply is sent</h4>
		<ul class="j5-checks">
			{#each u.checks as c (c.key)}
				<li><span class="chip tool">{c.key}</span> {c.text}</li>
			{:else}
				<li class="muted">No reply checks apply.</li>
			{/each}
		</ul>
	</li>
</ol>

<style>
	.j5-flow {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		grid-template-columns: repeat(4, minmax(0, 1fr));
		gap: 22px;
	}
	.j5-col {
		position: relative;
		background: var(--sunk);
		border-radius: var(--r);
		padding: 12px 12px 10px;
		display: flex;
		flex-direction: column;
		gap: 8px;
	}
	.j5-col + .j5-col::before {
		content: '→';
		position: absolute;
		left: -18px;
		top: 12px;
		color: var(--muted);
		font-size: 15px;
	}
	h3 {
		margin: 0;
		font: 600 13px/1.3 var(--sans);
		display: flex;
		align-items: center;
		gap: 8px;
	}
	h4 {
		margin: 6px 0 0;
		font: 600 11px/1.2 var(--cond);
		letter-spacing: 0.07em;
		text-transform: uppercase;
		color: var(--muted);
	}
	.j5-n {
		display: inline-grid;
		place-items: center;
		width: 20px;
		height: 20px;
		border-radius: 50%;
		background: var(--accent);
		color: var(--panel);
		font: 600 11px/1 var(--mono);
		flex: none;
	}
	ul,
	ol {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 6px;
	}
	.j5-quotes li {
		background: var(--panel);
		border-left: 3px solid var(--cust);
		border-radius: 6px;
		padding: 5px 8px;
		font-size: 13px;
	}
	/* chip stays at the top of a wrapped question */
	.j5-gates li {
		display: grid;
		grid-template-columns: auto minmax(0, 1fr);
		column-gap: 8px;
		align-items: start;
		font-size: 12.5px;
	}
	.j5-gates .chip {
		align-self: start;
	}
	.j5-q {
		color: var(--muted);
	}
	.j5-a {
		grid-column: 2;
	}
	.j5-steps li {
		display: grid;
		grid-template-columns: auto minmax(0, 1fr);
		gap: 8px;
		background: var(--panel);
		border: 1px solid var(--line-2);
		border-radius: 6px;
		padding: 6px 8px;
		font-size: 13px;
	}
	.j5-steps li.j5-write {
		border-color: color-mix(in srgb, var(--warn) 45%, var(--line));
	}
	.j5-steps .mono {
		overflow-wrap: anywhere;
	}
	.j5-sn {
		font: 600 11px/1.6 var(--mono);
		color: var(--muted);
	}
	.j5-tags {
		display: flex;
		flex-wrap: wrap;
		gap: 4px;
		margin-top: 4px;
	}
	.j5-changes li {
		background: var(--panel);
		border-radius: 6px;
		padding: 6px 8px;
		display: grid;
		gap: 4px;
	}
	.j5-trans {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: 6px;
	}
	.j5-from {
		display: inline-flex;
		flex-wrap: wrap;
		gap: 3px;
	}
	.j5-readonly {
		margin: 0;
		padding: 8px;
		border-radius: 6px;
		background: color-mix(in srgb, var(--ok) 10%, var(--panel));
		font-size: 13px;
	}
	.j5-checks li {
		font-size: 12.5px;
		display: flex;
		gap: 6px;
		align-items: baseline;
	}
	.j5-foot {
		margin: auto 0 0;
		font-size: 12px;
		color: var(--muted);
	}
	@media (max-width: 1100px) {
		.j5-flow {
			grid-template-columns: repeat(2, minmax(0, 1fr));
		}
		.j5-col:nth-child(3)::before {
			content: '↓';
			left: 12px;
			top: -20px;
		}
	}
	@media (max-width: 640px) {
		.j5-flow {
			grid-template-columns: minmax(0, 1fr);
		}
		.j5-col + .j5-col::before {
			content: '↓';
			left: 12px;
			top: -20px;
		}
	}
</style>
