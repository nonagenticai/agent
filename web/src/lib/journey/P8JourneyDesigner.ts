// P8 Journey designer: what the platform author's grid *produces*. The grid (D23, 11§4.3.1) is the backbone of every
// job of an industry: gate options (GT/GS/GA/GO, 11§4.3.4), the operation catalogue (11§4.3.5), the strata_rag
// knowledge requirements (11§4.3.3) and the use-case drafts (11§6 "Use cases + relate data") are all derived from its
// cells at the release cut — nobody writes them by hand. Pure functions over `IndustryJourney`, owned by P8.
import type { IndustryJourney, StageKey, ValidationFinding } from '$lib/api/types/journey';
import { OPS, objectKey } from './catalogue';

export interface DerivedOutput {
	key: 'themes' | 'stages' | 'activities' | 'objects' | 'operations' | 'kinds';
	/** The grid row(s) it comes from. */
	from: string;
	count: number;
	/** e.g. `10 required` */
	detail: string;
	/** What the runtime gets from it. */
	becomes: string;
	href: string | null;
}

/** Unique operations (counted by `object.verb`, variants not separate — K48) with their v1 R/O and modality. */
export function operationSummary(j: Pick<IndustryJourney, 'activities'>) {
	const ops = new Map<string, { required: boolean; stages: Set<StageKey> }>();
	for (const a of j.activities)
		for (const o of a.operations) {
			const cur = ops.get(o.op) ?? { required: false, stages: new Set<StageKey>() };
			cur.required ||= o.required;
			cur.stages.add(a.stage);
			ops.set(o.op, cur);
		}
	const modality = { do: 0, request: 0, observe: 0 };
	for (const k of ops.keys()) {
		const m = OPS[k]?.modality;
		if (m) modality[m] += 1;
	}
	return {
		total: ops.size,
		required: [...ops.values()].filter((o) => o.required).length,
		objectKeys: new Set([...ops.keys()].map((k) => k.split('.')[0])).size,
		modality,
		ops,
	};
}

/** The six things the grid generates, left to right in the order the runtime resolves them (11§5.1). */
export function derivedOutputs(j: IndustryJourney): DerivedOutput[] {
	const inScope = j.stages.filter((s) => !s.out_of_scope);
	const cust = j.activities.filter((a) => a.actor !== 'company');
	const useCases = cust.filter((a) => a.operations.length).length;
	const used = new Set(j.activities.flatMap((a) => a.objects.map((o) => objectKey(o.object))));
	const ops = operationSummary(j);
	const kinds = new Set(j.knowledge_kinds.map((k) => k.kind));
	const reqKinds = new Set(j.knowledge_kinds.filter((k) => k.required).map((k) => k.kind));
	return [
		{
			key: 'themes',
			from: 'Themes',
			count: j.themes.length,
			detail: `${j.themes.filter((t) => t.escalation_weight >= 0.5).length} raise escalation`,
			becomes: 'GT options: “which theme is the customer in?” — tone + stage prior',
			href: '/platform/gates',
		},
		{
			key: 'stages',
			from: 'Stage columns',
			count: inScope.length,
			detail:
				inScope.length === j.stages.length
					? 'all in scope'
					: `${j.stages.length - inScope.length} out of scope`,
			becomes: 'GS options: the stage descriptions are the option text',
			href: '/platform/gates',
		},
		{
			key: 'activities',
			from: 'Activities',
			count: j.activities.length,
			detail: `${cust.length} customer · ${j.activities.length - cust.length} company`,
			becomes: `GA options + ${useCases} use-case drafts (context space + procedure)`,
			href: null,
		},
		{
			key: 'objects',
			from: 'Business objects',
			count: used.size,
			detail: `of ${j.model_objects.length} in the business model`,
			becomes: 'GO options + the field subsets each activity may read',
			href: '/platform/objects',
		},
		{
			key: 'operations',
			from: 'Activity × object',
			count: ops.total,
			detail: `${ops.required} required · ${ops.objectKeys} object keys`,
			becomes: 'the job’s operation catalogue; R rows must be routed before go-live',
			href: '/platform/operations',
		},
		{
			key: 'kinds',
			from: 'Knowledge kinds',
			count: kinds.size,
			detail: `${reqKinds.size} required`,
			becomes: 'what each tenant’s strata_rag store must hold; replies cite them',
			href: null,
		},
	];
}

export type RuleStatus = 'pass' | 'red' | 'amber' | 'package' | 'optional';
export interface RuleCheck {
	rule: number;
	title: string;
	status: RuleStatus;
	findings: ValidationFinding[];
}

/** 11§7 rules 1–4, 6 and 7 as a checklist. Rules 2 and 6 are checked against `operation_def` at the Package step. */
export function ruleChecklist(findings: ValidationFinding[]): RuleCheck[] {
	const titles: [number, string, RuleStatus | null][] = [
		[1, 'Every activity object is a canonical object', null],
		[2, 'Every field and operation exists in the model', 'package'],
		[3, 'Every business-model object is used by an activity', null],
		[4, 'Every in-scope stage has activities, chat messages and objects', null],
		[6, 'Every request operation lands in a Case type', 'package'],
		[7, 'Evidence: imported tickets map onto activities', 'optional'],
	];
	return titles.map(([rule, title, fixed]) => {
		const f = findings.filter((x) => x.rule === rule);
		const status: RuleStatus =
			fixed ?? (f.some((x) => x.severity === 'red') ? 'red' : f.length ? 'amber' : 'pass');
		return { rule, title, status, findings: f };
	});
}

/** Per stage: how much runtime surface the column carries (a column is a capability, D23). */
export function stageLoad(j: IndustryJourney) {
	return j.stages.map((s) => {
		const acts = j.activities.filter((a) => a.stage === s.key);
		const ops = operationSummary({ activities: acts });
		return {
			key: s.key,
			label: s.label,
			out_of_scope: s.out_of_scope,
			activities: acts.length,
			operations: ops.total,
			required: ops.required,
			red: j.findings.some((f) => f.stage === s.key && f.severity === 'red'),
		};
	});
}

/** Theme row total of the priors; a row should sum to 1 (it is a distribution over stages). */
export const priorRowSum = (j: Pick<IndustryJourney, 'priors'>, theme: string) =>
	Math.round(j.priors.filter((p) => p.theme === theme).reduce((n, p) => n + p.weight, 0) * 100) / 100;
