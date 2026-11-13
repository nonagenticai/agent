// J17 draft validation (D54 TEST step): pure functions, no world access (the fixture client passes design snapshots
// in). A scenario's outcome depends on one design location (`probeTarget`); the fixture reads that location from the
// live view (Production) and from the draft view (Test environment) and grades both, so a draft change visibly
// changes results. Unit tested in tests/unit/journey-J17.test.ts.
import type {
	DraftTestRun,
	DraftValidation,
	ScenarioProbe,
	ScenarioResult,
	ScenarioStyle,
	ScenarioVerdict,
	StageKey,
	TestScenario,
} from '$lib/api/types/journey';
import { STAGE_KEYS, STAGE_LABEL } from './catalogue';

/** The ledger target (`kind`, `ref`) a probe reads; null for a static probe. */
export function probeTarget(p: ScenarioProbe): { kind: string; ref: Record<string, string | null> } | null {
	switch (p.type) {
		case 'rule_includes':
			return { kind: 'rule_values', ref: { id: p.rule } };
		case 'route_offered':
			return { kind: 'route', ref: { object: p.object, verb: p.verb, variant: '*', env: p.env } };
		case 'entity_attr':
			return { kind: 'entity_attributes', ref: { name: p.entity } };
		case 'gate_wording':
			return { kind: 'gate', ref: { gate: p.gate, option: p.option } };
		default:
			return null;
	}
}

/** Grade a probe against the snapshot (`{ v }` or null = absent) of its design location. */
export function gradeProbe(p: ScenarioProbe, snap: { v: unknown } | null): ScenarioVerdict {
	const v = snap?.v as unknown;
	let ok: boolean;
	switch (p.type) {
		case 'static':
			ok = p.pass;
			break;
		case 'rule_includes': {
			const val = (v as Record<string, string | null> | undefined)?.[p.key] ?? '';
			ok = val
				.toLowerCase()
				.split(',')
				.map((x) => x.trim())
				.includes(p.item.toLowerCase());
			break;
		}
		case 'route_offered': {
			const kind = (v as { target?: { kind?: string } } | undefined)?.target?.kind;
			ok = !!kind && kind !== 'not_offered';
			break;
		}
		case 'entity_attr':
			ok = ((v as { name: string }[] | undefined) ?? []).some((a) => a.name === p.attr);
			break;
		case 'gate_wording': {
			const r = v as { label?: string | null; description?: string | null; hidden?: boolean } | undefined;
			const text = `${r?.label ?? p.option} ${r?.description ?? ''}`.toLowerCase();
			const hit = p.words.some((w) => text.includes(w.toLowerCase()));
			ok = p.expect === 'match' ? hit : !hit;
			break;
		}
	}
	return ok ? 'pass' : 'fail';
}

/** Short stable hash (djb2, 7 hex) — the draft fingerprint a run records. */
export function shortHash(s: string): string {
	let h = 5381;
	for (let i = 0; i < s.length; i++) h = ((h << 5) + h + s.charCodeAt(i)) >>> 0;
	return h.toString(16).padStart(8, '0').slice(0, 7);
}

/** `key@value` of one unpublished change. */
export const changeKey = (key: string, after: unknown) => `${key}@${JSON.stringify(after ?? null)}`;
export const draftFingerprint = (keys: string[]) => shortHash([...keys].sort().join('|'));

export function summarize(results: ScenarioResult[]): DraftTestRun['summary'] {
	return {
		total: results.length,
		pass: results.filter((r) => r.draft === 'pass').length,
		fail: results.filter((r) => r.draft === 'fail').length,
		regressions: results.filter((r) => r.diff === 'regression').length,
		fixed: results.filter((r) => r.diff === 'fixed').length,
		live_pass: results.filter((r) => r.live === 'pass').length,
	};
}

export const diffOf = (live: ScenarioVerdict, draft: ScenarioVerdict) =>
	live === draft ? 'same' : live === 'pass' ? 'regression' : 'fixed';

// ---- page helpers ----------------------------------------------------------------------------------------------

export const STYLES: { key: ScenarioStyle; label: string; hint: string }[] = [
	{ key: 'plain', label: 'Plain question', hint: 'how the activity is usually asked' },
	{ key: 'indirect', label: 'Roundabout', hint: 'a real-life phrasing that never names the activity' },
	{ key: 'nl', label: 'Dutch', hint: 'Demo Retail serves NL and BE' },
];

export interface MatrixRow {
	activity: string;
	label: string;
	stage: StageKey;
	stage_label: string;
	cells: Partial<Record<ScenarioStyle, TestScenario>>;
}

/** Activities (rows, journey stage order) × phrasing styles (columns). */
export function matrixRows(scenarios: TestScenario[]): MatrixRow[] {
	const by = new Map<string, MatrixRow>();
	for (const s of scenarios) {
		const row = by.get(s.activity) ?? {
			activity: s.activity,
			label: s.activity_label,
			stage: s.stage,
			stage_label: STAGE_LABEL[s.stage],
			cells: {},
		};
		row.cells[s.style] = s;
		by.set(s.activity, row);
	}
	return [...by.values()].sort((a, b) => STAGE_KEYS.indexOf(a.stage) - STAGE_KEYS.indexOf(b.stage));
}

export type CellTone = 'pass' | 'fail' | 'regression' | 'fixed' | 'known';
/** How a cell reads: regression and fixed first (they are the draft's effect), then known failures (fail live too). */
export function cellTone(r: ScenarioResult | undefined): CellTone | null {
	if (!r) return null;
	if (r.diff === 'regression') return 'regression';
	if (r.diff === 'fixed') return 'fixed';
	return r.draft === 'pass' ? 'pass' : 'known';
}
export const TONE_LABEL: Record<CellTone, string> = {
	pass: 'passes',
	fail: 'fails',
	regression: 'regression',
	fixed: 'fixed by draft',
	known: 'fails live too',
};

export interface Verdict {
	tone: 'ok' | 'warn' | 'err' | 'muted';
	title: string;
	body: string;
}

/** The decision this page supports: can the draft be published? */
export function publishVerdict(v: DraftValidation): Verdict {
	const run = v.runs[0];
	if (!v.scenarios.length)
		return {
			tone: 'muted',
			title: 'No test scenarios for this job yet',
			body: 'Try the draft by hand in the test chat before you publish.',
		};
	if (!run)
		return {
			tone: 'warn',
			title: 'The draft has not been tested',
			body: `Run the ${v.scenarios.length} scenarios on the Test environment before you publish.`,
		};
	if (v.state === 'stale')
		return {
			tone: 'warn',
			title: `The draft changed since the last run`,
			body: `${v.changed_since.length} change${v.changed_since.length === 1 ? '' : 's'} not tested yet: run again before you publish.`,
		};
	if (run.summary.regressions)
		return {
			tone: 'err',
			title: `${run.summary.regressions} regression${run.summary.regressions === 1 ? '' : 's'}: fix before you publish`,
			body: 'A scenario that Production answers correctly today fails on the draft. Change or discard the draft change that caused it, then run again.',
		};
	return {
		tone: 'ok',
		title: 'Tested: no regressions',
		body: `Draft rev ${run.draft_rev} passed ${run.summary.pass} of ${run.summary.total} scenarios${run.summary.fixed ? `, ${run.summary.fixed} fixed by the draft` : ''}. Ready to publish.`,
	};
}
