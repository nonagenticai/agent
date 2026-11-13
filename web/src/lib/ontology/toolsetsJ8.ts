// J8 Toolsets: pure helpers over the computed toolset (spec 12 §2). A toolset is N2 ∩ N3 ∩ N4 (with the tenant's
// narrowing applied to N3), filtered per call by N5. These helpers say, per operation, WHICH link of that chain cut
// it, fold a toolset into a funnel, and spell out the per-call (N5) checks the runner still applies. Owned by J8.
import type {
	AgentRoleKey,
	SideEffect,
	Toolset,
	ToolsetActivityProcedure,
	ToolsetRow,
} from '$lib/api/types/ontology';
import { catalogueOp, opLabel } from './catalogue';

/** The link that removed an operation: role off, N2, N3 (template grant / ceiling), tenant narrowing, N4. */
export type CutLink = 'role' | 'N2' | 'N3' | 'narrowed' | 'N4';

export const CUT_LABEL: Record<CutLink, string> = {
	role: 'Role disabled',
	N2: 'N2 · not expected',
	N3: 'N3 · not granted',
	narrowed: 'Narrowed here',
	N4: 'N4 · no route',
};

/** Which link cut this row (null = callable). Same precedence as the runner's reason. */
export function cutLink(r: ToolsetRow, roleEnabled: boolean): CutLink | null {
	if (!roleEnabled) return 'role';
	if (r.expected === null) return 'N2';
	if (!r.granted) return 'N3';
	if (r.narrowed) return 'narrowed';
	if (r.route.kind === 'unrouted' || r.route.kind === 'not_offered') return 'N4';
	return null;
}

export interface FunnelStage {
	key: 'pool' | CutLink;
	label: string;
	who: string;
	/** Operations still in after this stage. */
	kept: number;
	/** Operations this stage removed. */
	cut: ToolsetRow[];
}

/**
 * The toolset as a funnel: the pool (everything the activity expects or the role grants) → N2 → N3 → tenant
 * narrowing → N4 = callable. A disabled role adds a first stage that cuts everything.
 */
export function funnel(ts: Toolset): FunnelStage[] {
	const by = (k: CutLink) => ts.rows.filter((r) => cutLink(r, ts.role_enabled) === k);
	let kept = ts.rows.length;
	const stages: FunnelStage[] = [
		{ key: 'pool', label: 'In play', who: 'activity ∪ role grant', kept, cut: [] },
	];
	const add = (key: CutLink, label: string, who: string) => {
		const cut = by(key);
		kept -= cut.length;
		stages.push({ key, label, who, kept, cut });
	};
	if (!ts.role_enabled) add('role', 'Role enabled', 'platform');
	add('N2', 'Activity expects', 'template');
	add('N3', 'Role grants', 'template');
	add('narrowed', 'Tenant narrowing', 'this tenant');
	add('N4', 'Routed', 'this tenant');
	return stages;
}

/** Callable / expected-by-the-activity, for a matrix cell. */
export function cellCounts(ts: Toolset) {
	const expected = ts.rows.filter((r) => r.expected !== null);
	const callable = ts.rows.filter((r) => r.effective);
	return {
		callable: callable.length,
		expected: expected.length,
		narrowed: ts.rows.filter((r) => r.narrowed).length,
		/** A required op of the activity is not callable: the procedure cannot complete. */
		requiredMissing: expected.filter((r) => r.expected === 'R' && !r.effective).length,
	};
}

export const IDENTITY_LABEL: Record<string, string> = {
	'0': 'public',
	'1': 'identified',
	'2': 'verified',
	'3': 'step-up',
	S: 'staff',
};

/** `2` → `verified`; `1→2` → `raises 1 → 2`; `1/2`, `1–2` → `identified or verified`. */
export function identityText(level: string): string {
	if (level.includes('→')) {
		const [a, b] = level.split('→');
		return `raises ${IDENTITY_LABEL[a!] ?? a} → ${IDENTITY_LABEL[b!] ?? b}`;
	}
	const parts = level.split(/[/–-]/).filter(Boolean);
	if (parts.length > 1) return parts.map((p) => IDENTITY_LABEL[p] ?? p).join(' or ');
	return IDENTITY_LABEL[level] ?? level;
}

export interface PerCall {
	identity_level: string;
	/** Human text for the identity requirement. */
	identity: string;
	/** W/X/P need a confirm-back from an inbound customer message. */
	confirm: boolean;
	/** P runs only through a hosted payment link (D27). */
	hosted_link: boolean;
	/** X: the runner verifies with a read; no answer → unknown_outcome, never auto-retried. */
	verify_read: boolean;
}

/** N5: the per-call checks the runner still applies to a callable operation (12 §3.3 steps 3–5, 10). */
export function perCall(op: string, side_effect: SideEffect): PerCall {
	const level = catalogueOp(op)?.identity_level ?? '0';
	return {
		identity_level: level,
		identity: identityText(level),
		confirm: side_effect !== 'R',
		hosted_link: side_effect === 'P',
		verify_read: side_effect === 'X' || side_effect === 'P',
	};
}

/** Procedure steps whose operation (with variant) is in a row of the toolset; `{*}` grants cover variants. */
export function rowForStep(
	ts: Toolset,
	step: { operation: string; variant: string | null },
): ToolsetRow | undefined {
	return (
		ts.rows.find((r) => r.op === step.operation && r.variant === step.variant) ??
		ts.rows.find((r) => r.op === step.operation && r.variant === null)
	);
}

export interface StepCheck {
	step_number: number;
	name: string;
	label: string;
	row: ToolsetRow | undefined;
	callable: boolean;
	cut: CutLink | null;
}

/** Each step of the activity's procedure against the procedure role's toolset: would the runner allow it? */
export function procedureCheck(proc: ToolsetActivityProcedure, ts: Toolset): StepCheck[] {
	return proc.steps.map((s) => {
		const row = rowForStep(ts, s);
		const cut = row ? cutLink(row, ts.role_enabled) : 'N2';
		return {
			step_number: s.step_number,
			name: s.name,
			label: opLabel(s.operation, s.variant),
			row,
			callable: !!row && row.effective,
			cut,
		};
	});
}

/** The procedure steps (by number) that call this row's operation, across the given procedures. */
export function stepsUsing(row: ToolsetRow, procs: ToolsetActivityProcedure[]): string[] {
	const out: string[] = [];
	for (const p of procs)
		for (const s of p.steps)
			if (s.operation === row.op && (s.variant === row.variant || row.variant === null))
				out.push(`${p.name} step ${s.step_number}`);
	return out;
}

export const ROLE_LABEL: Record<AgentRoleKey, string> = {
	qualifier: 'Qualifier',
	procedure: 'Procedure',
	post_gate: 'Post-gate',
	assistant: 'Assistant',
};
