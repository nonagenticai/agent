// J25 Tools + Backend systems — pure helpers. A tool is an operation the agent can call in this job (doc 10 §7.1,
// doc 12 §2): the platform's `operation_def` says what it does (`jobTools()`), the job's procedures say where it is
// used, and the job's route per environment says which system answers (`jobOperations()`, D67 cells). The backend
// systems view is the same routes read the other way round: per connected system, which tools land on it.
import type {
	ConnectorInstance,
	Env,
	ExpectedOperation,
	JobOperations,
	JobTool,
	OpStats,
	RouteTarget,
	SideEffect,
} from '$lib/api/types/connectors';
import type { ProcedureView } from '$lib/api/types/ontology';
import { cellOf, type Cell } from './J6-routes';
import { opKey } from './ops';
import { t3Summary } from './instances';

export const ENVS: Env[] = ['prod', 'test'];

/** What a tool does to data, in the tenant's words (side-effect class, doc 10 §3.1). */
export const EFFECT: Record<SideEffect, { label: string; variant: 'ok' | 'warn' | 'err' | 'muted' }> = {
	R: { label: 'reads only', variant: 'muted' },
	W: { label: 'changes data', variant: 'warn' },
	X: { label: 'irreversible change', variant: 'err' },
	P: { label: 'takes a payment', variant: 'err' },
};

/** One procedure step that calls a tool. */
export interface ToolStep {
	procedure: string;
	version: number;
	context_space: string;
	step_number: number;
	name: string;
	gate: boolean;
}

export interface ToolRow {
	key: string;
	op: ExpectedOperation;
	/** null when the backend sent no definition for it. */
	def: JobTool | null;
	steps: ToolStep[];
	prod: Cell;
	test: Cell;
	stats: OpStats | null;
}

/** The version of each procedure the test environment runs (the draft), else the one Production runs. */
export function runningProcedures(procs: ProcedureView[]): ProcedureView[] {
	const byName = new Map<string, ProcedureView>();
	for (const p of procs) {
		const draft = p.effective_draft ?? p.effective;
		if (draft) byName.set(p.name, p);
		else if (p.effective && !byName.has(p.name)) byName.set(p.name, p);
	}
	return [...byName.values()].sort((a, b) => a.name.localeCompare(b.name));
}

/** `object.verb{variant}` a procedure step calls. */
export const stepKey = (s: { operation: string; variant: string | null }) => {
	const [object = '', verb = ''] = s.operation.split('.');
	return opKey({ object, verb, variant: s.variant ?? '*' });
};

const EFFECT_ORDER: SideEffect[] = ['P', 'X', 'W', 'R'];

/** Every tool of the job: writes before reads (most consequential first), required before optional, then by key. */
export function toolRows(d: JobOperations, tools: JobTool[], procs: ProcedureView[]): ToolRow[] {
	const running = runningProcedures(procs);
	return d.ops
		.map((op): ToolRow => {
			const key = opKey(op);
			const steps: ToolStep[] = [];
			for (const p of running)
				for (const s of p.steps)
					if (stepKey(s) === key)
						steps.push({
							procedure: p.name,
							version: p.version,
							context_space: p.context_space,
							step_number: s.step_number,
							name: s.name,
							gate: s.gate,
						});
			return {
				key,
				op,
				def: tools.find((t) => t.key === key) ?? null,
				steps,
				prod: cellOf(d, op, 'prod'),
				test: cellOf(d, op, 'test'),
				stats: d.stats.find((s) => s.op === `${op.object}.${op.verb}`) ?? null,
			};
		})
		.sort(
			(a, b) =>
				EFFECT_ORDER.indexOf(a.op.side_effect) - EFFECT_ORDER.indexOf(b.op.side_effect) ||
				Number(b.op.required) - Number(a.op.required) ||
				a.key.localeCompare(b.key),
		);
}

/** A tool that cannot run for someone on that env (not counting "not offered", which is a choice). */
export const blocked = (c: Cell) => !c.runs && c.state !== 'not_offered';

export function toolSummary(rows: ToolRow[]): {
	total: number;
	required: number;
	changes: number;
	blockedProd: number;
	blockedTest: number;
	inProcedures: number;
} {
	return {
		total: rows.length,
		required: rows.filter((r) => r.op.required).length,
		changes: rows.filter((r) => r.op.side_effect !== 'R').length,
		blockedProd: rows.filter((r) => blocked(r.prod)).length,
		blockedTest: rows.filter((r) => blocked(r.test)).length,
		inProcedures: rows.filter((r) => r.steps.length > 0).length,
	};
}

/** A route choice the tools drawer offers for one env: a system that implements the tool, or a fallback. */
export interface RouteChoice {
	value: string;
	label: string;
	target: RouteTarget;
}

/**
 * What the tool can be routed to on `env` from the drawer: connected systems on that env whose template implements
 * it, the case queue (case and handover tools), built-in (derivations: identity checks, business rules), and "not
 * offered" for optional tools — the same rules the route write enforces. A deep link stays as it is
 * (its URL is edited in Operations & routes).
 */
export function routeChoices(d: JobOperations, op: ExpectedOperation, env: Env, current: RouteTarget | null) {
	const key = opKey(op);
	const star = opKey({ ...op, variant: '*' });
	const out: RouteChoice[] = [];
	for (const i of d.instances) {
		if (i.env !== env && i.env !== 'all') continue;
		if (i.template_key === 'agent_cases') continue;
		if (env === 'prod' && d.draft_instances.includes(i.id)) continue;
		const impl = (d.implements[i.id] ?? []).some((k) => k === key || k === star);
		if (!impl) continue;
		out.push({
			value: `instance:${i.id}`,
			label: `${i.label}${i.stub ? ' (stub)' : ''}`,
			target: { kind: 'instance', instance_id: i.id },
		});
	}
	if (op.is_derive || current?.kind === 'builtin')
		out.push({ value: 'builtin', label: 'Built into the agent', target: { kind: 'builtin' } });
	if (op.object === 'case' || op.object === 'handover')
		out.push({
			value: 'agent_cases',
			label: 'Case queue — a person finishes it',
			target: { kind: 'agent_cases' },
		});
	if (current?.kind === 'deep_link')
		out.push({ value: 'deep_link', label: `Link to your site (${current.url})`, target: current });
	if (!op.required)
		out.push({
			value: 'not_offered',
			label: 'Not offered — the agent says it cannot help',
			target: { kind: 'not_offered' },
		});
	return out;
}

export const choiceValue = (t: RouteTarget | null): string =>
	!t ? '' : t.kind === 'instance' ? `instance:${t.instance_id}` : t.kind;

// ---- backend systems ------------------------------------------------------------------------------------------

/** One tool routed to a system, on one env. */
export interface SystemUse {
	key: string;
	env: Env;
	cell: Cell;
	side_effect: SideEffect;
	required: boolean;
}

export interface JobSystem {
	instance: ConnectorInstance;
	uses: SystemUse[];
	/** Envs this job sends tools to it on. */
	envs: Env[];
	contract: { green: number; run: number; unverified: number };
	/** Tools routed here that cannot run (system failing, test red, untested, stub…). */
	blockedUses: number;
}

export type SystemHealth = 'ok' | 'warn' | 'err';

/** Health of a system as the agent experiences it: credential + target + last error, then the contract tests. */
export function systemHealth(s: Pick<JobSystem, 'instance' | 'contract'>): {
	word: SystemHealth;
	label: string;
} {
	const i = s.instance;
	if (i.status === 'auth_failed') return { word: 'err', label: 'Credential rejected' };
	if (i.status === 'invalid_target') return { word: 'err', label: 'Address refused' };
	if (i.stub) return { word: 'warn', label: 'Stub — test only' };
	// a licence bearer (and `none`) is authenticated without a stored credential
	if (!i.platform && !i.credential.set && i.credential_required !== false)
		return { word: 'err', label: 'No credential' };
	if (i.status === 'draft_template') return { word: 'warn', label: 'Draft template — test only' };
	if (s.contract.run > s.contract.green) return { word: 'err', label: 'Contract test red' };
	if (i.status === 'untested' || s.contract.run === 0) return { word: 'warn', label: 'Not tested yet' };
	return { word: 'ok', label: 'Healthy' };
}

/** Tools the job serves inside the platform, by route kind (no tenant system involved). */
export interface PlatformRoute {
	kind: 'builtin' | 'agent_cases' | 'deep_link' | 'platform';
	label: string;
	uses: SystemUse[];
}

/**
 * The job's systems: every connector instance at least one of its tools is routed to (draft routes, what the test
 * env runs and the next release publishes), worst health first; the platform-served routes; and the tenant's other
 * systems this job does not use.
 */
export function jobSystems(
	d: JobOperations,
	instances: ConnectorInstance[],
): { systems: JobSystem[]; platform: PlatformRoute[]; unused: ConnectorInstance[] } {
	const uses = new Map<string, SystemUse[]>();
	const plat: Record<PlatformRoute['kind'], SystemUse[]> = {
		builtin: [],
		agent_cases: [],
		deep_link: [],
		platform: [],
	};
	for (const op of d.ops)
		for (const env of ENVS) {
			const cell = cellOf(d, op, env);
			const t = cell.target;
			if (!t || t.kind === 'not_offered') continue;
			const use: SystemUse = {
				key: opKey(op),
				env,
				cell,
				side_effect: op.side_effect,
				required: op.required,
			};
			if (t.kind === 'instance') {
				const inst = instances.find((i) => i.id === t.instance_id);
				if (inst?.platform) plat.platform.push(use);
				else uses.set(t.instance_id, [...(uses.get(t.instance_id) ?? []), use]);
			} else plat[t.kind].push(use);
		}
	const rank: Record<SystemHealth, number> = { err: 0, warn: 1, ok: 2 };
	const systems = [...uses.entries()]
		.map(([id, u]): JobSystem | null => {
			const instance = instances.find((i) => i.id === id);
			if (!instance) return null;
			return {
				instance,
				uses: u,
				envs: ENVS.filter((e) => u.some((x) => x.env === e)),
				contract: t3Summary(instance),
				blockedUses: u.filter((x) => blocked(x.cell)).length,
			};
		})
		.filter((s): s is JobSystem => s !== null)
		.sort(
			(a, b) =>
				rank[systemHealth(a).word] - rank[systemHealth(b).word] ||
				a.instance.label.localeCompare(b.instance.label),
		);
	const LABEL: Record<PlatformRoute['kind'], string> = {
		platform: 'Platform services (knowledge, case store)',
		builtin: 'Built into the agent',
		agent_cases: 'Case queue — a person finishes it',
		deep_link: 'Link to your site',
	};
	const platform = (Object.keys(plat) as PlatformRoute['kind'][])
		.filter((k) => plat[k].length)
		.map((k) => ({ kind: k, label: LABEL[k], uses: plat[k] }));
	const unused = instances.filter((i) => !i.platform && !uses.has(i.id));
	return { systems, platform, unused };
}

/** Distinct tool keys of a use list (a tool routed on both envs counts once). */
export const toolKeys = (u: SystemUse[]) => [...new Set(u.map((x) => x.key))];
