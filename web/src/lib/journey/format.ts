// Small display helpers shared by the journey area pages.
import type { GeneratorRunSummary, GeneratorStepKey } from '$lib/api/types/journey';
import type { Funding } from '$lib/api/types/tenant';
import { duration } from '$lib/format';

/** Fake-timer lengths (tests set them to 0). */
export const journeyUi = { draftMs: 1600, pollMs: 1000 };

/** The fixture clock SLA chips count against, so the queue reads the same on every visit. */
export const FIXTURE_NOW = '2026-10-18T12:00:00Z';

export const STEP_LABEL: Record<GeneratorStepKey, string> = {
	input: '0 · Input',
	business_model: '1 · Business model',
	journey: 'A1–A2 · Journey + activities',
	objects: 'A3–A4 · Objects + example data',
	operations: 'C1–C5 · Operations + JSON',
	harmonise: 'Harmonise',
	validate: 'Validate',
	use_cases: 'Use cases',
	package: 'Package',
};

export const STEP_HINT: Record<GeneratorStepKey, string> = {
	input: 'Industry, department, job',
	business_model: 'Axis picker; the LLM proposes axis values from a description',
	journey: 'Themes, stage descriptions, priors, customer/company activities per stage',
	objects: 'Activity × object matrix: canonical objects, field subsets, example data; flag new objects',
	operations: 'do / request / observe operations with success JSON and error states',
	harmonise: 'B topics that extend A activities and vice versa',
	validate: 'Rules 1–7; every red item blocks tagging',
	use_cases: 'Title, actors, flows, sample dialogue, north-star metric',
	package: 'Approve and tag @vN (platform) or save refinements (tenant)',
};

export function tokens(n: number): string {
	return n >= 1e6 ? `${(n / 1e6).toFixed(2)} M` : n >= 1e3 ? `${(n / 1e3).toFixed(1)} k` : String(n);
}

export function eur(n: number): string {
	return `€${n.toFixed(2)}`;
}

/** Funding-lane labels (same lanes as conversation turns on T5 Usage). */
export const FUNDING_LABEL: Record<Funding, string> = {
	local_gpu: 'Internal GPU',
	platform_external: 'Platform external',
	tenant_byo: 'Tenant key (BYO)',
};

/**
 * D56: a generator run's usage, never a bill — tokens on a funding lane, attributed to the tenant (+ project, activity
 * `generator`) for tenant runs, or to the platform's own usage for platform runs.
 */
export function costLine(r: Pick<GeneratorRunSummary, 'cost' | 'tenant_name' | 'scope'>): string {
	const lane = FUNDING_LABEL[r.cost.funding];
	return r.cost.attributed_to === 'tenant'
		? `${tokens(r.cost.tokens)} tokens · ${lane} · usage of ${r.tenant_name} (activity generator)`
		: `${tokens(r.cost.tokens)} tokens · ${lane} · platform run, the platform's own usage (no tenant)`;
}

export function when(iso: string | null): string {
	return iso ? iso.slice(0, 16).replace('T', ' ') : '—';
}

/** "1 h 40 min left" / "overdue 50 min" against `now`. */
export function slaText(due: string, now = FIXTURE_NOW): { text: string; variant: 'ok' | 'warn' | 'err' } {
	const mins = Math.round((Date.parse(due) - Date.parse(now)) / 60000);
	const fmt = duration;
	if (mins < 0) return { text: `overdue ${fmt(-mins)}`, variant: 'err' };
	return { text: `${fmt(mins)} left`, variant: mins < 60 ? 'warn' : 'ok' };
}
