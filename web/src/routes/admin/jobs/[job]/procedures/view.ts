// Pure view helpers for J12 (list + detail). Steps name `object.verb{variant}` operations (12 §2.1, 11 §5.1), never
// a worker or a concrete system; the lifecycle, filters, history wording and success meter are cb's.
// from cb:web/src/routes/admin/jobs/[job]/procedures/view.ts@45ee128
import type {
	ProcedureHistoryEntry,
	ProcedureStep,
	ProcedureView,
	VersionStatus,
} from '$lib/api/types/ontology';
import { NEXT_STATUS } from '$lib/api/areas/ontology';

export interface FlowPhase {
	phase: number;
	label: string;
	steps: ProcedureStep[];
	/** The gating step's operation (`rule.evaluate{RET-01}`), or null. */
	gate: string | null;
}

export const stepOp = (s: Pick<ProcedureStep, 'operation' | 'variant'>) =>
	s.variant ? `${s.operation}{${s.variant}}` : s.operation;

/** Steps grouped by phase (ordered), parallel when a phase has more than one step. */
export function groupPhases(steps: ProcedureStep[]): FlowPhase[] {
	const order = [...new Set(steps.map((s) => s.phase))].sort((a, b) => a - b);
	return order.map((phase, i) => {
		const ps = steps.filter((s) => s.phase === phase).sort((a, b) => a.step_number - b.step_number);
		const gating = ps.find((s) => s.gate);
		return {
			phase,
			label: `Phase ${i + 1}${ps.length > 1 ? ' · parallel' : ''}`,
			steps: ps,
			gate: gating ? stepOp(gating) : null,
		};
	});
}

export function stepSummary(steps: ProcedureStep[]): { steps: number; phases: number } {
	return { steps: steps.length, phases: new Set(steps.map((s) => s.phase)).size };
}

const STATUS_ORDER: VersionStatus[] = ['PUBLISHED', 'CANARY', 'DRAFT', 'ARCHIVED', 'DISCARDED'];
export const STATUS_LABEL: Record<VersionStatus, string> = {
	PUBLISHED: 'Published',
	CANARY: 'Canary',
	DRAFT: 'Draft',
	ARCHIVED: 'Archived',
	DISCARDED: 'Discarded',
};

/** Segmented filter: All (n) plus one entry per status that occurs, with counts. */
export function statusFilters(items: Pick<ProcedureView, 'status'>[]): [string, string][] {
	const out: [string, string][] = [['all', `All (${items.length})`]];
	for (const s of STATUS_ORDER) {
		const n = items.filter((p) => p.status === s).length;
		if (n) out.push([s, `${STATUS_LABEL[s]} (${n})`]);
	}
	return out;
}

export const LIFECYCLE: VersionStatus[] = ['DRAFT', 'CANARY', 'PUBLISHED', 'ARCHIVED'];

export const nextStatuses = (s: VersionStatus): VersionStatus[] => NEXT_STATUS[s];

/** One audit row of a version, in words. */
export function historyText(h: ProcedureHistoryEntry): string {
	if (h.action === 'create') return 'Created as DRAFT';
	if (h.action === 'approve') return 'Approved';
	if (!h.ok) return `${h.from ?? '?'} → ${h.to ?? '?'} refused${h.reason ? ` (${h.reason})` : ''}`;
	if (h.reason?.startsWith('replaced by'))
		return `Archived: ${h.reason.slice(12)} was published in its place`;
	if (h.from === 'ARCHIVED' && h.to === 'PUBLISHED') return 'Rolled back: re-published from ARCHIVED';
	return `${h.from ?? '?'} → ${h.to ?? '?'}`;
}

/** The next version of a name (B5 R6: integers; a new version is above every base and tenant version). */
export function nextVersion(existing: number[] = []): number {
	return Math.max(0, ...existing) + 1;
}

/**
 * Success over the last 7 days: (completed + not_eligible) / runs. Null (no runs) shows "—", never 0 %.
 * `cls` colours the meter: ok from 95 %, warn from 80 %, else err.
 */
export function successView(rate: number | null): { text: string; cls: string } | null {
	if (rate === null) return null;
	const cls = rate >= 0.95 ? 's-ok' : rate >= 0.8 ? 's-warn' : 's-err';
	return { text: `${Math.round(rate * 100)}%`, cls };
}

/** The version the orchestrator runs for a procedure name (tenant PUBLISHED wins over base), or null. */
export function effectiveOf(rows: ProcedureView[], name: string): ProcedureView | null {
	return rows.find((p) => p.name === name && p.effective) ?? null;
}

// ---- J12 redesign: what each step does, how a version relates to the running one and to the release ----------

/** What a step does for the customer, from its operation (11 §5.1, 12 §2.1). */
export type StepKind = 'knowledge' | 'identity' | 'rule' | 'read' | 'change' | 'case';

export const STEP_KIND: Record<StepKind, { label: string; hint: string }> = {
	knowledge: { label: 'Read knowledge', hint: 'Searches the bound knowledge stores; replies cite them' },
	identity: { label: 'Identify', hint: 'Finds and verifies the customer before personal data is read' },
	rule: { label: 'Evaluate rule', hint: 'A business rule decides eligibility' },
	read: { label: 'Read data', hint: 'Reads from a connected system; no side effect' },
	change: { label: 'Change data', hint: 'Writes to a connected system; the customer confirms first' },
	case: { label: 'Case', hint: 'Opens or reads a case that staff follow up' },
};

export function stepKind(s: Pick<ProcedureStep, 'operation' | 'side_effect'>): StepKind {
	const obj = s.operation.split('.')[0];
	if (obj === 'knowledge') return 'knowledge';
	if (obj === 'party' || obj === 'credential') return s.side_effect === 'R' ? 'identity' : 'change';
	if (obj === 'rule') return 'rule';
	if (obj === 'case') return 'case';
	return s.side_effect === 'R' ? 'read' : 'change';
}

/** Where a step's dependency is defined, for the "uses" link on a step (job-relative path). */
export function stepUses(s: Pick<ProcedureStep, 'operation' | 'variant' | 'side_effect'>): {
	text: string;
	path: string;
} {
	const k = stepKind(s);
	if (k === 'knowledge') return { text: 'knowledge kinds', path: 'knowledge-kinds' };
	if (k === 'rule') return { text: `rule ${s.variant ?? ''}`.trim(), path: 'rules' };
	return { text: 'routes', path: 'operations' };
}

/** Steps of `sel` compared with the running version: added, changed (same operation, other bindings) or same. */
export type StepDiff = 'added' | 'changed' | 'same';
export function diffSteps(
	sel: ProcedureStep[],
	base: ProcedureStep[] | null,
): { marks: Map<number, StepDiff>; removed: ProcedureStep[] } {
	const marks = new Map<number, StepDiff>();
	if (!base) return { marks, removed: [] };
	const used = new Set<number>();
	for (const s of sel) {
		const i = base.findIndex((b, n) => !used.has(n) && stepOp(b) === stepOp(s));
		if (i < 0) {
			marks.set(s.step_number, 'added');
			continue;
		}
		used.add(i);
		const same = JSON.stringify(base[i]!.args) === JSON.stringify(s.args) && base[i]!.gate === s.gate;
		marks.set(s.step_number, same ? 'same' : 'changed');
	}
	return { marks, removed: base.filter((_, n) => !used.has(n)) };
}

/**
 * The version customers get in production (D54: only a release makes anything live): the one the live release
 * carries, the tenant version winning over the base one it shadows. Null without a release or when the release
 * carries none.
 */
export function liveOf(rows: ProcedureView[], release: string | null): ProcedureView | null {
	if (!release) return null;
	// `effective` is the server's truth: the live design runs this version. `released_in` is only the last release that
	// bundled a change to the name, so a version carried unchanged by a later release keeps the earlier id.
	const running = rows.find((p) => p.effective);
	if (running) return running;
	const inRel = rows.filter(
		(p) => p.released_in === release && (p.status === 'PUBLISHED' || p.status === 'ARCHIVED'),
	);
	return inRel.find((p) => p.origin === 'tenant') ?? inRel[0] ?? null;
}

/** A PUBLISHED version the live release does not carry yet: it goes live when the next release is applied. */
export function pendingOf(rows: ProcedureView[], release: string | null): ProcedureView | null {
	const live = liveOf(rows, release);
	// D54: `effective` follows the live release; the draft's runner (test chat) is `effective_draft`.
	const next = rows.find((p) => p.effective_draft ?? p.effective) ?? null;
	return next && next.version !== live?.version ? next : null;
}

/** Where a version stands towards customers, in one line (D54: draft → test → publish via a release → live). */
export function reachText(p: ProcedureView, live: ProcedureView | null, release: string | null): string {
	const keep = live ? ` Customers keep ${live.version} until then.` : '';
	if (live && p.version === live.version)
		return p.origin === 'tenant'
			? `Live in production: your version, applied with release ${release}.`
			: `Live in production: from the template, in release ${release}.`;
	if (p.shadowed && p.released_in === release)
		return `In release ${release} from the template, but your version ${live?.version ?? ''} runs instead.`;
	if (p.status === 'PUBLISHED')
		return `Published to the next release: it goes live when that release is approved and applied.${keep}`;
	if (p.status === 'CANARY')
		return `Draft in review: ${p.reviewers.length} approval${p.reviewers.length === 1 ? '' : 's'}; a Release manager publishes it. Once published it rides the next release.${keep}`;
	if (p.status === 'DRAFT')
		return 'Saved to draft, not live. Test it in the test chat, then move it to review.';
	if (p.status === 'ARCHIVED')
		return p.released_in
			? `Retired. Last carried by release ${p.released_in}; can be rolled back (again via a release).`
			: 'Retired. Can be rolled back (via a release).';
	return 'Discarded. Kept for the record.';
}

/** Versions of one procedure grouped by where they are: live, waiting for release, shadowed, in review, drafts. */
export function lanes(rows: ProcedureView[], release: string | null) {
	const live = liveOf(rows, release);
	const pending = pendingOf(rows, release);
	return {
		live,
		pending,
		shadowed: rows.filter((p) => p.shadowed && p.version !== live?.version),
		review: rows.filter((p) => p.status === 'CANARY'),
		drafts: rows.filter((p) => p.status === 'DRAFT'),
		retired: rows.filter(
			(p) => (p.status === 'ARCHIVED' || p.status === 'DISCARDED') && p.version !== live?.version,
		),
	};
}

/** Unpublished changes: drafts, versions in review, and published versions waiting for the next release. */
export function unpublished(rows: ProcedureView[], release: string | null): number {
	const names = [...new Set(rows.map((p) => p.name))];
	return (
		rows.filter((p) => p.status === 'DRAFT' || p.status === 'CANARY').length +
		names.filter((n) =>
			pendingOf(
				rows.filter((p) => p.name === n),
				release,
			),
		).length
	);
}

export const OUTCOME_LABEL: Record<string, [string, 'ok' | 'warn' | 'err' | 'muted']> = {
	resolved: ['Resolved', 'ok'],
	not_eligible: ['Not eligible', 'warn'],
	handed_over: ['Handed over', 'muted'],
	refused: ['Refused', 'err'],
};
