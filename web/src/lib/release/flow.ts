// Releases as a PR-style flow: which step a release is on and why Approve / Go live are disabled. Display only —
// the backend still refuses what it refuses (D67); this states the reason up front instead of after a click.
export type ReleaseTab = 'changes' | 'history' | 'template-updates';
export const RELEASE_TABS: readonly ReleaseTab[] = ['changes', 'history', 'template-updates'];

export interface FlowInput {
	/** Draft changes + template upgrades to adopt + a changed selection: what a new release would carry. */
	changes: number;
	/** Go-live checklist items that must be fixed (`checklistOf(board).mustFix`). */
	blockers: number;
	/** Open conflicts with your changes. */
	conflicts: number;
	/** The test suite must run (or run again) before this can go live. */
	testsNeeded: boolean;
	/** An approved release is waiting to go live. */
	waiting: boolean;
	/** The viewer may approve (release manager). */
	canApprove: boolean;
}

export interface Check {
	key: string;
	label: string;
	state: 'ok' | 'err' | 'warn' | 'muted';
	detail: string;
	href?: string;
	linkLabel?: string;
}

export const STEPS = ['Choose changes', 'Pass checks', 'Approve', 'Go live'] as const;

const plural = (n: number, w: string) => `${n} ${w}${n === 1 ? '' : 's'}`;

/** The first failing check, in the order the page states them. */
function checkReason(i: FlowInput): string | null {
	if (i.blockers) return `Fix ${plural(i.blockers, 'go-live blocker')}`;
	if (i.conflicts) return `Resolve ${plural(i.conflicts, 'conflict')}`;
	if (i.testsNeeded) return 'Run the test suite';
	return null;
}

export function approveReason(i: FlowInput): string | null {
	if (!i.changes) return 'Nothing to release';
	return checkReason(i) ?? (i.canApprove ? null : 'Needs approval by a release manager');
}

export function goLiveReason(i: FlowInput): string | null {
	if (!i.changes && !i.waiting) return 'Nothing to release';
	const c = checkReason(i);
	if (c) return c;
	return i.waiting ? null : 'Needs approval by a release manager';
}

/** 0 Choose changes · 1 Pass checks · 2 Approve · 3 Go live. */
export function currentStep(i: FlowInput): 0 | 1 | 2 | 3 {
	if (!i.changes && !i.waiting) return 0;
	if (checkReason(i)) return 1;
	return i.waiting ? 3 : 2;
}

/** The tab a release URL asks for (`?tab=`), and whether `#conflicts` asks for the conflicts to be expanded. */
export function releaseTabOf(search: string, hash: string): { tab: ReleaseTab; conflictsOpen: boolean } {
	const conflictsOpen = hash === '#conflicts';
	const t = new URLSearchParams(search).get('tab');
	const tab = (RELEASE_TABS as readonly string[]).includes(t ?? '') ? (t as ReleaseTab) : 'changes';
	return { tab, conflictsOpen };
}
