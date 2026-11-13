// What a release conflict means and how it may be resolved (from cb web/src/lib/conflicts.ts@45ee128; kinds and
// resolutions per spec 05 §4.6). Conflicts are never auto-resolved: an apply supersedes the open ones and
// recomputes them against the new base.
import type { ConflictKind, ConflictStatus, ReleaseConflict } from '$lib/api/types/release';

export function conflictText(kind: ConflictKind): string {
	switch (kind) {
		case 'base_changed':
			return 'The release changed this; your refinement still applies on top of the old version.';
		case 'base_removed':
			return 'The release removed this; your refinement has nothing under it.';
		case 'base_added':
			return 'The release now defines this too; yours wins until you choose.';
		case 'parent_missing':
			return 'What this belongs to is gone from the release.';
		case 'base_shadowed':
			return 'Your published version shadows a changed release version.';
		case 'job_setting':
			return 'The release changed the base value of a setting you override.';
		case 'excluded_by_rule':
			return 'The release adds a pack your exclusion rule also hides; nothing to decide.';
		case 'required_kind_introduced':
			return 'The release makes this knowledge kind required; bind it before the release can be applied.';
	}
}

export type ConflictOp = 'revert' | 'rebase';

/**
 * The resolutions of an open conflict. Revert ("Accept base") drops the tenant row; rebase ("Keep mine") accepts
 * the current base under it (a removed base turns an override into an extension). A tombstone over a removed row
 * and an orphan child can only be reverted. Superseded or resolved conflicts offer nothing.
 */
export function conflictActions(c: Pick<ReleaseConflict, 'kind' | 'status' | 'tenant_row'>): ConflictOp[] {
	if (c.status !== 'open') return [];
	// informational (D112): the rule already decided
	if (c.kind === 'excluded_by_rule' || c.kind === 'required_kind_introduced') return [];
	if (c.kind === 'parent_missing') return ['revert'];
	if (c.kind === 'base_removed' && c.tenant_row === 'tombstone') return ['revert'];
	return ['revert', 'rebase'];
}

export const CONFLICT_ACTION: Record<ConflictOp, { label: string; hint: string; done: string }> = {
	revert: {
		label: 'Revert · accept base',
		hint: 'Drop your refinement and use the release as it is.',
		done: 'Accepted base',
	},
	rebase: {
		label: 'Rebase · keep mine',
		hint: 'Keep your refinement on top of the new release.',
		done: 'Kept yours',
	},
};

export const OBJECT_LABEL: Record<string, string> = {
	activity: 'Activity',
	context_space: 'Context space',
	ontology_attribute: 'Attribute',
	gate_option: 'Gate option',
	knowledge_store: 'Knowledge store',
	knowledge_pack: 'Knowledge pack',
	knowledge_kind: 'Knowledge kind',
	job_setting: 'Job setting',
};

export const STATUS_VARIANT: Record<ConflictStatus, 'warn' | 'muted' | 'ok'> = {
	open: 'warn',
	superseded: 'muted',
	resolved: 'ok',
};

export const KIND_LABEL: Record<ConflictKind, string> = {
	base_changed: 'base changed',
	base_removed: 'base removed',
	base_added: 'base added',
	parent_missing: 'parent missing',
	base_shadowed: 'base shadowed',
	job_setting: 'job setting',
	excluded_by_rule: 'excluded by your rule',
	required_kind_introduced: 'newly required knowledge',
};
