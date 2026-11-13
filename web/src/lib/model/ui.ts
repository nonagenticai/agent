// Small view helpers for the model area's pages (P6, P7, P9, P10, P11).
import type { ObjectTier, SideEffect, TagItemKind, TemplateScope } from '$lib/api/types/model';

export const SCOPE_LABEL: Record<TemplateScope, string> = {
	base: 'Base',
	industry: 'Industry',
	base_dept: 'Base department',
	industry_dept: 'Department',
	job: 'Job',
};

export const TIER_VARIANT: Record<ObjectTier, 'ok' | 'tool' | 'muted'> = {
	base: 'ok',
	industry: 'tool',
	optional: 'muted',
};

export const KIND_LABEL: Record<TagItemKind, string> = {
	entity: 'Entities',
	attribute: 'Attributes',
	relation: 'Relations',
	job: 'Job definition',
	context_space: 'Context spaces',
	procedure: 'Procedures',
	connector_slot: 'Connector slots',
	knowledge_store: 'Knowledge stores',
	operation: 'Operations',
	error_state: 'Error states',
	department_archetype: 'Department archetypes',
	activity_object: 'Activity → object bindings',
	object: 'Objects',
	profile: 'Business models (profiles)',
	axis_rule: 'Axis rules',
	journey_theme: 'Journey themes',
	journey_stage: 'Journey stages',
	theme_stage_prior: 'Theme → stage priors',
	stage_knowledge_kind: 'Knowledge kinds per stage',
	activity: 'Activities',
	operation_expectation: 'Operation expectations',
	gate_question: 'Gate questions',
	agent_role: 'Agent roles',
	job_setting: 'Job settings',
	knowledge_pack: 'Platform knowledge packs',
	instruction: 'Instructions (prompt slots)',
	journey_channel: 'Journey channels',
	stage_scope: 'Stage scope per department',
	use_case_card: 'Use-case cards',
};

export const EFFECT_LABEL: Record<SideEffect, string> = {
	R: 'R · read',
	W: 'W · reversible write',
	X: 'X · irreversible / financial / identity',
	P: 'P · payment',
};

export const EFFECT_VARIANT: Record<SideEffect, 'muted' | 'accent' | 'warn' | 'err'> = {
	R: 'muted',
	W: 'accent',
	X: 'warn',
	P: 'err',
};

export const LEVEL_LABEL: Record<string, string> = {
	'0': '0 · public',
	'1': '1 · identified',
	'2': '2 · verified',
	'3': '3 · step-up',
	S: 'S · staff',
};

/** `2026-10-17T17:05:00Z` → `17 Oct 2026, 17:05` (UTC, fixed so tests and screenshots agree). */
export function when(iso: string): string {
	const d = new Date(iso);
	if (Number.isNaN(d.getTime())) return iso;
	const mon = d.toLocaleString('en-GB', { month: 'short', timeZone: 'UTC' });
	const hh = String(d.getUTCHours()).padStart(2, '0');
	const mm = String(d.getUTCMinutes()).padStart(2, '0');
	return `${d.getUTCDate()} ${mon} ${d.getUTCFullYear()}, ${hh}:${mm}`;
}

export const shortHash = (h: string) => h.slice(0, 12);
