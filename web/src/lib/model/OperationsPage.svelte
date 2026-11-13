<script lang="ts">
	// P11 Operations catalogue (D22, D27, K48, K49, 11 §3, 12 §2–§3.3): the verbs agents can perform on business
	// objects. Design intent: an operation is `object.verb{variant}`; its modality says who changes what, its side
	// effect + identity level decide what the runner checks, connector templates implement it and agent roles are
	// granted it. The page leads with that chain and an object × modality map; one drawer holds the whole story of
	// one operation (state transition, runner checks, I/O, implementers, callers, variant overrides, errors, JSON).
	import { onMount, untrack } from 'svelte';
	import { api } from '$lib/app.svelte';
	import { Resource, load, type Loadable } from '$lib/load.svelte';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Seg from '$lib/ui/Seg.svelte';
	import Tabs from '$lib/ui/Tabs.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import JsonView from '$lib/ui/JsonView.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';
	import type {
		ErrorStateDef,
		IdentityLevel,
		Modality,
		ObjectDetail,
		ObjectSummary,
		OperationAttrs,
		OperationFilter,
		OperationRow,
		SideEffect,
	} from '$lib/api/types/model';
	import type { ConnectorTemplateSummary } from '$lib/api/types/connectors';
	import type { AgentRoleView } from '$lib/api/types/ontology';
	import type { Paged } from '$lib/api/types/core';
	import { ATTR_KEYS, errorExample, successExample } from './logic';
	import { EFFECT_LABEL, EFFECT_VARIANT, LEVEL_LABEL } from './ui';
	import CreateOperation from './CreateOperation.svelte';
	import {
		MODALITIES,
		MODALITY_HINT,
		RISK_LADDER,
		callersOf,
		countByEffect,
		implementersOf,
		operationMatrix,
		runnerChecks,
		transitionOf,
	} from './operationsP11';

	let { object = '', q = '', op = '' }: { object?: string; q?: string; op?: string } = $props();

	/** The job template whose agent roles the "who may call it" answer is read from (the retail reference job). */
	const ROLE_JOB = 'customer_care_agent';

	let objectF = $state('');
	let modality = $state('all');
	let sideFx = $state<SideEffect | 'all'>('all');
	let level = $state('all');
	let builtinOnly = $state(false);
	let retailOnly = $state(false);
	let unimplementedOnly = $state(false);
	let search = $state('');
	let view = $state('map');
	let ready = $state(false);

	function filter(): OperationFilter {
		return {
			object: objectF || undefined,
			modality: modality === 'all' ? undefined : (modality as Modality),
			side_effect: sideFx === 'all' ? undefined : sideFx,
			identity_level: level === 'all' ? undefined : (level as IdentityLevel),
			builtin: builtinOnly ? true : undefined,
			retail_only: retailOnly || undefined,
			q: search.trim() || undefined,
		};
	}

	const res = new Resource<Paged<OperationRow>>(() => api().catalogOperations(filter()));
	const ref = new Resource<{ errors: ErrorStateDef[]; objects: ObjectSummary[]; all: OperationRow[] }>(
		async () => ({
			errors: await api().errorStateDefs(),
			objects: (await api().objectCatalog()).objects,
			all: (await api().catalogOperations()).items,
		}),
		() => false,
	);
	// Cross-area context: who implements (connector templates, 11 §5.3) and who may call (agent roles, 12 §2.1).
	const links = new Resource<{ templates: ConnectorTemplateSummary[]; roles: AgentRoleView[] }>(
		async () => ({
			templates: (await api().connectorTemplates()).items,
			roles: (await api().agentRoles(ROLE_JOB)).items,
		}),
		() => false,
	);

	onMount(() => {
		objectF = object;
		search = q;
		ready = true;
		ref.refresh();
		links.refresh();
		// `?op=order.cancel` deep-links straight into one operation's drawer.
		if (op)
			api()
				.catalogOperations({ q: op })
				.then((p) => {
					const hit = p.items.find((r) => r.key === op);
					if (hit) open(hit);
				})
				.catch(() => {});
	});
	$effect(() => {
		if (!ready) return;
		void [objectF, modality, sideFx, level, builtinOnly, retailOnly, search];
		untrack(() => res.refresh());
	});

	function clear() {
		objectF = '';
		modality = level = 'all';
		sideFx = 'all';
		builtinOnly = retailOnly = unimplementedOnly = false;
		search = '';
	}

	const errors = $derived(ref.value.state === 'ok' ? ref.value.data.errors : []);
	const objects = $derived(ref.value.state === 'ok' ? ref.value.data.objects : []);
	const allOps = $derived(ref.value.state === 'ok' ? ref.value.data.all : []);
	const effectCounts = $derived(countByEffect(allOps));
	const templates = $derived(links.value.state === 'ok' ? links.value.data.templates : null);
	const roles = $derived(links.value.state === 'ok' ? links.value.data.roles : null);
	const objectLabel = (k: string) => objects.find((o) => o.object === k)?.label ?? k;

	/** Connector-routed ops no platform template implements: tenants can only send them to a fallback. */
	const isGap = (o: OperationRow) => !!templates && !o.builtin && implementersOf(templates, o).length === 0;
	const gapCount = $derived(templates ? allOps.filter(isGap).length : 0);
	const visible = (items: OperationRow[]) => (unimplementedOnly ? items.filter(isGap) : items);

	const userFacing = (code: string) =>
		errors.find((e) => e.code === code.split(':')[0])?.user_facing ?? false;

	// ---- drawer ------------------------------------------------------------------------------------------------
	let openOp = $state<OperationRow | null>(null);
	let drawerOpen = $state(false);
	let errCode = $state('');
	let tab = $state('story');
	let jsonTab = $state('success');
	let detail = $state<Loadable<ObjectDetail>>({ state: 'loading' });
	let detailFor = '';
	function open(o: OperationRow) {
		openOp = o;
		errCode = o.error_states[0] ?? '';
		drawerOpen = true;
		if (detailFor !== o.object) {
			detailFor = o.object;
			detail = { state: 'loading' };
			const key = o.object;
			load(
				() => api().objectDetail(key),
				() => false,
			).then((d) => {
				if (detailFor === key) detail = d;
			});
		}
	}
	const def = $derived(detail.state === 'ok' ? detail.data : null);
	/** The operation's whole family (`*` row + variants), from the object detail. */
	const family = $derived(
		openOp && def ? def.operations.filter((r) => r.verb === openOp!.verb) : openOp ? [openOp] : [],
	);

	const ATTR_LABEL: Record<keyof OperationAttrs, string> = {
		modality: 'Modality',
		is_derive: 'Derive verb',
		identity_level: 'Identity level',
		side_effect: 'Side effect',
		idempotency: 'Idempotency',
		input_fields: 'Input fields',
		output_fields: 'Output fields',
		confirm_back: 'Confirm back',
		verify_read: 'Verify read',
		builtin: 'Built-in',
		op_kind: 'Op kind',
		guard_rules: 'Guard rules',
		raises_identity_level: 'Raises identity to',
	};
	function attrText(o: OperationRow, k: keyof OperationAttrs): string {
		const v = o[k];
		if (Array.isArray(v)) return v.length ? v.join(', ') : '—';
		if (typeof v === 'boolean') return v ? 'yes' : 'no';
		if ((k === 'identity_level' || k === 'raises_identity_level') && v != null)
			return LEVEL_LABEL[String(v)] ?? String(v);
		if (k === 'side_effect') return EFFECT_LABEL[v as SideEffect];
		return v == null ? '—' : String(v);
	}

	const SUCCESS_TEMPLATE = {
		operation: '<object>.<verb>',
		outcome: 'ok',
		object: '<object>',
		id: '<id>',
		status: '<canonical status after the operation>',
		data: { '<output_fields>': '…' },
		reference: '<confirmation / case / transaction ref>',
		effective_at: '<date>',
		next_steps: ['<what the customer should expect>'],
	};
	const ERROR_TEMPLATE = {
		operation: '<object>.<verb>',
		outcome: 'error',
		error: { code: '<code>', 'rule?': '<rule id>', reason: '<why>', retryable: false, user_facing: true },
	};
	let conv = $state('taxonomy');
</script>

<PageHead
	id="P11"
	sub="The verbs agents may perform on business objects — order.cancel, case.create{'{'}return{'}'}. Each one fixes its risk and the checks the runner applies; connector templates implement it, agent roles are granted it."
>
	{#snippet actions()}
		<CreateOperation
			{objects}
			operations={allOps}
			oncreated={async () => {
				await ref.refresh();
				await res.refresh();
			}}
		/>
		<a class="btn" href="/platform/objects">Objects</a>
		<a class="btn" href="/platform/agent-roles">Agent roles</a>
		<a class="btn" href="/platform/connector-templates">Connector templates</a>
	{/snippet}
</PageHead>

<!-- How an operation becomes a tool (12 §2: N1 → N2 → N3 → N4 → N5). -->
<ol class="chain" aria-label="How an operation becomes an agent tool" data-testid="op-chain">
	<li class="here">
		<span class="n">1</span>
		<b>Catalogue</b>
		<span>This page: modality, risk, inputs/outputs, errors.</span>
	</li>
	<li>
		<span class="n">2</span>
		<b><a href="/platform/templates">Job expects it</a></b>
		<span>The job template marks it required (R) or optional (O).</span>
	</li>
	<li>
		<span class="n">3</span>
		<b><a href="/platform/agent-roles">Role is granted it</a></b>
		<span>Qualifier and post-gate read; procedures get their activity’s ops.</span>
	</li>
	<li>
		<span class="n">4</span>
		<b><a href="/platform/connector-templates">Tenant routes it</a></b>
		<span>To a system whose connector template implements it, or a fallback.</span>
	</li>
	<li>
		<span class="n">5</span>
		<b>Runner checks it</b>
		<span>Identity, confirm-back, hosted link, verify read — per call.</span>
	</li>
</ol>

<!-- Risk ladder doubles as the side-effect filter. -->
<div class="ladder" role="group" aria-label="Side effect" data-testid="risk-ladder">
	{#each RISK_LADDER as r (r.effect)}
		<button
			type="button"
			class="rung fx-{r.effect}"
			class:on={sideFx === r.effect}
			aria-pressed={sideFx === r.effect}
			onclick={() => (sideFx = sideFx === r.effect ? 'all' : r.effect)}
		>
			<span class="rung-head"
				><span class="fx-badge">{r.effect}</span><b>{r.label}</b><span class="cnt"
					>{effectCounts[r.effect]}</span
				></span
			>
			<span class="rung-adds">{r.adds}</span>
		</button>
	{/each}
</div>

<Card>
	<div class="filters" data-testid="op-filters">
		<div class="field">
			<label for="op-q">Search</label>
			<input id="op-q" class="input" bind:value={search} placeholder="case.create, refund…" />
		</div>
		<div class="field">
			<label for="op-object">Object</label>
			<select id="op-object" class="input" bind:value={objectF}>
				<option value="">All objects</option>
				{#each objects as o (o.object)}<option value={o.object}>{o.label}</option>{/each}
				{#if objectF && !objects.some((o) => o.object === objectF)}<option value={objectF}>{objectF}</option
					>{/if}
			</select>
		</div>
		<div class="field">
			<span class="lbl">Modality</span>
			<Seg
				label="Modality"
				bind:active={modality}
				items={[
					['all', 'All'],
					['observe', 'Observe'],
					['do', 'Do'],
					['request', 'Request'],
				]}
			/>
		</div>
		<div class="field">
			<span class="lbl">Identity level</span>
			<Seg
				label="Identity level"
				bind:active={level}
				items={[
					['all', 'All'],
					['0', '0'],
					['1', '1'],
					['2', '2'],
					['3', '3'],
					['S', 'S'],
				]}
			/>
		</div>
		<div class="checks">
			<label><input type="checkbox" bind:checked={retailOnly} /> In the retail job</label>
			<label><input type="checkbox" bind:checked={builtinOnly} /> Built-ins only</label>
			<label title="Connector-routed operations no platform connector template implements"
				><input type="checkbox" bind:checked={unimplementedOnly} disabled={!templates} /> No connector
				template{#if templates}&nbsp;({gapCount}){/if}</label
			>
			<button class="btn sm" type="button" onclick={clear}>Clear filters</button>
		</div>
	</div>
</Card>

<LoadState resource={res}>
	{#snippet empty()}
		No operation matches these filters. <button class="table-action" type="button" onclick={clear}
			>Clear filters</button
		>
	{/snippet}
	{#snippet children(page)}
		{@const items = visible(page.items)}
		<Card
			title="{items.length} operation{items.length === 1 ? '' : 's'}"
			sub="Colour = side effect. R/O = required / optional in the retail customer-care job (11 §4.3.5). +n = variants."
		>
			<Tabs
				label="View"
				bind:active={view}
				items={[
					['map', 'Object × modality'],
					['list', 'List', items.length],
				]}
			/>
			{#if !items.length}
				<p class="muted pad">
					Every operation here has a connector template. <button
						class="table-action"
						type="button"
						onclick={() => (unimplementedOnly = false)}>Show all</button
					>
				</p>
			{:else if view === 'map'}
				{@const rows = operationMatrix(
					items,
					objects.map((o) => o.object),
				)}
				<div class="mx" data-testid="op-matrix">
					<div class="mx-head" aria-hidden="true">
						<span>Object</span>
						{#each MODALITIES as m (m)}<span><b>{m}</b><small>{MODALITY_HINT[m]}</small></span>{/each}
					</div>
					{#each rows as row (row.object)}
						<div class="mx-row">
							<div class="mx-obj">
								<a href="/platform/objects/{row.object}">{objectLabel(row.object)}</a>
								<span class="mono faint">{row.object} · {row.count}</span>
							</div>
							{#each MODALITIES as m (m)}
								<div class="mx-cell" data-modality={m}>
									<span class="mx-m">{m}</span>
									{#each row.cells[m] as g (g.base)}
										{@const h = g.head}
										<button
											type="button"
											class="op fx-{g.effect}"
											class:gap={isGap(h)}
											aria-label={h.key}
											title="{h.key} — {EFFECT_LABEL[g.effect]}, identity {LEVEL_LABEL[
												h.identity_level
											]}{isGap(h) ? ' — no connector template' : ''}"
											onclick={() => open(h)}
										>
											<span class="fx-dot" aria-hidden="true"></span>
											<span class="verb"
												>{h.verb}{#if h.variant !== '*'}<span class="vv">{'{'}{h.variant}{'}'}</span
													>{/if}</span
											>
											{#if g.variants.length && h.variant === '*'}<span class="plus"
													>+{g.variants.length}</span
												>{/if}
											{#if h.builtin}<span class="tag">built-in</span>{/if}
											{#if h.retail}<span class="ro" class:req={h.retail === 'R'}>{h.retail}</span>{/if}
										</button>
									{/each}
								</div>
							{/each}
						</div>
					{/each}
				</div>
				<p class="note">
					A dashed outline marks an operation no connector template implements — a tenant can only route it to
					a fallback (agent cases, deep link, not offered).
				</p>
			{:else}
				<div class="tbl-wrap">
					<table class="tbl clickable" data-testid="operations">
						<thead>
							<tr>
								<th>Operation</th>
								<th>Modality</th>
								<th>Id</th>
								<th>Effect</th>
								<th>Idem.</th>
								<th>Confirm</th>
								<th>Verify read</th>
								<th>Kind</th>
								<th>Retail</th>
							</tr>
						</thead>
						<tbody>
							{#each items as o (o.key)}
								{@const inh = (k: keyof OperationAttrs) => o.inherited.includes(k)}
								<tr class:variant={o.variant !== '*'} onclick={() => open(o)}>
									<td>
										<button
											class="table-action mono"
											type="button"
											onclick={(e) => (e.stopPropagation(), open(o))}>{o.key}</button
										>
										{#if o.variant !== '*'}<span class="cell-sub"
												>{o.discriminator?.field} = {o.discriminator?.value} · {o.overridden.length} override{o
													.overridden.length === 1
													? ''
													: 's'}</span
											>{/if}
									</td>
									<td class:inh={inh('modality')}
										>{o.modality}{#if o.is_derive}
											<Chip text="derive" variant="tool" />{/if}</td
									>
									<td class="mono" class:inh={inh('identity_level')}
										>{o.identity_level}{#if !inh('identity_level') && o.variant !== '*'}
											●{/if}</td
									>
									<td class:inh={inh('side_effect')}
										><Chip
											text={o.side_effect}
											variant={EFFECT_VARIANT[o.side_effect]}
											title={EFFECT_LABEL[o.side_effect]}
										/></td
									>
									<td class="mono" class:inh={inh('idempotency')}
										>{o.idempotency}{#if !inh('idempotency') && o.variant !== '*'}
											●{/if}</td
									>
									<td class:inh={inh('confirm_back')}>{o.confirm_back ? 'yes' : '—'}</td>
									<td class="mono" class:inh={inh('verify_read')}>{o.verify_read ?? '—'}</td>
									<td>
										{#if o.builtin}<Chip text="built-in" variant="accent" />{/if}
										{#if o.op_kind === 'hosted_payment_link'}<Chip
												text="hosted link only"
												variant="err"
											/>{/if}
									</td>
									<td
										>{#if o.retail}<Chip
												text={o.retail}
												variant={o.retail === 'R' ? 'cust' : 'muted'}
											/>{:else}<span class="muted">—</span>{/if}</td
									>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
				<p class="note">Variant rows: grey = inherited from <span class="mono">*</span>, ● = overridden.</p>
			{/if}
		</Card>
	{/snippet}
</LoadState>

<Card
	title="What an operation returns"
	sub="Every operation answers in one success and one error shape (gen:36–37, 11 §3.2). User-facing errors get a reply; system errors escalate to a human."
>
	<Tabs
		label="Outcome convention"
		bind:active={conv}
		items={[
			['taxonomy', 'Error taxonomy', errors.length],
			['success', 'Success shape'],
			['error', 'Error shape'],
		]}
	/>
	<div class="pad">
		{#if conv === 'success'}
			<JsonView value={SUCCESS_TEMPLATE} />
		{:else if conv === 'error'}
			<JsonView value={ERROR_TEMPLATE} />
		{:else if ref.value.state === 'unavailable'}
			<Unavailable reason={ref.value.reason} retry={ref.retry} />
		{:else if errors.length}
			<div class="tax" data-testid="error-states">
				<div>
					<h3>Reply to the customer</h3>
					<ul>
						{#each errors.filter((e) => !e.escalate) as e (e.code)}
							<li>
								<span class="mono"
									>{e.code}{e.code === 'invalid_state'
										? ':<status>'
										: e.code === 'ineligible'
											? ':<rule>'
											: ''}</span
								>{#if e.retryable}<Chip text="retryable" variant="muted" />{/if}
								<span class="muted">{e.description}</span>
							</li>
						{/each}
					</ul>
				</div>
				<div>
					<h3>Escalate</h3>
					<ul>
						{#each errors.filter((e) => e.escalate) as e (e.code)}
							<li>
								<span class="mono">{e.code}</span>{#if e.retryable}<Chip
										text="retryable"
										variant="muted"
									/>{/if}
								<span class="muted">{e.description}</span>
							</li>
						{/each}
					</ul>
				</div>
			</div>
		{/if}
	</div>
</Card>

<Drawer bind:open={drawerOpen} title={openOp?.key ?? ''} wide>
	{#snippet sub()}{openOp?.description ?? ''}{/snippet}
	{#if openOp}
		{@const o = openOp}
		{@const tr = transitionOf(def, o)}
		<div class="stack">
			<div class="chips">
				<Chip text={o.modality} variant="accent" />
				{#if o.is_derive}<Chip text="derive" variant="tool" />{/if}
				<Chip text={EFFECT_LABEL[o.side_effect]} variant={EFFECT_VARIANT[o.side_effect]} />
				<Chip text="identity {LEVEL_LABEL[o.identity_level]}" variant="muted" />
				{#if o.builtin}<Chip text="built-in (K49)" variant="accent" />{/if}
				{#if o.op_kind === 'hosted_payment_link'}<Chip text="hosted link only (D27)" variant="err" />{/if}
				{#if o.retail}<Chip
						text="retail job: {o.retail === 'R' ? 'required' : 'optional'}"
						variant="cust"
					/>{/if}
			</div>

			{#if family.length > 1}
				<div class="fam" role="group" aria-label="Variants">
					{#each family as f (f.key)}
						<button
							type="button"
							class="btn sm"
							class:on={f.key === o.key}
							aria-pressed={f.key === o.key}
							onclick={() => open(f)}>{f.variant === '*' ? `${f.object}.${f.verb}` : `{${f.variant}}`}</button
						>
					{/each}
				</div>
			{/if}

			<Tabs
				label="Operation"
				bind:active={tab}
				items={[
					['story', 'Overview'],
					['attrs', 'Attributes'],
					['errors', 'Errors & JSON', o.error_states.length],
				]}
			/>

			{#if tab === 'story'}
				<section>
					<h3>
						What it does to the <a href="/platform/objects/{o.object}">{objectLabel(o.object)}</a>
					</h3>
					{#if detail.state === 'loading'}
						<p class="muted">Loading the state machine…</p>
					{:else if detail.state === 'unavailable'}
						<Unavailable reason={detail.reason} />
					{:else if tr.kind === 'read'}
						<p class="muted">
							Reads only — no state changes{o.is_derive ? '; a derive verb computes and stores nothing' : ''}.
						</p>
					{:else if tr.kind === 'create'}
						<div class="flow">
							<span class="st new">new</span><span class="arr">→</span><span class="st"
								>{tr.initial ?? 'created'}</span
							>
						</div>
						<p class="muted">Creates a {objectLabel(o.object).toLowerCase()} record.</p>
					{:else if tr.kind === 'data'}
						<p class="muted">Changes data on the record; its status stays where it is.</p>
					{:else}
						<ul class="moves" data-testid="op-transition">
							{#each tr.moves as m (m.from + m.to)}
								<li class="flow">
									<span class="st">{m.from}</span><span class="arr">→</span><span class="st to">{m.to}</span
									>{#if m.guard}<Chip text="guard {m.guard}" variant="warn" />{/if}
								</li>
							{/each}
						</ul>
						{#if tr.blocked_from.length}
							<p class="note">
								From any other state ({tr.blocked_from.join(', ')}) it fails with
								<span class="mono">invalid_state</span>.
							</p>
						{/if}
					{/if}
				</section>

				<section>
					<h3>Runner checks before it runs</h3>
					<ul class="checks-list" data-testid="op-checks">
						{#each runnerChecks(o) as c (c.key)}
							<li class:off={!c.applies}>
								<span class="tick" aria-hidden="true">{c.applies ? '●' : '○'}</span>
								<div>
									<b>{c.label}</b>{#if c.applies && c.error}<span class="mono faint"> → {c.error}</span>{/if}
									<span class="muted">{c.detail}</span>
								</div>
							</li>
						{/each}
					</ul>
				</section>

				<section class="grid g2">
					<div>
						<h3>Inputs</h3>
						<div class="chips">
							{#each o.input_fields as f (f)}<Chip text={f} variant="muted" />{:else}<span class="muted"
									>none</span
								>{/each}
						</div>
					</div>
					<div>
						<h3>Returns</h3>
						<div class="chips">
							{#each o.output_fields as f (f)}<Chip text={f} variant="muted" />{:else}<span class="muted"
									>none</span
								>{/each}
						</div>
					</div>
				</section>

				<section class="grid g2">
					<div data-testid="op-implementers">
						<h3>Implemented by</h3>
						{#if links.value.state === 'unavailable'}
							<Unavailable reason={links.value.reason} retry={links.retry} />
						{:else if !templates}
							<p class="muted">Loading…</p>
						{:else if o.builtin}
							<p class="muted">Runs inside agent (K49) — no connector route needed.</p>
						{:else}
							{@const impl = implementersOf(templates, o)}
							{#if impl.length}
								<ul class="plain">
									{#each impl as i (i.key)}
										<li>
											<a href="/platform/connector-templates/{i.key}/{i.latest}">{i.label}</a>
											<span class="mono faint">@{i.versions.join(', @')}</span>
											{#if !i.exact}<Chip
													text="via *"
													variant="muted"
													title="Covered by the unqualified verb"
												/>{/if}
											{#if i.platform}<Chip text="platform" variant="tool" />{/if}
										</li>
									{/each}
								</ul>
							{:else}
								<Callout kind="gap" title="No connector template">
									<span
										>Tenants can only route it to a fallback: agent cases, a deep link, or not offered.</span
									>
								</Callout>
							{/if}
						{/if}
					</div>
					<div data-testid="op-callers">
						<h3>May be called by <span class="faint">(customer-care job)</span></h3>
						{#if links.value.state === 'unavailable'}
							<Unavailable reason={links.value.reason} retry={links.retry} />
						{:else if !roles}
							<p class="muted">Loading…</p>
						{:else}
							<ul class="plain">
								{#each callersOf(roles, o) as c (c.role)}
									<li class:off={c.status !== 'may'}>
										<Chip
											text={c.status === 'may' ? 'may call' : c.status === 'disabled' ? 'disabled' : 'no'}
											variant={c.status === 'may' ? 'ok' : 'muted'}
										/>
										<b>{c.label}</b>
										<span class="muted">{c.why}</span>
										{#if c.activities.length}<span class="cell-sub">{c.activities.join(' · ')}</span>{/if}
									</li>
								{/each}
							</ul>
							<a class="note" href="/platform/agent-roles">Edit grants on Agent roles →</a>
						{/if}
					</div>
				</section>
			{:else if tab === 'attrs'}
				{#if o.variant !== '*'}
					<Callout kind="info" title="Variant of {o.object}.{o.verb}">
						<span
							>Discriminator <span class="mono">{o.discriminator?.field} = {o.discriminator?.value}</span>.
							Overrides {o.overridden.map((k) => ATTR_LABEL[k]).join(', ') || 'nothing'}; everything else
							comes from <span class="mono">{o.object}.{o.verb}</span>.</span
						>
					</Callout>
				{/if}
				<table class="tbl dense" data-testid="op-attrs">
					<tbody>
						{#each Object.keys(ATTR_LABEL).filter((k) => k !== 'raises_identity_level' || o.verb === 'verify') as k (k)}
							{@const key = k as keyof OperationAttrs}
							<tr class:inh={o.inherited.includes(key)}>
								<td class="lblc">{ATTR_LABEL[key]}</td>
								<td class="mono">{attrText(o, key)}</td>
								<td>
									{#if o.variant !== '*' && ATTR_KEYS.includes(key)}{#if o.inherited.includes(key)}<span
												class="muted">inherited from *</span
											>{:else}<Chip text="override" variant="accent" />{/if}{/if}
								</td>
							</tr>
						{/each}
						<tr><td class="lblc">Stages</td><td>{o.stages.join(', ') || '—'}</td><td></td></tr>
					</tbody>
				</table>
			{:else}
				<div>
					<div class="chips">
						{#each o.error_states as e (e)}<Chip
								text={e}
								variant={userFacing(e) ? 'warn' : 'err'}
								title={userFacing(e) ? 'gets a reply' : 'escalates'}
							/>{/each}
					</div>
					<p class="note">
						Generated from the common taxonomy, the object's state machine (invalid_state) and its rules
						(ineligible) — complete by construction. Amber = reply, red = escalate.
					</p>
				</div>
				<Tabs
					label="Example"
					bind:active={jsonTab}
					items={[
						['success', 'Success example'],
						['error', 'Error example'],
					]}
				/>
				{#if jsonTab === 'success'}
					<JsonView value={successExample(o, tr.moves[0]?.to ?? tr.initial)} />
				{:else}
					<select class="input" bind:value={errCode} aria-label="Error code">
						{#each o.error_states as e (e)}<option value={e}>{e}</option>{/each}
					</select>
					{#if errCode}<JsonView value={errorExample(o, errCode, errors)} />{/if}
				{/if}
			{/if}
		</div>
	{/if}
</Drawer>

<style>
	/* ---- chain ---- */
	.chain {
		list-style: none;
		margin: 0 0 14px;
		padding: 0;
		display: grid;
		grid-template-columns: repeat(5, minmax(0, 1fr));
		gap: 8px;
		counter-reset: none;
	}
	.chain li {
		position: relative;
		display: grid;
		gap: 3px;
		align-content: start;
		padding: 10px 12px 10px 38px;
		border: 1px solid var(--line);
		border-radius: var(--r);
		background: var(--panel);
		font-size: 12.5px;
	}
	.chain li span:last-child {
		color: var(--muted);
	}
	.chain li.here {
		border-color: var(--accent);
		background: color-mix(in srgb, var(--accent) 7%, var(--panel));
	}
	.chain .n {
		position: absolute;
		left: 10px;
		top: 10px;
		width: 20px;
		height: 20px;
		border-radius: 50%;
		display: grid;
		place-items: center;
		font: 600 11px/1 var(--mono);
		background: var(--sunk);
		color: var(--muted) !important;
	}
	.chain .here .n {
		background: var(--accent);
		color: var(--panel) !important;
	}
	.chain li:not(:last-child)::after {
		content: '→';
		position: absolute;
		right: -8px;
		top: 50%;
		transform: translate(50%, -50%);
		color: var(--faint);
		font-size: 12px;
		z-index: 1;
	}

	/* ---- side-effect colours ---- */
	.fx-R {
		--fx: var(--muted);
	}
	.fx-W {
		--fx: var(--accent);
	}
	.fx-X {
		--fx: var(--warn);
	}
	.fx-P {
		--fx: var(--err);
	}

	/* ---- ladder ---- */
	.ladder {
		display: grid;
		grid-template-columns: repeat(4, minmax(0, 1fr));
		gap: 8px;
		margin-bottom: 14px;
	}
	.rung {
		display: grid;
		gap: 4px;
		text-align: left;
		padding: 10px 12px;
		border: 1px solid var(--line);
		border-left: 4px solid var(--fx);
		border-radius: var(--r);
		background: var(--panel);
		color: var(--fg);
		cursor: pointer;
		font: inherit;
	}
	.rung:hover {
		border-color: var(--fx);
	}
	.rung.on {
		border-color: var(--fx);
		background: color-mix(in srgb, var(--fx) 10%, var(--panel));
	}
	.rung-head {
		display: flex;
		gap: 8px;
		align-items: center;
		font-size: 13px;
	}
	.fx-badge {
		font: 600 11px/1 var(--mono);
		padding: 3px 6px;
		border-radius: 4px;
		background: color-mix(in srgb, var(--fx) 16%, transparent);
		color: var(--fx);
	}
	.cnt {
		margin-left: auto;
		font: 600 15px/1 var(--mono);
	}
	.rung-adds {
		font-size: 12px;
		color: var(--muted);
	}

	/* ---- filters ---- */
	.filters {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(180px, max-content));
		gap: 12px 18px;
		align-items: end;
	}
	.lbl {
		font-size: 12px;
		color: var(--muted);
		font-weight: 500;
	}
	.checks {
		display: flex;
		gap: 12px;
		align-items: center;
		flex-wrap: wrap;
		font-size: 13px;
	}
	.checks label {
		display: inline-flex;
		gap: 6px;
		align-items: center;
	}

	/* ---- matrix ---- */
	.mx {
		margin-top: 10px;
		display: grid;
	}
	.mx-head,
	.mx-row {
		display: grid;
		grid-template-columns: 170px repeat(3, minmax(0, 1fr));
		gap: 0 12px;
	}
	.mx-head {
		padding: 6px 0;
		border-bottom: 1px solid var(--line);
		font: 600 12px/1.2 var(--cond);
		letter-spacing: 0.04em;
		text-transform: uppercase;
		color: var(--muted);
	}
	.mx-head small {
		display: block;
		text-transform: none;
		letter-spacing: 0;
		font: 400 11.5px/1.3 var(--sans);
		color: var(--faint);
	}
	.mx-row {
		padding: 8px 0;
		border-bottom: 1px solid var(--line-2);
	}
	.mx-obj {
		display: grid;
		align-content: start;
		gap: 2px;
		font-weight: 500;
	}
	.mx-obj .mono {
		font-size: 11px;
		font-weight: 400;
	}
	.mx-cell {
		display: flex;
		flex-wrap: wrap;
		gap: 5px;
		align-content: start;
	}
	.mx-m {
		display: none;
	}
	.op {
		display: inline-flex;
		align-items: center;
		gap: 5px;
		padding: 4px 8px;
		border: 1px solid color-mix(in srgb, var(--fx) 45%, var(--line));
		border-radius: 999px;
		background: color-mix(in srgb, var(--fx) 8%, var(--panel));
		color: var(--fg);
		font: 500 12px/1.2 var(--mono);
		cursor: pointer;
	}
	.op:hover,
	.op:focus-visible {
		border-color: var(--fx);
		background: color-mix(in srgb, var(--fx) 16%, var(--panel));
	}
	.op.gap {
		border-style: dashed;
	}
	.fx-dot {
		width: 7px;
		height: 7px;
		border-radius: 50%;
		background: var(--fx);
	}
	.vv {
		color: var(--muted);
	}
	.plus,
	.tag,
	.ro {
		font-size: 10.5px;
		color: var(--muted);
	}
	.tag {
		color: var(--tool);
	}
	.ro {
		padding: 1px 4px;
		border-radius: 3px;
		border: 1px solid var(--line);
	}
	.ro.req {
		color: var(--cust);
		border-color: color-mix(in srgb, var(--cust) 50%, var(--line));
	}
	.pad {
		padding-top: 10px;
	}

	/* ---- list ---- */
	.variant td:first-child {
		padding-left: 26px;
	}
	.inh {
		color: var(--faint);
	}
	.lblc {
		color: var(--muted);
		width: 160px;
	}

	/* ---- error taxonomy ---- */
	.tax {
		display: grid;
		grid-template-columns: minmax(0, 2fr) minmax(0, 1fr);
		gap: 18px;
	}
	.tax > div:first-child ul {
		display: block;
		columns: 2;
		column-gap: 18px;
	}
	.tax > div:first-child li {
		break-inside: avoid;
		margin-bottom: 6px;
	}
	.tax ul,
	.plain,
	.moves,
	.checks-list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 6px;
	}
	.tax li {
		display: flex;
		flex-wrap: wrap;
		gap: 4px 8px;
		align-items: baseline;
		font-size: 13px;
	}
	.tax li .muted {
		flex-basis: 100%;
		font-size: 12px;
	}

	/* ---- drawer ---- */
	h3 {
		margin: 0 0 6px;
		font: 600 13px/1.2 var(--cond);
		letter-spacing: 0.04em;
		text-transform: uppercase;
		color: var(--muted);
	}
	h3 a {
		text-transform: none;
		letter-spacing: 0;
	}
	section {
		display: grid;
		gap: 4px;
	}
	.fam {
		display: flex;
		flex-wrap: wrap;
		gap: 5px;
	}
	.fam .btn {
		font-family: var(--mono);
	}
	.fam .btn.on {
		border-color: var(--accent);
		color: var(--accent);
	}
	.flow {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		align-items: center;
	}
	.st {
		font: 500 12px/1 var(--mono);
		padding: 5px 9px;
		border-radius: var(--r);
		border: 1px solid var(--line);
		background: var(--sunk);
	}
	.st.to {
		border-color: var(--accent);
		color: var(--accent);
		background: color-mix(in srgb, var(--accent) 8%, var(--panel));
	}
	.st.new {
		border-style: dashed;
		background: transparent;
		color: var(--muted);
	}
	.arr {
		color: var(--faint);
	}
	.checks-list li {
		display: grid;
		grid-template-columns: 16px 1fr;
		gap: 8px;
		font-size: 13px;
	}
	.checks-list li > div {
		display: grid;
		gap: 1px;
	}
	.checks-list li .muted {
		font-size: 12px;
	}
	.tick {
		color: var(--accent);
	}
	.off {
		opacity: 0.6;
	}
	.off .tick {
		color: var(--faint);
	}
	.plain li {
		display: flex;
		flex-wrap: wrap;
		gap: 4px 8px;
		align-items: center;
		font-size: 13px;
	}
	.plain .cell-sub {
		flex-basis: 100%;
	}
	.faint {
		color: var(--faint);
	}

	@media (max-width: 1100px) {
		.chain {
			grid-template-columns: repeat(2, minmax(0, 1fr));
		}
		.chain li::after {
			display: none;
		}
		.chain li.here {
			grid-column: 1 / -1;
		}
		.ladder {
			grid-template-columns: repeat(2, minmax(0, 1fr));
		}
	}
	@media (max-width: 720px) {
		.chain {
			grid-template-columns: minmax(0, 1fr);
		}
		.mx-head {
			display: none;
		}
		.mx-row {
			grid-template-columns: minmax(0, 1fr);
			gap: 6px;
		}
		.mx-cell:empty,
		.mx-cell:not(:has(.op)) {
			display: none;
		}
		.mx-m {
			display: inline;
			font: 600 11px/1.6 var(--cond);
			text-transform: uppercase;
			color: var(--faint);
			width: 64px;
		}
		.tax {
			grid-template-columns: minmax(0, 1fr);
		}
		.tax > div:first-child ul {
			columns: 1;
		}
	}
</style>
