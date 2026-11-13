// P14 Gate library helpers: layer groups with their meaning, and the plain-language rule / options / usage lines.
import type { GateGroup, GateQuestion, GateUsage } from '$lib/api/types/ontology';

export interface GroupDef {
	key: GateGroup;
	label: string;
	sub: string;
}

/** Library groups in template-layer order: base questions every job inherits, then industry and job waves. */
export const GROUPS: GroupDef[] = [
	{
		key: 'base',
		label: 'Base pre-gate',
		sub: 'base layer · every job inherits these · route the message to theme, stage, activity, object and record',
	},
	{
		key: 'safety',
		label: 'Safety',
		sub: 'base layer · fail closed to a person (D29) · tenants can never hide them',
	},
	{
		key: 'post',
		label: 'Post-gate R1',
		sub: 'base layer · fact-check rubric on every draft reply before it is sent',
	},
	{
		key: 'billing',
		label: 'Billing',
		sub: 'base_dept/billing · added on top of base for any billing department',
	},
	{
		key: 'utilities',
		label: 'Utilities',
		sub: 'industry/utilities · added on top of base for utilities jobs',
	},
	{
		key: 'itsd',
		label: 'IT service desk wave',
		sub: 'job/it_service_desk_agent · job-specific questions',
	},
];

/** The decision rule as a reader would say it: when the gate acts. */
export function ruleText(q: GateQuestion): string {
	if (q.threshold === '—' || !q.threshold.trim()) return 'always';
	if (q.type === 'noul' && /any true/.test(q.threshold)) return 'if true';
	return `if ${q.threshold}`;
}

export function optionsSummary(q: GateQuestion): string {
	if (q.type === 'noul') return 'true / false';
	if (q.generated_from) return q.options.length ? `${q.options.length} at last cut` : 'built per case';
	if (q.type === 'score') return `${q.options.length} levels, authored`;
	return `${q.options.length} options, authored`;
}

export function usageSummary(u: GateUsage): string {
	const n = u.jobs.length;
	if (!n) return 'Not asked by any job yet';
	const refined = u.jobs.reduce((a, j) => a + j.refinements, 0);
	const jobs = `Asked by ${n} job${n === 1 ? '' : 's'}`;
	return refined ? `${jobs} · ${refined} tenant refinement${refined === 1 ? '' : 's'}` : jobs;
}
