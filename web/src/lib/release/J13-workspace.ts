// J13 — the job's draft workspace (D54, owner 2026-10-19). Pure helpers: the base/refinements/draft stack, the
// Draft → Test → Publish path, where each unpublished change lives, and which table rows a draft change touches.
import { hrefOf, pageById, routesOf } from '$lib/pages';
import { jobPath } from '$lib/scope';
import type { TemplatePin } from '$lib/api/types/core';
import type { DraftChange, ReleaseConflict } from '$lib/api/types/release';

/** Design areas as a tenant admin names them. */
export const AREA_LABEL: Record<string, string> = {
	refinements: 'Refinements',
	settings: 'Job settings',
	gates: 'Gates',
	rules: 'Business rules',
	ontology: 'Ontology',
	routes: 'Routes',
	toolsets: 'Toolsets',
	knowledge: 'Knowledge',
	procedures: 'Procedures',
	journey: 'Journey',
	selection: 'Selection',
	handover: 'Handover SLA',
};

export const areaLabel = (area: string) => AREA_LABEL[area] ?? area.replace(/_/g, ' ');

/** The design page that owns a change, as a link (title + href); null href for an unknown page id. */
export function pageLink(pageId: string, job: string): { title: string; href: string | null } {
	const p = pageById(pageId);
	if (!p) return { title: pageId, href: null };
	return { title: p.title, href: hrefOf(p, routesOf(p)[0]!, job) };
}

/** Ledger key of a refinable row (`overlay:<object>:<key>`) or a job setting (`setting:<key>`). */
export const overlayDraftKey = (object: string, key: string) => `overlay:${object}:${key}`;
export const settingDraftKey = (key: string) => `setting:${key}`;

/** The unpublished change at a ledger key, if any. */
export function draftAt(changes: DraftChange[], key: string): DraftChange | undefined {
	return changes.find((c) => c.key === key);
}

/** Pages with unpublished changes, most changes first (then page id), with their counts. */
export function changesByPage(changes: DraftChange[]): { page: string; count: number }[] {
	const m = new Map<string, number>();
	for (const c of changes) m.set(c.page, (m.get(c.page) ?? 0) + 1);
	return [...m.entries()]
		.map(([page, count]) => ({ page, count }))
		.sort((a, b) => b.count - a.count || a.page.localeCompare(b.page, undefined, { numeric: true }));
}

/** Template layers bottom-up as the stack draws them (`base` first, the job layer last). */
export function baseLayers(pins: TemplatePin[]): { layer: string; tag: string; scope: string }[] {
	const rank = (l: string) =>
		l.startsWith('base')
			? 0
			: l.startsWith('profile')
				? 1
				: l.startsWith('industry_dept')
					? 3
					: l.startsWith('industry')
						? 2
						: l.startsWith('job')
							? 4
							: 5;
	return [...pins]
		.sort((a, b) => rank(a.layer) - rank(b.layer) || a.layer.localeCompare(b.layer))
		.map((p) => ({
			layer: p.layer,
			tag: `${p.layer}@v${p.version}`,
			scope: p.layer.split('/')[0]!.replace(/_/g, ' '),
		}));
}

export type PathStatus = 'done' | 'current' | 'todo';
export interface PathStep {
	key: 'draft' | 'test' | 'publish';
	label: string;
	status: PathStatus;
	detail: string;
	links: { label: string; href: string }[];
}

/**
 * Draft → Test → Publish for this job right now. With nothing unpublished the draft equals Production and the path
 * is idle; with changes the draft is the current step and Test / Publish are next.
 */
export function workspacePath(opts: {
	job: string;
	drafts: number;
	pages: number;
	liveRelease: string | null;
	approvedWaiting: string | null;
	newerPins: number;
}): PathStep[] {
	const { job, drafts, pages, liveRelease, approvedWaiting, newerPins } = opts;
	const some = drafts > 0;
	const publish: string[] = [];
	if (some) publish.push(`the next cut bundles your ${drafts} change${drafts === 1 ? '' : 's'}`);
	if (newerPins) publish.push(`${newerPins} template upgrade${newerPins === 1 ? '' : 's'}`);
	const waiting = approvedWaiting ? ` ${approvedWaiting} is approved and waits to be applied.` : '';
	return [
		{
			key: 'draft',
			label: 'Draft',
			status: some ? 'current' : 'done',
			detail: some
				? `${drafts} unpublished change${drafts === 1 ? '' : 's'} on ${pages} page${pages === 1 ? '' : 's'}. Saved, not live.`
				: `Nothing unpublished — the draft is what Production runs${liveRelease ? ` (${liveRelease})` : ''}.`,
			links: [],
		},
		{
			key: 'test',
			label: 'Test',
			status: 'todo',
			detail: some
				? 'The test chat and test-env routes run the draft. Check the scenarios still pass.'
				: 'The test chat runs the same design as Production.',
			links: [
				{ label: 'Test in test chat', href: '/chat' },
				{ label: 'Validation grid', href: jobPath(job, '/validation') },
			],
		},
		{
			key: 'publish',
			label: 'Publish',
			status: 'todo',
			detail:
				(publish.length
					? `Approve a release: ${publish.join(' + ')}; apply makes it live.`
					: 'Nothing new to publish.') + waiting,
			links: [{ label: 'Review & publish', href: jobPath(job, '/release') }],
		},
	];
}

/** Open conflicts first (newest first), each once per (object, key) — the newest wins. */
export function openConflicts(rows: ReleaseConflict[]): ReleaseConflict[] {
	const seen = new Set<string>();
	return rows
		.filter((c) => c.status === 'open')
		.sort((a, b) => b.detected_at.localeCompare(a.detected_at) || b.id - a.id)
		.filter((c) => {
			const k = `${c.object}|${c.key}`;
			if (seen.has(k)) return false;
			seen.add(k);
			return true;
		});
}
