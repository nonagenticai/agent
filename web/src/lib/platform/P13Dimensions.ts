// P13 Dimensions — pure helpers: what a dimension does in a compile, whether a value is in use, why it must stay,
// and how one job selection's values turn into the layers a cut stacks (05 §2.1, D8).
// D59: a dimension "selects or parametrises content in the layer stack" — channel is the real one; region is a
// placeholder (recorded, selects nothing). D60: areas group context spaces inside a department, and a department
// (the industry_dept layer) specialises a base department archetype.
import type {
	DepartmentArchetype,
	Dimension,
	DimensionSelectionRow,
	DimensionUsage,
	DimensionValueUsage,
} from '$lib/api/types/platform';
import { parseDepartment } from '$lib/api/types/core';

/** How a dimension takes part in a compile. */
export type DimensionRole = 'narrows' | 'qualifies' | 'recorded';

export const ROLE_TEXT: Record<DimensionRole, { title: string; text: string }> = {
	narrows: {
		title: 'Groups context spaces',
		text: 'Areas are groupings of context spaces inside a department (D60); a selection keeps only the chosen areas’ context spaces.',
	},
	qualifies: {
		title: 'Switches job layers on',
		text: 'A job layer can be qualified by a value (job × channel=email); it compiles only when the selection names that value.',
	},
	recorded: {
		title: 'Placeholder · recorded in the release',
		text: 'Selects nothing (D59). The values are frozen into every release; data residency is a tenant setting and languages a job setting now.',
	},
};

/** D59/D60: the role a dimension's kind declares (falls back to what its usage shows). */
export function roleOfDim(dim: Dimension, usage: DimensionUsage | null): DimensionRole | null {
	if (dim.kind === 'grouping') return 'narrows';
	if (dim.kind === 'placeholder') return 'recorded';
	if (dim.kind === 'dimension') return 'qualifies';
	return usage ? roleOf(usage, dim.key) : null;
}

/** D60: the archetype an industry department extends, by department key (`customer_service` → its archetype). */
export function archetypeOf(
	archetypes: DepartmentArchetype[] | null,
	industry: string,
	department: string,
): DepartmentArchetype | null {
	return (
		archetypes?.find((a) => a.specialised_by.some((d) => d.industry === industry && d.key === department)) ??
		null
	);
}

/** D60: which department each area belongs to (`order-support` → `retail · Customer service`). */
export function areaDepartments(archetypes: DepartmentArchetype[] | null): Record<string, string[]> {
	const out: Record<string, string[]> = {};
	for (const a of archetypes ?? [])
		for (const d of a.specialised_by)
			for (const ar of d.areas) (out[ar] ??= []).push(`${d.industry} · ${d.label}`);
	return out;
}

export function usageOf(usage: DimensionUsage, dimension: string): DimensionValueUsage[] {
	return usage.values.filter((u) => u.dimension === dimension);
}

export function roleOf(usage: DimensionUsage, dimension: string): DimensionRole {
	const rows = usageOf(usage, dimension);
	if (rows.some((u) => u.context_spaces.length)) return 'narrows';
	if (rows.some((u) => u.layers.length)) return 'qualifies';
	return 'recorded';
}

/** `in_use`: a selection names it today · `frozen`: only past releases do · `authored`: only a qualified layer
 * reads it · `unused`: nothing yet (still kept). */
export type ValueStatus = 'in_use' | 'frozen' | 'authored' | 'unused';

export function statusOf(u: DimensionValueUsage): ValueStatus {
	if (u.selections.length) return 'in_use';
	if (u.releases.length) return 'frozen';
	if (u.layers.length) return 'authored';
	return 'unused';
}

export const STATUS_CHIP: Record<ValueStatus, { text: string; variant: 'ok' | 'accent' | 'warn' | 'muted' }> =
	{
		in_use: { text: 'in use', variant: 'ok' },
		frozen: { text: 'in releases only', variant: 'accent' },
		authored: { text: 'layer ready, not picked', variant: 'warn' },
		unused: { text: 'unused', variant: 'muted' },
	};

export interface UsageTotals {
	values: number;
	in_use: number;
	in_releases: number;
	unused: number;
	selections: number;
}

export function totals(dims: Dimension[], usage: DimensionUsage): UsageTotals {
	const all = usage.values;
	return {
		values: dims.reduce((n, d) => n + d.values.length, 0),
		in_use: all.filter((u) => statusOf(u) === 'in_use').length,
		in_releases: all.filter((u) => u.releases.length).length,
		unused: all.filter((u) => statusOf(u) === 'unused').length,
		selections: usage.selections.length,
	};
}

/**
 * The concrete reason values are insert-only: the oldest release that froze a value, and the values it froze.
 * Re-cutting or rolling back to it must resolve every one of them again.
 */
export function oldestFrozen(
	usage: DimensionUsage,
): { id: string; tenant_name: string; at: string | null; state: string; values: string[] } | null {
	const byRelease = new Map<
		string,
		{ id: string; tenant_name: string; at: string | null; state: string; values: string[] }
	>();
	for (const u of usage.values)
		for (const r of u.releases) {
			const k = `${r.tenant_id}/${r.id}`;
			const e = byRelease.get(k) ?? {
				id: r.id,
				tenant_name: r.tenant_name,
				at: r.at,
				state: r.state,
				values: [],
			};
			e.values.push(`${u.dimension}=${u.value}`);
			byRelease.set(k, e);
		}
	const list = [...byRelease.values()];
	if (!list.length) return null;
	// Releases without a timestamp (pre-approval era) sort by the cut date in their id; no date sorts last.
	const when = (e: { id: string; at: string | null }) =>
		e.at ?? e.id.match(/\d{4}-\d{2}-\d{2}/)?.[0] ?? '9999';
	return list.sort((a, b) => when(a).localeCompare(when(b)))[0] ?? null;
}

export interface PathStep {
	/** Layer path, or a label when the step is not a layer. */
	layer: string;
	/** What in the selection put it there. */
	because: string;
	status: 'applied' | 'skipped' | 'recorded';
	/** Department areas / added rows / recorded values shown under the step. */
	detail: string[];
	/** The value(s) this step reads, `dimension=value`, for highlighting. */
	values: string[];
	href?: string;
}

/**
 * The layer stack one selection compiles into, with the dimension values that pick or skip each layer:
 * base (+ department archetypes) → profile rules → industry → industry_dept (extends an archetype; areas narrow it)
 * → job → job × channel → placeholder values (recorded only).
 */
export function selectionPath(
	sel: DimensionSelectionRow,
	dims: Dimension[],
	usage: DimensionUsage,
	archetypes: DepartmentArchetype[] | null = null,
): PathStep[] {
	const steps: PathStep[] = [
		{ layer: 'base', because: 'always', status: 'applied', detail: [], values: [] },
		{
			layer: 'profile rules',
			because: `business model ${sel.selection.profile}`,
			status: 'applied',
			detail: [],
			values: [],
		},
		{
			layer: `industry/${sel.selection.industry}`,
			because: `industry ${sel.selection.industry}`,
			status: 'applied',
			detail: [],
			values: [],
		},
	];
	const areas = sel.values.area ?? [];
	for (const layer of sel.selection.departments) {
		const dep = parseDepartment(layer)?.[1] ?? layer;
		const arch = archetypeOf(archetypes, sel.selection.industry, dep);
		steps.push({
			layer,
			because: arch ? `department ${dep} · extends ${arch.label} archetype` : `department ${dep}`,
			status: 'applied',
			detail: areas.length
				? areas.map((a) => {
						const n = usage.values.find((u) => u.dimension === 'area' && u.value === a)?.context_spaces
							.length;
						return `area ${a}${n ? ` · ${n} context space${n === 1 ? '' : 's'}` : ''}`;
					})
				: ['no areas: the whole department applies'],
			values: areas.map((a) => `area=${a}`),
		});
	}
	steps.push({
		layer: `job/${sel.job}`,
		because: `job ${sel.job_label}`,
		status: 'applied',
		detail: [],
		values: [],
	});
	const qualified = usage.values.flatMap((u) =>
		u.layers.filter((l) => l.job === sel.job).map((l) => ({ u, l })),
	);
	for (const { u, l } of qualified) {
		const on = (sel.values[u.dimension] ?? []).includes(u.value);
		steps.push({
			layer: l.path,
			because: on ? `${u.dimension}=${u.value} selected` : `${u.dimension}=${u.value} not selected`,
			status: on ? 'applied' : 'skipped',
			detail: l.adds,
			values: [`${u.dimension}=${u.value}`],
			href: l.row_id ? `/platform/layers?row=${encodeURIComponent(l.row_id)}` : undefined,
		});
	}
	for (const d of dims) {
		if (roleOf(usage, d.key) !== 'recorded') continue;
		const vals = sel.values[d.key] ?? [];
		if (!vals.length) continue;
		steps.push({
			layer: `${d.label.toLowerCase()} (placeholder)`,
			because: 'recorded in the release · selects nothing',
			status: 'recorded',
			detail: vals,
			values: vals.map((v) => `${d.key}=${v}`),
		});
	}
	return steps;
}

export function selectionId(s: Pick<DimensionSelectionRow, 'tenant_id' | 'job'>): string {
	return `${s.tenant_id}/${s.job}`;
}
