// Base vs tenant, side by side (from cb web/src/lib/refinements.ts@45ee128, adapted to the fixture seam): each
// refinable object is read as both layers — the job's current release (base) and this tenant's rows (override,
// extension, tombstone) — and paired by natural key.
import type {
	ContextSpacePatch,
	OverlayItem,
	RefinableObject,
	RefinementOp,
	ReleaseConflict,
	RowState,
	TenantRow,
} from '$lib/api/types/release';

export type SideObject = Exclude<RefinableObject, 'job_setting'>;
export type FieldDiff = { field: string; base: string; tenant: string };

export interface SideRow {
	object: SideObject;
	key: string;
	base: OverlayItem | null;
	tenant: TenantRow | null;
	state: RowState;
	diff: FieldDiff[];
	conflict: ReleaseConflict | null;
}

/** Not configuration: provenance flags and bookkeeping columns. */
export const IGNORED_FIELDS = new Set(['key', 'origin', 'hidden', 'content_hash', 'base_hash', 'generated']);

/** A value as the page prints it; an unset value reads "not set", never "null". */
export function fmtValue(v: unknown): string {
	if (v === null || v === undefined || v === '') return 'not set';
	if (typeof v === 'string') return v;
	if (typeof v === 'boolean') return v ? 'yes' : 'no';
	if (Array.isArray(v))
		return v.length ? v.map((x) => (typeof x === 'string' ? x : JSON.stringify(x))).join(', ') : 'none';
	if (typeof v === 'object') return JSON.stringify(v);
	return String(v);
}

/** The configuration fields whose base and tenant values differ, in the base item's field order. */
export function diffFields(base: OverlayItem, tenant: OverlayItem): FieldDiff[] {
	const fields = [...new Set([...Object.keys(base), ...Object.keys(tenant)])].filter(
		(f) => !IGNORED_FIELDS.has(f),
	);
	return fields
		.filter((f) => JSON.stringify(base[f] ?? null) !== JSON.stringify(tenant[f] ?? null))
		.map((f) => ({ field: f, base: fmtValue(base[f]), tenant: fmtValue(tenant[f]) }));
}

/** Pairs the two layers by key; rows sorted by key. Open conflicts attach to their row. */
export function sideBySide(
	object: SideObject,
	base: OverlayItem[],
	tenant: TenantRow[],
	openConflicts: ReleaseConflict[] = [],
): SideRow[] {
	const b = new Map(base.map((i) => [i.key, i]));
	const t = new Map(tenant.map((i) => [i.key, i]));
	const keys = [...new Set([...b.keys(), ...t.keys()])].sort((x, y) => x.localeCompare(y));
	return keys.map((key) => {
		const bi = b.get(key) ?? null;
		const ti = t.get(key) ?? null;
		const state: RowState = ti
			? ti.kind === 'tombstone'
				? 'tombstone'
				: bi
					? 'override'
					: 'extension'
			: 'base';
		const conflict =
			openConflicts.find((c) => c.object === object && c.status === 'open' && c.key === key) ?? null;
		return {
			object,
			key,
			base: bi,
			tenant: ti,
			state,
			diff: state === 'override' && bi && ti?.item ? diffFields(bi, ti.item) : [],
			conflict,
		};
	});
}

/**
 * What an admin may do with a row: revert an override (and rebase it when the release changed under it), unhide a
 * tombstone, hide a base row, remove an extension (revert).
 */
export function actionsFor(row: Pick<SideRow, 'state' | 'conflict' | 'base'>): RefinementOp[] {
	switch (row.state) {
		case 'override':
			return row.conflict ? ['revert', 'rebase'] : ['revert'];
		case 'tombstone':
			return row.base ? ['unhide'] : ['revert'];
		case 'base':
			return row.base?.required === true ? [] : ['hide'];
		case 'extension':
			return ['revert'];
	}
}

export const ACTION_LABEL: Record<RefinementOp, string> = {
	revert: 'Revert',
	rebase: 'Rebase',
	hide: 'Hide',
	unhide: 'Unhide',
};

export const ACTION_DONE: Record<RefinementOp, string> = {
	revert: 'Reverted',
	rebase: 'Rebased',
	hide: 'Hidden',
	unhide: 'Unhidden',
};

export const STATE_LABEL: Record<RowState, string> = {
	base: 'From the release',
	override: 'Refined',
	extension: 'Added by you',
	tombstone: 'Hidden',
};

export const STATE_VARIANT = {
	base: 'muted',
	override: 'accent',
	extension: 'ok',
	tombstone: 'warn',
} as const;

/** The one-line value of a row on one side only, per object. */
export function summaryOf(object: SideObject, item: OverlayItem): string {
	switch (object) {
		case 'activity':
			return `${fmtValue(item.label)} · ${fmtValue(item.stage)}${item.required ? ' · required' : ''}`;
		case 'context_space':
			return `${fmtValue(item.label)} · asks ${fmtValue(item.required)}`;
		case 'ontology_attribute':
			return `${fmtValue(item.label)} (${fmtValue(item.type)})`;
		case 'gate_option':
			return fmtValue(item.label);
		case 'knowledge_store':
			return `${fmtValue(item.label)} · ${fmtValue(item.kind)}`;
		case 'knowledge_pack':
			// PK (T3): `{key: stage|kind|pack_key, label, pin}`
			return `${fmtValue(item.pin)} · ${fmtValue(item.label)}`;
		case 'instruction':
			// KH T4: `{key: <slot>, text, sections}`
			return fmtValue(item.text);
	}
}

// ---- context-space override (routes::ontology::put_space) ------------------------------------------------------

/** `key`, `area`, `request_type` and `procedure` route the space and come with the release only. */
export const SPACE_ROUTING = ['key', 'area', 'request_type', 'procedure'] as const;
export const SPACE_FIELD_LABEL: Record<string, string> = {
	label: 'Label',
	required: 'Required fields',
	entities: 'Entities',
	key: 'Key',
	area: 'Area',
	request_type: 'Request type',
	procedure: 'Procedure',
};
const SLOT_RE = /^[a-z][a-z0-9_]{0,63}$/;
export const SPACE_LABEL_MAX = 120;

export type SpaceDraft = { label: string; required: string; entities: string[] };
type SpaceValues = { label: string; required: string[]; entities: string[] };

export function spaceValues(item: OverlayItem): SpaceValues {
	return {
		label: String(item.label ?? ''),
		required: Array.isArray(item.required) ? (item.required as string[]) : [],
		entities: Array.isArray(item.entities) ? (item.entities as string[]) : [],
	};
}

export function spaceDraft(s: SpaceValues): SpaceDraft {
	return { label: s.label, required: s.required.join(', '), entities: [...s.entities] };
}

/** `a, b ,, c` → `['a', 'b', 'c']`. */
export function parseList(text: string): string[] {
	return text
		.split(',')
		.map((x) => x.trim())
		.filter(Boolean);
}

/** Client-side checks mirroring the server's 422s (the server stays the authority). */
export function spaceFormErrors(d: SpaceDraft): string[] {
	const e: string[] = [];
	const label = d.label.trim();
	if (!label) e.push('Label is required.');
	else if ([...label].length > SPACE_LABEL_MAX) e.push(`Label is at most ${SPACE_LABEL_MAX} characters.`);
	const req = parseList(d.required);
	const bad = req.filter((f) => !SLOT_RE.test(f));
	if (bad.length) e.push(`Required fields are snake_case names (not ${bad.join(', ')}).`);
	if (new Set(req).size !== req.length) e.push('Each required field once.');
	return e;
}

/** Only the fields that differ from `current`; `{}` when nothing changed. */
export function spacePatch(current: SpaceValues, d: SpaceDraft): ContextSpacePatch {
	const out: ContextSpacePatch = {};
	const label = d.label.trim();
	if (label !== current.label) out.label = label;
	const req = parseList(d.required);
	if (JSON.stringify(req) !== JSON.stringify(current.required)) out.required = req;
	if (JSON.stringify(d.entities) !== JSON.stringify(current.entities)) out.entities = [...d.entities];
	return out;
}
