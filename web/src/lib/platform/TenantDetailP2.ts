// P2 Tenant detail — pure derivations: "can this tenant serve its customers right now?" as a chain of gates a
// customer turn passes (04 §2b status, 04 §4b gate-2 verdict, 05 release on the bus, 06 §4.6 provision-down), the
// lifecycle state machine (K42: active ↔ suspended → archived via offboard only) and labels for outbox rows.
import type { TemplatePin } from '$lib/api/types/core';
import type { FootprintEvent, ProvisionRow, TenantDetail } from '$lib/api/types/platform';
import type { DiagramState, DiagramTransition } from '$lib/ui/StateDiagram.svelte';
import { TARGET_LABEL, age, hhmm, utc } from './ui';

export type GateTone = 'ok' | 'warn' | 'err' | 'off';

export interface ServingGate {
	key: 'status' | 'licence' | 'release' | 'provision';
	label: string;
	tone: GateTone;
	/** One short line: what the gate sees. */
	detail: string;
}

export interface Serving {
	tone: GateTone;
	headline: string;
	gates: ServingGate[];
}

/** `ready`, `pending (next retry 09:42)`, `failed (slot_incomplete)`, `dead`. */
export function provLabel(r: Pick<ProvisionRow, 'status' | 'next_attempt_at' | 'last_error'>): string {
	switch (r.status) {
		case 'succeeded':
			return 'ready';
		case 'pending':
			return `pending (next retry ${hhmm(r.next_attempt_at)})`;
		case 'failed':
			return `failed (${r.last_error?.code ?? 'error'})`;
		case 'dead':
			return r.last_error ? `dead (${r.last_error.code})` : 'dead';
		default:
			return r.status.replace('_', ' ');
	}
}

/** The latest non-superseded, non-cancelled outbox row per target, in Licences → Knowledge → Ingest order. */
export function latestPerTarget(rows: ProvisionRow[]): ProvisionRow[] {
	const out: ProvisionRow[] = [];
	for (const target of ['licences', 'strata_rag', 'strata_ingest'] as const) {
		const mine = rows
			.filter((r) => r.target === target)
			.sort((a, b) => b.updated_at.localeCompare(a.updated_at));
		const live = mine.find((r) => r.status !== 'superseded' && r.status !== 'cancelled') ?? mine[0];
		if (live) out.push(live);
	}
	return out;
}

const BROKEN = new Set(['failed', 'dead']);
const OPEN = new Set(['pending', 'in_flight']);

/** Walks the four gates a customer turn needs. The worst gate decides the headline. */
export function servingOf(d: TenantDetail): Serving {
	const t = d.tenant;
	if (t.status === 'archived')
		return {
			tone: 'off',
			headline: 'Offboarded — this tenant serves nothing and nothing here can change',
			gates: [
				{
					key: 'status',
					label: 'Workspace',
					tone: 'off',
					detail: `archived ${utc(t.archived_at)} (terminal)`,
				},
				{ key: 'licence', label: 'Licence (gate 2)', tone: 'off', detail: 'revoked at offboard' },
				{ key: 'release', label: 'Job on the bus', tone: 'off', detail: 'scopes removed' },
				{
					key: 'provision',
					label: 'Knowledge & ingest',
					tone: 'off',
					detail: 'licences revoked, data retained',
				},
			],
		};

	const gates: ServingGate[] = [];
	gates.push(
		t.status === 'suspended'
			? {
					key: 'status',
					label: 'Workspace',
					tone: 'err',
					detail: `suspended ${utc(t.suspended_at)} — members get 403 tenant_suspended`,
				}
			: {
					key: 'status',
					label: 'Workspace',
					tone: 'ok',
					detail: `active since ${utc(t.created_at).slice(0, 10)}`,
				},
	);

	const v = d.verdict;
	gates.push(
		!v
			? {
					key: 'licence',
					label: 'Licence (gate 2)',
					tone: 'err',
					detail: 'no verdict row — the edge fails closed',
				}
			: v.fails_closed
				? {
						key: 'licence',
						label: 'Licence (gate 2)',
						tone: 'err',
						detail:
							v.status === 'active'
								? `verdict ${age(v.age_seconds)} old (> 5 min) — the edge fails closed`
								: `verdict ${v.status}${v.reason ? `: ${v.reason}` : ''} — the edge refuses turns`,
					}
				: {
						key: 'licence',
						label: 'Licence (gate 2)',
						tone: v.reason ? 'warn' : 'ok',
						detail: `verdict ${v.status}, checked ${age(v.age_seconds)} ago${v.reason ? ` · ${v.reason}` : ''}`,
					},
	);

	const live = d.jobs.filter((j) => j.current_release && j.on_bus);
	const released = d.jobs.filter((j) => j.current_release);
	gates.push(
		live.length
			? {
					key: 'release',
					label: 'Job on the bus',
					tone: 'ok',
					detail: `${live.length} of ${d.jobs.length} job${d.jobs.length === 1 ? '' : 's'} released and serving`,
				}
			: {
					key: 'release',
					label: 'Job on the bus',
					tone: 'warn',
					detail: !d.jobs.length
						? 'no job yet — valid, but nothing to serve'
						: released.length
							? 'released, but no agent is on the bus'
							: 'no release cut yet — nothing to serve',
				},
	);

	const rows = latestPerTarget(d.provisioning);
	const lic = rows.find((r) => r.target === 'licences');
	const broken = rows.filter((r) => BROKEN.has(r.status));
	const open = rows.filter((r) => OPEN.has(r.status));
	gates.push(
		lic && BROKEN.has(lic.status)
			? {
					key: 'provision',
					label: 'Knowledge & ingest',
					tone: 'err',
					detail: `licence bundle ${lic.status} — no rag/ingest token can be minted`,
				}
			: broken.length
				? {
						key: 'provision',
						label: 'Knowledge & ingest',
						tone: 'warn',
						detail: broken.map((r) => `${TARGET_LABEL[r.target]} ${provLabel(r)}`).join(' · '),
					}
				: open.length
					? {
							key: 'provision',
							label: 'Knowledge & ingest',
							tone: 'warn',
							detail: open.map((r) => `${TARGET_LABEL[r.target]} ${provLabel(r)}`).join(' · '),
						}
					: rows.length
						? { key: 'provision', label: 'Knowledge & ingest', tone: 'ok', detail: 'all targets ready' }
						: { key: 'provision', label: 'Knowledge & ingest', tone: 'warn', detail: 'nothing queued yet' },
	);

	const worst = gates.some((g) => g.tone === 'err')
		? 'err'
		: gates.some((g) => g.tone === 'warn')
			? 'warn'
			: 'ok';
	const blocker = gates.find((g) => g.tone === 'err');
	const headline =
		worst === 'err'
			? `Not serving customers — blocked at ${blocker!.label.toLowerCase()}`
			: worst === 'warn'
				? `Serving, degraded — ${gates
						.filter((g) => g.tone === 'warn')
						.map((g) => g.label.toLowerCase())
						.join(' and ')} need${gates.filter((g) => g.tone === 'warn').length === 1 ? 's' : ''} attention`
				: 'Serving customers — every gate is green';
	return { tone: worst, headline, gates };
}

// ---- lifecycle (K42, 04 §7b) ---------------------------------------------------------------------------------------

export const LIFECYCLE_STATES: DiagramState[] = [
	{ id: 'active', label: 'active', initial: true },
	{ id: 'suspended', label: 'suspended' },
	{ id: 'archived', label: 'archived', terminal: true },
];

export const LIFECYCLE_TRANSITIONS: DiagramTransition[] = [
	{ from: 'active', to: 'suspended', label: 'suspend' },
	{ from: 'suspended', to: 'active', label: 'resume' },
	{ from: 'active', to: 'archived', label: 'offboard' },
	{ from: 'suspended', to: 'archived', label: 'offboard' },
];

// ---- template pins (D15) -------------------------------------------------------------------------------------------

const PIN_KIND: [RegExp, string][] = [
	[/^base$/, 'base'],
	[/^industry\//, 'industry'],
	[/^industry_dept\//, 'department'],
	[/^job\//, 'job'],
];

/** `industry_dept/retail/customer_service` → `department`. Unknown prefixes keep their first segment. */
export function pinKind(p: TemplatePin): string {
	return PIN_KIND.find(([re]) => re.test(p.layer))?.[1] ?? p.layer.split('/')[0]!;
}

// ---- history -------------------------------------------------------------------------------------------------------

/** Lifecycle events the history card tints: the ones that changed what the tenant can do. */
export function eventTone(e: FootprintEvent): 'ok' | 'warn' | 'err' | 'muted' {
	if (!e.ok || /\.(failed|dead|refused|gate_refused)$/.test(e.action)) return 'err';
	if (/^(tenant\.(suspend|offboard)|provision\.drift|user\.disable)/.test(e.action)) return 'warn';
	if (/^(tenant\.create|tenant\.import|release\.apply|provision\.succeeded)/.test(e.action)) return 'ok';
	return 'muted';
}

/** A run of the same event (e.g. hourly `licence.gate_refused` while suspended), newest first. */
export interface HistoryGroup {
	first: FootprintEvent;
	/** Oldest event of the run. */
	last: FootprintEvent;
	count: number;
}

/** Folds consecutive events with the same action, resource and outcome into one row with a count. */
export function collapseHistory(events: FootprintEvent[]): HistoryGroup[] {
	const out: HistoryGroup[] = [];
	for (const e of events) {
		const g = out.at(-1);
		if (g && g.first.action === e.action && g.first.resource_id === e.resource_id && g.first.ok === e.ok) {
			g.last = e;
			g.count++;
		} else out.push({ first: e, last: e, count: 1 });
	}
	return out;
}
