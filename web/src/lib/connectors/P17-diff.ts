// P17 pure helpers: what changed between two versions of one connector template, which tenant systems can move
// to which version, and how a vendor fault / status lands in the canonical taxonomy. Rules: doc 10 §4.2 (template
// contents, pin semantics), doc 11 §6.1 pinning rule 4 (explicit tenant re-pin, contract tests re-run), doc 10 §3.1
// (error taxonomy: system errors escalate, user outcomes get a reply).
import type { ConnectorTemplate, ConnectorTemplateOp, TemplateInstanceUse } from '$lib/api/types/connectors';
import type { ErrorStateDef } from '$lib/api/types/model';
import { idempotencyLabel, opKey } from './ops';

export type OpChange = 'added' | 'removed' | 'changed' | 'same';

export interface OpDiff {
	op: string;
	change: OpChange;
	/** Human lines: `+ field delivery_type ← $.data.order…`, `status DECLINED → rejected removed`. */
	details: string[];
}

export interface TemplateDiff {
	against: string | null;
	/** Template-level changes (vendor pin, auth, webhooks, credential fields, rate limit). */
	meta: { label: string; before: string; after: string }[];
	ops: OpDiff[];
	counts: Record<OpChange, number>;
}

/** Vendor API versions inside paths (`/admin/api/2026-07/…`) are a template-level pin, not a per-op change. */
const API_SEGMENT = /\b\d{4}-\d{2}\b/g;
const normPath = (p: string) => p.replace(API_SEGMENT, '{api}');

function opDetails(a: ConnectorTemplateOp, b: ConnectorTemplateOp): string[] {
	const out: string[] = [];
	if (a.method !== b.method || normPath(a.path) !== normPath(b.path))
		out.push(`endpoint ${a.method} ${a.path} → ${b.method} ${b.path}`);
	if (a.request !== b.request) out.push('request template rewritten');
	const fa = new Map(a.field_map.map((f) => [f.field, f]));
	const fb = new Map(b.field_map.map((f) => [f.field, f]));
	for (const [field, f] of fb) {
		const old = fa.get(field);
		if (!old) out.push(`+ maps ${field} ← ${f.path}`);
		else if (old.path !== f.path || (old.transform ?? '') !== (f.transform ?? ''))
			out.push(`~ ${field}: ${old.path} → ${f.path}`);
	}
	for (const field of fa.keys()) if (!fb.has(field)) out.push(`− no longer maps ${field}`);
	const sa = new Map(a.status_map.map((s) => [s.system, s.canonical]));
	const sb = new Map(b.status_map.map((s) => [s.system, s.canonical]));
	for (const [sys, can] of sb)
		if (!sa.has(sys)) out.push(`+ status ${sys} → ${can}`);
		else if (sa.get(sys) !== can) out.push(`~ status ${sys}: ${sa.get(sys)} → ${can}`);
	for (const sys of sa.keys()) if (!sb.has(sys)) out.push(`− status ${sys} unmapped`);
	const ka = new Map(a.fault_map.map((f) => [f.system, f.code]));
	const kb = new Map(b.fault_map.map((f) => [f.system, f.code]));
	for (const [sys, code] of kb) if (ka.get(sys) !== code) out.push(`+ fault ${sys} → ${code}`);
	for (const sys of ka.keys()) if (!kb.has(sys)) out.push(`− fault ${sys} unmapped`);
	if (idempotencyLabel(a.idempotency) !== idempotencyLabel(b.idempotency))
		out.push(`idempotency ${idempotencyLabel(a.idempotency)} → ${idempotencyLabel(b.idempotency)}`);
	if (a.verify_read !== b.verify_read)
		out.push(`verify_read ${a.verify_read ?? '—'} → ${b.verify_read ?? '—'}`);
	if (a.pagination !== b.pagination)
		out.push(`pagination ${a.pagination ?? 'none'} → ${b.pagination ?? 'none'}`);
	if (a.timeout_ms !== b.timeout_ms) out.push(`timeout ${a.timeout_ms} → ${b.timeout_ms} ms`);
	if (a.side_effect !== b.side_effect) out.push(`side effect ${a.side_effect} → ${b.side_effect}`);
	return out;
}

/** Diff `cur` against `prev` (the next older version). With no previous version every op is `added`. */
export function diffTemplates(prev: ConnectorTemplate | null, cur: ConnectorTemplate): TemplateDiff {
	const meta: TemplateDiff['meta'] = [];
	const ops: OpDiff[] = [];
	if (prev) {
		const m = (label: string, a: string, b: string) => {
			if (a !== b) meta.push({ label, before: a || '—', after: b || '—' });
		};
		m('Vendor API pin', prev.vendor_api_pin, cur.vendor_api_pin);
		m('Auth', `${prev.auth_kind} · ${prev.auth_detail}`, `${cur.auth_kind} · ${cur.auth_detail}`);
		m(
			'Credential fields',
			prev.credential_schema.map((f) => f.name).join(', '),
			cur.credential_schema.map((f) => f.name).join(', '),
		);
		m('Rate limit', prev.rate_limit_model.value ?? '', cur.rate_limit_model.value ?? '');
		m(
			'Webhooks (inert)',
			prev.webhooks.map((w) => w.topic).join(', '),
			cur.webhooks.map((w) => w.topic).join(', '),
		);
		m('Transport', prev.transport, cur.transport);
	}
	const before = new Map((prev?.ops ?? []).map((o) => [opKey(o), o]));
	for (const o of cur.ops) {
		const k = opKey(o);
		const old = before.get(k);
		if (!old) ops.push({ op: k, change: 'added', details: [] });
		else {
			const details = opDetails(old, o);
			ops.push({ op: k, change: details.length ? 'changed' : 'same', details });
		}
	}
	const now = new Set(cur.ops.map(opKey));
	for (const k of before.keys()) if (!now.has(k)) ops.push({ op: k, change: 'removed', details: [] });
	const counts: Record<OpChange, number> = { added: 0, removed: 0, changed: 0, same: 0 };
	for (const o of ops) counts[o.change]++;
	return { against: prev?.version ?? null, meta, ops, counts };
}

const cmp = (a: string, b: string) => a.localeCompare(b, 'en', { numeric: true });

export type UpgradeVerdict =
	| { kind: 'current'; note: string }
	| { kind: 'can_upgrade'; to: string; warnings: string[] }
	| { kind: 'blocked'; to: string; reasons: string[] }
	| { kind: 'no_target'; note: string };

/**
 * Can this tenant system move to the newest active version? Re-pinning is the tenant's explicit act (doc 11 §6.1
 * rule 4) and re-runs T3; it is refused onto a draft or retired version. A target that drops an operation some job
 * routes to this system would strand that route, so it blocks; overrides on fields the target no longer maps warn.
 */
export function upgradeVerdict(inst: TemplateInstanceUse, latest: ConnectorTemplate | null): UpgradeVerdict {
	if (!latest) return { kind: 'no_target', note: 'No active version of this template exists to re-pin to.' };
	const c = cmp(inst.pinned_version, latest.version);
	if (c === 0) return { kind: 'current', note: 'On the newest active version.' };
	if (c > 0) return { kind: 'current', note: `Ahead of the newest active version (@${latest.version}).` };
	const has = new Set(latest.ops.map(opKey));
	const reasons = inst.routed_ops
		.filter((op) => !has.has(op) && !has.has(op.replace(/\{[^}]*\}$/, '')))
		.map((op) => `@${latest.version} drops ${op}, which a job routes here`);
	if (reasons.length) return { kind: 'blocked', to: latest.version, reasons };
	const warnings: string[] = [];
	for (const ov of inst.overrides) {
		const op = latest.ops.find((o) => opKey(o) === ov.op);
		if (!op || !op.field_map.some((f) => f.field === ov.field))
			warnings.push(`override ${ov.op}.${ov.field} would be dropped`);
	}
	warnings.push('T3 contract tests re-run on re-pin');
	return { kind: 'can_upgrade', to: latest.version, warnings };
}

export interface FaultClass {
	/** `invalid_state:in_transit` → `invalid_state`. */
	base: string;
	def: ErrorStateDef | null;
	/** reply = user outcome (agent answers); escalate = system error (handover); unknown = not in error_state_def. */
	handling: 'reply' | 'escalate' | 'unknown';
}

export function classifyCode(code: string, taxonomy: ErrorStateDef[]): FaultClass {
	const base = code.split(':')[0]!.trim();
	const def = taxonomy.find((d) => d.code === base) ?? null;
	return { base, def, handling: !def ? 'unknown' : def.user_facing ? 'reply' : 'escalate' };
}

/** Group ops by canonical object, preserving template order, for the operation index. */
export function groupByObject(ops: ConnectorTemplateOp[]): { object: string; ops: ConnectorTemplateOp[] }[] {
	const out: { object: string; ops: ConnectorTemplateOp[] }[] = [];
	for (const o of ops) {
		const g = out.find((x) => x.object === o.object);
		if (g) g.ops.push(o);
		else out.push({ object: o.object, ops: [o] });
	}
	return out;
}
