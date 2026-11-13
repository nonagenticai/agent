// Analytics view-models (J1 KPIs, J2 charts). Adapted from cb:web/src/routes/admin/jobs/[job]/analytics/series.ts
// @45ee128 onto the usage-based fixture shapes (turns, contained, escalated, not eligible).
import type { JobAnalytics } from '$lib/api/types/runtime';

/** Relative change of a rate or count against the previous window; null when there is nothing to compare. */
export function kpiDelta(value: number | null, previous: number | null): number | null {
	if (value == null || previous == null || previous === 0) return null;
	return (value - previous) / previous;
}

const total = (a: JobAnalytics, k: 'turns' | 'cases' | 'contained' | 'escalated' | 'not_eligible') =>
	a.days.reduce((n, d) => n + d[k], 0);

export interface AnalyticsTotals {
	turns: number;
	cases: number;
	containment: number | null;
	escalation: number | null;
	containmentPrev: number | null;
	escalationPrev: number | null;
}

export function totals(a: JobAnalytics): AnalyticsTotals {
	const cases = total(a, 'cases');
	const p = a.previous;
	return {
		turns: total(a, 'turns'),
		cases,
		containment: cases ? total(a, 'contained') / cases : null,
		escalation: cases ? total(a, 'escalated') / cases : null,
		containmentPrev: p.cases ? p.contained / p.cases : null,
		escalationPrev: p.cases ? p.escalated / p.cases : null,
	};
}

/** Stacked series for "cases per day by outcome": contained, not eligible, escalated, and the rest (open). */
export function toSeries(a: JobAnalytics) {
	return [
		{ key: 'contained', label: 'Contained', cls: 's-ok', values: a.days.map((d) => d.contained) },
		{ key: 'not_eligible', label: 'Not eligible', cls: 's-warn', values: a.days.map((d) => d.not_eligible) },
		{ key: 'escalated', label: 'Escalated to staff', cls: 's-err', values: a.days.map((d) => d.escalated) },
		{
			key: 'other',
			label: 'Still open / abandoned',
			cls: 's-cust',
			values: a.days.map((d) => Math.max(0, d.cases - d.contained - d.not_eligible - d.escalated)),
		},
	];
}

/** True when the window holds no conversation at all (a new job). */
export const isEmptyWindow = (a: JobAnalytics) => a.days.every((d) => d.turns === 0);
