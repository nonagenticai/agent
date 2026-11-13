<script lang="ts">
	// J7 concept band: one conversation turn of this job with its three gate points (same model as the P14 library
	// flow: P1 message gate → qualifier → P2 request gate → procedure → R1 resolution gate). Each gate point shows how
	// many questions this job asks there and what they did in the last 7 days; clicking one filters the list.
	import Lock from '$lib/ontology/Lock.svelte';
	import { fmt } from '$lib/format';
	import { TURN, acted } from '$lib/ontology/gatesJ7';
	import type { EffGateQuestion, GatePoint } from '$lib/api/types/ontology';

	let {
		questions,
		point,
		onpick,
	}: {
		questions: EffGateQuestion[];
		point: GatePoint | 'all';
		onpick: (p: GatePoint | 'all') => void;
	} = $props();

	const STAGES: Record<GatePoint, { before: string; beforeSub: string }> = {
		P1: { before: 'Customer message', beforeSub: 'arrives on a channel' },
		P2: { before: 'Qualifier', beforeSub: 'builds the request' },
		R1: { before: 'Procedure', beforeSub: 'runs operations, drafts a reply' },
	};

	function sumUp(p: GatePoint) {
		const qs = questions.filter((q) => q.point === p);
		const safety = qs.filter((q) => q.safety);
		return {
			n: qs.length,
			closed: qs.filter((q) => q.fail === 'closed').length,
			checked: Math.max(0, ...qs.map((q) => q.stats.verdicts_7d)),
			fired: safety.reduce((a, q) => a + (acted(q) ?? 0), 0),
			unsure: qs.filter((q) => !q.safety).reduce((a, q) => a + q.stats.below_threshold, 0),
			outages: qs.reduce((a, q) => a + q.stats.fail_open + q.stats.fail_closed, 0),
			refined: qs.filter((q) => q.threshold_refined || q.options.some((o) => o.refined)).length,
		};
	}
	const toggle = (p: GatePoint) => onpick(point === p ? 'all' : p);
</script>

<ol class="jtf" aria-label="Where this job's gates run in one conversation turn" data-testid="job-gate-flow">
	{#each TURN as s (s.point)}
		{@const st = STAGES[s.point]}
		{@const u = sumUp(s.point)}
		<li class="jtf-step stage">
			<div class="jtf-stage">
				<span class="jtf-label">{st.before}</span>
				<span class="jtf-sub">{st.beforeSub}</span>
			</div>
		</li>
		<li class="jtf-step gate">
			<button
				type="button"
				class="jtf-gate"
				class:on={point === s.point}
				aria-pressed={point === s.point}
				onclick={() => toggle(s.point)}
			>
				<span class="jtf-head"><span class="jtf-pt mono">{s.point}</span> <b>{s.label}</b></span>
				<span class="jtf-sub">{s.sub}</span>
				<span class="jtf-n">
					{u.n}
					{u.n === 1 ? 'question' : 'questions'}{#if u.closed}<span class="jtf-closed"
							><Lock /> {u.closed} fail closed</span
						>{/if}{#if u.refined}<span class="jtf-ref">{u.refined} refined</span>{/if}
				</span>
				{#if u.checked}
					<span class="jtf-week">
						<span><b>{fmt.n(u.checked)}</b> checked</span>
						{#if u.closed}<span class:hot={u.fired > 0}><b>{fmt.n(u.fired)}</b> safety stops</span>{/if}
						<span><b>{fmt.n(u.unsure)}</b> unsure</span>
						<span class:warm={u.outages > 0}><b>{fmt.n(u.outages)}</b> outages</span>
					</span>
				{:else}
					<span class="jtf-week muted">No verdicts yet</span>
				{/if}
			</button>
		</li>
	{/each}
	<li class="jtf-step stage">
		<div class="jtf-stage">
			<span class="jtf-label">Reply sent</span>
			<span class="jtf-sub">or a person takes over</span>
		</div>
	</li>
</ol>

<style>
	.jtf {
		list-style: none;
		margin: 0 0 6px;
		padding: 0;
		display: flex;
		align-items: stretch;
	}
	.jtf-step {
		display: flex;
		align-items: stretch;
		position: relative;
		min-width: 0;
	}
	.jtf-step.stage {
		flex: 0.7 1 0;
	}
	.jtf-step.gate {
		flex: 1.5 1 0;
	}
	.jtf-step + .jtf-step {
		padding-left: 18px;
	}
	.jtf-step + .jtf-step::before {
		content: '';
		position: absolute;
		left: 3px;
		top: 50%;
		width: 10px;
		border-top: 2px solid var(--line);
	}
	.jtf-step + .jtf-step::after {
		content: '';
		position: absolute;
		left: 10px;
		top: calc(50% - 4px);
		border: 5px solid transparent;
		border-left-color: var(--line);
		border-right: 0;
	}
	.jtf-stage,
	.jtf-gate {
		display: flex;
		flex-direction: column;
		gap: 4px;
		width: 100%;
		padding: 10px 12px;
		border-radius: 8px;
		text-align: left;
		min-width: 0;
	}
	.jtf-stage {
		border: 1px dashed var(--line);
		justify-content: center;
		color: var(--muted);
	}
	.jtf-gate {
		border: 1px solid color-mix(in srgb, var(--tool) 45%, var(--line));
		background: color-mix(in srgb, var(--tool) 7%, var(--panel));
		color: var(--fg);
		cursor: pointer;
		font: inherit;
	}
	.jtf-gate:hover {
		border-color: var(--tool);
	}
	.jtf-gate.on {
		border-color: var(--tool);
		box-shadow: 0 0 0 2px color-mix(in srgb, var(--tool) 35%, transparent);
	}
	.jtf-head {
		font: 600 13.5px/1.25 var(--cond);
	}
	.jtf-pt {
		color: var(--tool);
	}
	.jtf-label {
		font: 500 13px/1.25 var(--cond);
		color: var(--fg);
	}
	.jtf-sub {
		font-size: 12px;
		line-height: 1.35;
		color: var(--muted);
	}
	.jtf-n {
		font-size: 12px;
		display: flex;
		flex-wrap: wrap;
		gap: 2px 10px;
	}
	.jtf-closed {
		color: var(--err);
	}
	.jtf-ref {
		color: var(--accent);
	}
	.jtf-week {
		margin-top: auto;
		padding-top: 6px;
		border-top: 1px solid var(--line-2);
		font-size: 12px;
		display: flex;
		flex-wrap: wrap;
		gap: 2px 10px;
		color: var(--muted);
	}
	.jtf-week b {
		color: var(--fg);
		font-family: var(--mono);
		font-weight: 600;
	}
	.jtf-week .hot b {
		color: var(--err);
	}
	.jtf-week .warm b {
		color: var(--warn);
	}
	@media (max-width: 980px) {
		.jtf {
			flex-direction: column;
		}
		.jtf-step + .jtf-step {
			padding-left: 0;
			padding-top: 16px;
		}
		.jtf-step + .jtf-step::before {
			left: 22px;
			top: 2px;
			width: 0;
			height: 8px;
			border-top: 0;
			border-left: 2px solid var(--line);
		}
		.jtf-step + .jtf-step::after {
			left: 18px;
			top: 9px;
			border: 5px solid transparent;
			border-top-color: var(--line);
			border-bottom: 0;
		}
		.jtf-stage {
			flex-direction: row;
			align-items: baseline;
			gap: 8px;
			padding: 6px 12px;
		}
	}
</style>
