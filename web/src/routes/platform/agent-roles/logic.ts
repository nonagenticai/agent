// P15 pure helpers: what each agent role can actually reach, summarised per side-effect class, so the page can show
// least privilege at a glance (ceiling = max side effect; grants = what sits under it).
import type { AgentRoleView, SideEffect } from '$lib/api/types/ontology';
import { SIDE_EFFECT_RANK, catalogueOp, opLabel } from '$lib/ontology/catalogue';

export const SIDE_EFFECTS: SideEffect[] = ['R', 'W', 'X', 'P'];

export interface RoleOp {
	/** `case.create{return}` */
	label: string;
	/** `case.create` */
	op: string;
	side_effect: SideEffect;
	identity_level: string;
}

/** Strip a `{variant}` suffix: `case.create{return}` → `case.create`. */
export const baseOp = (label: string) => label.replace(/\{.*\}$/, '');

/** Every distinct operation the role can call: its own grants, or for `procedure` the union over all activities. */
export function roleOps(role: AgentRoleView): RoleOp[] {
	if (!role.enabled) return [];
	const labels = role.grants.some((g) => g.derive_from_activity)
		? role.derived.flatMap((d) => d.ops)
		: role.grants.map((g) => opLabel(`${g.object}.${g.verb}`, g.variant));
	const seen = new Set<string>();
	const out: RoleOp[] = [];
	for (const label of labels) {
		if (seen.has(label)) continue;
		seen.add(label);
		const c = catalogueOp(baseOp(label));
		out.push({
			label,
			op: baseOp(label),
			side_effect: c?.side_effect ?? 'R',
			identity_level: c?.identity_level ?? '—',
		});
	}
	return out;
}

export type LadderCell =
	| { kind: 'allowed'; ops: RoleOp[] }
	| { kind: 'locked' }
	/** Role disabled: nothing runs regardless of the ceiling. */
	| { kind: 'off'; underCeiling: boolean };

/** One row per role, one cell per side-effect class R/W/X/P. */
export function ceilingLadder(roles: AgentRoleView[]) {
	return roles.map((r) => {
		const ops = roleOps(r);
		const cells: Record<SideEffect, LadderCell> = {} as Record<SideEffect, LadderCell>;
		for (const se of SIDE_EFFECTS) {
			const under = SIDE_EFFECT_RANK[se] <= SIDE_EFFECT_RANK[r.max_side_effect];
			cells[se] = !r.enabled
				? { kind: 'off', underCeiling: under }
				: under
					? { kind: 'allowed', ops: ops.filter((o) => o.side_effect === se) }
					: { kind: 'locked' };
		}
		return { role: r, total: ops.length, cells };
	});
}

/** The highest side effect among the ops (R when empty). */
export function peakSideEffect(labels: string[]): SideEffect {
	let peak: SideEffect = 'R';
	for (const l of labels) {
		const se = catalogueOp(baseOp(l))?.side_effect ?? 'R';
		if (SIDE_EFFECT_RANK[se] > SIDE_EFFECT_RANK[peak]) peak = se;
	}
	return peak;
}
