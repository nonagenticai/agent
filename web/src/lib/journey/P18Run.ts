// P18 generator run detail: pure view-model helpers (no Svelte). The page reads a run as
// IN (industry/department/job + description) → LLM drafts ⇄ human decisions per step → Validate gate → OUT
// (template tag @vN for platform runs, tenant refinements for tenant runs, D8).
import type { GeneratorDraftItem, GeneratorRun, GeneratorStepKey } from '$lib/api/types/journey';
import { tagHref } from '$lib/model/logic';
import { STEP_LABEL } from './format';

/** Steps where the LLM drafts (Input, Validate and Package are human/machine only). */
export const DRAFT_STEPS: GeneratorStepKey[] = [
	'business_model',
	'journey',
	'objects',
	'operations',
	'harmonise',
	'use_cases',
];

export const stepCode = (k: GeneratorStepKey) =>
	STEP_LABEL[k].includes(' · ') ? STEP_LABEL[k].split(' · ')[0]! : '';
export const stepName = (k: GeneratorStepKey) => STEP_LABEL[k].replace(/^.* · /, '');

/** What each step writes into the template (spec 11 §7 "Output (overlay kind)"). */
export const STEP_PRODUCES: Record<GeneratorStepKey, string> = {
	input: 'layer keys',
	business_model: 'profile',
	journey: 'themes, stages, activities',
	objects: 'activity × object rows',
	operations: 'operation expectations',
	harmonise: 'edits to the above',
	validate: 'coverage report',
	use_cases: 'context spaces + procedures',
	package: 'template tag / refinements',
};

export interface Tally {
	kept: number;
	dropped: number;
	pending: number;
	total: number;
}

const KEPT: GeneratorDraftItem['decision'][] = ['accepted', 'renamed', 'merged'];

export function tally(items: GeneratorDraftItem[]): Tally {
	const kept = items.filter((i) => KEPT.includes(i.decision)).length;
	const dropped = items.filter((i) => i.decision === 'dropped').length;
	const pending = items.filter((i) => i.decision === 'pending').length;
	return { kept, dropped, pending, total: items.length };
}

export function stepTally(r: GeneratorRun, k: GeneratorStepKey): Tally {
	return tally(r.items.filter((i) => i.step === k));
}

export type NodeState = 'done' | 'current' | 'blocked' | 'todo';

export function nodeState(r: GeneratorRun, k: GeneratorStepKey): NodeState {
	const s = r.steps.find((x) => x.key === k);
	if (!s) return 'todo';
	if (s.status === 'current' && r.red && ['validate', 'use_cases', 'package'].includes(k)) return 'blocked';
	if (s.status === 'blocked') return 'blocked';
	return s.status === 'done' ? 'done' : s.status === 'current' ? 'current' : 'todo';
}

// ---- output ------------------------------------------------------------------------------------------------------

export interface OutputTarget {
	label: string;
	href: string | null;
}

/** `industry/retail@v2` → link to the P7 template tag; tenant refinements → the job's J13 page. */
export function outputTargets(r: GeneratorRun): OutputTarget[] {
	if (r.scope === 'tenant')
		return r.output.map((o) => ({ label: o, href: `/admin/jobs/${encodeURIComponent(r.job)}/refinements` }));
	return r.output.map((o) => {
		const m = /^(.+)@v(\d+)$/.exec(o);
		return { label: o, href: m ? tagHref(m[1]!, Number(m[2])) : null };
	});
}

// ---- validation rules (spec 11 §7) ---------------------------------------------------------------------------------

export const RULES: { n: number; text: string }[] = [
	{ n: 1, text: 'Every activity object is canonical or an approved extension object' },
	{ n: 2, text: 'Every field exists on its object; every operation has success + error shapes' },
	{ n: 3, text: 'Every object in the business model is used by an activity' },
	{ n: 4, text: 'Every journey stage has an activity, or is marked out of scope' },
	{ n: 5, text: 'Every required operation’s identity level is reachable' },
	{ n: 6, text: 'Every request operation has a Case type or pending status to land in' },
	{ n: 7, text: 'Evidence: imported tickets/chats are covered (optional)' },
];

/** Rules 1 and 4 are checked live while drafting; the rest run at the Validate step. */
const LIVE_RULES = [1, 4];

export type RuleState = 'red' | 'pass' | 'later' | 'optional';

export function ruleState(r: GeneratorRun, n: number): RuleState {
	if (r.findings.some((f) => f.rule === n && f.severity === 'red')) return 'red';
	if (r.status === 'packaged') return 'pass';
	if (n === 7) return 'optional';
	const idx = (k: GeneratorStepKey) => r.steps.findIndex((s) => s.key === k);
	return LIVE_RULES.includes(n) || idx(r.current_step) >= idx('validate') ? 'pass' : 'later';
}

// ---- the decision the page asks for --------------------------------------------------------------------------------

export interface NextAction {
	tone: 'ok' | 'err' | 'warn' | 'accent' | 'muted';
	title: string;
	detail: string;
	/** Step tab the action lives on, or 'rules' for the validation card. */
	target: GeneratorStepKey | 'rules' | null;
	cta: string | null;
}

export function nextAction(r: GeneratorRun): NextAction {
	const cur = stepName(r.current_step);
	if (r.status === 'packaged')
		return {
			tone: 'ok',
			title:
				r.scope === 'platform'
					? `Landed: tagged ${r.output.join(' + ')}`
					: 'Landed: tenant refinements saved',
			detail:
				r.scope === 'platform'
					? 'Tenants pinned to an older tag see “newer template available” (D15).'
					: `${r.tagged ?? 'Refinements'} on ${r.tenant_name} / ${r.job} — saved to that job’s draft; they reach customers only after the tenant tests and publishes them (D54).`,
			target: 'package',
			cta: null,
		};
	if (r.status === 'abandoned')
		return { tone: 'muted', title: 'Abandoned', detail: 'Nothing was tagged.', target: null, cta: null };
	if (r.red) {
		const gate = ['use_cases', 'package'].includes(r.current_step) ? 'tagging' : 'Validate';
		return {
			tone: 'err',
			title: `${r.red} red validation item${r.red === 1 ? '' : 's'} block${r.red === 1 ? 's' : ''} ${gate}`,
			detail: 'Fix the draft, propose the missing object, or mark the stage out of scope.',
			target: 'rules',
			cta: 'Resolve red items',
		};
	}
	const t = stepTally(r, r.current_step);
	const proposals = r.items.filter((i) => i.step === r.current_step && i.new_object && i.proposal === 'none');
	if (t.pending)
		return {
			tone: 'warn',
			title: `${t.pending} draft${t.pending === 1 ? '' : 's'} at ${cur} await${t.pending === 1 ? 's' : ''} your decision`,
			detail:
				'Accept, rename, merge or drop each one; nothing the LLM drafts reaches a template unapproved.' +
				(proposals.length ? ` ${proposals.length} names a new object — propose it first.` : ''),
			target: r.current_step,
			cta: 'Review drafts',
		};
	if (r.current_step === 'package')
		return {
			tone: 'accent',
			title: r.scope === 'platform' ? `Ready to tag ${r.output.join(' + ')}` : 'Ready to save refinements',
			detail: 'Validation is green. Packaging is the only step that publishes anything.',
			target: 'package',
			cta: 'Go to Package',
		};
	if (DRAFT_STEPS.includes(r.current_step) && !t.total)
		return {
			tone: 'accent',
			title: `${cur}: nothing drafted yet`,
			detail: 'Let the LLM draft this step, then decide each draft.',
			target: r.current_step,
			cta: 'Open step',
		};
	return {
		tone: 'accent',
		title: `${cur} is decided`,
		detail: 'Continue to the next step.',
		target: r.current_step,
		cta: 'Open step',
	};
}

// ---- A3–A4 activity × object matrix --------------------------------------------------------------------------------

export interface MatrixCell {
	activity: string;
	object: string;
	item: GeneratorDraftItem;
}

/** Draft titles read `stage:activity × object{variant}`; the matrix keys on the bare object name. */
export function objectMatrix(items: GeneratorDraftItem[]): {
	activities: string[];
	objects: string[];
	cells: MatrixCell[];
} {
	const cells: MatrixCell[] = [];
	for (const it of items) {
		if (it.step !== 'objects') continue;
		const [a, o] = it.title.split(' × ');
		if (!a || !o) continue;
		cells.push({ activity: a.trim(), object: o.trim().replace(/\{.*$/, ''), item: it });
	}
	const uniq = (xs: string[]) => [...new Set(xs)];
	return { activities: uniq(cells.map((c) => c.activity)), objects: uniq(cells.map((c) => c.object)), cells };
}

/** Usage per step from the run's usage rows (D56: tokens on a funding lane, never €). */
export function costByStep(r: GeneratorRun): { step: GeneratorStepKey; tokens: number; calls: number }[] {
	const out = new Map<GeneratorStepKey, { tokens: number; calls: number }>();
	for (const l of r.cost_lines ?? []) {
		const c = out.get(l.step) ?? { tokens: 0, calls: 0 };
		c.tokens += l.tokens;
		c.calls += 1;
		out.set(l.step, c);
	}
	return r.steps.filter((s) => out.has(s.key)).map((s) => ({ step: s.key, ...out.get(s.key)! }));
}
