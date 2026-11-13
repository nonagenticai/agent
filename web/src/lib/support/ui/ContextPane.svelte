<script lang="ts" module>
	import type { HandoverAgentStep, HandoverState } from '$lib/api/types/journey';

	/** Staff-facing state words (never "return to agent"). */
	export const STATE_WORDS: Record<HandoverState, string> = {
		queued: 'Waiting for a person',
		assigned: 'With a person',
		waiting_customer: 'Waiting on customer',
		resolved: 'Resolved',
		returned_to_agent: 'Handed back',
	};

	/** A state in staff words; an unknown state reads "Updated", never its raw code. */
	export const stateWord = (s: string | null | undefined): string =>
		(s && (STATE_WORDS as Record<string, string>)[s]) || 'Updated';

	const OUTCOME: Record<HandoverAgentStep['outcome'], string> = {
		ok: 'done',
		failed: 'failed',
		blocked: 'blocked',
		not_found: 'not found',
	};

	/** Drops decision codes from agent-written text: `(B4)`, `B4: `, a bare `D64`. The raw text stays in Details. */
	export function plain(s: string | null | undefined): string {
		return (s ?? '')
			.replace(/\s*\((?:[A-Z]{1,3}\s?\d{0,3})(?:\s*[,/·]\s*[A-Z]{1,3}\s?\d{0,3})*\)/g, '')
			.replace(/\b[A-Z]\d{1,3}:\s*/g, '')
			.replace(/\b[BD]\d{1,3}\b\s*/g, '')
			.replace(/\s{2,}/g, ' ')
			.trim();
	}

	/** An operation in words: `delivery.read` → "Delivery read", `case.create{complaint}` → "Created a complaint case". */
	export function opLabel(op: string): string {
		if (/^post-gate\b/.test(op)) return 'Checked the reply before sending';
		if (/^gate\b/.test(op)) return 'Understood the request';
		const made = /^case\.create\{(\w+)\}$/.exec(op);
		if (made)
			return made[1] === 'handover'
				? 'Handed over to a person'
				: `Created a ${made[1]!.replaceAll('_', ' ')} case`;
		const words = op
			.replace(/\{.*\}$/, '')
			.replace(/[._:]+/g, ' ')
			.trim();
		return words ? words[0]!.toUpperCase() + words.slice(1) : op;
	}

	/** "Track parcel — failed: carrier timeout". */
	export const stepText = (s: HandoverAgentStep) =>
		`${opLabel(s.op)} — ${OUTCOME[s.outcome] ?? s.outcome}${s.detail ? `: ${plain(s.detail)}` : ''}`;
</script>

<script lang="ts">
	// The context pane, in the spec's order: Why handed over · What the agent did · What it collected (copy) ·
	// Customer (linked record + last 5 cases, or Link customer) · SLA (one zone label) · History (collapsed) · Details
	// (collapsed: raw ids, release, decision codes, trace link). Handover data comes from the job's handover queue
	// (the existing handover view endpoint); decision codes and raw ids appear only inside Details.
	import { api } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { workAttrs } from '$lib/auth/roles';
	import { REASON } from '$lib/journey/J21Handover';
	import type { HandoverItem } from '$lib/api/types/journey';
	import type { PartyDetail, PartySummary } from '$lib/api/types/conversations';
	import type { CustomerContext } from '$lib/api/types/support';
	import {
		COLLAPSE_AT,
		IDENTITY_CHIP,
		focusLabel,
		itemsSummary,
		money,
		shortDate,
	} from '$lib/support/context';
	import LookupNote from '$lib/support/ui/LookupNote.svelte';
	import { clockOf, relative, zoneLabel } from '$lib/support/time';
	import { slaOf } from '$lib/support/inbox.svelte';

	let {
		job,
		conversationId,
		itemId,
		caseId,
		now,
		rev = 0,
		detailsOpen = $bindable(false),
		linking = $bindable(false),
		identified = $bindable(null),
	}: {
		job: string;
		conversationId: string;
		/** The handover item the conversation holds (or last held). */
		itemId: string | null;
		/** The conversation's runtime case (the party link and the trace). */
		caseId: string | null;
		now: number;
		/** Bumped by the workspace when the item changed (a `handover` frame, a resync): re-read. */
		rev?: number;
		detailsOpen?: boolean;
		/** The Link customer search is open (the header's overflow opens it too). */
		linking?: boolean;
		/** True once the customer is known (signed in or picked by staff); null until the context loads. */
		identified?: boolean | null;
	} = $props();

	let item = $state<HandoverItem | null>(null);
	let party = $state<PartyDetail | null>(null);
	let loadError = $state<string | null>(null);
	let loading = $state(true);
	let copied = $state<string | null>(null);
	let q = $state('');
	let found = $state<PartySummary[]>([]);
	let linkError = $state<string | null>(null);
	// KYC: who the customer is and every record we hold for them, loaded when the conversation opens and on every `rev`.
	let ctx = $state<CustomerContext | null>(null);
	let ctxError = $state<string | null>(null);
	let open = $state<Record<string, boolean>>({});
	const known = $derived(!!ctx && ctx.identity.source !== null && !!ctx.customer);
	$effect(() => {
		identified = ctx ? known : null;
	});

	const lists = $derived.by(() => {
		if (!ctx) return [];
		const c = ctx;
		const byOrder = (id: string) => {
			const o = c.orders.find((x) => x.order_id === id);
			return o ? `${shortDate(o.placed_at, now)} · ${itemsSummary(o, 1)}` : 'order';
		};
		return [
			{
				key: 'orders',
				title: 'Orders',
				rows: c.orders.map((o) => ({
					id: o.order_id,
					primary: `${shortDate(o.placed_at, now)} · ${itemsSummary(o)}`,
					secondary: o.status,
				})),
			},
			{
				key: 'deliveries',
				title: 'Deliveries',
				rows: c.deliveries.map((d) => ({
					id: d.delivery_id,
					primary: `${byOrder(d.order_id)} · ${d.carrier}`,
					secondary: d.delivered_at
						? `${d.status} ${shortDate(d.delivered_at, now)}`
						: d.eta
							? `${d.status} · ETA ${shortDate(d.eta, now)}`
							: d.status,
				})),
			},
			{
				key: 'payments',
				title: 'Payments',
				rows: c.payments.map((p) => ({
					id: p.payment_id,
					primary: `${money(p.amount, p.currency)} · ${byOrder(p.order_id)}`,
					secondary:
						p.refunded_amount > 0
							? `${p.status} · refunded ${money(p.refunded_amount, p.currency)}`
							: p.status,
				})),
			},
			{
				key: 'cases',
				title: 'Cases',
				rows: c.cases.map((k) => ({
					id: k.case_id,
					primary: k.title,
					secondary: `${k.state} · ${shortDate(k.opened_at, now)}`,
				})),
			},
		];
	});

	async function loadContext(j: string, c: string) {
		try {
			const r = await api().customerContext(j, c);
			ctx = r;
			ctxError = null;
		} catch (e) {
			ctxError = e instanceof Error ? e.message : 'Could not load the customer.';
		}
	}
	$effect(() => {
		const j = job;
		const c = conversationId;
		rev;
		void loadContext(j, c);
	});
	// A different conversation starts with no stale customer and every list collapsed.
	$effect(() => {
		conversationId;
		ctx = null;
		ctxError = null;
		open = {};
	});

	// The workspace is tenant-wide: every read names the conversation's job (never the active job).
	async function loadItem(j: string, id: string | null) {
		if (!id) return null;
		const q = await api().jobHandovers(j);
		return q.items.find((h) => h.id === id) ?? null;
	}
	async function loadParty(j: string, c: string | null) {
		if (!c) return null;
		try {
			return await api().jobCaseParty(j, c);
		} catch {
			return null;
		}
	}
	$effect(() => {
		const j = job;
		const id = itemId;
		const c = caseId;
		conversationId;
		rev;
		let live = true;
		loading = true;
		loadError = null;
		Promise.all([loadItem(j, id), loadParty(j, c)])
			.then(([h, p]) => {
				if (!live) return;
				item = h;
				party = p;
			})
			.catch((e) => {
				if (live) loadError = e instanceof Error ? e.message : 'Could not load the handover.';
			})
			.finally(() => {
				if (live) loading = false;
			});
		return () => {
			live = false;
		};
	});

	// The same rule (and the same window start) as the inbox row's badge.
	const sla = $derived(
		item
			? slaOf(
					{
						first_response_due: item.first_response_due,
						first_response_at: item.first_response_at,
						resolve_due: item.resolve_due,
						sla_starts_at: item.clock_starts_at ?? item.queued_at,
					},
					now,
				)
			: null,
	);

	async function copy(label: string, value: string) {
		try {
			await navigator.clipboard?.writeText(value);
			copied = label;
			setTimeout(() => {
				if (copied === label) copied = null;
			}, 1500);
		} catch {
			copied = null;
		}
	}

	async function search() {
		linkError = null;
		try {
			found = (await api().parties({ q: q.trim() || undefined })).items.slice(0, 8);
		} catch (e) {
			linkError = e instanceof Error ? e.message : 'Could not search customers.';
		}
	}
	async function link(p: PartySummary) {
		if (!caseId) return;
		linkError = null;
		try {
			party = await api().jobLinkCaseParty(job, caseId, p.id);
			void loadContext(job, conversationId);
			linking = false;
			found = [];
			q = '';
		} catch (e) {
			linkError = e instanceof Error ? e.message : 'Could not link the customer.';
		}
	}
	const linkAttrs = $derived(workAttrs(session));
</script>

<aside class="ctx" aria-label="Context">
	<section class="cust" data-testid="customer-block">
		<h3>Customer</h3>
		{#if ctx && known && ctx.customer}
			<p class="who">
				<strong>{ctx.customer.name}</strong>
				<span class="chip ok" data-testid="identity-chip">{IDENTITY_CHIP[ctx.identity.source!]}</span>
			</p>
			<p class="muted">{ctx.customer.email}</p>
			<LookupNote {job} degraded={ctx.degraded} />
			{#if ctx.focus.length}
				<ul class="focus" aria-label="Records in focus">
					{#each ctx.focus as f (f.kind + f.id)}
						{@const l = focusLabel(f, ctx)}
						<li data-focus={f.kind}>
							<strong>{l.label}</strong>{#if f.why}<span> — {f.why}</span>{/if}
							<small class="muted">{l.id}</small>
						</li>
					{/each}
				</ul>
			{/if}
			{#each lists as l (l.key)}
				{#if l.rows.length}
					<h4>{l.title}</h4>
					<ul class="rows" data-list={l.key}>
						{#each open[l.key] ? l.rows : l.rows.slice(0, COLLAPSE_AT) as r (r.id)}
							<li>
								<span>{r.primary}</span>
								<span class="muted">{r.secondary}</span>
								<small class="muted">{r.id}</small>
							</li>
						{/each}
					</ul>
					{#if l.rows.length > COLLAPSE_AT}
						<button
							class="btn sm ghost"
							type="button"
							aria-expanded={!!open[l.key]}
							onclick={() => (open[l.key] = !open[l.key])}
							>{open[l.key] ? 'Show fewer' : `Show all ${l.rows.length}`}</button
						>
					{/if}
				{/if}
			{/each}
		{:else if ctx}
			<p class="muted" data-testid="not-signed-in">
				Not signed in — the customer gets general help only. If you know who this is, pick them.
			</p>
			{#if caseId}
				{#if linking}
					<form
						class="link"
						aria-label="Link customer"
						onsubmit={(e) => {
							e.preventDefault();
							void search();
						}}
					>
						<input
							type="search"
							aria-label="Search customers by name"
							placeholder="Search customers by name"
							bind:value={q}
						/>
						<button class="btn sm" type="submit">Find</button>
						<button class="btn sm ghost" type="button" onclick={() => (linking = false)}>Cancel</button>
					</form>
					{#if linkError}<p class="err" role="alert">{linkError}</p>{/if}
					{#each found as p (p.id)}
						<button class="btn sm pick" type="button" {...linkAttrs} onclick={() => link(p)}
							>Pick {p.display_name ?? 'this customer'}</button
						>
					{/each}
				{:else}
					<button class="btn sm" type="button" {...linkAttrs} onclick={() => (linking = true)}
						>Link customer</button
					>
				{/if}
			{/if}
		{:else if ctxError}
			<p class="err" role="alert">{ctxError}</p>
		{:else}
			<p class="muted">Loading…</p>
		{/if}
	</section>

	{#if loading && !item}
		<p class="muted">Loading…</p>
	{:else if loadError}
		<p class="err" role="alert">{loadError}</p>
	{/if}

	{#if item}
		<section>
			<h3>Why handed over</h3>
			<p class="why"><strong>{REASON[item.reason]?.label ?? 'Handed over'}</strong></p>
			{#if plain(REASON[item.reason]?.hint)}<p class="muted">{plain(REASON[item.reason]?.hint)}</p>{/if}
			{#if plain(item.suggested_next)}<p>
					<span class="muted">Suggested first step:</span>
					{plain(item.suggested_next)}
				</p>{/if}
		</section>

		<section>
			<h3>What the agent did</h3>
			{#if item.agent_did.length}
				<ol class="steps">
					{#each item.agent_did as s, i (i)}
						<li class={s.outcome}>{stepText(s)}</li>
					{/each}
				</ol>
			{:else}
				<p class="muted">Nothing yet — a person took it straight away.</p>
			{/if}
		</section>

		{#if !known}
			<section>
				<h3>What it collected</h3>
				{#if item.collected.length}
					<dl class="kv">
						{#each item.collected as c, i (i)}
							<dt>
								{c.label}{#if c.verified}<span class="ok" title="Checked against a system"> ✓</span>{/if}
							</dt>
							<dd>
								<span>{c.value}</span>
								<button
									class="btn sm ghost"
									type="button"
									aria-label="Copy {c.label}"
									onclick={() => copy(c.label, c.value)}>{copied === c.label ? 'Copied' : 'Copy'}</button
								>
							</dd>
						{/each}
					</dl>
				{:else}
					<p class="muted">Nothing collected.</p>
				{/if}
			</section>
		{/if}
	{:else if !loading && !loadError}
		<section>
			<h3>Why handed over</h3>
			<p class="muted">No handover on this conversation — the agent is answering it.</p>
		</section>
	{/if}

	{#if known && ctx}
		<section data-testid="looked-at">
			<h3>What the agent looked at</h3>
			{#if ctx.focus.length}
				<ul class="steps">
					{#each ctx.focus as f (f.kind + f.id)}
						{@const l = focusLabel(f, ctx)}
						<li>{l.label}{f.why ? ` — ${f.why}` : ''}</li>
					{/each}
				</ul>
			{:else}
				<p class="muted">Nothing specific — it answered from general help.</p>
			{/if}
		</section>
	{/if}

	{#if item}
		<section>
			<h3>SLA</h3>
			<p class="muted">Times in {zoneLabel()}</p>
			<dl class="kv">
				<dt>First response</dt>
				<dd>
					{#if item.first_response_at}answered {clockOf(
							item.first_response_at,
							now,
						)}{:else if item.first_response_due}due {clockOf(item.first_response_due, now)} ({relative(
							item.first_response_due,
							now,
						)}){:else}no target{/if}
				</dd>
				<dt>Resolve</dt>
				<dd>
					{#if item.resolve_due}due {clockOf(item.resolve_due, now)} ({relative(
							item.resolve_due,
							now,
						)}){:else}no target{/if}
				</dd>
			</dl>
			{#if sla === 'breached'}<span class="chip err">SLA breached</span>
			{:else if sla === 'at_risk'}<span class="chip warn">SLA at risk</span>{/if}
		</section>

		<details class="sec">
			<summary>History</summary>
			<ol class="hist">
				{#each item.history as h, i (i)}
					<li>
						<span class="muted">{clockOf(h.at, now)}</span>
						{h.actor === 'agent' ? 'Agent' : h.actor_name}: {stateWord(h.to)}{plain(h.note)
							? ` — ${plain(h.note)}`
							: ''}
					</li>
				{/each}
			</ol>
		</details>
	{/if}

	<details class="sec" data-details bind:open={detailsOpen}>
		<summary>Details</summary>
		<dl class="kv raw">
			<dt>Conversation</dt>
			<dd>{conversationId}</dd>
			<dt>Job</dt>
			<dd>{job}</dd>
			{#if item}
				<dt>Handover</dt>
				<dd>{item.id}</dd>
				<dt>Reason code</dt>
				<dd>{item.reason}</dd>
				<dt>Trigger</dt>
				<dd>{item.trigger}</dd>
				<dt>Release</dt>
				<dd>{item.release_id ?? '—'}</dd>
				<dt>Queue</dt>
				<dd>{item.queue}</dd>
				{#if known}
					{#each item.collected as c, i (i)}
						<dt>Collected · {c.label}</dt>
						<dd>{c.value}</dd>
					{/each}
				{/if}
				<dt>Steps</dt>
				<dd>{item.agent_did.map((s) => `${s.op}: ${s.outcome}`).join(' · ') || '—'}</dd>
				{#if REASON[item.reason]?.hint}<dt>Rule</dt>
					<dd>{REASON[item.reason]?.hint}</dd>{/if}
			{/if}
			{#if caseId}
				<dt>Case</dt>
				<dd>{caseId}</dd>
			{/if}
		</dl>
		{#if caseId}
			<a href="/admin/jobs/{encodeURIComponent(job)}/cases/{encodeURIComponent(caseId)}"
				>Open the case trace</a
			>
		{/if}
	</details>
</aside>

<style>
	.ctx {
		display: grid;
		align-content: start;
		gap: 14px;
		padding: 12px;
		min-height: 0;
		height: 100%;
		overflow-y: auto;
		border: 1px solid var(--line);
		border-radius: 8px;
		background: var(--panel);
		font-size: 13px;
	}
	h3 {
		margin: 0 0 4px;
		font-size: 11.5px;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		color: var(--muted);
	}
	p {
		margin: 0 0 4px;
	}
	.muted {
		color: var(--muted);
	}
	.err {
		color: var(--err);
	}
	.steps,
	.hist {
		margin: 0;
		padding-left: 18px;
		display: grid;
		gap: 3px;
	}
	.steps li.failed,
	.steps li.blocked {
		color: var(--err);
	}
	.kv {
		display: grid;
		grid-template-columns: minmax(80px, auto) 1fr;
		gap: 4px 10px;
		margin: 0;
	}
	.kv dt {
		color: var(--muted);
	}
	.kv dd {
		margin: 0;
		display: flex;
		gap: 6px;
		align-items: start;
		justify-content: space-between;
		overflow-wrap: anywhere;
	}
	.kv.raw dd {
		font: 12px/1.4 var(--mono);
	}
	.ok {
		color: var(--ok);
	}
	.sec summary {
		cursor: pointer;
		font-size: 11.5px;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		color: var(--muted);
	}
	.link {
		display: flex;
		gap: 6px;
		margin-top: 6px;
	}
	.link input {
		flex: 1;
		min-width: 0;
	}
	.who {
		display: flex;
		gap: 8px;
		align-items: center;
		flex-wrap: wrap;
	}
	.chip.ok {
		font-size: 11px;
		padding: 1px 7px;
		border-radius: 999px;
		border: 1px solid var(--ok);
		color: var(--ok);
	}
	h4 {
		margin: 10px 0 3px;
		font-size: 11.5px;
		color: var(--muted);
	}
	.focus,
	.rows {
		margin: 0;
		padding: 0;
		list-style: none;
		display: grid;
		gap: 4px;
	}
	.focus li {
		padding: 6px 8px;
		border-left: 3px solid var(--accent);
		background: color-mix(in srgb, var(--accent) 10%, transparent);
		border-radius: 4px;
		display: grid;
	}
	.rows li {
		display: grid;
	}
	.rows small,
	.focus small {
		font-size: 11px;
	}
	.pick {
		margin-top: 4px;
		display: block;
	}
</style>
