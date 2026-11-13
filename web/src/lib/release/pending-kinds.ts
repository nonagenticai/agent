// D80: the ledger kinds still on sample data (agent-api `design::PENDING`, sent as `draftSummary.pending_kinds`),
// as the areas a tenant knows: "not yet part of releases: gates, ontology, …".

/** Each ledger kind → the area it belongs to (plan B2 §2.5). An unknown kind reads as itself. */
const AREA_OF_KIND: Record<string, string> = {
	gate: 'gates',
	gate_question: 'gates',
	gate_setting: 'gates',
	entity: 'ontology',
	relation: 'ontology',
	space_required: 'ontology',
	entity_hidden: 'ontology',
	entity_synonyms: 'ontology',
	entity_attributes: 'ontology',
	knowledge: 'knowledge',
	narrowing: 'knowledge',
	embedding_pin: 'knowledge',
	procedure: 'procedures',
	route: 'operations',
	job_operation: 'operations',
	activity: 'operations',
	handover_policy: 'handover',
	role_setting: 'roles',
	llm_use: 'providers',
	provider_slot: 'providers',
	mapping_override: 'connectors',
	tenant_template: 'connectors',
	tenant_calendar: 'calendar',
	tool_policy: 'Tool input policy',
	iam_attachment: 'Identity procedure attachment',
};

/** The areas of `kinds`, first-seen order, each once. */
export function pendingAreas(kinds: readonly string[] | null | undefined): string[] {
	return [...new Set((kinds ?? []).map((k) => AREA_OF_KIND[k] ?? k.replace(/_/g, ' ')))];
}

/** "Not yet part of releases: gates, ontology, …" — null when every kind is part of releases (or none were sent). */
export function pendingKindsLine(kinds: readonly string[] | null | undefined): string | null {
	const areas = pendingAreas(kinds);
	return areas.length ? `Not yet part of releases: ${areas.join(', ')}.` : null;
}

/**
 * The warn chip of a draft change whose kind the live release does not cover (`DraftChange.uncovered`): publishing
 * makes the draft value live. Null when covered (or the backend did not say).
 */
export function uncoveredChip(release: string | null | undefined): { text: string; title: string } | null {
	if (!release) return null;
	return {
		text: `Not in live ${release}`,
		title: `Not part of the live version ${release}: publishing makes this draft value live.`,
	};
}
