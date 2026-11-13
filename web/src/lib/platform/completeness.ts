// KC completeness matrix (plan 2026-10-24-kc-layer-contract §5): layer × object type judged against the layer
// contract (§1). A required kind with no rows is an `empty` violation; on a job layer, any kind that belongs above job
// level is a `misplaced` violation. Ported from the plan's matrix.py (REQ / JOB_OK; archived);
// `base_dept/<archetype>` (D116, S1) takes the archetype contract. Pure: layers + rows in, matrix out.
import type { DepartmentArchetype, Layer, LayerRow, LayerScope } from '$lib/api/types/platform';
import { SCOPES, layerLabel } from './model';

/** The object types the matrix shows and judges, in column order, with their short header. */
export const MATRIX_COLUMNS = [
	{ kind: 'entity', short: 'ent' },
	{ kind: 'attribute', short: 'attr' },
	{ kind: 'relation', short: 'rel' },
	{ kind: 'object', short: 'obj' },
	{ kind: 'operation', short: 'op' },
	{ kind: 'gate_question', short: 'gate' },
	{ kind: 'activity', short: 'act' },
	{ kind: 'procedure', short: 'proc' },
	{ kind: 'operation_expectation', short: 'exp' },
	{ kind: 'connector_slot', short: 'slot' },
	{ kind: 'context_space', short: 'space' },
	{ kind: 'knowledge_store', short: 'store' },
	{ kind: 'instruction', short: 'instr' },
	{ kind: 'stage_knowledge_kind', short: 'kinds' },
	{ kind: 'knowledge_pack', short: 'packs' },
	{ kind: 'job_setting', short: 'set' },
] as const;

export type MatrixKind = (typeof MATRIX_COLUMNS)[number]['kind'];

/** The scopes the contract judges by "MUST contain". Profiles are not judged; jobs are judged by `JOB_ALLOWED`. */
export type ContractScope = Extract<LayerScope, 'base' | 'industry' | 'base_dept' | 'industry_dept'>;

/** §1 "MUST contain", per scope. Kinds outside `MATRIX_COLUMNS` are listed (the contract) but not judged, as in
 *  `matrix.py`. */
export const REQUIRED: Record<ContractScope, readonly string[]> = {
	base: [
		'entity',
		'attribute',
		'relation',
		'object',
		'operation',
		'error_state',
		'gate_question',
		'agent_role',
		'journey_theme',
		'journey_stage',
		'activity',
		'instruction',
		'stage_knowledge_kind',
		'knowledge_pack',
	],
	industry: [
		'entity',
		'attribute',
		'relation',
		'profile',
		'axis_rule',
		'journey_stage',
		'theme_stage_prior',
		'instruction',
		'stage_knowledge_kind',
		'knowledge_pack',
	],
	base_dept: [
		'activity',
		'procedure',
		'gate_question',
		'operation_expectation',
		'connector_slot',
		'context_space',
		'instruction',
		'stage_knowledge_kind',
		'job_setting',
	],
	industry_dept: [
		'activity',
		'procedure',
		'gate_question',
		'operation_expectation',
		'connector_slot',
		'context_space',
		'knowledge_store',
		'instruction',
		'stage_knowledge_kind',
		'knowledge_pack',
	],
};

/** §1 job row: the only kinds a job layer (and its channel sub-layers) may hold. */
export const JOB_ALLOWED: readonly string[] = [
	'job',
	'instruction',
	'job_setting',
	'narrowing',
	'connector_slot',
];

export type CellState = 'filled' | 'none' | 'empty' | 'misplaced';

export interface MatrixCell {
	kind: MatrixKind;
	count: number;
	/** The contract requires this kind on this layer. */
	required: boolean;
	/** Required-but-empty, or (job) a kind that belongs above job level. */
	violation: boolean;
	state: CellState;
}

export interface MatrixRow {
	/** Layer id, or `archetype:<key>` for a synthesised archetype row. */
	id: string;
	label: string;
	scope: LayerScope;
	/** The layer, or null for an archetype that has no `base_dept` layer yet (synthesised from the catalogue). */
	layer: Layer | null;
	cells: MatrixCell[];
	/** Required kinds with no rows (non-job layers). */
	empty: number;
	/** Kinds above job level (job layers). */
	misplaced: number;
	ok: boolean;
	verdict: string;
	/** Region sub-layers are optional add-ons (MAY, D111): shown, not judged. */
	judged: boolean;
}

const COUNTED = (r: LayerRow) => r.is_current && r.op !== 'tombstone';

function judgeRow(
	id: string,
	label: string,
	scope: LayerScope,
	layer: Layer | null,
	counts: Map<string, number>,
	judged: boolean,
): MatrixRow {
	const req = judged && scope !== 'job' && scope !== 'profile' ? REQUIRED[scope] : [];
	const cells = MATRIX_COLUMNS.map(({ kind }): MatrixCell => {
		const count = counts.get(kind) ?? 0;
		if (scope === 'job') {
			// §1 job column "channel dimension layers": a channel sub-layer carries its channel's own context space
			const channelSpace = kind === 'context_space' && layer?.dimension?.[0] === 'channel';
			const bad = judged && count > 0 && !JOB_ALLOWED.includes(kind) && !channelSpace;
			return {
				kind,
				count,
				required: false,
				violation: bad,
				state: bad ? 'misplaced' : count ? 'filled' : 'none',
			};
		}
		const required = req.includes(kind);
		const bad = required && count === 0;
		return { kind, count, required, violation: bad, state: bad ? 'empty' : count ? 'filled' : 'none' };
	});
	const misplaced = scope === 'job' ? cells.filter((c) => c.violation).length : 0;
	const empty = scope === 'job' ? 0 : cells.filter((c) => c.violation).length;
	const ok = misplaced === 0 && empty === 0;
	const verdict = !judged
		? 'not judged (region add-on)'
		: ok
			? 'ok'
			: scope === 'job'
				? `${misplaced} kind${misplaced === 1 ? '' : 's'} above job level`
				: `${empty} required kind${empty === 1 ? '' : 's'} empty`;
	return { id, label, scope, layer, cells, empty, misplaced, ok, verdict, judged };
}

/**
 * The completeness matrix: one row per non-profile layer, in compile order (scope, then label). Counts are current,
 * non-tombstone rows by kind. An archetype with no `base_dept/<key>` layer gets a synthesised row from the catalogue
 * (activities, knowledge kinds), as `matrix.py` does, so the archetype step's gaps show before the layer exists.
 */
export function completenessMatrix(
	layers: Layer[],
	rows: LayerRow[],
	archetypes: Pick<DepartmentArchetype, 'key' | 'activities' | 'knowledge_kinds'>[] = [],
): MatrixRow[] {
	const byLayer = new Map<string, Map<string, number>>();
	for (const r of rows) {
		if (!COUNTED(r)) continue;
		let m = byLayer.get(r.layer_id);
		if (!m) byLayer.set(r.layer_id, (m = new Map()));
		m.set(r.kind, (m.get(r.kind) ?? 0) + 1);
	}
	const out: MatrixRow[] = layers
		.filter((l) => l.scope !== 'profile')
		.map((l) =>
			judgeRow(
				l.id,
				layerLabel(l),
				l.scope,
				l,
				byLayer.get(l.id) ?? new Map(),
				// Region sub-layers are MAY (D111). Job dimension (channel) sub-layers keep the job rule.
				!l.region,
			),
		);
	for (const a of archetypes) {
		if (layers.some((l) => l.scope === 'base_dept' && l.department === a.key && !l.region)) continue;
		const counts = new Map<string, number>([
			['activity', (a.activities ?? []).length],
			['stage_knowledge_kind', (a.knowledge_kinds ?? []).length],
		]);
		out.push(judgeRow(`archetype:${a.key}`, `base_dept/${a.key}`, 'base_dept', null, counts, true));
	}
	const rank = (s: LayerScope) => SCOPES.indexOf(s);
	return out.sort((x, y) => rank(x.scope) - rank(y.scope) || x.label.localeCompare(y.label));
}

/** Matrix totals for the card header: layers judged, how many pass, violations by type. */
export function matrixTotals(m: MatrixRow[]): {
	judged: number;
	ok: number;
	empty: number;
	misplaced: number;
} {
	const judged = m.filter((r) => r.judged);
	return {
		judged: judged.length,
		ok: judged.filter((r) => r.ok).length,
		empty: judged.reduce((n, r) => n + r.empty, 0),
		misplaced: judged.reduce((n, r) => n + r.misplaced, 0),
	};
}
