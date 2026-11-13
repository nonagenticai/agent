// J6 display helpers over the route matrix. D67: whether a cell can run is decided by the backend
// (`jobOperations().cells`, see `$lib/api/areas/readiness`); this module only looks cells up, labels them, counts
// and groups them for the gate, the blockers and the legend.
import type { CellState, Env, ExpectedOperation, JobOperations, RouteCell } from '$lib/api/types/connectors';
import { opKey } from './ops';

export type { CellState };

export const CELL: Record<CellState, { label: string; variant: 'ok' | 'warn' | 'err' | 'muted' | 'accent' }> =
	{
		ready: { label: 'ready', variant: 'ok' },
		fallback: { label: 'fallback', variant: 'ok' },
		unverified: { label: 'confirm-back', variant: 'muted' },
		needs_ok: { label: 'awaits your OK', variant: 'accent' },
		untested: { label: 'not tested', variant: 'warn' },
		red: { label: 'test red', variant: 'err' },
		failing: { label: 'system failing', variant: 'err' },
		stub: { label: 'stub only', variant: 'warn' },
		unrouted: { label: 'no route', variant: 'err' },
		not_offered: { label: 'not offered', variant: 'muted' },
	};

const ORDER: CellState[] = [
	'unrouted',
	'failing',
	'red',
	'stub',
	'needs_ok',
	'untested',
	'unverified',
	'fallback',
	'ready',
	'not_offered',
];

export interface Cell extends RouteCell {
	op: ExpectedOperation;
	key: string;
	env: Env;
}

/** The backend's cell for `op` on `env`. */
export function cellOf(d: JobOperations, op: ExpectedOperation, env: Env): Cell {
	const key = opKey(op);
	const c = d.cells[`${key}@${env}`];
	if (!c) throw new Error(`jobOperations() has no cell for ${key}@${env}`);
	return { ...c, op, key, env };
}

/** Required ops that cannot run for customers on `env`, worst first. */
export function blockersOn(d: JobOperations, env: Env): Cell[] {
	return d.ops
		.filter((o) => o.required)
		.map((o) => cellOf(d, o, env))
		.filter((c) => !c.runs)
		.sort((a, b) => ORDER.indexOf(a.state) - ORDER.indexOf(b.state));
}

/** Optional ops on `env` that are routed but held back (test red, awaiting OK, failing, untested). */
export function heldBack(d: JobOperations, env: Env): Cell[] {
	return d.ops
		.filter((o) => !o.required)
		.map((o) => cellOf(d, o, env))
		.filter((c) => ['red', 'needs_ok', 'failing', 'untested', 'stub'].includes(c.state))
		.sort((a, b) => ORDER.indexOf(a.state) - ORDER.indexOf(b.state));
}

/** Prod-env summary for the gate: required (object, verb) pairs that run vs total (K48 unit). */
export function gate(d: JobOperations, env: Env): { runs: number; routed: number; total: number } {
	const pairs = new Map<string, Cell[]>();
	for (const o of d.ops.filter((x) => x.required)) {
		const k = `${o.object}.${o.verb}`;
		pairs.set(k, [...(pairs.get(k) ?? []), cellOf(d, o, env)]);
	}
	let runs = 0;
	let routed = 0;
	for (const cells of pairs.values()) {
		if (cells.every((c) => c.state !== 'unrouted')) routed++;
		if (cells.every((c) => c.runs)) runs++;
	}
	return { runs, routed, total: pairs.size };
}

/** Cells grouped by state for one env (legend counts). */
export function tally(d: JobOperations, env: Env): Partial<Record<CellState, number>> {
	const out: Partial<Record<CellState, number>> = {};
	for (const o of d.ops) {
		const s = cellOf(d, o, env).state;
		out[s] = (out[s] ?? 0) + 1;
	}
	return out;
}

export const needsAttention = (c: Cell) => !c.runs && c.state !== 'not_offered';

export interface Cause {
	id: string;
	state: CellState;
	instance: string | null;
	why: string;
	cells: Cell[];
}

/** One fix per cause: cells landing on the same system in the same state share it; unrouted ones stay single. */
export function byCause(cells: Cell[]): Cause[] {
	const out: Cause[] = [];
	for (const c of cells) {
		const id = c.instance ? `${c.state}|${c.instance}` : `${c.state}|${c.key}`;
		const g = out.find((x) => x.id === id);
		if (g) g.cells.push(c);
		else out.push({ id, state: c.state, instance: c.instance, why: c.why, cells: [c] });
	}
	for (const g of out)
		if (
			g.cells.length > 1 &&
			g.instance &&
			(g.state === 'untested' || g.state === 'red' || g.state === 'needs_ok')
		)
			g.why = `${g.cells.length} operations on ${g.instance} have no green contract test yet.`;
	return out;
}

/** The identity levels a job's own operation may require, as the app labels them (10 §3.1). */
export { IDENTITY_LEVELS, levelLabel } from '$lib/tenant/channelsT13';

/** The ops a tenant template offers that the job has not added yet. */
export function addableOwnOps<T extends { added: boolean }>(available: readonly T[]): T[] {
	return available.filter((a) => !a.added);
}

/** `key@version` + op → a stable row key for the available list. */
export const availableKey = (a: { template: string; version: string; op: string }): string =>
	`${a.template}@${a.version}:${a.op}`;
