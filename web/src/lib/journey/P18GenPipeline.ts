// P18 Discovery generator — the pipeline as a product model (11§7, plan P8 Task 3). Pure data + functions, owned by
// P18. A run walks the steps in order; the LLM drafts at some of them, a person decides at every one, Validate is the
// gate (red blocks Package), Package is the only step that publishes (`@vN` tag or a refinement revision).
import type { GeneratorRunSummary, GeneratorStepKey } from '$lib/api/types/journey';
import { tagHref } from '$lib/model/logic';

/** Prefill for the New-run drawer (from a not-started industry). */
export interface P18GenSeed {
	industry: string;
	department: string;
	job: string;
}

export type P18Phase = 'frame' | 'draft' | 'check' | 'ship';

export interface P18StepMeta {
	key: GeneratorStepKey;
	/** Short code shown on the node (`A1–A2`). */
	code: string;
	label: string;
	phase: P18Phase;
	/** What the LLM drafts here (null = no LLM call). */
	drafts: string | null;
	/** What the person decides. */
	decides: string;
	/** The overlay kind(s) it writes (11§7 "Output"). */
	writes: string;
}

export const P18_PHASES: { key: P18Phase; label: string; hint: string }[] = [
	{ key: 'frame', label: 'Frame', hint: 'What are we drafting?' },
	{ key: 'draft', label: 'Draft', hint: 'The LLM proposes, you accept / merge / rename / drop' },
	{ key: 'check', label: 'Check', hint: 'Harmonise, then machine rules 1–7' },
	{ key: 'ship', label: 'Ship', hint: 'Nothing is published before this' },
];

export const P18_STEPS: P18StepMeta[] = [
	{
		key: 'input',
		code: '0',
		label: 'Input',
		phase: 'frame',
		drafts: null,
		decides: 'industry, department, job, description',
		writes: 'layer keys',
	},
	{
		key: 'business_model',
		code: '1',
		label: 'Business model',
		phase: 'frame',
		drafts: 'axis values from the description',
		decides: 'the profile selection',
		writes: 'profile',
	},
	{
		key: 'journey',
		code: 'A1–A2',
		label: 'Journey + activities',
		phase: 'draft',
		drafts: 'themes, stages, activities per stage',
		decides: 'which activities stay',
		writes: 'journey_stage · activity',
	},
	{
		key: 'objects',
		code: 'A3–A4',
		label: 'Objects + example data',
		phase: 'draft',
		drafts: 'activity × object, fields, example rows',
		decides: 'each mapping; new objects → proposal',
		writes: 'activity_object',
	},
	{
		key: 'operations',
		code: 'C1–C5',
		label: 'Operations + JSON',
		phase: 'draft',
		drafts: 'do / request / observe, success JSON, errors',
		decides: 'each expectation',
		writes: 'operation_expectation',
	},
	{
		key: 'harmonise',
		code: 'H',
		label: 'Harmonise',
		phase: 'check',
		drafts: 'topics that extend activities',
		decides: 'the edits',
		writes: 'edits above',
	},
	{
		key: 'validate',
		code: 'V',
		label: 'Validate',
		phase: 'check',
		drafts: null,
		decides: 'resolve every red item',
		writes: '—',
	},
	{
		key: 'use_cases',
		code: 'U',
		label: 'Use cases',
		phase: 'ship',
		drafts: 'cards: actors, flows, dialogue, north star',
		decides: 'approve each card',
		writes: 'use_case_card',
	},
	{
		key: 'package',
		code: 'P',
		label: 'Package',
		phase: 'ship',
		drafts: null,
		decides: 'approve and tag',
		writes: '@vN tag / refinement rev',
	},
];

export const P18_ORDER: GeneratorStepKey[] = P18_STEPS.map((s) => s.key);

/** Plan P8 Task 3 `Step::order(RunClass::Tenant)`: a tenant run refines an existing job, so it skips the
 *  platform-only steps (business model, operations, harmonise — B-P8-4: no new tenant operations). */
export const P18_TENANT_STEPS: GeneratorStepKey[] = [
	'input',
	'journey',
	'objects',
	'validate',
	'use_cases',
	'package',
];

export function stepsFor(scope: 'platform' | 'tenant'): GeneratorStepKey[] {
	return scope === 'tenant' ? P18_TENANT_STEPS : P18_ORDER;
}

/** `Step::needs_llm` — false for Input, Validate, Package. */
export function llmStepsFor(scope: 'platform' | 'tenant'): GeneratorStepKey[] {
	return stepsFor(scope).filter((k) => P18_STEPS.find((s) => s.key === k)?.drafts);
}

export const stepMeta = (k: GeneratorStepKey): P18StepMeta => P18_STEPS.find((s) => s.key === k)!;

export type P18SegState = 'done' | 'current' | 'blocked' | 'todo' | 'skipped';

/** One segment per pipeline step for a run's progress bar. */
export function runSegments(
	r: Pick<GeneratorRunSummary, 'scope' | 'status' | 'current_step'>,
): { key: GeneratorStepKey; state: P18SegState }[] {
	const path = stepsFor(r.scope);
	const ci = P18_ORDER.indexOf(r.current_step);
	return P18_ORDER.map((key, i) => {
		if (r.status === 'packaged') return { key, state: path.includes(key) ? 'done' : 'skipped' };
		if (!path.includes(key) && i !== ci) return { key, state: 'skipped' };
		if (i < ci) return { key, state: 'done' };
		if (i === ci) return { key, state: r.status === 'blocked' ? 'blocked' : 'current' };
		return { key, state: 'todo' };
	});
}

/** Runs that still need a person (open), newest first, and the finished ones. */
export function splitRuns<T extends Pick<GeneratorRunSummary, 'status'>>(runs: T[]) {
	return {
		open: runs.filter((r) => r.status === 'running' || r.status === 'blocked'),
		done: runs.filter((r) => r.status === 'packaged' || r.status === 'abandoned'),
	};
}

/** The one-line "what this run needs from you now". */
export function nextAction(
	r: Pick<GeneratorRunSummary, 'status' | 'current_step' | 'red' | 'scope'>,
): string {
	const m = stepMeta(r.current_step);
	if (r.status === 'packaged')
		return r.scope === 'tenant' ? 'Refinement saved — reaches chat with the next release' : 'Tagged';
	if (r.status === 'abandoned') return 'Abandoned — start a new run to continue';
	if (r.red)
		return `${r.red} red validation item${r.red > 1 ? 's' : ''} block${r.red > 1 ? '' : 's'} Package — resolve at ${m.label}`;
	return `Decide the ${m.label.toLowerCase()} drafts`;
}

export interface P18Output {
	label: string;
	/** tagged = published by Package; provisional = written, not tagged (`@v0` / pending); */
	state: 'tagged' | 'provisional';
	href: string | null;
	where: string;
}

/** Where a run's output went (or is going). Platform: template layers; tenant: the job's refinements (D8). */
export function runOutputs(
	r: Pick<GeneratorRunSummary, 'scope' | 'status' | 'output' | 'job'> & {
		tagged?: string | null;
		provisional_tags?: string[];
	},
): P18Output[] {
	if (r.scope === 'tenant')
		return r.output.map((o) => ({
			label: o,
			state: r.status === 'packaged' ? 'tagged' : 'provisional',
			href: `/admin/jobs/${r.job}/refinements`,
			where: r.status === 'packaged' ? `${r.tagged ?? 'refinement'} · job refinements` : 'pending refinement',
		}));
	return r.output.map((o) => {
		const m = /^(.+)@v(\d+)$/.exec(o);
		const layer = m?.[1] ?? o;
		const v = m ? Number(m[2]) : null;
		if (r.status === 'packaged' && v != null)
			return { label: o, state: 'tagged', href: tagHref(layer, v), where: 'template tag' };
		const prov = (r.provisional_tags ?? []).find((p) => p.startsWith(`${layer}@v`));
		if (prov && v != null)
			return {
				label: prov,
				state: 'provisional',
				href: tagHref(layer, Number(prov.split('@v')[1])),
				where: `provisional tag — becomes @v${v} at Package`,
			};
		return {
			label: layer,
			state: 'provisional',
			href: '/platform/templates',
			where: v != null ? `draft, not tagged — becomes @v${v} at Package` : 'draft, not tagged',
		};
	});
}

/** Totals for the summary strip. */
export function runTotals(runs: GeneratorRunSummary[]) {
	const { open, done } = splitRuns(runs);
	return {
		open: open.length,
		blocked: open.filter((r) => r.status === 'blocked' || r.red > 0).length,
		packaged: done.filter((r) => r.status === 'packaged').length,
		/** D56: tenant-run tokens, metered as those tenants' usage (activity `generator`) — not a bill. */
		tenantTokens: runs
			.filter((r) => r.cost.attributed_to === 'tenant')
			.reduce((a, r) => a + r.cost.tokens, 0),
		/** Platform-run tokens: the platform's own usage. */
		platformTokens: runs
			.filter((r) => r.cost.attributed_to === 'platform')
			.reduce((a, r) => a + r.cost.tokens, 0),
	};
}

/** Runs parked at each step (for the pipeline diagram). */
export function runsAtStep(
	runs: GeneratorRunSummary[],
): Partial<Record<GeneratorStepKey, GeneratorRunSummary[]>> {
	const out: Partial<Record<GeneratorStepKey, GeneratorRunSummary[]>> = {};
	for (const r of splitRuns(runs).open) (out[r.current_step] ??= []).push(r);
	return out;
}
