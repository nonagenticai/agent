// J16 Conflicts — pure helpers for the conflict board (spec 05 §4.6, D15, D54). A release moves the base under a
// tenant refinement; the conflict shows base before → after next to the tenant's row, what customers get while it is
// open, and the choices. A choice is a draft change (D54): the conflict records it (`decision`) and stays open until
// the release carrying it is applied — then it is resolved. Discarding the draft change undoes the decision unless an
// approved cut already carries it. Conflicts never block a publish (05 §4.3): an apply supersedes the open ones and
// recomputes them against the new base.
import type { ConflictCard, ConflictKind, ReleaseConflict } from '$lib/api/types/release';
import type { ConflictOp } from './conflicts';

/** Where a conflict is on its way: still to decide, decided in the draft, decided and published, or history. */
export type ConflictStage = 'open' | 'draft' | 'published' | 'resolved' | 'superseded';

export function stageOf(c: Pick<ConflictCard, 'status' | 'draft' | 'published_in'>): ConflictStage {
	if (c.status === 'open') return c.draft ? 'draft' : 'open';
	if (c.status === 'superseded') return 'superseded';
	if (c.published_in) return 'published';
	return 'resolved';
}

/** "Kept yours" / "Took the template’s" for a decision or a published resolution. */
export function choiceText(choice: 'reverted' | 'rebased' | null | undefined): string {
	return choice === 'rebased' ? 'Kept yours' : 'Took the template’s';
}

export const STAGE_LABEL: Record<ConflictStage, string> = {
	open: 'To decide',
	draft: 'Decided · in draft',
	published: 'Decided · published',
	resolved: 'Resolved',
	superseded: 'Superseded',
};
export const STAGE_VARIANT: Record<ConflictStage, 'warn' | 'accent' | 'ok' | 'muted'> = {
	open: 'warn',
	draft: 'accent',
	published: 'ok',
	resolved: 'ok',
	superseded: 'muted',
};

/** One field of the three-way comparison: base before, base now, the tenant's row. */
export interface FieldRow {
	field: string;
	before: string;
	after: string;
	mine: string;
	/** The base moved on this field. */
	base_moved: boolean;
	/** The tenant's row differs from the new base on this field. */
	mine_differs: boolean;
}

const show = (v: unknown): string => {
	if (v === undefined || v === null) return '—';
	if (Array.isArray(v)) return v.length ? v.join(', ') : 'none';
	if (typeof v === 'object') return JSON.stringify(v);
	return String(v);
};
const eq = (a: unknown, b: unknown) => JSON.stringify(a ?? null) === JSON.stringify(b ?? null);

/**
 * Field-by-field three-way rows over the union of fields (`key` left out: it is the card's title). The tenant column
 * shows `hidden` for a tombstone; the base columns show `—` where a row does not exist.
 */
export function fieldRows(
	before: Record<string, unknown> | null,
	after: Record<string, unknown> | null,
	mine: Record<string, unknown> | null,
	opts: { mineHidden?: boolean; mineAbout?: 'row' | 'child' } = {},
): FieldRow[] {
	const sameSubject = (opts.mineAbout ?? 'row') === 'row';
	const fields: string[] = [];
	for (const o of [before, after, sameSubject ? mine : null])
		for (const k of Object.keys(o ?? {})) if (k !== 'key' && !fields.includes(k)) fields.push(k);
	return fields.map((f) => {
		const b = before?.[f];
		const a = after?.[f];
		const m = sameSubject ? mine?.[f] : undefined;
		const mineText = opts.mineHidden ? 'hidden' : sameSubject && mine ? show(m) : '—';
		return {
			field: f,
			before: before ? show(b) : '—',
			after: after ? show(a) : '—',
			mine: mineText,
			base_moved: !eq(before ? b : undefined, after ? a : undefined),
			mine_differs: !!mine && sameSubject && !opts.mineHidden && !eq(m, a),
		};
	});
}

/** Customer effect while the conflict stays open, when the fixture does not say it more precisely. */
export function openEffect(c: Pick<ReleaseConflict, 'kind' | 'tenant_row'>): string {
	switch (c.kind) {
		case 'base_changed':
			return 'Customers keep getting your version, built on the old base; the template’s change does not reach them here.';
		case 'base_removed':
			return c.tenant_row === 'tombstone'
				? 'None: your hide covers a row the template no longer has.'
				: 'Customers keep getting your version; it now stands alone, without a template row under it.';
		case 'base_added':
			return 'Customers get your version; the template’s new row of the same name is ignored until you choose.';
		case 'parent_missing':
			return 'Your row is ignored: what it belongs to is gone, so the agent skips it.';
		case 'base_shadowed':
			return 'Customers get your published version; the template’s newer versions stay unused.';
		case 'job_setting':
			return 'Customers keep getting your setting; the template’s new value does not reach them.';
		case 'excluded_by_rule':
			return 'None: your exclusion rule keeps the new pack out of this job’s searches.';
		case 'required_kind_introduced':
			return 'Apply refuses this release until the kind has knowledge: bind a store or pin a platform pack.';
	}
}

/** What each choice does to this conflict, in the tenant's words. */
export function choiceEffect(c: Pick<ReleaseConflict, 'kind' | 'tenant_row'>, op: ConflictOp): string {
	if (op === 'revert') {
		if (c.kind === 'parent_missing')
			return 'Remove your row; nothing of yours is left that depends on the missing parent.';
		if (c.tenant_row === 'tombstone') return 'Remove your leftover hide. Customers notice no difference.';
		if (c.tenant_row === 'extension') return 'Drop your own row and use the template’s new one.';
		return 'Drop your refinement and run the template as it is now.';
	}
	if (c.kind === 'base_removed')
		return 'Keep your version as your own row; it no longer follows the template.';
	if (c.kind === 'base_added') return 'Keep your row; it overrides the template’s new one from now on.';
	return 'Keep your refinement as it is and mark it as made on the new base. The template’s change stays out.';
}

/** The kind's one-word name for the card header. */
export const KIND_TITLE: Record<ConflictKind, string> = {
	base_changed: 'The template changed this',
	base_removed: 'The template removed this',
	base_added: 'The template added this too',
	parent_missing: 'Its parent is gone',
	base_shadowed: 'Newer template versions shadowed',
	job_setting: 'The template changed this setting',
	excluded_by_rule: 'Excluded by your rule',
	required_kind_introduced: 'Newly required knowledge',
};

/** Where the tenant's row stands, as a short tag. */
export const MINE_LABEL: Record<ReleaseConflict['tenant_row'], string> = {
	override: 'your override',
	extension: 'your own row',
	tombstone: 'your hide',
	none: 'no row of yours',
};

/** "Template upgrade job/customer_care_agent v2 → v3" or "New cut". */
export function causeText(c: Pick<ConflictCard, 'cause'>): string | null {
	const x = c.cause;
	if (!x) return null;
	if (x.kind === 'new_cut') return 'New cut of the same template version';
	return `Template upgrade ${x.layer ?? ''} ${x.from ?? '?'} → ${x.to ?? '?'}`.replace(/\s+/g, ' ');
}

/**
 * The rows worth reading: only fields where the template moved or yours differs (the rest are named as unchanged).
 * A row the template removed collapses to one "whole row" line — field-by-field "removed" is noise.
 */
export function compactRows(
	before: Record<string, unknown> | null,
	after: Record<string, unknown> | null,
	rows: FieldRow[],
	mineText: string,
): { shown: FieldRow[]; unchanged: string[] } {
	if (before && !after) {
		const name = before.label ?? before.key ?? 'row';
		return {
			shown: [
				{
					field: 'whole row',
					before: String(name),
					after: 'removed',
					mine: mineText,
					base_moved: true,
					mine_differs: false,
				},
			],
			unchanged: [],
		};
	}
	return {
		shown: rows.filter((r) => r.base_moved || r.mine_differs),
		unchanged: rows.filter((r) => !r.base_moved && !r.mine_differs).map((r) => r.field),
	};
}
