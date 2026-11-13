// Task 13: the PR-style release flow — stated blockers, the steps strip, and bookmarked old URLs.
import { describe, expect, it } from 'vitest';
import {
	approveReason,
	currentStep,
	goLiveReason,
	releaseTabOf,
	type FlowInput,
} from '../../src/lib/release/flow';
import { REDIRECTS, redirectTarget } from '../../src/lib/redirects';

const base: FlowInput = {
	changes: 2,
	blockers: 0,
	conflicts: 0,
	testsNeeded: false,
	waiting: false,
	canApprove: true,
};
const f = (o: Partial<FlowInput>): FlowInput => ({ ...base, ...o });

describe('Go live / Approve reasons', () => {
	it('nothing to release when the draft equals live', () => {
		expect(goLiveReason(f({ changes: 0 }))).toBe('Nothing to release');
		expect(approveReason(f({ changes: 0 }))).toBe('Nothing to release');
	});
	it('a waiting approved release is something to release', () => {
		expect(goLiveReason(f({ changes: 0, waiting: true }))).toBeNull();
	});
	it('reasons follow the stated order', () => {
		const all = f({ blockers: 2, conflicts: 3, testsNeeded: true });
		expect(goLiveReason(all)).toBe('Fix 2 go-live blockers');
		expect(goLiveReason({ ...all, blockers: 1 })).toBe('Fix 1 go-live blocker');
		expect(goLiveReason({ ...all, blockers: 0 })).toBe('Resolve 3 conflicts');
		expect(goLiveReason({ ...all, blockers: 0, conflicts: 1 })).toBe('Resolve 1 conflict');
		expect(goLiveReason({ ...all, blockers: 0, conflicts: 0 })).toBe('Run the test suite');
		expect(goLiveReason(f({}))).toBe('Needs approval by a release manager');
		expect(goLiveReason(f({ waiting: true }))).toBeNull();
	});
	it('Approve states the same checks, and who may approve', () => {
		expect(approveReason(f({ blockers: 2 }))).toBe('Fix 2 go-live blockers');
		expect(approveReason(f({ conflicts: 1 }))).toBe('Resolve 1 conflict');
		expect(approveReason(f({ testsNeeded: true }))).toBe('Run the test suite');
		expect(approveReason(f({ canApprove: false }))).toBe('Needs approval by a release manager');
		expect(approveReason(f({}))).toBeNull();
	});
});

describe('steps strip', () => {
	it('Choose changes → Pass checks → Approve → Go live', () => {
		expect(currentStep(f({ changes: 0 }))).toBe(0);
		expect(currentStep(f({ blockers: 1 }))).toBe(1);
		expect(currentStep(f({ conflicts: 1 }))).toBe(1);
		expect(currentStep(f({ testsNeeded: true }))).toBe(1);
		expect(currentStep(f({}))).toBe(2);
		expect(currentStep(f({ waiting: true }))).toBe(3);
		expect(currentStep(f({ waiting: true, blockers: 1 }))).toBe(1);
	});
});

describe('bookmarked old URLs', () => {
	it('/refinements redirects to the changes tab of Releases', () => {
		expect(REDIRECTS['/admin/jobs/[job]/refinements']).toBe('/admin/jobs/[job]/release?tab=changes');
		expect(redirectTarget('/admin/jobs/[job]/refinements', 'customer_care')).toBe(
			'/admin/jobs/customer_care/release?tab=changes',
		);
		expect(redirectTarget('/admin/jobs/[job]/release', 'customer_care')).toBeNull();
	});
	it('#conflicts opens the changes tab with the conflicts expanded', () => {
		expect(releaseTabOf('', '#conflicts')).toEqual({ tab: 'changes', conflictsOpen: true });
		expect(releaseTabOf('?tab=history', '')).toEqual({ tab: 'history', conflictsOpen: false });
		expect(releaseTabOf('?tab=template-updates', '')).toEqual({
			tab: 'template-updates',
			conflictsOpen: false,
		});
		expect(releaseTabOf('?tab=nope', '')).toEqual({ tab: 'changes', conflictsOpen: false });
		expect(releaseTabOf('', '')).toEqual({ tab: 'changes', conflictsOpen: false });
	});
});

describe('the /refinements route', () => {
	it('redirects (307) to the changes tab of the same job', async () => {
		const { load } = await import('../../src/routes/admin/jobs/[job]/refinements/+page');
		let thrown: { status?: number; location?: string } | undefined;
		try {
			load({ params: { job: 'customer_care' } });
		} catch (e) {
			thrown = e as { status?: number; location?: string };
		}
		expect(thrown).toMatchObject({ status: 307, location: '/admin/jobs/customer_care/release?tab=changes' });
	});
});
