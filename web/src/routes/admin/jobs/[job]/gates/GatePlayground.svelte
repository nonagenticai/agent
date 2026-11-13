<script lang="ts">
	// J7 playground: run one message through one gate point of this job (`POST /api/jobs/{job}/gates/evaluate`) and
	// see every question's answer, confidence and what the point decided (allow / block / handover). D54: it runs the
	// DRAFT when the draft's gates differ from the live release (what the test chat runs), else live; either can be
	// picked. Nothing is stored and no case opens. Read-only, so every role may use it.
	import { api } from '$lib/app.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Seg from '$lib/ui/Seg.svelte';
	import { ApiError } from '$lib/api/errors';
	import type { GateEvaluation, GateRunPoint } from '$lib/api/types/ontology';
	import { ACTION_TEXT, DECISION_TEXT, RUN_POINT_LABEL, ruleSummary } from '$lib/ontology/gateRules';

	let { tick = null }: { tick?: unknown } = $props();

	const SAMPLES: { label: string; point: GateRunPoint; text: string; tool?: string }[] = [
		{
			label: 'Return',
			point: 'request',
			text: 'Hi! I want to return the Alba linen throw from order O-1001, the colour is wrong.',
		},
		{ label: 'Card number', point: 'request', text: 'My card is 4111 1111 1111 1111, please refund it.' },
		{
			label: 'Unsure safety (fails closed)',
			point: 'request',
			text: 'I think maybe the heater is unsafe, not sure.',
		},
		{
			label: 'Chargeback before refund',
			point: 'tool',
			text: 'Refund me now or I file a chargeback with my bank.',
			tool: 'refund.issue',
		},
		{
			label: 'Unsupported reply',
			point: 'response',
			text: 'Your parcel will definitely arrive tomorrow, guarantee.',
		},
	];

	let point = $state<GateRunPoint>('request');
	let text = $state('');
	let tool = $state('');
	let against = $state<'auto' | 'live' | 'draft'>('auto');
	let busy = $state(false);
	let error = $state<string | null>(null);
	let result = $state<GateEvaluation | null>(null);

	// A design change elsewhere on the page (a saved refinement) makes the last result stale.
	$effect(() => {
		void tick;
		result = null;
	});

	function sample(s: (typeof SAMPLES)[number]) {
		point = s.point;
		text = s.text;
		tool = s.tool ?? '';
		void run();
	}
	async function run(e?: SubmitEvent) {
		e?.preventDefault();
		busy = true;
		error = null;
		try {
			result = await api().evaluateGates({
				point,
				text,
				...(point === 'tool' && tool.trim() ? { tool: tool.trim() } : {}),
				...(against === 'auto' ? {} : { against }),
			});
		} catch (err) {
			result = null;
			error = err instanceof ApiError ? err.message : String(err);
		} finally {
			busy = false;
		}
	}
	const pct = (n: number) => `${Math.round(n * 100)}%`;
	const DECISION_VARIANT = { allow: 'ok', block: 'warn', handover: 'err' } as const;
</script>

<section class="card gp" aria-label="Gate playground" data-testid="gate-playground">
	<header class="gp-h">
		<h2>Playground</h2>
		<span class="muted">Try a message against this job's gates. Nothing is stored; no case opens.</span>
	</header>
	<form class="gp-form" onsubmit={run}>
		<div class="gp-row">
			<Seg
				label="Gate point"
				items={(['request', 'tool', 'response'] as const).map((p) => [p, RUN_POINT_LABEL[p]])}
				bind:active={point}
			/>
			{#if point === 'tool'}
				<input
					class="input mono gp-tool"
					aria-label="Tool name"
					placeholder="tool, e.g. refund.issue"
					bind:value={tool}
				/>
			{/if}
			<Seg
				label="Design to run"
				items={[
					['auto', 'Auto'],
					['draft', 'Draft'],
					['live', 'Live'],
				]}
				bind:active={against}
			/>
		</div>
		<textarea
			class="input"
			rows="2"
			aria-label="Message text"
			placeholder={point === 'response' ? 'A draft reply the bot would send' : 'A customer message'}
			bind:value={text}
		></textarea>
		<div class="gp-row">
			<button type="submit" class="btn primary sm" disabled={busy || !text.trim()}>Evaluate</button>
			<span class="muted gp-samples-l">Samples:</span>
			{#each SAMPLES as s (s.label)}
				<button type="button" class="btn sm" disabled={busy} onclick={() => sample(s)}>{s.label}</button>
			{/each}
		</div>
	</form>

	{#if error}<p class="gp-err" role="alert">{error}</p>{/if}
	{#if result}
		{@const r = result}
		<div class="gp-res" role="status" data-testid="gate-eval-result">
			<div class="gp-dec">
				<Chip text={DECISION_TEXT[r.decision]} variant={DECISION_VARIANT[r.decision]} />
				<span>
					{r.gate_point} · {RUN_POINT_LABEL[r.point]}{#if r.deciding.length}
						— decided by <span class="mono">{r.deciding.join(', ')}</span>{/if}
				</span>
				<Chip
					text="ran against {r.ran_against}"
					variant={r.ran_against === 'draft' ? 'warn' : 'ok'}
					title={r.ran_against === 'draft'
						? 'The draft design: what the test chat runs'
						: `The live release${r.release_id ? ` ${r.release_id}` : ''}: what customers get`}
				/>
				<span class="muted gp-diff">
					{r.draft_differs
						? `Draft gates differ from live${r.release_id ? ` (${r.release_id})` : ''}.`
						: 'Draft and live gates are the same here.'}
				</span>
			</div>
			<div class="tbl-wrap">
				<table class="tbl dense">
					<thead>
						<tr><th>Question</th><th>Answer</th><th class="r">Confidence</th><th>Rule</th><th>Result</th></tr>
					</thead>
					<tbody>
						{#each r.answers as a (a.gate)}
							<tr class:hot={a.action === 'handover' || a.action === 'block'}>
								<td>
									<span class="mono gp-key">{a.gate}</span>
									<span class="gp-q">{a.question}</span>
								</td>
								<td>
									<b>{a.answer}</b>
									<span class="gp-probs muted">
										{#each a.probabilities.slice(0, 3) as p (p.key)}<span>{p.label} {pct(p.p)}</span>{/each}
									</span>
								</td>
								<td class="r mono">{pct(a.confidence)}</td>
								<td class="mono gp-rule">{ruleSummary(a.type, a.rule)} · fail {a.fail}</td>
								<td>
									{#if a.unsure}
										<Chip
											text={a.fail === 'closed' ? 'unsure → fails closed' : 'unsure → fails open'}
											variant={a.fail === 'closed' ? 'err' : 'muted'}
										/>
									{:else if a.fired}
										<Chip
											text="fired · {a.action}"
											variant={a.action === 'annotate' ? 'accent' : a.action === 'block' ? 'warn' : 'err'}
											title={a.action ? ACTION_TEXT[a.action] : undefined}
										/>
									{:else}
										<Chip text="pass" />
									{/if}
								</td>
							</tr>
						{:else}
							<tr><td colspan="5" class="muted">No question is asked at this point for this input.</td></tr>
						{/each}
					</tbody>
				</table>
			</div>
			{#if r.skipped.length}
				<p class="note">
					Not asked:
					{#each r.skipped as s, i (s.gate)}{i ? ' · ' : ''}<span class="mono">{s.gate}</span>
						({s.reason}){/each}
				</p>
			{/if}
		</div>
	{/if}
</section>

<style>
	.gp {
		padding: 14px;
		margin-bottom: 16px;
		min-width: 0;
	}
	.gp-h {
		display: flex;
		flex-wrap: wrap;
		gap: 2px 10px;
		align-items: baseline;
		margin-bottom: 8px;
	}
	.gp-h h2 {
		margin: 0;
		font: 600 15px/1.3 var(--cond);
	}
	.gp-form {
		display: grid;
		gap: 8px;
	}
	.gp-row {
		display: flex;
		flex-wrap: wrap;
		gap: 6px 10px;
		align-items: center;
	}
	.gp-tool {
		width: 200px;
		max-width: 100%;
	}
	.gp-samples-l {
		font-size: 12.5px;
	}
	.gp-err {
		color: var(--err);
		margin: 8px 0 0;
	}
	.gp-res {
		margin-top: 12px;
		display: grid;
		gap: 8px;
	}
	.gp-dec {
		display: flex;
		flex-wrap: wrap;
		gap: 4px 10px;
		align-items: center;
	}
	.gp-diff {
		font-size: 12.5px;
	}
	.gp-key {
		font-weight: 600;
		color: var(--tool);
		margin-right: 6px;
	}
	.gp-q {
		font-size: 12.5px;
	}
	.gp-probs {
		display: flex;
		flex-wrap: wrap;
		gap: 0 8px;
		font-size: 11.5px;
	}
	.gp-rule {
		font-size: 11.5px;
	}
	tr.hot td {
		background: color-mix(in srgb, var(--err) 5%, var(--panel));
	}
</style>
