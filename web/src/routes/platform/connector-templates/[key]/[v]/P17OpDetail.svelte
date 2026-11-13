<script lang="ts">
	// P17 one operation as a translation pipeline: canonical call → request → vendor response → canonical fields,
	// canonical status, canonical error (doc 10 §4.2, doc 11 §5.3, doc 12 §8.1), then write safety and fixtures.
	import type { ConnectorTemplateOp, SideEffect } from '$lib/api/types/connectors';
	import type { ErrorStateDef } from '$lib/api/types/model';
	import { idempotencyLabel, needsKey } from '$lib/connectors/ops';
	import { classifyCode, type OpDiff } from '$lib/connectors/P17-diff';
	import Chip from '$lib/ui/Chip.svelte';
	import JsonView from '$lib/ui/JsonView.svelte';

	let {
		op,
		vendor,
		change,
		taxonomy,
		states,
	}: {
		op: ConnectorTemplateOp;
		vendor: string;
		change: OpDiff | null;
		taxonomy: ErrorStateDef[] | null;
		/** Canonical states of op.object (null while unknown). */
		states: string[] | null;
	} = $props();

	const EFFECT: Record<SideEffect, { text: string; variant: 'muted' | 'work' | 'warn' | 'err' }> = {
		R: { text: 'R · read only', variant: 'muted' },
		W: { text: 'W · reversible write', variant: 'work' },
		X: { text: 'X · irreversible write', variant: 'err' },
		P: { text: 'P · payment', variant: 'err' },
	};
	const write = $derived(needsKey(op.side_effect));
	const idemRefused = $derived(write && op.idempotency.kind === 'none');
	const stateOk = (s: string) => !states || states.includes(s.split(':')[0]!.trim());
</script>

<div class="p17-op" data-testid="p17-op-detail">
	<div class="p17-op-head">
		<Chip text={EFFECT[op.side_effect].text} variant={EFFECT[op.side_effect].variant} />
		<span class="mono small">{op.method} {op.path}</span>
		<span class="cell-sub">{op.op_kind} · timeout {op.timeout_ms} ms</span>
		{#if change && change.change !== 'same'}
			<Chip
				text="{change.change} in this version"
				variant={change.change === 'added' ? 'ok' : 'work'}
				title={change.details.join('\n')}
			/>
		{/if}
	</div>

	<ol class="p17-steps">
		<li>
			<h3 class="p17-h"><span class="p17-n">1</span> Canonical call → {vendor} request</h3>
			<p class="p17-hint">
				Agent fills <span class="mono">{'{{canonical.*}}'}</span> slots; secrets and the idempotency key are injected
				by the runner, never by the model.
			</p>
			<pre class="code">{op.request || '(no body — path parameters only)'}</pre>
			<p class="small">Pagination: <span class="mono">{op.pagination ?? 'none'}</span></p>
		</li>

		<li>
			<h3 class="p17-h"><span class="p17-n">2</span> Response → canonical fields</h3>
			{#if op.field_map.length === 0}<p class="muted">Returns the canonical shape as-is.</p>{:else}
				<div class="tbl-wrap p17-scroll">
					<table class="tbl dense">
						<thead><tr><th>{vendor} JSONPath</th><th>Canonical field</th><th>Transform</th></tr></thead>
						<tbody>
							{#each op.field_map as f, i (i)}
								<tr>
									<td class="mono small">{f.path}</td>
									<td class="mono"><b>{f.field}</b></td>
									<td class="mono small">{f.transform ?? '—'}</td>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
			{/if}
		</li>

		<li>
			<h3 class="p17-h">
				<span class="p17-n">3</span>
				{vendor} status → canonical <span class="mono">{op.object}</span> status
			</h3>
			{#if op.status_map.length === 0}
				<p class="muted">This operation returns no status field.</p>
			{:else}
				<ul class="p17-map">
					{#each op.status_map as s, i (i)}
						<li>
							<span class="mono small p17-sys">{s.system}</span>
							<span class="p17-arrow" aria-hidden="true">→</span>
							<span class="p17-can">
								<Chip text={s.canonical} variant={stateOk(s.canonical) ? 'accent' : 'warn'} />
								{#if !stateOk(s.canonical)}<span class="cell-sub"
										>not a state of {op.object} — an error outcome</span
									>{/if}
							</span>
						</li>
					{/each}
				</ul>
			{/if}
		</li>

		<li>
			<h3 class="p17-h"><span class="p17-n">4</span> {vendor} fault → canonical error</h3>
			<p class="p17-hint">
				User outcomes get a reply; system errors escalate to a person (doc 10 §3.1). Never free text.
			</p>
			<ul class="p17-map">
				{#each op.fault_map as f, i (i)}
					{@const c = taxonomy ? classifyCode(f.code, taxonomy) : null}
					<li>
						<span class="mono small p17-sys">{f.system}</span>
						<span class="p17-arrow" aria-hidden="true">→</span>
						<span class="p17-can">
							<span class="mono">{f.code}</span>
							{#if c?.handling === 'reply'}<Chip
									text="agent replies"
									variant="cust"
									title={c.def?.description}
								/>
							{:else if c?.handling === 'escalate'}<Chip
									text="escalates{c.def?.retryable ? ' · retryable' : ''}"
									variant="warn"
									title={c.def?.description}
								/>
							{:else if c}<Chip
									text="not in error_state_def"
									variant="err"
									title="Used by doc 12 but missing from the canonical taxonomy"
								/>{/if}
						</span>
					</li>
				{/each}
			</ul>
		</li>

		<li>
			<h3 class="p17-h"><span class="p17-n">5</span> Write safety</h3>
			<dl class="kvs">
				<dt>Idempotency</dt>
				<dd>
					<span class="mono">{idempotencyLabel(op.idempotency)}</span>
					{#if idemRefused}<Chip text="refused for {op.side_effect}" variant="err" />{/if}
					<span class="cell-sub"
						>{write
							? 'A retried write must not happen twice: the step key travels in a header, or a search for it runs before the create.'
							: 'Reads are naturally idempotent.'}</span
					>
				</dd>
				<dt>verify_read</dt>
				<dd>
					<span class="mono">{op.verify_read ?? (write ? '— (confirm-back only)' : 'n/a (read)')}</span>
					{#if write}<span class="cell-sub"
							>After a timeout the write is <span class="mono">unknown_outcome</span> until this read confirms
							it.</span
						>{/if}
				</dd>
				{#if op.op_kind === 'hosted_payment_link'}
					<dt>Payment</dt>
					<dd>{op.side_effect} · hosted payment link only (D27)</dd>
				{/if}
			</dl>
		</li>

		<li>
			<h3 class="p17-h"><span class="p17-n">6</span> Recorded fixtures (T1)</h3>
			{#if op.fixtures.length === 0}<p class="muted">No fixtures recorded yet — T1 cannot run.</p>{:else}
				{#each op.fixtures as fx (fx.name)}
					<details class="p17-fx">
						<summary
							><span class="mono">{fx.status || 'timeout'}</span>
							{fx.name} <span class="muted">→ {fx.outcome}</span></summary
						>
						<JsonView value={fx.body} />
					</details>
				{/each}
			{/if}
		</li>
	</ol>
</div>

<style>
	.p17-op-head {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
		align-items: center;
		margin-bottom: 12px;
		overflow-wrap: anywhere;
	}
	.p17-steps {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 18px;
	}
	.p17-steps > li {
		border-left: 2px solid var(--line);
		padding-left: 14px;
		min-width: 0;
	}
	.p17-h {
		margin: 0 0 6px;
		font-size: 13px;
		font-weight: 600;
		display: flex;
		gap: 8px;
		align-items: center;
		flex-wrap: wrap;
	}
	.p17-n {
		display: inline-grid;
		place-items: center;
		width: 20px;
		height: 20px;
		border-radius: 50%;
		background: var(--sunk);
		border: 1px solid var(--line);
		font-size: 11px;
		margin-left: -26px;
	}
	.p17-hint {
		margin: 0 0 6px;
		color: var(--muted);
		font-size: 12px;
	}
	.p17-map {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 4px;
	}
	.p17-map li {
		display: grid;
		grid-template-columns: minmax(0, 1fr) auto minmax(0, 1fr);
		gap: 8px;
		align-items: baseline;
		padding: 4px 0;
		border-bottom: 1px solid var(--line-2);
	}
	.p17-map li:last-child {
		border-bottom: 0;
	}
	.p17-sys {
		overflow-wrap: anywhere;
	}
	.p17-arrow {
		color: var(--muted);
	}
	.p17-can {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		align-items: baseline;
	}
	.p17-scroll td {
		overflow-wrap: anywhere;
	}
	.small {
		font-size: 12px;
	}
	.p17-fx summary {
		cursor: pointer;
		padding: 4px 0;
	}
	pre.code {
		white-space: pre-wrap;
		overflow-wrap: anywhere;
	}
	@media (max-width: 560px) {
		.p17-map li {
			grid-template-columns: minmax(0, 1fr);
			gap: 2px;
		}
		.p17-arrow {
			display: none;
		}
	}
</style>
