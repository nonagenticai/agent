// T11 System detail — pure helpers. One connected system is a translation chain: jobs call canonical operations,
// the pinned template turns each into a vendor call (with this tenant's overrides + env IDs) and maps fields,
// statuses and faults back. Per op, readiness (routed + a green T3 on a non-stub instance, doc 10 §4.5, doc 12 §3.5)
// is the backend's (D67: `connectorInstance().op_readiness`); failed calls split into user outcomes (agent replies)
// and system errors (escalate, doc 10 §3.1).
import type {
	ContractResult,
	Env,
	ErrorHandling,
	OpReadiness,
	SideEffect,
	SystemDetail,
	SystemInsight,
	SystemOpInsight,
} from '$lib/api/types/connectors';
import type { OperationStat } from '$lib/api/types/runtime';

export type Readiness = OpReadiness;

export const READINESS: Record<
	Readiness,
	{ label: string; variant: 'ok' | 'err' | 'warn' | 'accent' | 'muted' }
> = {
	ready: { label: 'ready', variant: 'ok' },
	blocked: { label: 'not ready — T3 red', variant: 'err' },
	needs_ok: { label: 'awaits your OK', variant: 'accent' },
	unverified: { label: 'write unverified', variant: 'warn' },
	untested: { label: 'untested', variant: 'warn' },
	stub: { label: 'stub — never ready', variant: 'muted' },
	unrouted: { label: 'not routed', variant: 'muted' },
};

export interface OpRow {
	op: string;
	object: string;
	side_effect: SideEffect;
	result: ContractResult;
	/** Envs on which at least one job routes this op to this system. */
	routed: Env[];
	jobs: string[];
	readiness: Readiness;
	calls: number | null;
	p95_ms: number | null;
	/** Failed calls the agent answered (user outcomes) vs escalated (system errors), last 30 days. */
	replied: number;
	escalated: number;
	errors: SystemOpInsight['errors_30d'];
	insight: SystemOpInsight | null;
}

const NOT_RUN = (op: string, side_effect: SideEffect): ContractResult => ({
	op,
	side_effect,
	outcome: 'not_run',
	code: null,
	note: '',
	at: null,
});

/** System errors (escalate + unlisted) vs user outcomes in one op's 30-day error mix. */
export function splitErrors(errors: { count: number; handling: ErrorHandling }[]): {
	replied: number;
	escalated: number;
} {
	let replied = 0;
	let escalated = 0;
	for (const e of errors)
		if (e.handling === 'reply') replied += e.count;
		else escalated += e.count;
	return { replied, escalated };
}

/** One row per template op: T3 result, routing, readiness, 30-day calls and their outcome split. */
export function opRows(d: SystemDetail, insight: SystemInsight | null, stats: OperationStat[]): OpRow[] {
	const i = d.instance;
	return d.template.ops.map((o) => {
		const result = i.contract.find((c) => c.op === o.op) ?? NOT_RUN(o.op, o.side_effect);
		const uses = d.used_by.filter((u) => u.ops.includes(o.op));
		const routed = [...new Set(uses.map((u) => u.env))];
		const ins = insight?.ops.find((x) => x.op === o.op) ?? null;
		const st = stats.find((s) => s.instance === i.id && s.op === o.op) ?? null;
		const errors = ins?.errors_30d ?? [];
		const readiness = d.op_readiness[o.op];
		if (!readiness) throw new Error(`connectorInstance() has no op_readiness for ${o.op}`);
		return {
			op: o.op,
			object: ins?.object ?? o.op.split('.')[0]!,
			side_effect: o.side_effect,
			result,
			routed,
			jobs: [...new Set(uses.map((u) => u.job))],
			readiness,
			calls: st?.calls ?? null,
			p95_ms: st?.p95_ms ?? null,
			...splitErrors(errors),
			errors,
			insight: ins,
		};
	});
}

/** Rows grouped by canonical object, template order kept. */
export function byObject(rows: OpRow[]): { object: string; rows: OpRow[] }[] {
	const out: { object: string; rows: OpRow[] }[] = [];
	for (const r of rows) {
		const g = out.find((x) => x.object === r.object);
		if (g) g.rows.push(r);
		else out.push({ object: r.object, rows: [r] });
	}
	return out;
}

export interface Verdict {
	ready: number;
	routed: number;
	tone: 'ok' | 'warn' | 'err';
	headline: string;
}

/** "Can my agent rely on this system?" — counted over the ops routed here on this instance's env. */
export function verdictOf(d: SystemDetail, rows: OpRow[]): Verdict {
	const i = d.instance;
	const routed = rows.filter((r) => r.readiness !== 'unrouted');
	const ready = routed.filter((r) => r.readiness === 'ready').length;
	const env = i.env === 'all' ? 'every env' : i.env === 'prod' ? 'production' : 'test';
	if (i.status === 'auth_failed' || i.status === 'invalid_target')
		return {
			ready,
			routed: routed.length,
			tone: 'err',
			headline: `Every call to this system fails right now — ${i.status === 'auth_failed' ? 'the credential was refused' : 'the base URL is refused'}.`,
		};
	if (i.stub)
		return {
			ready: 0,
			routed: routed.length,
			tone: 'warn',
			headline: 'A stub serves test routes with recorded answers; it never makes a route ready.',
		};
	if (routed.length === 0)
		return {
			ready: 0,
			routed: 0,
			tone: 'warn',
			headline: 'No job routes an operation here yet — the agent never calls this system.',
		};
	if (ready === routed.length)
		return {
			ready,
			routed: routed.length,
			tone: 'ok',
			headline: `Every routed operation is ready on ${env}.`,
		};
	return {
		ready,
		routed: routed.length,
		tone: routed.some((r) => r.readiness === 'blocked') ? 'err' : 'warn',
		headline: `${routed.length - ready} of ${routed.length} routed operations are not ready on ${env}.`,
	};
}

export type TodoAction =
	| { kind: 'run'; op: string; label: string }
	| { kind: 'tab'; tab: T11Tab; label: string }
	| { kind: 'link'; href: string; label: string };

export interface Todo {
	key: string;
	tone: 'err' | 'warn' | 'info';
	title: string;
	body: string;
	action: TodoAction | null;
}

export type T11Tab = 'ops' | 'upgrade' | 'mapping' | 'connection';

/** The decisions this page exists for, most urgent first. */
export function todosOf(d: SystemDetail, rows: OpRow[], insight: SystemInsight | null): Todo[] {
	const i = d.instance;
	const out: Todo[] = [];
	// A refused credential fails every op; its red tests are one cause, listed once.
	const authRed = (r: OpRow) => i.status === 'auth_failed' && r.result.code === 'connector_auth_failed';
	const nAuth = rows.filter((r) => r.readiness === 'blocked' && authRed(r)).length;
	if (i.status === 'auth_failed')
		out.push({
			key: 'cred',
			tone: 'err',
			title: 'Replace the credential',
			body: `${i.last_error?.message ?? 'The stored credential was refused.'}${nAuth ? ` ${nAuth} routed op${nAuth > 1 ? 's' : ''} fail${nAuth > 1 ? '' : 's'} T3 on it.` : ''}`,
			action: { kind: 'tab', tab: 'connection', label: 'Go to credential' },
		});
	if (i.status === 'invalid_target')
		out.push({
			key: 'url',
			tone: 'err',
			title: 'Fix the base URL',
			body: i.last_error?.message ?? 'The base URL resolves to a non-public address.',
			action: { kind: 'tab', tab: 'connection', label: 'Edit connection' },
		});
	if (
		i.credential_required !== false &&
		!i.platform &&
		!i.stub &&
		!i.credential.set &&
		i.status !== 'auth_failed'
	)
		out.push({
			key: 'cred-missing',
			tone: 'err',
			title: 'Set the credential',
			body: 'No credential is stored, so no call can be made.',
			action: { kind: 'tab', tab: 'connection', label: 'Set credential' },
		});
	for (const r of rows.filter((x) => x.readiness === 'blocked' && !authRed(x)))
		out.push({
			key: `red-${r.op}`,
			tone: 'err',
			title: `${r.op} is routed but its contract test is red`,
			body: `${r.result.code ?? 'red'} — ${r.result.note}. Fix it on the system side, then re-run.`,
			action: { kind: 'run', op: r.op, label: `Re-run ${r.op}` },
		});
	const ok = rows.filter((x) => x.readiness === 'needs_ok');
	if (ok.length)
		out.push({
			key: 'needs-ok',
			tone: 'warn',
			title: `${ok.length} production write${ok.length > 1 ? 's' : ''} await your OK`,
			body: `${ok.map((r) => r.op).join(', ')} — one tagged test write each, closed afterwards.`,
			action: { kind: 'tab', tab: 'ops', label: 'Review writes' },
		});
	const unv = rows.filter((x) => x.readiness === 'unverified');
	if (unv.length)
		out.push({
			key: 'unverified',
			tone: 'warn',
			title: `${unv.length} irreversible op${unv.length > 1 ? 's' : ''} only provable on a test instance`,
			body: `${unv.map((r) => r.op).join(', ')} run in production with confirm-back + verify_read. Prove them on a test-env instance of the same template.`,
			action: { kind: 'link', href: '/admin/connections/systems', label: 'Test systems' },
		});
	const untested = rows.filter((x) => x.readiness === 'untested');
	if (untested.length)
		out.push({
			key: 'untested',
			tone: 'warn',
			title: `${untested.length} routed op${untested.length > 1 ? 's' : ''} never tested`,
			body: untested.map((r) => r.op).join(', '),
			action: untested.every((r) => r.side_effect === 'R')
				? { kind: 'run', op: '*', label: 'Run all reads' }
				: { kind: 'tab', tab: 'ops', label: 'Open operations' },
		});
	const up = insight?.upgrade;
	if (up)
		out.push({
			key: 'upgrade',
			tone: up.verdict.kind === 'blocked' ? 'warn' : 'info',
			title:
				up.verdict.kind === 'blocked'
					? `@${up.to} is available but would strand a route`
					: `@${up.to} is available — ${up.counts.changed} op${up.counts.changed === 1 ? '' : 's'} change${up.counts.changed === 1 ? 's' : ''}`,
			body:
				up.verdict.kind === 'blocked'
					? up.verdict.reasons.join('; ')
					: `You stay on @${up.from} until you re-pin; re-pinning re-runs the contract tests.`,
			action: { kind: 'tab', tab: 'upgrade', label: 'Review upgrade' },
		});
	return out;
}

/** Days since an ISO timestamp, against `now` (injectable for tests). */
export function ageDays(at: string | null, now = Date.now()): number | null {
	return at ? Math.max(0, Math.floor((now - Date.parse(at)) / 86_400_000)) : null;
}

/** The status vocabulary per object as this pin applies it (system status → canonical), de-duplicated. */
export function statusMapsByObject(
	insight: SystemInsight,
): { object: string; ops: string[]; map: { system: string; canonical: string }[] }[] {
	const out: { object: string; ops: string[]; map: { system: string; canonical: string }[] }[] = [];
	for (const o of insight.ops.filter((x) => x.status_map.length)) {
		let g = out.find((x) => x.object === o.object);
		if (!g) out.push((g = { object: o.object, ops: [], map: [] }));
		g.ops.push(o.op);
		for (const m of o.status_map)
			if (!g.map.some((x) => x.system === m.system && x.canonical === m.canonical)) g.map.push(m);
	}
	return out;
}

/** Vendor faults → canonical codes across the template, de-duplicated, system errors first. */
export function faultMap(
	insight: SystemInsight,
): { system: string; code: string; handling: ErrorHandling }[] {
	const seen = new Map<string, { system: string; code: string; handling: ErrorHandling }>();
	for (const o of insight.ops) for (const f of o.fault_map) seen.set(`${f.system}→${f.code}`, f);
	const rank = { escalate: 0, unlisted: 1, reply: 2 } as const;
	return [...seen.values()].sort(
		(a, b) => rank[a.handling] - rank[b.handling] || a.code.localeCompare(b.code),
	);
}

/** Routed ops counted per readiness, in decision order (ready first, then what blocks, then what waits). */
export function readinessMix(rows: OpRow[]): { kind: Readiness; count: number; ops: string[] }[] {
	const order: Readiness[] = ['ready', 'blocked', 'needs_ok', 'unverified', 'untested', 'stub'];
	return order
		.map((kind) => {
			const ops = rows.filter((r) => r.readiness === kind).map((r) => r.op);
			return { kind, count: ops.length, ops };
		})
		.filter((x) => x.count > 0);
}
