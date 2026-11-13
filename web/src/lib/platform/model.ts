// Pure model logic for the platform pages: axis-rule evaluation, layer labels, tombstone cascades and the compile
// of the effective model from overlay rows (the configurator's compile, re-cut: no tenant scope, every error
// reported, never first-fail). No world access here — the fixture client passes rows in; unit tested directly.
import type {
	AxisCondition,
	CompileError,
	CompileTarget,
	EffectiveConfig,
	Items,
	Layer,
	LayerRow,
	Origin,
	RowKind,
} from '$lib/api/types/platform';
import { areaOfContextSpace } from '$lib/context-space-key';
import {
	isBaseOnlyPreview,
	parseDepartment,
	selectionIssues,
	selectionValues,
	type Selection,
} from '$lib/api/types/core';

/** Verdict rows older than this make the edge refuse turns (K35). */
export const VERDICT_MAX_AGE_S = 300;

export const SCOPES = ['base', 'profile', 'industry', 'base_dept', 'industry_dept', 'job'] as const;

/** `industry_dept/retail/customer_service`, `profile/metered_supply`, `job/customer_care_agent/channel=email`,
 *  D111 region sub-layers `base/region/nl`, `industry/retail/region/nl` (Rust `LayerKey::label`). */
export function layerLabel(
	l: Pick<Layer, 'scope' | 'industry' | 'department' | 'job' | 'rule' | 'dimension' | 'region'>,
): string {
	return [
		l.scope,
		l.industry,
		l.department,
		l.region && `region/${l.region}`,
		l.job,
		l.rule,
		l.dimension?.join('='),
	]
		.filter(Boolean)
		.join('/');
}

/** `axis = v` also holds for descendants of v (`provider.implicit` satisfies `provider`). */
function holds(selected: string[] | undefined, value: string): boolean {
	return !!selected?.some((s) => s === value || s.startsWith(`${value}.`));
}

/** Evaluates an axis-rule condition against a profile selection. */
export function evaluate(c: AxisCondition, sel: Record<string, string[]>): boolean {
	if ('eq' in c) return holds(sel[c.eq[0]], c.eq[1]);
	if ('in' in c) return c.in[1].some((v) => holds(sel[c.in[0]], v));
	if ('and' in c) return c.and.every((x) => evaluate(x, sel));
	if ('or' in c) return c.or.some((x) => evaluate(x, sel));
	return !evaluate(c.not, sel);
}

/** Human form of a condition: `order_type = continuous ∧ payment_type = balancing`. */
export function describe(c: AxisCondition): string {
	if ('eq' in c) return `${c.eq[0]} = ${c.eq[1]}`;
	if ('in' in c) return `${c.in[0]} ∈ {${c.in[1].join(', ')}}`;
	if ('and' in c) return c.and.map(describe).join(' ∧ ');
	if ('or' in c) return `(${c.or.map(describe).join(' ∨ ')})`;
	return `¬(${describe(c.not)})`;
}

/** Current rows elsewhere that a tombstoned entity takes down with it (its attributes and relations). */
export function cascadeOf(row: Pick<LayerRow, 'kind' | 'key' | 'op'>, current: LayerRow[]): LayerRow[] {
	if (row.op !== 'tombstone' || row.kind !== 'entity') return [];
	return current.filter(
		(r) =>
			r.is_current &&
			r.op !== 'tombstone' &&
			((r.kind === 'attribute' && r.key.startsWith(`${row.key}.`)) ||
				(r.kind === 'relation' && r.key.split('|').some((part, i) => i !== 1 && part === row.key))),
	);
}

/** FNV-1a over the canonical JSON, twice with different seeds: a short stable "bundle hash". */
export function contentHash(v: unknown): string {
	const s = canon(v);
	const fnv = (seed: number) => {
		let h = seed >>> 0;
		for (let i = 0; i < s.length; i++) {
			h ^= s.charCodeAt(i);
			h = Math.imul(h, 0x01000193) >>> 0;
		}
		return h.toString(16).padStart(8, '0');
	};
	return `${fnv(0x811c9dc5)}${fnv(0x01000193).slice(0, 4)}`;
}

function canon(v: unknown): string {
	if (v === null || typeof v !== 'object') return JSON.stringify(v) ?? 'null';
	if (Array.isArray(v)) return `[${v.map(canon).join(',')}]`;
	const o = v as Record<string, unknown>;
	return `{${Object.keys(o)
		.sort()
		.map((k) => `${JSON.stringify(k)}:${canon(o[k])}`)
		.join(',')}}`;
}

export interface CompileInput extends CompileTarget {
	layers: Layer[];
	rows: LayerRow[];
	/** The profile's axis values (null = no profile). */
	profile: Record<string, string[]> | null;
	/** Axis rules, any order; fired ones are applied by priority. */
	rules: { key: string; priority: number; condition: AxisCondition; layer_id: string }[];
	/** industry → department keys that exist. */
	departments: { industry: string; key: string }[];
	/** The platform compile page's preview (Rust `Mode::Preview`): only it compiles the base-only selection. */
	preview?: boolean;
}

/** D106/D115: the selection's hierarchy issues as compile errors (`key` = the selection path); [] only for the
 *  base-only selection when `preview` (the platform compile page — Rust `Mode::Preview`). A non-empty list is
 *  refused before any layer is chosen (Rust `selection_refusal`). */
export function selectionRefusal(sel: Selection, job: string | null, preview = false): CompileError[] {
	if (preview && isBaseOnlyPreview(sel, job)) return [];
	return selectionIssues(sel).map((i) => ({ code: i.code, key: i.path, row_id: null, message: i.message }));
}

export type CompileOutcome = { ok: true; config: EffectiveConfig } | { ok: false; errors: CompileError[] };

type Bucket =
	| 'entities'
	| 'attributes'
	| 'relations'
	| 'jobs'
	| 'context_spaces'
	| 'procedures'
	| 'connectors'
	| 'knowledge_stores';
const BUCKET: Record<RowKind, Bucket> = {
	entity: 'entities',
	attribute: 'attributes',
	relation: 'relations',
	job: 'jobs',
	context_space: 'context_spaces',
	procedure: 'procedures',
	connector_slot: 'connectors',
	knowledge_store: 'knowledge_stores',
};

/**
 * The effective model for a selection: base → fired profile rules → industry → per department (its D116
 * `base_dept/<extends>` layer once, at the first department extending it, + that layer's region sub-layer when the
 * selection names the region → industry_dept) → job (+ the job's dimension layers). `add` sets, `patch` merges, `tombstone` removes (an entity takes its attributes and relations).
 * Then validates the whole result and returns every error at once.
 */
export function compileModel(input: CompileInput): CompileOutcome {
	const { layers, rows, selection: sel, job: jobKey } = input;
	const errors: CompileError[] = [];
	const warnings: EffectiveConfig['warnings'] = [];
	const err = (code: string, key: string, row_id: string | null, message: string) =>
		errors.push({ code, key, row_id, message });

	const refused = selectionRefusal(sel, jobKey, !!input.preview);
	if (refused.length) return { ok: false, errors: refused };
	sel.departments.forEach((dept, n) => {
		const [industry, name] = parseDepartment(dept)!;
		if (!input.departments.some((d) => d.industry === industry && d.key === name))
			err(
				'invalid_selection',
				`selection.departments[${n}]`,
				null,
				`${industry} has no department “${name}”.`,
			);
	});

	const fired = input.profile
		? input.rules.filter((r) => evaluate(r.condition, input.profile!)).sort((a, b) => a.priority - b.priority)
		: [];
	const byId = new Map(layers.map((l) => [l.id, l]));
	const chosen: Layer[] = [];
	const push = (l: Layer | undefined) => l && chosen.push(l);
	push(layers.find((l) => l.scope === 'base'));
	for (const r of fired) push(byId.get(r.layer_id));
	if (sel.industry) push(layers.find((l) => l.scope === 'industry' && l.industry === sel.industry));
	// D115: the departments in their declared order (primary first). D116 (Rust `fold_order`, the archetype
	// position): before a department, the cross-industry `base_dept/<extends>` layer it specialises — once, at the
	// first department extending it — then that layer's region sub-layer when the selection names the region.
	const foldedArchetypes = new Set<string>();
	if (sel.industry)
		for (const dept of sel.departments) {
			const d = layers.find(
				(l) => l.scope === 'industry_dept' && l.industry === sel.industry && layerLabel(l) === dept,
			);
			const a = d?.extends;
			if (a && !foldedArchetypes.has(a)) {
				foldedArchetypes.add(a);
				push(layers.find((l) => l.scope === 'base_dept' && l.department === a && !l.region));
				if (sel.region)
					push(layers.find((l) => l.scope === 'base_dept' && l.department === a && l.region === sel.region));
			}
			push(d);
		}
	if (jobKey) {
		push(layers.find((l) => l.scope === 'job' && l.job === jobKey && !l.dimension));
		for (const l of layers)
			if (
				l.scope === 'job' &&
				l.job === jobKey &&
				l.dimension &&
				selectionValues(sel, l.dimension[0]).includes(l.dimension[1])
			)
				push(l);
	}

	const out: Record<Bucket, Items> = {
		entities: {},
		attributes: {},
		relations: {},
		jobs: {},
		context_spaces: {},
		procedures: {},
		connectors: {},
		knowledge_stores: {},
	};
	const provenance: Record<string, Origin[]> = {};
	const lastRow = new Map<string, string>();
	const pk = (kind: RowKind, key: string) => `${kind === 'connector_slot' ? 'connector_slot' : kind}:${key}`;

	for (const layer of chosen) {
		// A layer applies as a set: its adds and patches first, then its tombstones (which cascade).
		const own = rows.filter((r) => r.layer_id === layer.id && r.is_current);
		const ordered = [...own.filter((r) => r.op !== 'tombstone'), ...own.filter((r) => r.op === 'tombstone')];
		for (const row of ordered) {
			const bucket = out[BUCKET[row.kind]];
			const id = pk(row.kind, row.key);
			(provenance[id] ??= []).push({ row_id: row.id, layer, op: row.op, fields: row.fields });
			lastRow.set(id, row.id);
			if (row.op === 'add') bucket[row.key] = { ...row.fields };
			else if (row.op === 'patch') {
				if (!bucket[row.key])
					err(
						'patch_target_missing',
						row.key,
						row.id,
						`${layerLabel(layer)} patches ${row.kind} ${row.key}, which no earlier layer adds.`,
					);
				else bucket[row.key] = { ...bucket[row.key], ...row.fields };
			} else {
				delete bucket[row.key];
				if (row.kind === 'entity') {
					const gone = [
						...Object.keys(out.attributes).filter((k) => k.startsWith(`${row.key}.`)),
						...Object.keys(out.relations).filter((k) =>
							k.split('|').some((p, i) => i !== 1 && p === row.key),
						),
					];
					for (const k of gone) {
						if (k.includes('|')) delete out.relations[k];
						else delete out.attributes[k];
					}
					if (gone.length)
						warnings.push({
							code: 'tombstone_cascade',
							key: row.key,
							message: `Removing ${row.key} also removed ${gone.join(', ')}.`,
						});
				}
			}
		}
	}

	// Validation: report everything.
	for (const key of Object.keys(out.attributes)) {
		const parent = key.split('.')[0]!;
		if (!out.entities[parent])
			err(
				'parent_missing',
				key,
				lastRow.get(pk('attribute', key)) ?? null,
				`Attribute ${key} belongs to ${parent}, which this selection does not have.`,
			);
	}
	for (const key of Object.keys(out.relations)) {
		const [from, , to] = key.split('|');
		for (const end of [from, to])
			if (end && !out.entities[end])
				err(
					'dangling_relation',
					key,
					lastRow.get(pk('relation', key)) ?? null,
					`Relation ${key} points at ${end}, which this selection does not have.`,
				);
	}
	if (jobKey && !out.jobs[jobKey])
		err('unknown_job', jobKey, null, `No base layer defines the job ${jobKey}.`);
	const job = jobKey ? (out.jobs[jobKey] ?? null) : null;
	if (jobKey)
		for (const [key, p] of Object.entries(out.procedures))
			for (const s of (p.uses_slots as string[] | undefined) ?? [])
				if (!out.connectors[s])
					err(
						'unknown_connector_slot',
						key,
						lastRow.get(pk('procedure', key)) ?? null,
						`Procedure ${key} uses connector slot “${s}”, which the job does not declare.`,
					);

	if (errors.length) return { ok: false, errors };

	const areas = sel.areas;
	const spaces = Object.fromEntries(
		Object.entries(out.context_spaces).filter(
			([k]) => !areas.length || areas.includes(areaOfContextSpace(k) ?? ''),
		),
	);
	for (const [k, e] of Object.entries(out.entities))
		if (!Object.keys(out.attributes).some((a) => a.startsWith(`${k}.`)))
			warnings.push({
				code: 'entity_without_attributes',
				key: k,
				message: `${e.label ?? k} has no attributes yet.`,
			});
	if (input.preview && isBaseOnlyPreview(sel, jobKey))
		warnings.push({
			code: 'base_only_preview',
			key: '',
			message: 'Base model only: a preview, not a job (a job needs an industry and a department).',
		});

	const config: Omit<EffectiveConfig, 'hash'> = {
		entities: out.entities,
		attributes: out.attributes,
		relations: out.relations,
		job: job && jobKey ? { key: jobKey, ...job } : null,
		job_content: jobKey
			? {
					context_spaces: spaces,
					procedures: out.procedures,
					connectors: out.connectors,
					knowledge_stores: out.knowledge_stores,
				}
			: { context_spaces: spaces, procedures: {}, connectors: {}, knowledge_stores: out.knowledge_stores },
		provenance,
		warnings,
		layers: chosen.map(layerLabel),
		rules_fired: fired.map((r) => r.key),
	};
	return {
		ok: true,
		config: { ...config, hash: contentHash({ ...config, provenance: undefined, warnings: undefined }) },
	};
}
