<script lang="ts">
	// J25 Tools — every tool the agent can call in this job (doc 10 §7.1, doc 12 §2/§3.3): what it does (the
	// platform's operation_def, read-only), what it changes, the identity it needs, where the job's procedures call it,
	// and which system answers it in Production and Test. Who answers is the tenant's choice: changing it here saves
	// to the job's draft like Operations & routes does (D54) — the test chat runs it at once, Production when a
	// release is published. Data: jobTools + jobOperations (connectors), procedures (ontology).
	import { onMount } from 'svelte';
	import { api, app } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { canWrite, writeAttrs, writeHint } from '$lib/auth/roles';
	import { Resource } from '$lib/load.svelte';
	import { fmt } from '$lib/format';
	import { jobPath } from '$lib/scope';
	import { toast } from '$lib/ui/toaster.svelte';
	import type { Env, JobOperations, JobTool } from '$lib/api/types/connectors';
	import type { ProcedureView } from '$lib/api/types/ontology';
	import { CELL } from '$lib/connectors/J6-routes';
	import { targetLabel } from '$lib/connectors/ops';
	import {
		blocked,
		choiceValue,
		EFFECT,
		ENVS,
		routeChoices,
		toolRows,
		toolSummary,
		type ToolRow,
	} from '$lib/connectors/J25-tools';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Freshness from '$lib/ui/Freshness.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Seg from '$lib/ui/Seg.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import DraftBar from '$lib/release/DraftBar.svelte';
	import ToolPolicyEditor from '$lib/iam/ToolPolicyEditor.svelte';
	import AttachProcedures from '$lib/iam/AttachProcedures.svelte';

	interface Data {
		ops: JobOperations;
		tools: JobTool[];
		procs: ProcedureView[];
	}

	const res = new Resource<Data>(
		async () => {
			const [ops, tools, procs] = await Promise.all([
				api().jobOperations(),
				api().jobTools(),
				api()
					.procedures()
					.then((p) => p.items)
					.catch(() => [] as ProcedureView[]),
			]);
			return { ops, tools, procs };
		},
		(d) => d.ops.ops.length === 0,
	);
	onMount(() => void res.refresh());
	const writable = $derived(canWrite(session, 'integrations_manager'));

	let filter = $state('all');
	let open = $state(false);
	let selKey = $state<string | null>(null);
	let pick = $state<Record<Env, string>>({ prod: '', test: '' });
	let saving = $state<Env | null>(null);
	let err = $state<{ env: Env; code: string; message: string } | null>(null);
	let tick = $state(0);

	const ENV_LABEL: Record<Env, string> = { prod: 'Production', test: 'Test' };
	const sysHref = (id: string) => `/admin/connections/systems/${encodeURIComponent(id)}`;
	const procHref = (name: string) => jobPath(app.job, `/procedures/${encodeURIComponent(name)}`);
	const RETRY: Record<string, string> = {
		nat: 'repeating it changes nothing',
		key: 'each call carries an idempotency key',
		'key+dup': 'idempotency key, plus a duplicate search before it writes',
	};
	const IDENTITY: Record<string, string> = {
		'0': 'anyone — no identity needed',
		'1': 'identified customer',
		'0→2': 'anyone may ask; a match raises the customer from public to verified',
		'1→2': 'raises the customer from identified to verified',
		'1–2': 'identified or verified customer, by variant',
		'2': 'verified customer',
		'3': 'step-up verification',
	};

	const rowsOf = (d: Data) => toolRows(d.ops, d.tools, d.procs);
	function shown(rows: ToolRow[]): ToolRow[] {
		switch (filter) {
			case 'changes':
				return rows.filter((r) => r.op.side_effect !== 'R');
			case 'reads':
				return rows.filter((r) => r.op.side_effect === 'R');
			case 'attention':
				return rows.filter((r) => blocked(r.prod) || blocked(r.test));
			default:
				return rows;
		}
	}
	const sel = $derived.by(() => {
		if (!selKey || res.value.state !== 'ok') return null;
		return rowsOf(res.value.data).find((r) => r.key === selKey) ?? null;
	});

	function show(r: ToolRow) {
		selKey = r.key;
		pick = {
			prod: r.prod.inherited ? '' : choiceValue(r.prod.target),
			test: r.test.inherited ? '' : choiceValue(r.test.target),
		};
		err = null;
		open = true;
	}

	function routeText(r: ToolRow, env: Env, d: JobOperations): string {
		const c = env === 'prod' ? r.prod : r.test;
		if (!c.target) return 'no system yet';
		const label = targetLabel(c.target, (id) => d.instances.find((i) => i.id === id)?.label ?? id);
		return label + (c.inherited ? ` (as every ${r.op.object}.${r.op.verb})` : '');
	}

	async function saveRoute(d: Data, r: ToolRow, env: Env) {
		const cur = env === 'prod' ? r.prod : r.test;
		const choice = routeChoices(d.ops, r.op, env, cur.target).find((c) => c.value === pick[env]);
		if (!choice) return;
		saving = env;
		err = null;
		try {
			const w = await api().setOperationRoute({
				object: r.op.object,
				verb: r.op.verb,
				variant: r.op.variant,
				env,
				target: choice.target,
			});
			toast(
				`${r.key} on ${ENV_LABEL[env]} → ${choice.label}. Saved to draft (rev ${w.draft_rev}); try it in the test chat.`,
			);
			tick++;
			await res.refresh();
		} catch (e) {
			const x = e as { code?: string; message: string };
			err = { env, code: x.code ?? 'error', message: x.message };
		} finally {
			saving = null;
		}
	}
</script>

<PageHead
	id="J25"
	sub="Everything the agent can do in this job: what each tool does, what it changes, the identity it needs, where your procedures use it and which of your systems answers it."
>
	{#snippet actions()}
		<Freshness resource={res} />
		<a class="btn" href={jobPath(app.job, '/backends')}>Backend systems</a>
		<a class="btn" href="/chat">Test chat</a>
	{/snippet}
</PageHead>

<DraftBar page="J6" {tick} />

<LoadState
	resource={res}
	empty="This job has no tools yet: they come from its templates' operations once its activities are enabled."
>
	{#snippet children(d)}
		{@const rows = rowsOf(d)}
		{@const sum = toolSummary(rows)}

		<section class="j25-sum" aria-label="Tools at a glance" data-testid="tools-summary">
			<div>
				<b class="j25-big">{sum.total}</b>
				<span>tools · {sum.required} required</span>
			</div>
			<div>
				<b class="j25-big">{sum.changes}</b>
				<span>change your data — the customer confirms first</span>
			</div>
			<div>
				<b class="j25-big">{sum.inProcedures}</b>
				<span>used by a procedure step</span>
			</div>
			<div class:bad={sum.blockedProd > 0}>
				<b class="j25-big">{sum.blockedProd}</b>
				<span>cannot run in Production yet</span>
				{#if sum.blockedProd}<a href={jobPath(app.job, '/operations')}>Fix in Operations &amp; routes</a>{/if}
			</div>
		</section>

		<div class="j25-bar">
			<Seg
				items={[
					['all', 'All'],
					['changes', 'Changes data'],
					['reads', 'Reads only'],
					['attention', 'Cannot run yet'],
				]}
				bind:active={filter}
				label="Filter tools"
			/>
			<span class="muted">
				What a tool does is set by the platform. Which system answers it is yours — per environment.
			</span>
		</div>

		<ul class="j25-list" data-testid="tools">
			{#each shown(rows) as r (r.key)}
				{@const fx = EFFECT[r.op.side_effect]}
				<li class="j25-tool" class:attn={blocked(r.prod)}>
					<div class="j25-what">
						<span class="row">
							<button type="button" class="table-action mono j25-name" onclick={() => show(r)}>{r.key}</button
							>
							<Chip
								text={r.op.required ? 'required' : 'optional'}
								variant={r.op.required ? 'accent' : 'muted'}
							/>
						</span>
						<span class="j25-desc">{r.def?.description || r.op.note || 'No description'}</span>
						<span class="j25-meta">
							<Chip text={fx.label} variant={fx.variant} />
							<span title={IDENTITY[r.op.identity_level] ?? ''}>identity level {r.op.identity_level}</span>
							{#if r.def?.transitions.length}
								<span class="mono j25-state"
									>{r.op.object}: {[...new Set(r.def.transitions.map((t) => t.from))].join(' / ')} → {[
										...new Set(r.def.transitions.map((t) => t.to)),
									].join(' / ')}</span
								>
							{/if}
						</span>
					</div>
					<div class="j25-used">
						<span class="j25-k">Used in</span>
						{#if r.steps.length}
							{#each r.steps.slice(0, 3) as s (`${s.procedure}#${s.step_number}`)}
								<a href={procHref(s.procedure)}
									><span class="mono">{s.procedure}</span> · step {s.step_number}</a
								>
							{/each}
							{#if r.steps.length > 3}<span class="muted">+{r.steps.length - 3} more</span>{/if}
						{:else}
							<span class="muted">no procedure step — the agent calls it while it answers</span>
						{/if}
					</div>
					<div class="j25-routes">
						{#each ENVS as env (env)}
							{@const c = env === 'prod' ? r.prod : r.test}
							<span class="j25-route" data-testid="tool-{r.key}-{env}">
								<span class="j25-k">{ENV_LABEL[env]}</span>
								{#if c.instance && !d.ops.instances.find((i) => i.id === c.instance)?.platform}
									<a href={sysHref(c.instance)}>{routeText(r, env, d.ops)}</a>
								{:else}
									<span>{routeText(r, env, d.ops)}</span>
								{/if}
								<Chip text={CELL[c.state].label} variant={CELL[c.state].variant} />
							</span>
						{/each}
					</div>
					<div class="j25-calls">
						{#if r.stats}
							<b class="mono">{fmt.n(r.stats.calls_7d)}</b> calls · 7 days
							<span class="muted" class:s-err={r.stats.error_rate >= 0.05}
								>{fmt.pct(r.stats.error_rate)} errors · p95 {fmt.ms(r.stats.p95_ms)}</span
							>
						{:else}<span class="muted">no calls yet</span>{/if}
					</div>
				</li>
			{:else}
				<li class="j25-tool muted">No tools match this filter.</li>
			{/each}
		</ul>
	{/snippet}
</LoadState>

<AttachProcedures onchanged={() => tick++} />

<Drawer bind:open title={sel?.key ?? ''} wide>
	{#if sel && res.value.state === 'ok'}
		{@const r = sel}
		{@const d = res.value.data}
		{@const fx = EFFECT[r.op.side_effect]}
		<div class="stack" data-testid="tool-drawer">
			<p>{r.def?.description || r.op.note || 'No description'}</p>
			<p class="row">
				<Chip text={r.op.required ? 'required' : 'optional'} variant={r.op.required ? 'accent' : 'muted'} />
				<Chip text={fx.label} variant={fx.variant} />
				<a class="btn sm" href="/chat">Try it in the test chat</a>
			</p>

			<section>
				<h3>What it does</h3>
				<dl class="kvs">
					<dt>Identity</dt>
					<dd>
						Level {r.op.identity_level} — {IDENTITY[r.op.identity_level] ?? 'as the operation requires'}
					</dd>
					<dt>Changes</dt>
					<dd>
						{#if r.def?.transitions.length}
							{#each r.def.transitions as t (`${t.from}>${t.to}`)}
								<span class="mono j25-tr">{t.object}: {t.from} → {t.to}</span>
							{/each}
						{:else if r.op.side_effect === 'R'}
							Nothing — it only reads.
						{:else if r.op.modality === 'request'}
							Opens a new {r.op.object} record in the system that answers it.
						{:else}
							Updates the {r.op.object} record; its status does not change.
						{/if}
					</dd>
					{#if r.op.side_effect !== 'R'}
						<dt>Before it runs</dt>
						<dd>
							{r.def?.confirm_back
								? 'The agent asks the customer to confirm.'
								: 'Runs without a customer confirmation.'}
							{#if r.def?.idempotency}<span class="cell-sub"
									>Retry-safe: {RETRY[r.def.idempotency] ?? r.def.idempotency}</span
								>{/if}
						</dd>
					{/if}
					<dt>Needs</dt>
					<dd class="mono">{r.def?.input_fields.join(', ') || '—'}</dd>
					<dt>Returns</dt>
					<dd class="mono">{r.def?.output_fields.join(', ') || '—'}</dd>
					<dt>Stages</dt>
					<dd>{r.op.stages.join(', ')}</dd>
				</dl>
			</section>

			<section>
				<h3>Where your procedures call it</h3>
				{#if r.steps.length}
					<ul class="j25-steps">
						{#each r.steps as s (`${s.procedure}#${s.step_number}`)}
							<li>
								<a href={procHref(s.procedure)}><span class="mono">{s.procedure}</span> v{s.version}</a>
								· step {s.step_number} “{s.name}”{s.gate ? ' · ends the case as not eligible when false' : ''}
							</li>
						{/each}
					</ul>
				{:else}
					<p class="muted">
						No procedure step calls it. The agent can still use it while it answers, if its role may — see <a
							href={jobPath(app.job, '/toolsets')}>Toolsets</a
						>.
					</p>
				{/if}
			</section>

			<section>
				<h3>Inputs &amp; state</h3>
				<p class="muted small">
					Say which inputs the agent works out from the conversation and which come from stored state — for
					example a customer id or a sign-in token that another tool produces. The agent never asks people for
					an id, number or email.
				</p>
				{#key `${r.key}:${JSON.stringify(r.def?.policy ?? null)}`}
					<ToolPolicyEditor
						op={r.key}
						inputFields={r.def?.input_fields ?? []}
						outputFields={r.def?.output_fields ?? []}
						policy={r.def?.policy ?? null}
						tools={d.tools.map((t) => ({ key: t.key, output_fields: t.output_fields }))}
						onsaved={async () => {
							tick++;
							await res.refresh();
						}}
					/>
				{/key}
			</section>

			<section>
				<h3>Who answers it</h3>
				<p class="muted small">
					Saved to the draft: the test chat uses it at once, customers when you publish a release.
				</p>
				{#each ENVS as env (env)}
					{@const c = env === 'prod' ? r.prod : r.test}
					{@const choices = routeChoices(d.ops, r.op, env, c.target)}
					<div class="j25-env" data-testid="route-editor-{env}">
						<div class="row">
							<b>{ENV_LABEL[env]}</b>
							<Chip text={CELL[c.state].label} variant={CELL[c.state].variant} />
						</div>
						<span class="cell-sub">{c.why}</span>
						<div class="row j25-pick">
							<select
								aria-label="Who answers {r.key} on {ENV_LABEL[env]}"
								class="input"
								bind:value={pick[env]}
								disabled={!writable || saving !== null}
							>
								<option value="" disabled
									>{c.inherited ? `As every ${r.op.object}.${r.op.verb}…` : 'Choose who answers…'}</option
								>
								{#each choices as ch (ch.value)}<option value={ch.value}>{ch.label}</option>{/each}
							</select>
							<button
								class="btn sm primary"
								type="button"
								{...writeAttrs(session, 'integrations_manager')}
								disabled={!writable ||
									saving !== null ||
									!pick[env] ||
									pick[env] === (c.inherited ? '' : choiceValue(c.target))}
								onclick={() => saveRoute(d, r, env)}>Save to draft</button
							>
						</div>
						{#if err && err.env === env}
							<p class="s-err small" role="alert"><b class="mono">{err.code}</b> — {err.message}</p>
						{/if}
					</div>
				{/each}
				{#if !writable}<p class="note">
						{writeHint('integrations_manager')} — who answers a tool is read-only for you.
					</p>{/if}
				<p class="muted small">
					A link to your site, or a vendor-specific note, is set in
					<a href={jobPath(app.job, '/operations')}>Operations &amp; routes</a>.
				</p>
			</section>
		</div>
	{/if}
</Drawer>

<style>
	.j25-sum {
		display: grid;
		grid-template-columns: repeat(4, minmax(0, 1fr));
		gap: 1px;
		background: var(--line);
		border: 1px solid var(--line);
		border-radius: var(--r);
		overflow: hidden;
		margin-bottom: 14px;
	}
	.j25-sum > div {
		background: var(--panel);
		padding: 12px 14px;
		display: flex;
		flex-direction: column;
		gap: 2px;
		font-size: 13px;
		color: var(--muted);
	}
	.j25-sum > div.bad .j25-big {
		color: var(--err);
	}
	.j25-big {
		font-size: 22px;
		color: var(--fg);
	}
	.j25-bar {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 8px 14px;
		margin-bottom: 10px;
		font-size: 13px;
	}
	.j25-list {
		list-style: none;
		margin: 0;
		padding: 0;
		background: var(--panel);
		border: 1px solid var(--line);
		border-radius: var(--r);
	}
	.j25-tool {
		display: grid;
		grid-template-columns: minmax(0, 1.6fr) minmax(0, 1fr) minmax(0, 1.2fr) minmax(110px, 0.6fr);
		gap: 8px 16px;
		padding: 12px 16px;
		border-top: 1px solid var(--line);
	}
	.j25-tool:first-child {
		border-top: 0;
	}
	.j25-tool.attn {
		box-shadow: inset 3px 0 0 var(--err);
	}
	.j25-what,
	.j25-used,
	.j25-routes,
	.j25-calls {
		display: flex;
		flex-direction: column;
		gap: 4px;
		min-width: 0;
		font-size: 13px;
	}
	.j25-name {
		font-weight: 600;
		overflow-wrap: anywhere;
		text-align: left;
	}
	.j25-meta {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 4px 10px;
		font-size: 12px;
		color: var(--muted);
	}
	.j25-state,
	.j25-tr {
		overflow-wrap: anywhere;
	}
	.j25-tr {
		display: block;
	}
	.j25-k {
		font: 600 11px/1.3 var(--mono);
		text-transform: uppercase;
		letter-spacing: 0.04em;
		color: var(--faint);
	}
	.j25-route {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 2px 8px;
		overflow-wrap: anywhere;
	}
	.j25-route .j25-k {
		min-width: 76px;
	}
	.j25-steps {
		margin: 0;
		padding-left: 18px;
	}
	.j25-env {
		display: flex;
		flex-direction: column;
		gap: 4px;
		padding: 10px 12px;
		border: 1px solid var(--line);
		border-radius: var(--r);
		margin-bottom: 8px;
	}
	.j25-pick select {
		flex: 1 1 220px;
		min-width: 0;
	}
	.small {
		font-size: 12px;
	}
	h3 {
		font-size: 13px;
		margin: 0 0 6px;
	}
	@media (max-width: 900px) {
		.j25-sum {
			grid-template-columns: repeat(2, minmax(0, 1fr));
		}
		.j25-tool {
			grid-template-columns: 1fr;
		}
	}
</style>
