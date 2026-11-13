// J14 Review & publish (D54): where each part of the job's change stream sits in the publish pipeline
// Draft → Test → Cut & approve → Apply to Production → Live, and the one next step the tenant_admin should take.
// Pure: the page reads the API, this module only derives. Approve (the cut) and apply are two steps with two audit
// events (05 §4.2–4.3, K41); apply refuses with 409 release_not_ready while readiness gaps remain (K25, J3).
// Test reads real evidence (D54 "tested before publish"): the J17 draft validation run (scenarios graded on the draft
// in the Test environment) and the L2 test cases (saved test-chat conversations, stale once the draft moved on).
import type { DraftChange, ReleaseRow, ReleaseStatus } from '$lib/api/types/release';
import type { DraftValidation, ValidationCut } from '$lib/api/types/journey';
import type { TestCase } from '$lib/api/types/live';

export type StageKey = 'draft' | 'test' | 'cut' | 'apply' | 'live';
/** done = nothing waits here · waiting = something sits here for a person · blocked = cannot move on · idle = empty. */
export type StageState = 'done' | 'waiting' | 'blocked' | 'idle';

export interface PipelineStage {
	key: StageKey;
	title: string;
	state: StageState;
	/** One short fact (`4 changes`, `cfg-2026-10-18.1`). */
	headline: string;
	/** One line of why / what is next. */
	detail: string;
}

export interface PipelineInput {
	drafts: DraftChange[];
	status: ReleaseStatus;
	releases: ReleaseRow[];
	/** J17 validation runs per release cut (null = could not be read). */
	validation: ValidationCut[] | null;
	/** J17 draft validation (`draftValidation()`): scenario runs on the draft (null/omitted = not read). */
	draftTest?: DraftValidation | null;
	/** L2 saved test cases (`testCases()`): `exercised` ledger keys, `stale` once the draft moved on. */
	testCases?: TestCase[] | null;
	/** D15: template layers the person ticked to adopt in the next cut (`approveRelease({adopt_pins})`). */
	adopt?: string[];
}

/** How well one unpublished change has been tried in the Test environment. */
/** `checklist`: a change the Test env cannot exercise (the selection, a Production route): the go-live checklist
 * proves it (compile, contract tests), not a test chat or scenario. */
export type ChangeTest = 'tested' | 'stale' | 'not_tested' | 'checklist';

/** Whether a test-env turn can exercise draft change `key`: a test chat runs the TEST routes of the draft selection, so
 * the selection itself and Production routes (`route:<op>:prod`, connectors::places) are proven elsewhere. */
export function testableInTest(key: string): boolean {
	return key !== 'selection' && !/^route:.+:prod$/.test(key);
}

export interface ChangeEvidence {
	change: DraftChange;
	state: ChangeTest;
	/** Scenarios whose design location this change touches (J17), and what the latest run said about them. */
	scenarios: number;
	regressions: number;
	fixed: number;
	/** Saved test cases (L2) that exercised it: still on this draft / saved on an older draft. */
	cases: number;
	stale_cases: number;
}

export interface TestEvidence {
	/** nothing = no unpublished change; else the worst state of any change. */
	state: 'nothing' | ChangeTest;
	counts: Record<ChangeTest, number>;
	changes: ChangeEvidence[];
	/** Changes no scenario covers and no test case exercised. */
	unexercised: DraftChange[];
	run: { draft_rev: number; at: string; by: string; pass: number; total: number; fixed: number } | null;
	/** Latest run vs the draft now: `current` = it tested exactly this draft. */
	run_state: DraftValidation['state'] | null;
	regressions: number;
	cases: { total: number; current: number; stale: number };
	draft_rev: number | null;
}

export interface TestResult {
	release: string;
	ran_at: string | null;
	pass: number;
	rejected: number;
	pending: number;
	total: number;
}

export type NextAction = 'fix_selection' | 'fix_readiness' | 'apply' | 'test_and_cut' | 'cut' | 'none';

export interface NextStep {
	action: NextAction;
	title: string;
	body: string;
}

const plural = (n: number, one: string, many = `${one}s`) => `${n} ${n === 1 ? one : many}`;

/** Changes not carried by any approved cut yet — the next cut bundles them. */
export function pendingDrafts(drafts: DraftChange[]): DraftChange[] {
	return drafts.filter((d) => !d.in_release);
}

export function testResultOf(cut: ValidationCut | undefined): TestResult | null {
	if (!cut) return null;
	const n = (v: string) => cut.rows.filter((r) => r.verdict === v).length;
	return {
		release: cut.release.id,
		ran_at: cut.ran_at,
		pass: n('pass'),
		rejected: n('rejected'),
		pending: n('pending'),
		total: cut.rows.length,
	};
}

/**
 * D54 "tested before publish": what the Test environment has proven about each unpublished change. A change is
 * tested when the latest J17 run covered it with its current value, or a test case saved on this draft exercised it;
 * stale when only an older run / older test cases tried it (it changed since); not tested otherwise.
 */
export function testEvidence(
	drafts: DraftChange[],
	v: DraftValidation | null | undefined,
	cases: TestCase[] | null | undefined,
): TestEvidence {
	const pending = pendingDrafts(drafts);
	const tc = cases ?? [];
	const changes: ChangeEvidence[] = pending.map((d) => {
		const imp = v?.impacts.find((i) => i.change_id === d.id || i.key === d.key);
		const scen = imp?.covered ?? 0;
		const fresh = tc.filter((c) => !c.stale && c.exercised.includes(d.key)).length;
		const old = tc.filter((c) => c.stale && c.exercised.includes(d.key)).length;
		const runTested = !!imp && scen > 0 && !imp.untested && (v?.runs.length ?? 0) > 0;
		const runStale = !!imp && scen > 0 && imp.untested && (v?.runs.length ?? 0) > 0;
		const state: ChangeTest =
			runTested || fresh
				? 'tested'
				: runStale || old
					? 'stale'
					: testableInTest(d.key)
						? 'not_tested'
						: 'checklist';
		return {
			change: d,
			state,
			scenarios: scen,
			regressions: runTested ? (imp?.regressions ?? 0) : 0,
			fixed: runTested ? (imp?.fixed ?? 0) : 0,
			cases: fresh,
			stale_cases: old,
		};
	});
	const counts: Record<ChangeTest, number> = { tested: 0, stale: 0, not_tested: 0, checklist: 0 };
	for (const c of changes) counts[c.state]++;
	const run = v?.runs[0] ?? null;
	return {
		state: !changes.length ? 'nothing' : counts.not_tested ? 'not_tested' : counts.stale ? 'stale' : 'tested',
		counts,
		changes,
		unexercised: changes
			.filter((c) => c.state !== 'checklist' && !c.scenarios && !c.cases && !c.stale_cases)
			.map((c) => c.change),
		run: run
			? {
					draft_rev: run.draft_rev,
					at: run.at,
					by: run.by_name,
					pass: run.summary.pass,
					total: run.summary.total,
					fixed: run.summary.fixed,
				}
			: null,
		run_state: v?.state ?? null,
		regressions: v?.state === 'current' ? (run?.summary.regressions ?? 0) : 0,
		cases: {
			total: tc.length,
			current: tc.filter((c) => !c.stale).length,
			stale: tc.filter((c) => c.stale).length,
		},
		draft_rev: v?.current.draft_rev ?? null,
	};
}

const evidenceKnown = (input: PipelineInput) => input.draftTest != null || input.testCases != null;

/** The release each stage is about: the newest approved (waiting) cut and the current (live) one. */
export function releasesOf(input: Pick<PipelineInput, 'releases' | 'status'>) {
	const current = input.releases.find((r) => r.state === 'current') ?? null;
	const waiting = input.status.approved_waiting
		? (input.releases.find((r) => r.id === input.status.approved_waiting) ?? null)
		: null;
	return { current, waiting };
}

export function pipelineStages(input: PipelineInput): PipelineStage[] {
	const pending = pendingDrafts(input.drafts);
	const ups = input.status.newer_pins;
	const { current, waiting } = releasesOf(input);
	const blockers = input.status.missing_slots.length;
	const cuts = input.validation ?? [];
	const tested = (id: string | undefined) => !!id && !!cuts.find((c) => c.release.id === id)?.ran_at;

	const draft: PipelineStage = {
		key: 'draft',
		title: 'Draft',
		state: pending.length || ups.length ? 'waiting' : 'done',
		headline: pending.length ? plural(pending.length, 'unpublished change') : 'No unpublished changes',
		detail: ups.length
			? `${plural(ups.length, 'template upgrade')} available (${ups.map((p) => `${p.layer}@v${p.latest}`).join(', ')}).`
			: pending.length
				? 'Saved on the design pages; not live until published.'
				: 'Everything you saved is in a release.',
	};

	const lastRun = [...cuts].filter((c) => c.ran_at).sort((a, b) => b.ran_at!.localeCompare(a.ran_at!))[0];
	const ev = evidenceKnown(input) ? testEvidence(input.drafts, input.draftTest, input.testCases) : null;
	const test: PipelineStage = ev
		? testStage(ev)
		: {
				key: 'test',
				title: 'Test',
				state: pending.length || (waiting && !tested(waiting.id)) ? 'waiting' : 'done',
				headline: pending.length
					? 'Draft not validated'
					: waiting && !tested(waiting.id)
						? `${waiting.id} not validated`
						: lastRun
							? `Validated ${lastRun.release.id}`
							: 'Not validated yet',
				detail: pending.length
					? 'Try the draft in the test chat; validation runs on a cut.'
					: lastRun
						? `Last validation run: ${lastRun.release.id}.`
						: 'Run the validation grid on a cut.',
			};

	const cut: PipelineStage = waiting
		? {
				key: 'cut',
				title: 'Cut & approve',
				state: 'waiting',
				headline: waiting.id,
				detail: pending.length
					? `Approved; ${plural(pending.length, 'newer change')} not in it — cut again to include them.`
					: 'Approved and frozen; waiting to be applied.',
			}
		: {
				key: 'cut',
				title: 'Cut & approve',
				state: pending.length || input.adopt?.length ? 'waiting' : 'idle',
				headline: pending.length || input.adopt?.length ? 'Ready to cut' : 'Nothing to cut',
				detail: input.adopt?.length
					? `The cut freezes the draft and adopts ${input.adopt.map((l) => `${l}@v${ups.find((u) => u.layer === l)?.latest ?? '?'}`).join(', ')}.`
					: pending.length
						? 'A cut freezes the draft into an immutable release.'
						: ups.length
							? 'Tick a template upgrade to cut a release that adopts it.'
							: 'No draft changes and no template change.',
			};

	const apply: PipelineStage = waiting
		? {
				key: 'apply',
				title: 'Apply to Production',
				state: blockers ? 'blocked' : 'waiting',
				headline: blockers ? `Blocked · ${plural(blockers, 'readiness gap')}` : 'Ready to apply',
				detail: blockers
					? `${input.status.missing_slots.map((m) => `${m.dependency}.${m.slot}`).join(', ')} missing.`
					: `Applying makes ${waiting.id} current.`,
			}
		: {
				key: 'apply',
				title: 'Apply to Production',
				state: 'idle',
				headline: 'Nothing waiting',
				detail: 'Approved cuts wait here until applied.',
			};

	const live: PipelineStage = {
		key: 'live',
		title: 'Live',
		state: current ? 'done' : 'idle',
		headline: current ? current.id : 'Nothing live yet',
		detail: current ? 'Production runs this release.' : 'Apply a release to go live.',
	};

	return [draft, test, cut, apply, live];
}

/** The Test stage from real evidence: tested / stale / not tested, and regressions the latest run found. */
export function testStage(ev: TestEvidence): PipelineStage {
	const c = ev.counts;
	const base = { key: 'test' as const, title: 'Test' };
	if (ev.state === 'nothing')
		return {
			...base,
			state: 'done',
			headline: 'Nothing to test',
			detail: 'No unpublished change; the test chat runs what Production runs.',
		};
	const n = ev.changes.length;
	if (ev.regressions)
		return {
			...base,
			state: 'waiting',
			headline: `${plural(ev.regressions, 'regression')} on rev ${ev.run?.draft_rev ?? '?'}`,
			detail: `${c.tested} of ${n} changes tested; a scenario Production passes fails on the draft. Fix it before you publish.`,
		};
	if (ev.state === 'tested')
		return {
			...base,
			state: 'done',
			headline: `Tested · ${plural(n, 'change')}`,
			detail: ev.run
				? `Draft rev ${ev.run.draft_rev}: ${ev.run.pass} of ${ev.run.total} scenarios pass, no regressions.`
				: `Every change was exercised in the test chat on this draft.`,
		};
	const parts = [
		c.tested ? `${c.tested} tested` : null,
		c.stale ? `${c.stale} stale` : null,
		c.not_tested ? `${c.not_tested} not tested` : null,
	].filter(Boolean);
	return {
		...base,
		state: 'waiting',
		headline: parts.join(' · '),
		detail:
			ev.state === 'stale'
				? 'The draft changed since it was tested: run the scenarios again or retry the test chat.'
				: ev.unexercised.length
					? `${plural(ev.unexercised.length, 'change')} no test exercised. Try ${ev.unexercised.length === 1 ? 'it' : 'them'} in the test chat.`
					: 'Run the scenarios on the draft before you publish.',
	};
}

/** The single decision this page asks for now. */
export function nextStep(input: PipelineInput): NextStep {
	const pending = pendingDrafts(input.drafts);
	const { current, waiting } = releasesOf(input);
	const missing = input.status.missing_slots;
	if (input.status.selection_error)
		return {
			action: 'fix_selection',
			title: 'Fix the selection first',
			body: `${input.status.selection_error} Nothing can be cut until it compiles.`,
		};
	if (waiting && missing.length)
		return {
			action: 'fix_readiness',
			title: `${waiting.id} cannot reach Production yet`,
			body:
				`Apply is refused until ${plural(missing.length, 'readiness gap')} ${missing.length === 1 ? 'is' : 'are'} fixed. ` +
				(pending.length
					? `Your ${plural(pending.length, 'newer change')} ${pending.length === 1 ? 'is' : 'are'} not in it: test ${pending.length === 1 ? 'it' : 'them'} in the test chat, then cut again so everything ships together.`
					: 'Fix them on Readiness, then apply.'),
		};
	if (waiting)
		return {
			action: 'apply',
			title: `Apply ${waiting.id} to Production`,
			body: pending.length
				? `It is ready. ${plural(pending.length, 'newer change')} will wait for the next cut.`
				: `It is ready; applying makes it current${current ? ` instead of ${current.id}` : ''}.`,
		};
	if (pending.length) {
		const ev = evidenceKnown(input) ? testEvidence(input.drafts, input.draftTest, input.testCases) : null;
		if (ev?.regressions)
			return {
				action: 'test_and_cut',
				title: `Fix ${plural(ev.regressions, 'regression')} before you publish`,
				body: 'The latest test run found a scenario that Production answers correctly and the draft does not. Change or discard the draft change behind it, run the scenarios again, then Approve & release.',
			};
		if (ev?.state === 'tested')
			return {
				action: 'test_and_cut',
				title: `Tested: publish ${plural(pending.length, 'change')}`,
				body: 'Every change was tried on this draft without regressions. Approve & release cuts it and applies it in one step.',
			};
		if (ev)
			return {
				action: 'test_and_cut',
				title: `Test ${plural(ev.counts.stale + ev.counts.not_tested, 'change')} before you publish`,
				body: `${ev.counts.tested} tested, ${ev.counts.stale} stale, ${ev.counts.not_tested} not tested. Run the scenarios on the draft or try it in the test chat; then Approve & release.`,
			};
		return {
			action: 'test_and_cut',
			title: `Test, then publish ${plural(pending.length, 'change')}`,
			body: 'Try the draft in the test chat. When it behaves, Approve & release cuts it and applies it in one step.',
		};
	}
	if (input.status.base_changed || input.status.newer_pins.length)
		return {
			action: 'cut',
			title: 'A template change is waiting',
			body: input.status.newer_pins.length
				? 'A newer template tag is out. Tick it under “What the next release publishes” to adopt it in the next cut; any conflicts it raises with your refinements show up under Conflicts.'
				: 'The selection or the platform layers compile to a different bundle. Cut a release to take it in.',
		};
	return {
		action: 'none',
		title: 'Nothing to publish',
		body: current
			? `Production runs ${current.id}. Changes you save on the design pages collect here.`
			: 'Save changes on the design pages; they collect here as the draft.',
	};
}
