// J4 job journey: pure helpers that turn the effective grid into the two questions the page answers —
// "what did we inherit vs refine?" and "is each in-scope activity ready to serve customers?".
import type {
	ActivityOutcome,
	JobJourney,
	JobJourneyActivity,
	JobKnowledgeKind,
	JourneyStage,
} from '$lib/api/types/journey';

export type J4Lens = 'refined' | 'ready' | 'live';

export interface ActivityGap {
	kind: 'route' | 'knowledge';
	/** `case.read · test` or the missing knowledge kind. */
	what: string;
}

/** Why an enabled activity cannot serve yet: unrouted required operations (J6) and, for activities that answer from
 *  the knowledge store, required knowledge kinds of its stage that the bound store does not hold (J10). */
export function activityGaps(a: JobJourneyActivity, kinds: JobKnowledgeKind[]): ActivityGap[] {
	if (a.refinement === 'hidden') return [];
	const gaps: ActivityGap[] = (a.routing?.unrouted ?? []).map((what) => ({ kind: 'route', what }));
	if (a.objects.some((o) => o.object === 'knowledge'))
		for (const k of kinds)
			if (k.stage === a.stage && k.required && !k.present) gaps.push({ kind: 'knowledge', what: k.kind });
	return gaps;
}

export type Readiness = 'ready' | 'gap' | 'off';
export function readinessOf(a: JobJourneyActivity, kinds: JobKnowledgeKind[]): Readiness {
	if (a.refinement === 'hidden') return 'off';
	return activityGaps(a, kinds).length ? 'gap' : 'ready';
}

/** Lens → tone for JourneyGrid's `activityTone`. */
export function toneOf(
	lens: J4Lens,
	a: JobJourneyActivity,
	kinds: JobKnowledgeKind[],
	outcome: ActivityOutcome | undefined,
): string | null {
	if (a.refinement === 'hidden') return null;
	if (lens === 'refined') return a.refinement === 'base' ? 'inherited' : 'refined';
	if (lens === 'ready') return readinessOf(a, kinds) === 'gap' ? 'gap' : 'ready';
	return outcome ? (containment(outcome) < 0.5 ? 'gap' : 'ready') : 'idle';
}

export function containment(o: ActivityOutcome): number {
	return o.conversations ? o.contained / o.conversations : 0;
}

export interface StageSummary {
	stage: JourneyStage;
	enabled: number;
	hidden: number;
	refined: number;
	gaps: number;
	conversations: number;
	missingKinds: string[];
}

export function stageSummaries(j: JobJourney): StageSummary[] {
	const out = new Map((j.outcomes?.rows ?? []).map((r) => [r.activity, r]));
	return j.stages.map((s) => {
		const acts = j.activities.filter((a) => a.stage === s.key);
		const on = acts.filter((a) => a.refinement !== 'hidden');
		return {
			stage: s,
			enabled: on.length,
			hidden: acts.length - on.length,
			refined: acts.filter((a) => a.refinement === 'extension' || a.refinement === 'relabelled').length,
			gaps: on.filter((a) => readinessOf(a, j.knowledge_kinds) === 'gap').length,
			conversations: on.reduce((n, a) => n + (out.get(a.key)?.conversations ?? 0), 0),
			missingKinds: j.knowledge_kinds
				.filter((k) => k.stage === s.key && k.required && !k.present)
				.map((k) => k.kind),
		};
	});
}

export interface J4Totals {
	inherited: number;
	hidden: number;
	extensions: number;
	relabelled: number;
	enabled: number;
	ready: number;
	gaps: number;
	/** Draft refinements not yet published by a release (D54): in the test chat, not in Production. */
	unreleased: number;
	stagesInScope: number;
}

export function totals(j: JobJourney): J4Totals {
	const on = j.activities.filter((a) => a.refinement !== 'hidden');
	const gaps = on.filter((a) => readinessOf(a, j.knowledge_kinds) === 'gap').length;
	return {
		inherited: j.activities.filter((a) => a.refinement !== 'extension').length,
		hidden: j.activities.length - on.length,
		extensions: j.activities.filter((a) => a.refinement === 'extension').length,
		relabelled: j.activities.filter((a) => a.refinement === 'relabelled').length,
		enabled: on.length,
		ready: on.length - gaps,
		gaps,
		unreleased: j.pending.length,
		stagesInScope: j.stages.filter((s) => !s.out_of_scope).length,
	};
}
