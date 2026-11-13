// P5 Compile preview — pure views over an EffectiveConfig: the layer stack that built it, where later layers
// overrode earlier ones, and the difference against a reference compile. No world access; unit tested directly.
import type { CompileTarget, EffectiveConfig, Items, Layer, Origin, RowOp } from '$lib/api/types/platform';
import {
	departmentKey,
	emptySelection,
	parseDepartment,
	withSelectionValues,
	type Selection,
} from '$lib/api/types/core';
import { layerLabel } from './model';

/** The seven effective object kinds P5 shows, in display order, with their provenance key prefix. */
export const COMPILE_KINDS = [
	['entity', 'Entities', 'Entity'],
	['attribute', 'Attributes', 'Attribute'],
	['relation', 'Relations', 'Relation'],
	['context_space', 'Context spaces', 'Context space'],
	['procedure', 'Procedures', 'Procedure'],
	['connector_slot', 'Connector slots', 'Connector slot'],
	['knowledge_store', 'Knowledge stores', 'Knowledge store'],
] as const;
export type CompileKind = (typeof COMPILE_KINDS)[number][0];

export function kindLabel(kind: string, plural = false): string {
	const k = COMPILE_KINDS.find(([id]) => id === kind);
	return k ? (plural ? k[1] : k[2]) : kind;
}

export function itemsOf(c: EffectiveConfig, kind: CompileKind): Items {
	switch (kind) {
		case 'entity':
			return c.entities;
		case 'attribute':
			return c.attributes;
		case 'relation':
			return c.relations;
		case 'context_space':
			return c.job_content.context_spaces;
		case 'procedure':
			return c.job_content.procedures;
		case 'connector_slot':
			return c.job_content.connectors;
		case 'knowledge_store':
			return c.job_content.knowledge_stores;
	}
}

export function countsOf(c: EffectiveConfig): Record<CompileKind, number> {
	return Object.fromEntries(COMPILE_KINDS.map(([k]) => [k, Object.keys(itemsOf(c, k)).length])) as Record<
		CompileKind,
		number
	>;
}

/** One layer of the compiled stack, with what its rows did to the result. */
export interface StackLayer {
	label: string;
	scope: Origin['layer']['scope'];
	tag: string | null;
	ops: Record<RowOp, number>;
	/** First row the layer contributed (a deep link into the layer explorer). */
	first_row: string | null;
	/** Why this layer is in the stack (the axis rule that fired, the dimension value chosen…). */
	because: string;
}

function because(l: Origin['layer']): string {
	switch (l.scope) {
		case 'base':
			return 'always first · shared objects + department archetypes';
		case 'profile':
			return `axis rule ${l.rule} fired`;
		case 'industry':
			return `industry = ${l.industry}`;
		case 'base_dept':
			// D116: the cross-industry department a selected department extends.
			return `base department = ${l.department}`;
		case 'industry_dept':
			// D60: the department layer specialises a base department archetype.
			return l.extends
				? `department = ${l.department} · extends archetype ${l.extends}`
				: `department = ${l.department}`;
		case 'job':
			return l.dimension ? `${l.dimension[0]} = ${l.dimension[1]}` : `job = ${l.job}`;
	}
}

/** The applied layers in compile order (c.layers), each with its add/patch/tombstone counts. */
export function stackOf(c: EffectiveConfig): StackLayer[] {
	const by = new Map<string, StackLayer>();
	for (const origins of Object.values(c.provenance))
		for (const o of origins) {
			const label = layerLabel(o.layer);
			let s = by.get(label);
			if (!s) {
				s = {
					label,
					scope: o.layer.scope,
					tag: o.layer.tag,
					ops: { add: 0, patch: 0, tombstone: 0 },
					first_row: o.row_id,
					because: because(o.layer),
				};
				by.set(label, s);
			}
			s.ops[o.op]++;
			if (s.first_row === null || o.row_id < s.first_row) s.first_row = o.row_id;
		}
	return c.layers.map(
		(label) =>
			by.get(label) ?? {
				label,
				scope: label.split('/')[0] as StackLayer['scope'],
				tag: null,
				ops: { add: 0, patch: 0, tombstone: 0 },
				first_row: null,
				because: 'selected (no rows yet)',
			},
	);
}

export type OverrideOutcome = 'patched' | 'removed' | 'cascade';

/** An item a later layer changed or removed: the decision points of a compile. */
export interface Override {
	kind: string;
	key: string;
	outcome: OverrideOutcome;
	origins: Origin[];
	/** For a cascade: the entity whose tombstone took this item down. */
	by?: string;
}

/**
 * Items more than one layer touched (a patch on an earlier add, a tombstone), plus attributes/relations an
 * entity tombstone took with it. Job-definition rows are left out: the job card shows those.
 */
export function overridesOf(c: EffectiveConfig): Override[] {
	const out: Override[] = [];
	for (const [id, origins] of Object.entries(c.provenance)) {
		const [kind, ...rest] = id.split(':');
		const key = rest.join(':');
		if (kind === 'job') continue;
		const last = origins[origins.length - 1];
		if (!last) continue;
		if (last.op === 'tombstone') out.push({ kind: kind!, key, outcome: 'removed', origins });
		else if (origins.length > 1) out.push({ kind: kind!, key, outcome: 'patched', origins });
	}
	for (const w of c.warnings)
		if (w.code === 'tombstone_cascade') {
			const gone = w.message.replace(/^.*also removed /, '').replace(/\.$/, '');
			for (const key of gone.split(', ').filter(Boolean))
				out.push({
					kind: key.includes('|') ? 'relation' : 'attribute',
					key,
					outcome: 'cascade',
					origins: c.provenance[`${key.includes('|') ? 'relation' : 'attribute'}:${key}`] ?? [],
					by: w.key,
				});
		}
	const rank: Record<OverrideOutcome, number> = { removed: 0, cascade: 1, patched: 2 };
	return out.sort(
		(a, b) =>
			rank[a.outcome] - rank[b.outcome] ||
			(a.by ?? a.key).localeCompare(b.by ?? b.key) ||
			a.key.localeCompare(b.key),
	);
}

/** `synonyms: purchase, ord_no` — a row's fields in one line (reasons and labels first). */
export function fieldsLine(fields: Record<string, unknown> | undefined): string {
	if (!fields) return '';
	return Object.entries(fields)
		.map(
			([k, v]) =>
				`${k}: ${Array.isArray(v) ? v.join(', ') : typeof v === 'object' ? JSON.stringify(v) : String(v)}`,
		)
		.join(' · ');
}

export interface DiffEntry {
	kind: CompileKind;
	key: string;
	change: 'added' | 'removed' | 'changed';
	/** Changed: the field names whose value differs. */
	fields?: string[];
}

export interface CompileDiff {
	entries: DiffEntry[];
	added: number;
	removed: number;
	changed: number;
	layers_added: string[];
	layers_removed: string[];
	same: boolean;
}

function same(a: unknown, b: unknown): boolean {
	return JSON.stringify(a) === JSON.stringify(b);
}

/** What `next` has that `ref` does not, and the reverse, per kind (hash equality short-circuits to "same"). */
export function diffConfigs(ref: EffectiveConfig, next: EffectiveConfig): CompileDiff {
	const entries: DiffEntry[] = [];
	for (const [kind] of COMPILE_KINDS) {
		const a = itemsOf(ref, kind);
		const b = itemsOf(next, kind);
		for (const key of Object.keys(b).sort())
			if (!(key in a)) entries.push({ kind, key, change: 'added' });
			else if (!same(a[key], b[key]))
				entries.push({
					kind,
					key,
					change: 'changed',
					fields: [...new Set([...Object.keys(a[key]!), ...Object.keys(b[key]!)])]
						.filter((f) => !same(a[key]![f], b[key]![f]))
						.sort(),
				});
		for (const key of Object.keys(a).sort()) if (!(key in b)) entries.push({ kind, key, change: 'removed' });
	}
	const layers_added = next.layers.filter((l) => !ref.layers.includes(l));
	const layers_removed = ref.layers.filter((l) => !next.layers.includes(l));
	const n = (c: DiffEntry['change']) => entries.filter((e) => e.change === c).length;
	return {
		entries,
		added: n('added'),
		removed: n('removed'),
		changed: n('changed'),
		layers_added,
		layers_removed,
		same: ref.hash === next.hash || (!entries.length && !layers_added.length && !layers_removed.length),
	};
}

// ---- deep links (P12 Business models and P4 Layers preselect a compile) -----------------------------------------

/** Query params P5 reads besides dimension keys. A dimension value travels as `?<dimension key>=<v1,v2>`. */
const SELECTION_PARAMS = ['industry', 'department', 'areas', 'job', 'profile'] as const;

const list = (v: string | undefined) =>
	(v ?? '')
		.split(',')
		.map((a) => a.trim())
		.filter(Boolean);

/**
 * The compile a `/platform/compile?…` link asks for, or `null` when the link carries none (the page then opens on
 * its default). `department` (department layer keys `industry_dept/<i>/<d>`, or bare keys of `industry`), `areas` and dimension values are comma-separated; dimension params
 * are read only for the known `dimensionKeys`.
 */
export function selectionFromQuery(
	q: Pick<URLSearchParams, 'get'>,
	dimensionKeys: string[],
): CompileTarget | null {
	const get = (k: string) => q.get(k)?.trim() || undefined;
	let selection: Selection = {
		...emptySelection(),
		profile: get('profile') ?? null,
		industry: get('industry') ?? '',
		areas: list(get('areas')),
	};
	// `department` names department layer keys; a bare department key is read inside `industry`.
	selection.departments = list(get('department')).map((d) =>
		parseDepartment(d) || !selection.industry ? d : departmentKey(selection.industry, d),
	);
	let dims = false;
	for (const k of dimensionKeys) {
		const vals = list(get(k));
		if (vals.length) {
			dims = true;
			selection = withSelectionValues(selection, k, vals);
		}
	}
	const any = SELECTION_PARAMS.some((k) => get(k)) || dims;
	return any ? { selection, job: get('job') ?? null } : null;
}

/** `/platform/compile?…` preselecting `t` (empty parts left out; no selection → the plain page). */
export function compileHref(t: CompileTarget): string {
	const { selection: sel, job } = t;
	const q = new URLSearchParams();
	if (sel.industry) q.set('industry', sel.industry);
	// a department of the selection's industry is written by its bare key (`department=customer_service`)
	const short = (d: string) => {
		const p = parseDepartment(d);
		return p && p[0] === sel.industry ? p[1] : d;
	};
	if (sel.departments.length) q.set('department', sel.departments.map(short).join(','));
	if (sel.areas.length) q.set('areas', sel.areas.join(','));
	if (job) q.set('job', job);
	if (sel.profile) q.set('profile', sel.profile);
	for (const [k, v] of Object.entries(sel.dimensions)) if (v.length) q.set(k, v.join(','));
	if (sel.region) q.set('region', sel.region);
	const s = q.toString();
	return s ? `/platform/compile?${s}` : '/platform/compile';
}

/**
 * The compile that pulls `l` in: an industry layer needs its industry, a department layer its industry +
 * department, a job layer its job (+ the dimension value of a variant). Base is in every compile and a profile layer
 * is pulled in by a business model, not a selection, so both give an empty selection. A job's rows patch its
 * department's objects, so a job alone may not compile: `known` compiles (the compile references) lend the first one
 * with the same job as context (industry, departments, areas, profile), with the variant on top.
 */
export function selectionOfLayer(
	l: Pick<Layer, 'scope' | 'industry' | 'department' | 'job' | 'dimension'>,
	known: CompileTarget[] = [],
): CompileTarget {
	const empty = emptySelection();
	switch (l.scope) {
		case 'industry':
			return { selection: { ...empty, industry: l.industry ?? '' }, job: null };
		case 'industry_dept':
			return {
				selection: {
					...empty,
					industry: l.industry ?? '',
					departments: l.industry && l.department ? [departmentKey(l.industry, l.department)] : [],
				},
				job: null,
			};
		case 'job': {
			const ctx = known.find((k) => k.job === l.job)?.selection;
			const base: Selection = ctx
				? {
						...empty,
						profile: ctx.profile,
						industry: ctx.industry,
						departments: [...ctx.departments],
						areas: [...ctx.areas],
					}
				: empty;
			return {
				selection: l.dimension ? withSelectionValues(base, l.dimension[0], [l.dimension[1]]) : base,
				job: l.job ?? null,
			};
		}
		default:
			return { selection: empty, job: null };
	}
}
