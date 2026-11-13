// T10 display helpers: "can my agents do their job with the systems I've connected?" — the job's required (object,
// verb) pairs (K48 unit) per env, each at its worst variant. D67: whether a route runs is the backend's
// (`jobOperations().cells`); this module only folds cells into pairs; which ops each system serves; upgrade
// availability; catalogue gap fillers.
import type {
	CellState,
	ConnectorInstance,
	Env,
	JobOperations,
	RouteTarget,
	SystemCatalogueRow,
} from '$lib/api/types/connectors';
import { opKey } from './ops';

/** Worst-first: ok < untested < failing < stub < missing. `failing` = routed to a system whose last call failed. */
export type Fit = 'ok' | 'untested' | 'failing' | 'stub' | 'missing';
const RANK: Record<Fit, number> = { ok: 0, untested: 1, failing: 2, stub: 3, missing: 4 };

export const FIT_TEXT: Record<Fit, string> = {
	ok: 'ready',
	untested: 'not tested',
	failing: 'system failing',
	stub: 'stub only',
	missing: 'unrouted',
};
export const FIT_VARIANT: Record<Fit, 'ok' | 'warn' | 'err' | 'muted'> = {
	ok: 'ok',
	untested: 'warn',
	failing: 'err',
	stub: 'warn',
	missing: 'err',
};

/** A backend cell state as a fit (a confirm-back write runs, so it fits). */
const FIT_OF: Record<CellState, Fit> = {
	ready: 'ok',
	fallback: 'ok',
	unverified: 'ok',
	needs_ok: 'untested',
	untested: 'untested',
	red: 'untested',
	failing: 'failing',
	stub: 'stub',
	unrouted: 'missing',
	not_offered: 'missing',
};

export interface FitRow {
	key: string;
	fit: Fit;
	/** Where the runner sends it on this env (null = unrouted). */
	target: RouteTarget | null;
	suggested: RouteTarget | null;
}

export interface PairFit {
	/** `order.read` — variants roll up into their pair. */
	pair: string;
	required: boolean;
	fit: Fit;
	rows: FitRow[];
	/** Instance ids (or `built-in`, `deep link`, `agent_cases`) the pair's rows land on. */
	via: string[];
}

export interface EnvFit {
	env: Env;
	pairs: PairFit[];
	required: Record<Fit, number> & { total: number };
	optional: { served: number; total: number };
}

function viaOf(t: RouteTarget | null): string | null {
	if (!t) return null;
	switch (t.kind) {
		case 'instance':
			return t.instance_id;
		case 'agent_cases':
			return 'agent_cases';
		case 'builtin':
			return 'built-in';
		case 'deep_link':
			return 'deep link';
		case 'not_offered':
			return null;
	}
}

/** Required + optional (object, verb) pairs of the job on `env`, each at its worst variant's fit. */
export function jobFit(d: JobOperations, env: Env): EnvFit {
	const pairs = new Map<string, PairFit>();
	for (const o of d.ops) {
		const c = d.cells[`${opKey(o)}@${env}`];
		if (!c) throw new Error(`jobOperations() has no cell for ${opKey(o)}@${env}`);
		const target = c.target;
		const fit = FIT_OF[c.state];
		const pair = `${o.object}.${o.verb}`;
		const p = pairs.get(pair) ?? { pair, required: false, fit: 'ok' as Fit, rows: [], via: [] };
		p.required ||= o.required;
		p.rows.push({ key: opKey(o), fit, target, suggested: c.suggested });
		if (RANK[fit] > RANK[p.fit]) p.fit = fit;
		const v = viaOf(target);
		if (v && !p.via.includes(v)) p.via.push(v);
		pairs.set(pair, p);
	}
	const list = [...pairs.values()];
	const req = list.filter((p) => p.required);
	const required = { ok: 0, untested: 0, failing: 0, stub: 0, missing: 0, total: req.length };
	for (const p of req) required[p.fit]++;
	const opt = list.filter((p) => !p.required);
	return {
		env,
		pairs: list,
		required,
		optional: { served: opt.filter((p) => p.fit !== 'missing').length, total: opt.length },
	};
}

/** Pairs that keep the job from being ready on that env, worst first. */
export function blockers(f: EnvFit): PairFit[] {
	return f.pairs.filter((p) => p.required && p.fit !== 'ok').sort((a, b) => RANK[b.fit] - RANK[a.fit]);
}

/** What one connected system serves for the job, per env it is used on: `order.read` + fit. */
export type ServedOp = FitRow & { required: boolean; env: Env };

export function servesFor(fits: EnvFit[], instanceId: string): ServedOp[] {
	const out: ServedOp[] = [];
	for (const f of fits)
		for (const p of f.pairs)
			for (const r of p.rows) {
				if (viaOf(r.target) !== instanceId) continue;
				if (out.some((x) => x.key === r.key)) continue;
				out.push({ ...r, required: p.required, env: f.env });
			}
	return out.sort((a, b) => Number(b.required) - Number(a.required) || a.key.localeCompare(b.key));
}

const cmpVersion = (a: string, b: string) => a.localeCompare(b, 'en', { numeric: true });

/** A newer ACTIVE version of the pinned template key (the only legal re-pin target), or null. */
export function upgradeFor(
	inst: Pick<ConnectorInstance, 'template_key' | 'template_version' | 'template_scope'>,
	catalogue: SystemCatalogueRow[],
): string | null {
	const newer = catalogue
		.filter(
			(r) =>
				r.key === inst.template_key &&
				r.scope === inst.template_scope &&
				r.status === 'active' &&
				cmpVersion(r.version, inst.template_version) > 0,
		)
		.sort((a, b) => cmpVersion(b.version, a.version));
	return newer[0]?.version ?? null;
}

/** Which of the job's unserved required pairs a catalogue template would serve (variant → `*` fallback). */
export function fillsGaps(row: Pick<SystemCatalogueRow, 'implements'>, gaps: PairFit[]): string[] {
	const impl = row.implements ?? [];
	return gaps
		.filter((g) =>
			g.rows
				.filter((r) => r.fit === 'missing' && !r.suggested)
				.every((r) => impl.includes(r.key) || impl.includes(r.key.replace(/\{[^}]+\}$/, ''))),
		)
		.map((g) => g.pair);
}
