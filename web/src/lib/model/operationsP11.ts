// P11 Operations catalogue view logic (pure, unit-tested in tests/unit/model-P11.test.ts).
// An operation is one verb on one object (`object.verb{variant}`, 11 §3). This file answers, per operation:
// what it does to the object's state machine, which runner checks it triggers (12 §3.3), which platform connector
// templates implement it (11 §5.3) and which agent roles of a job may call it (12 §2, N3).
import type { ConnectorTemplateSummary } from '$lib/api/types/connectors';
import type { AgentRoleView } from '$lib/api/types/ontology';
import type { Modality, ObjectDef, OperationRow, SideEffect } from '$lib/api/types/model';

export const MODALITIES: Modality[] = ['observe', 'do', 'request'];

export const MODALITY_HINT: Record<Modality, string> = {
	observe: 'reads state, changes nothing',
	do: 'the customer changes the record now',
	request: 'asks the provider; leaves a pending case',
};

const RANK: Record<SideEffect, number> = { R: 0, W: 1, X: 2, P: 3 };

/** The risk ladder: each side-effect class adds runner checks on top of the one before (12 §3.3, D27). */
export const RISK_LADDER: { effect: SideEffect; label: string; adds: string }[] = [
	{ effect: 'R', label: 'Read', adds: 'Identity level only' },
	{ effect: 'W', label: 'Reversible write', adds: '+ customer confirms back' },
	{ effect: 'X', label: 'Irreversible', adds: '+ verify read after the write' },
	{ effect: 'P', label: 'Payment', adds: '+ hosted payment link only' },
];

/** One matrix cell entry: an `object.verb` with the variants that passed the filter. */
export interface VerbGroup {
	/** `object.verb` */
	base: string;
	/** The `*` row when it passed the filter, else the first variant. */
	head: OperationRow;
	variants: OperationRow[];
	/** Highest side effect inside the group (the colour of the cell). */
	effect: SideEffect;
}

export interface MatrixRow {
	object: string;
	cells: Record<Modality, VerbGroup[]>;
	count: number;
}

/** Groups operations into object × modality cells, keeping catalogue order; variants sit with their verb. */
export function operationMatrix(ops: OperationRow[], objectOrder: string[] = []): MatrixRow[] {
	const rows = new Map<string, MatrixRow>();
	const groups = new Map<string, VerbGroup>();
	for (const o of ops) {
		let row = rows.get(o.object);
		if (!row) {
			row = { object: o.object, cells: { observe: [], do: [], request: [] }, count: 0 };
			rows.set(o.object, row);
		}
		row.count++;
		const base = `${o.object}.${o.verb}`;
		let g = groups.get(base);
		if (!g) {
			g = { base, head: o, variants: [], effect: o.side_effect };
			groups.set(base, g);
			row.cells[o.modality].push(g);
		}
		if (o.variant === '*') g.head = o;
		else g.variants.push(o);
		if (RANK[o.side_effect] > RANK[g.effect]) g.effect = o.side_effect;
	}
	const pos = (k: string) => {
		const i = objectOrder.indexOf(k);
		return i < 0 ? Number.MAX_SAFE_INTEGER : i;
	};
	return [...rows.values()].sort((a, b) => pos(a.object) - pos(b.object));
}

export const countByEffect = (ops: OperationRow[]): Record<SideEffect, number> => {
	const out: Record<SideEffect, number> = { R: 0, W: 0, X: 0, P: 0 };
	for (const o of ops) out[o.side_effect]++;
	return out;
};

// ---------------------------------------------------------------------------------------------------------------
// State transition

export type TransitionKind = 'read' | 'create' | 'transition' | 'data';

export interface TransitionView {
	kind: TransitionKind;
	/** One per `object_transition` row of this verb. */
	moves: { from: string; to: string; guard: string | null }[];
	/** For a create: the state a new record starts in. */
	initial: string | null;
	/** States the verb cannot run from (`invalid_state:<s>`). */
	blocked_from: string[];
}

const CREATE_VERBS = new Set(['create', 'place', 'book', 'add']);

/** What the operation does to its object's state machine (11 §3.1–§3.2). */
export function transitionOf(
	def: Pick<ObjectDef, 'states' | 'transitions'> | null,
	op: OperationRow,
): TransitionView {
	const full = `${op.object}.${op.verb}`;
	const moves = (def?.transitions ?? [])
		.filter((t) => t.verb === full)
		.map((t) => ({ from: t.from_status, to: t.to_status, guard: t.guard_rule }));
	const allowed = new Set(moves.map((m) => m.from));
	const blocked = moves.length ? (def?.states ?? []).map((s) => s.status).filter((s) => !allowed.has(s)) : [];
	if (moves.length) return { kind: 'transition', moves, initial: null, blocked_from: blocked };
	if (op.modality === 'observe') return { kind: 'read', moves: [], initial: null, blocked_from: [] };
	if (CREATE_VERBS.has(op.verb))
		return {
			kind: 'create',
			moves: [],
			initial: def?.states.find((s) => s.initial)?.status ?? null,
			blocked_from: [],
		};
	return { kind: 'data', moves: [], initial: null, blocked_from: [] };
}

// ---------------------------------------------------------------------------------------------------------------
// Runner checks (12 §3.3 steps 3–5, 8b, 10)

export interface RunnerCheck {
	key: string;
	label: string;
	applies: boolean;
	detail: string;
	/** The error the runner returns when the check fails. */
	error: string | null;
}

export function runnerChecks(op: OperationRow): RunnerCheck[] {
	const write = op.side_effect !== 'R';
	return [
		{
			key: 'identity',
			label: `Identity level ≥ ${op.identity_level}`,
			applies: op.identity_level !== '0',
			detail:
				op.identity_level === '0'
					? 'Public: anyone may call it.'
					: op.identity_level === 'S'
						? 'Staff only.'
						: 'The case must be at this level (a step-up raises it).',
			error: op.identity_level === '0' ? null : 'identity_required',
		},
		{
			key: 'confirm',
			label: 'Customer confirms back',
			applies: write,
			detail: write
				? 'A customer message confirming this exact call must precede it.'
				: 'Reads need no confirmation.',
			error: write ? 'confirmation_required' : null,
		},
		{
			key: 'hosted',
			label: 'Hosted payment link only',
			applies: op.side_effect === 'P',
			detail:
				op.side_effect === 'P'
					? 'Returns a PSP-hosted link; card data never passes through the conversation (D27).'
					: 'Not a payment.',
			error: op.side_effect === 'P' ? 'not_entitled' : null,
		},
		{
			key: 'idempotency',
			label: `Idempotency ${op.idempotency}`,
			applies: op.idempotency !== 'nat',
			detail:
				op.idempotency === 'nat'
					? 'Naturally idempotent: safe to repeat.'
					: op.idempotency === 'key+dup'
						? 'Keyed call, plus a duplicate check on the record itself.'
						: 'Keyed call: a retry returns the stored outcome.',
			error: op.idempotency === 'key+dup' ? 'duplicate' : null,
		},
		{
			key: 'verify',
			label: op.verify_read ? `Verify with ${op.verify_read}` : 'Verify read',
			applies: !!op.verify_read,
			detail: op.verify_read
				? 'Read back after the write; no answer → unknown_outcome, never retried automatically.'
				: 'No write-then-verify.',
			error: op.verify_read ? 'unknown_outcome' : null,
		},
	];
}

// ---------------------------------------------------------------------------------------------------------------
// Implementers (connector templates) and callers (agent roles)

export interface Implementer {
	key: string;
	label: string;
	versions: string[];
	/** Latest implementing version (link target). */
	latest: string;
	/** Implements this exact variant; false = covered by the template's `*` row. */
	exact: boolean;
	platform: boolean;
}

/** Template versions are `3`, `2026-10.1`, …: compare digit runs numerically. */
const vcmp = (a: string, b: string) => a.localeCompare(b, 'en', { numeric: true });

export function implementersOf(templates: ConnectorTemplateSummary[], op: OperationRow): Implementer[] {
	const by = new Map<string, Implementer>();
	for (const t of templates) {
		const hits = t.implements.filter(
			(i) => i.object === op.object && i.verb === op.verb && (i.variant === op.variant || i.variant === '*'),
		);
		if (!hits.length) continue;
		const exact = hits.some((i) => i.variant === op.variant);
		const cur = by.get(t.key);
		if (!cur) {
			by.set(t.key, {
				key: t.key,
				label: t.label,
				versions: [t.version],
				latest: t.version,
				exact,
				platform: t.platform_connector,
			});
		} else {
			cur.versions.push(t.version);
			if (vcmp(t.version, cur.latest) > 0) cur.latest = t.version;
			cur.exact ||= exact;
		}
	}
	return [...by.values()]
		.map((i) => ({ ...i, versions: i.versions.sort(vcmp) }))
		.sort((a, b) => Number(a.platform) - Number(b.platform) || a.label.localeCompare(b.label));
}

export type CallerStatus = 'may' | 'no' | 'disabled';

export interface Caller {
	role: string;
	label: string;
	used_by: string;
	status: CallerStatus;
	/** Why (or why not), in one line. */
	why: string;
	/** Procedure role: the activities whose operations include this one. */
	activities: string[];
}

function stepCovers(step: string, op: OperationRow): boolean {
	const base = `${op.object}.${op.verb}`;
	if (step === op.key) return true;
	// An activity that uses a variant makes the unqualified verb callable for that variant.
	return op.variant === '*' && step.startsWith(`${base}{`);
}

export function callersOf(roles: AgentRoleView[], op: OperationRow): Caller[] {
	return roles.map((r) => {
		const base = { role: r.role, label: r.label, used_by: r.used_by, activities: [] as string[] };
		if (!r.enabled) return { ...base, status: 'disabled' as const, why: 'Disabled in v1.' };
		if (RANK[op.side_effect] > RANK[r.max_side_effect])
			return {
				...base,
				status: 'no' as const,
				why: `Side effect ${op.side_effect} exceeds the role’s max ${r.max_side_effect}.`,
			};
		if (r.grants.some((g) => g.derive_from_activity)) {
			const acts = r.derived.filter((d) => d.ops.some((s) => stepCovers(s, op))).map((d) => d.label);
			return acts.length
				? {
						...base,
						status: 'may' as const,
						why: `Derived: used by ${acts.length} activit${acts.length === 1 ? 'y' : 'ies'}.`,
						activities: acts,
					}
				: { ...base, status: 'no' as const, why: 'No activity of this job uses it.' };
		}
		const hit = r.grants.find(
			(g) =>
				(g.object === '*' || g.object === op.object) &&
				(g.verb === '*' || g.verb === op.verb) &&
				(g.variant === null || g.variant === op.variant) &&
				g.modalities.includes(op.modality),
		);
		return hit
			? { ...base, status: 'may' as const, why: 'Explicit grant.' }
			: { ...base, status: 'no' as const, why: 'Not granted.' };
	});
}
