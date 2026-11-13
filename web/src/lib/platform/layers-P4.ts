// P4 Layers: copy and pure helpers for the layer explorer (what each scope is for, what a row does, which
// template tags compose a layer). No world access; unit tested in tests/unit/platform-P4.test.ts.
import type { JobTemplateTag, Layer, LayerRow, LayerScope, RowOp } from '$lib/api/types/platform';
import { layerLabel } from './model';

/** What each scope contributes to a job's compiled config, in compile order (05 §2.1). */
export const SCOPE_INFO: Record<LayerScope, { title: string; blurb: string }> = {
	base: {
		title: 'Base',
		blurb: 'Shared objects, jobs and department archetypes every agent starts from.',
	},
	profile: { title: 'Business model', blurb: 'Rows added only when an axis rule fires for the profile.' },
	industry: { title: 'Industry', blurb: 'Industry objects and synonyms; removes what the industry lacks.' },
	base_dept: {
		title: 'Base department',
		blurb:
			'A cross-industry department (D116): the procedures, slots and spaces that department does in every industry.',
	},
	industry_dept: {
		title: 'Department',
		blurb:
			'Specialises a base department archetype for one industry: its context spaces (grouped by area) and knowledge.',
	},
	job: { title: 'Job', blurb: 'Procedures, connector slots and channel variants for one job.' },
};

export const OP_INFO: Record<RowOp, { verb: string; variant: 'accent' | 'tool' | 'err'; hint: string }> = {
	add: { verb: 'Adds', variant: 'accent', hint: 'creates an object' },
	patch: { verb: 'Overrides', variant: 'tool', hint: 'replaces the named fields of an earlier object' },
	tombstone: {
		verb: 'Removes',
		variant: 'err',
		hint: 'hides an earlier object (an entity takes its attributes and relations)',
	},
};

/** The layer's name without its scope prefix: `retail/customer_service`, `customer_care_agent/channel=email`. */
export function shortName(l: Layer): string {
	return layerLabel(l).replace(new RegExp(`^${l.scope}/?`), '') || l.scope;
}

/** `industry/retail@v2` → `v2`. */
export function tagVersion(tag: string | null): string | null {
	return tag?.split('@')[1] ?? null;
}

/** Template tags whose pins include this layer (by layer key), with the pin that names it. */
export function templatesFor(
	l: Layer,
	templates: JobTemplateTag[],
): { template: JobTemplateTag; pinned: number }[] {
	const key = layerLabel(l);
	return templates.flatMap((t) => {
		const pin = t.pins.find((p) => p.layer === key);
		return pin ? [{ template: t, pinned: pin.version }] : [];
	});
}

/** The kinds that a set of rows holds, with counts, in first-seen order. */
export function kindCounts(rows: Pick<LayerRow, 'kind'>[]): [string, number][] {
	const m = new Map<string, number>();
	for (const r of rows) m.set(r.kind, (m.get(r.kind) ?? 0) + 1);
	return [...m];
}

function short(v: unknown): string {
	if (Array.isArray(v)) return v.length > 3 ? `${v.slice(0, 3).join(', ')} +${v.length - 3}` : v.join(', ');
	if (v && typeof v === 'object') return '{…}';
	return String(v);
}

/** One line for a row's fields: `data_type: enum · values: PostNL, DHL, DPD +1`. Tombstones show their reason. */
export function fieldLine(row: Pick<LayerRow, 'op' | 'fields'>, max = 3): string {
	const f = row.fields;
	if (row.op === 'tombstone') return typeof f.reason === 'string' ? f.reason : 'No reason recorded.';
	return Object.entries(f)
		.slice(0, max)
		.map(([k, v]) => `${k}: ${short(v)}`)
		.join(' · ');
}

/**
 * The layer a `?layer=` deep link names: its id (`L-rule-bundles`) or its name (`profile/bundles`, as `layerLabel`
 * prints it). Case-insensitive; `undefined` when nothing matches.
 */
export function findLayer<L extends Layer>(all: L[], ref: string): L | undefined {
	const want = ref.trim().toLowerCase();
	if (!want) return undefined;
	return (
		all.find((l) => l.id.toLowerCase() === want) ?? all.find((l) => layerLabel(l).toLowerCase() === want)
	);
}

/** `/platform/layers?layer=…` opening one layer (by id or name). */
export function layerHref(ref: string): string {
	return `/platform/layers?layer=${encodeURIComponent(ref)}`;
}
