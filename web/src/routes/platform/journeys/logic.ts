// P8 index (`/platform/journeys`): the journey grids per industry (A1). `journeyIndustries` answers one row per
// industry (or per industry + department); the index groups them by industry and lists the industries the platform has
// not drafted yet apart. Pure functions, no api() calls.
import type { IndustryJourneySummary, JourneyStatus } from '$lib/api/types/journey';

export interface IndexDepartment {
	/** `null` for an industry-level grid. */
	department: string | null;
	label: string;
	status: JourneyStatus | 'not_started';
	tag: string | null;
	activities: number;
	red: number;
	href: string;
}

export interface IndexIndustry {
	industry: string;
	label: string;
	/** The least complete status of its grids: draft < partial < tagged. */
	status: JourneyStatus;
	tag: string | null;
	activities: number;
	red: number;
	href: string;
	/** Every grid of the industry; more than one when it has department grids. */
	grids: IndexDepartment[];
}

export interface JourneyIndex {
	started: IndexIndustry[];
	notStarted: { industry: string; label: string; href: string }[];
	totals: { industries: number; red: number; notStarted: number };
}

const RANK: Record<JourneyStatus, number> = { draft: 0, partial: 1, tagged: 2 };

export const journeyHref = (industry: string, department?: string | null): string =>
	`/platform/journeys/${encodeURIComponent(industry)}${department ? `?department=${encodeURIComponent(department)}` : ''}`;

/** A department key as words: `customer_service` → `customer service`. */
export const departmentLabel = (r: Pick<IndustryJourneySummary, 'department' | 'department_label'>): string =>
	r.department_label ?? (r.department ?? '').replaceAll('_', ' ');

/** `GET /api/platform/journeys` rows → the industries with grids, and the not-started ones. */
export function groupJourneys(rows: readonly IndustryJourneySummary[]): JourneyIndex {
	const byIndustry = new Map<string, IndustryJourneySummary[]>();
	const notStartedRows: IndustryJourneySummary[] = [];
	for (const r of rows) {
		if (r.status === 'not_started') {
			if (!notStartedRows.some((n) => n.industry === r.industry)) notStartedRows.push(r);
			continue;
		}
		byIndustry.set(r.industry, [...(byIndustry.get(r.industry) ?? []), r]);
	}
	const started: IndexIndustry[] = [...byIndustry.values()].map((rs) => {
		const grids: IndexDepartment[] = rs.map((r) => ({
			department: r.department ?? null,
			label: r.department ? departmentLabel(r) : 'Industry grid',
			status: r.status,
			tag: r.tag,
			activities: r.activities,
			red: r.red,
			href: journeyHref(r.industry, r.department),
		}));
		const first = rs[0]!;
		const status = rs.reduce<JourneyStatus>(
			(worst, r) => (r.status !== 'not_started' && RANK[r.status] < RANK[worst] ? r.status : worst),
			'tagged',
		);
		return {
			industry: first.industry,
			label: first.label,
			status,
			tag: rs.length === 1 ? first.tag : (rs.find((r) => !r.department)?.tag ?? null),
			activities: rs.reduce((n, r) => n + r.activities, 0),
			red: rs.reduce((n, r) => n + r.red, 0),
			// an industry with several grids opens on its first one; the page lists every grid
			href: journeyHref(first.industry, first.department),
			grids,
		};
	});
	const notStarted = notStartedRows
		.filter((n) => !byIndustry.has(n.industry))
		.map((n) => ({
			industry: n.industry,
			label: n.label,
			href: `/platform/generator?new=${encodeURIComponent(n.industry)}`,
		}));
	return {
		started,
		notStarted,
		totals: {
			industries: started.length,
			red: started.reduce((n, i) => n + i.red, 0),
			notStarted: notStarted.length,
		},
	};
}
