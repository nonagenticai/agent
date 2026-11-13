// D119: the qualifier's and post-gate's grants derive from the job's folded operation expectations instead of
// hand-written job-layer `agent_role` rows. Mirrors `derive_role_grants` (crates/agent-model/src/kinds/role.rs): the
// web has no catalogue compiler, so the fixtures and the platform page derive here.
import type { AgentRoleGrant, AgentRoleKey, SideEffect } from '$lib/api/types/ontology';
import { SIDE_EFFECT_RANK, catalogueOp } from '$lib/ontology/catalogue';

/** The roles whose grants derive; a layer that sets grants on one keeps them. */
export const DERIVED_ROLES: readonly AgentRoleKey[] = ['qualifier', 'post_gate'];

const KNOWLEDGE_SEARCH = 'knowledge.search';

/** One observe grant per expected observe operation within `max`; the post-gate also keeps `knowledge.search`, the
 *  qualifier never has it. Any other role derives nothing. */
export function deriveRoleGrants(
	role: AgentRoleKey,
	max: SideEffect,
	expectedOps: string[],
): AgentRoleGrant[] {
	if (!DERIVED_ROLES.includes(role)) return [];
	const ops: string[] = role === 'post_gate' ? [KNOWLEDGE_SEARCH] : [];
	for (const op of expectedOps) {
		const c = catalogueOp(op);
		if (c?.modality !== 'observe' || SIDE_EFFECT_RANK[c.side_effect] > SIDE_EFFECT_RANK[max]) continue;
		if (op !== KNOWLEDGE_SEARCH && !ops.includes(op)) ops.push(op);
	}
	return ops.map((op) => {
		const [object, verb] = op.split('.') as [string, string];
		return {
			object,
			verb,
			variant: null,
			modalities: ['observe'],
			derive_from_activity: false,
			derived: true,
		};
	});
}

/** Every grant of the role is derived (so the page says where they come from). */
export const grantsFromExpectations = (grants: AgentRoleGrant[]) =>
	grants.length > 0 && grants.every((g) => g.derived);
