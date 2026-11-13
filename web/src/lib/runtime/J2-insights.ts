// J2 Analytics view-model: turns the analytics payload into the answers an operator asks, in order —
// "are we on target?", "did the last release help?", "where does it fall short?", "why do people take over, and
// who can fix that?", "are systems and gates holding up?". Pure functions; owned by the J2 page (runtime area).
import type { DayPoint, JobAnalytics, OperationStat, ReleaseWindow } from '$lib/api/types/runtime';
import type { StageKey } from '$lib/api/types/journey';

// ---- headline ---------------------------------------------------------------------------------------------------

export type TargetState = 'met' | 'close' | 'below' | 'unmeasured';

/** Where the north star stands against its target. "close" = within 5 points. */
export function targetState(value: number | null, target: number | null): TargetState {
	if (value == null) return 'unmeasured';
	if (target == null) return 'met';
	if (value >= target) return 'met';
	return target - value <= 0.05 ? 'close' : 'below';
}

const pts = (x: number) => `${Math.round(Math.abs(x) * 100)} pts`;
const pct = (x: number) => `${Math.round(x * 100)}%`;

/** One plain sentence answering "is the agent doing its job?". */
export function headline(value: number | null, target: number | null, previous: number | null): string {
	if (value == null) return 'Not measured for this job yet.';
	const vs =
		target == null
			? 'No target is set on the job.'
			: value >= target
				? `That meets the ${pct(target)} target.`
				: `${pts(target - value)} short of the ${pct(target)} target.`;
	const trend =
		previous == null
			? ''
			: Math.abs(value - previous) < 0.005
				? ' Flat on the previous window.'
				: ` ${value > previous ? 'Up' : 'Down'} ${pts(value - previous)} on the previous window.`;
	return `${pct(value)} of cases ended without a person. ${vs}${trend}`;
}

// ---- trend ------------------------------------------------------------------------------------------------------

export interface TrendPoint {
	day: string;
	cases: number;
	/** Rolling containment / handover share of cases over up to `span` days ending this day; null without cases. */
	containment: number | null;
	handover: number | null;
}

/** Rolling rates so a quiet Sunday does not read as a collapse. `span` 1 = daily. */
export function rollingRates(days: DayPoint[], span = 7): TrendPoint[] {
	return days.map((d, i) => {
		const w = days.slice(Math.max(0, i - span + 1), i + 1);
		const cases = w.reduce((n, x) => n + x.cases, 0);
		return {
			day: d.day,
			cases: d.cases,
			containment: cases ? w.reduce((n, x) => n + x.contained, 0) / cases : null,
			handover: cases ? w.reduce((n, x) => n + x.escalated, 0) / cases : null,
		};
	});
}

/** Releases applied inside the window, as chart markers (day index + id). */
export function releaseMarkers(days: DayPoint[], windows: ReleaseWindow[]) {
	return windows
		.filter((w) => w.applied_at)
		.map((w) => ({ id: w.release_id, day: w.applied_at!.slice(0, 10), current: w.status === 'current' }))
		.map((m) => ({ ...m, index: days.findIndex((d) => d.day === m.day) }))
		.filter((m) => m.index >= 0);
}

// ---- release comparison -------------------------------------------------------------------------------------------

/** Below this many cases in either window a comparison is an early read, not a verdict. */
export const MIN_CASES = 100;
export const MIN_ANSWERS = 30;

export type Verdict = 'better' | 'worse' | 'flat' | 'early' | 'none';

export interface MetricCompare {
	key: 'containment' | 'handover' | 'csat' | 'p50' | 'cost';
	label: string;
	before: number | null;
	after: number | null;
	/** Which direction is good. */
	good: 'up' | 'down';
	verdict: Verdict;
}

export interface ReleaseCompare {
	current: ReleaseWindow;
	previous: ReleaseWindow | null;
	/** Approved but not applied: what to apply next to see its effect. */
	waiting: ReleaseWindow | null;
	metrics: MetricCompare[];
	/** Overall call on the north-star-like metrics (containment + handover). */
	overall: Verdict;
	/** Days the current release has been live, at `now`. */
	liveDays: number;
}

const rate = (n: number, of: number) => (of ? n / of : null);

function judge(
	before: number | null,
	after: number | null,
	good: 'up' | 'down',
	flat: number,
	enough: boolean,
): Verdict {
	if (before == null || after == null) return 'none';
	if (!enough) return 'early';
	const d = after - before;
	if (Math.abs(d) < flat) return 'flat';
	return d > 0 === (good === 'up') ? 'better' : 'worse';
}

/** Compare the live release with the one before it. Null without a live release. */
export function compareReleases(windows: ReleaseWindow[], now: string): ReleaseCompare | null {
	const current = windows.find((w) => w.status === 'current');
	if (!current) return null;
	const earlier = windows.filter((w) => w.status === 'earlier');
	const previous = earlier[0] ?? null;
	const waiting = windows.find((w) => w.status === 'approved') ?? null;
	const enough = !!previous && previous.cases >= MIN_CASES && current.cases >= MIN_CASES;
	const answers =
		!!previous && previous.csat_responses >= MIN_ANSWERS && current.csat_responses >= MIN_ANSWERS;
	const b = previous;
	const metrics: MetricCompare[] = [
		{
			key: 'containment',
			label: 'Resolved without a person',
			before: b ? rate(b.contained, b.cases) : null,
			after: rate(current.contained, current.cases),
			good: 'up',
			verdict: 'none',
		},
		{
			key: 'handover',
			label: 'Handed to staff',
			before: b ? rate(b.escalated, b.cases) : null,
			after: rate(current.escalated, current.cases),
			good: 'down',
			verdict: 'none',
		},
		{
			key: 'csat',
			label: 'Customer rating',
			before: b?.csat ?? null,
			after: current.csat,
			good: 'up',
			verdict: 'none',
		},
		{
			key: 'p50',
			label: 'Median time to answer',
			before: b?.p50_ms ?? null,
			after: current.p50_ms,
			good: 'down',
			verdict: 'none',
		},
		{
			key: 'cost',
			label: 'Tokens per case',
			before: b?.tokens_per_case ?? null,
			after: current.tokens_per_case,
			good: 'down',
			verdict: 'none',
		},
	];
	for (const m of metrics) {
		const flat =
			m.key === 'csat' ? 0.1 : m.key === 'p50' ? 500 : m.key === 'cost' ? 30 : 0.02; /* rates: 2 points */
		m.verdict = judge(m.before, m.after, m.good, flat, m.key === 'csat' ? answers && enough : enough);
	}
	const core = metrics.filter((m) => m.key === 'containment' || m.key === 'handover').map((m) => m.verdict);
	const overall: Verdict = core.includes('none')
		? 'none'
		: core.includes('early')
			? 'early'
			: core.includes('worse')
				? core.includes('better')
					? 'flat'
					: 'worse'
				: core.includes('better')
					? 'better'
					: 'flat';
	const liveDays = current.applied_at ? (Date.parse(now) - Date.parse(current.applied_at)) / 86_400_000 : 0;
	return { current, previous, waiting, metrics, overall, liveDays };
}

// ---- journey stages ---------------------------------------------------------------------------------------------

/** The seven D23 stages, in journey order (11 §6). */
export const STAGE_ORDER: StageKey[] = [
	'orientation',
	'investigation',
	'purchase',
	'delivery',
	'change',
	'support',
	'exit',
];

/** Activities whose whole point is a person (the customer asks for one): never a containment gap. */
export const BY_DESIGN = new Set(['support:ask_for_person']);

export interface ActivityRow {
	activity: string;
	name: string;
	cases: number;
	contained: number;
	escalated: number;
	rate: number;
	p50_ms: number | null;
	/** Handovers that would not have happened had this activity met the target containment. */
	gap: number;
}

export interface StageRow {
	stage: string;
	label: string;
	cases: number;
	rate: number;
	escalated: number;
	activities: ActivityRow[];
}

/** By-activity rows grouped into journey stages (journey order), worst gaps first inside a stage. */
export function stageRows(a: JobAnalytics, target: number | null): StageRow[] {
	const t = target ?? 0.7;
	const by = new Map<string, ActivityRow[]>();
	for (const r of a.by_activity) {
		const [stage = 'other', act = r.activity] = r.activity.split(':');
		const rate = r.cases ? r.contained / r.cases : 0;
		const row: ActivityRow = {
			...r,
			name: act.replaceAll('_', ' '),
			rate,
			gap: BY_DESIGN.has(r.activity) ? 0 : Math.max(0, Math.round(r.cases * (t - rate))),
		};
		by.set(stage, [...(by.get(stage) ?? []), row]);
	}
	const order = (s: string) => {
		const i = STAGE_ORDER.indexOf(s as StageKey);
		return i < 0 ? 99 : i;
	};
	return [...by.entries()]
		.sort(([x], [y]) => order(x) - order(y))
		.map(([stage, rows]) => {
			const cases = rows.reduce((n, r) => n + r.cases, 0);
			const contained = rows.reduce((n, r) => n + r.contained, 0);
			return {
				stage,
				label: stage[0]!.toUpperCase() + stage.slice(1),
				cases,
				rate: cases ? contained / cases : 0,
				escalated: rows.reduce((n, r) => n + r.escalated, 0),
				activities: rows.sort((p, q) => q.gap - p.gap || q.cases - p.cases),
			};
		});
}

/** The activities whose gap to target costs the most handovers (the operator's to-do list). */
export function topGaps(stages: StageRow[], n = 3): ActivityRow[] {
	return stages
		.flatMap((s) => s.activities)
		.filter((r) => r.gap > 0)
		.sort((p, q) => q.gap - p.gap)
		.slice(0, n);
}

// ---- handover reasons ---------------------------------------------------------------------------------------------

export type ReasonOwner = 'customer' | 'agent' | 'system' | 'safety';

export const OWNER_LABEL: Record<ReasonOwner, { title: string; hint: string }> = {
	customer: { title: 'Customer’s choice', hint: 'Keep the hand-off quick; not a failure to fix.' },
	agent: {
		title: 'The agent could not finish',
		hint: 'Fix in procedures, gates or knowledge, then release.',
	},
	system: { title: 'A system let it down', hint: 'Fix the connection or the route, not the agent.' },
	safety: { title: 'Safety, by design', hint: 'Locked safety gates. Expected; watch for spikes.' },
};

/** Where an escalation reason is fixed: owner group + the job page (path after `/admin/jobs/{job}`). */
export const REASON_FIX: Record<string, { owner: ReasonOwner; page: string; label: string }> = {
	human_request: { owner: 'customer', page: '/handover', label: 'Handover queue' },
	frustration: { owner: 'agent', page: '/procedures', label: 'Procedures' },
	low_confidence: { owner: 'agent', page: '/gates', label: 'Gates' },
	upstream_unavailable: { owner: 'system', page: '/operations', label: 'Operations & routes' },
	unknown_outcome: { owner: 'system', page: '/operation-calls', label: 'Operation calls' },
	safety: { owner: 'safety', page: '/gates', label: 'Gates' },
};

export interface ReasonGroup {
	owner: ReasonOwner;
	count: number;
	share: number;
	reasons: { reason: string; label: string; count: number; page: string; pageLabel: string }[];
}

/** Escalation reasons grouped by who can act on them, biggest group first. */
export function reasonGroups(a: JobAnalytics): ReasonGroup[] {
	const total = a.escalation_reasons.reduce((n, r) => n + r.count, 0);
	const groups = new Map<ReasonOwner, ReasonGroup>();
	for (const r of a.escalation_reasons) {
		const fix = REASON_FIX[r.reason] ?? { owner: 'agent' as const, page: '/cases', label: 'Cases' };
		const g = groups.get(fix.owner) ?? { owner: fix.owner, count: 0, share: 0, reasons: [] };
		g.count += r.count;
		g.reasons.push({ ...r, page: fix.page, pageLabel: fix.label });
		groups.set(fix.owner, g);
	}
	return [...groups.values()]
		.map((g) => ({ ...g, share: total ? g.count / total : 0 }))
		.sort((x, y) => y.count - x.count);
}

// ---- systems ------------------------------------------------------------------------------------------------------

export interface SystemRow {
	system: string;
	calls: number;
	errors: number;
	denied: number;
	/** Share of calls that returned (errors count against; policy denials do not — they are the guard working). */
	success: number;
	p95_ms: number | null;
	worst: { op: string; errors: number; calls: number } | null;
}

/** Operation stats per backend system (instance routes; builtin/fallback grouped as "built-in"). */
export function systemRows(stats: OperationStat[]): SystemRow[] {
	const by = new Map<string, OperationStat[]>();
	for (const s of stats) {
		const key = s.route === 'builtin' || !s.instance ? 'built-in' : s.instance;
		by.set(key, [...(by.get(key) ?? []), s]);
	}
	return [...by.entries()]
		.map(([system, rows]) => {
			const calls = rows.reduce((n, r) => n + r.calls, 0);
			const errors = rows.reduce((n, r) => n + r.errors, 0);
			const worst = rows
				.filter((r) => r.errors > 0)
				.sort((p, q) => q.errors / q.calls - p.errors / p.calls)[0];
			const p95 = rows.map((r) => r.p95_ms).filter((x): x is number => x != null);
			return {
				system,
				calls,
				errors,
				denied: rows.reduce((n, r) => n + r.denied, 0),
				success: calls ? 1 - errors / calls : 1,
				p95_ms: p95.length ? Math.max(...p95) : null,
				worst: worst ? { op: worst.op, errors: worst.errors, calls: worst.calls } : null,
			};
		})
		.sort((x, y) => x.success - y.success);
}

// ---- cost ---------------------------------------------------------------------------------------------------------

export function tokensPerCase(a: JobAnalytics, cases: number): number | null {
	return cases ? Math.round(a.tokens / cases) : null;
}

/** Weighted median-ish answer time over activities (case-weighted mean of the per-activity medians). */
export function typicalAnswerMs(a: JobAnalytics): number | null {
	const rows = a.by_activity.filter((r) => r.p50_ms != null && r.cases > 0);
	const n = rows.reduce((s, r) => s + r.cases, 0);
	return n ? Math.round(rows.reduce((s, r) => s + r.p50_ms! * r.cases, 0) / n) : null;
}
