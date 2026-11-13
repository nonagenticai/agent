// Pure rules of the model area, shared by the fixture API, the pages and the tests:
//   - a template layer's content at a version, its hash, and the diff between two versions (D15, 11 §6.1);
//   - the `invalid_state:<s>` errors an object's state machine derives per verb (11 §3.2, §6.1);
//   - variant inheritance of `operation_def` rows (`*` + overrides) and each operation's generated error list.
// Owned by the model area agent.
import type {
	DraftRevision,
	ErrorStateDef,
	ObjectDef,
	ObjectTransition,
	OperationAttrs,
	OperationDefRow,
	OperationRow,
	TagDiffRow,
	TagItem,
	TemplateScope,
	VerbStates,
} from '$lib/api/types/model';
import type { JsonObject } from '$lib/api/types/core';

// ---------------------------------------------------------------------------------------------------------------
// Template layer content

/** An authored item across versions: present from `from` to `to` (inclusive), its detail changing at some tags. */
export interface VersionedItem extends TagItem {
	from: number;
	to?: number;
	/** version → detail from that version on. */
	changes?: Record<number, string>;
	/**
	 * Platform seed only (not shown): for a structured kind (object, profile, gate_question, …), the structured
	 * source's top-level fields as they were in the revision cut at that version (a `from` or `changes` version).
	 * Absent = today's structured source. A job/industry layer refining a lower layer's item states its refinement
	 * here for every version it carries.
	 */
	structured?: Record<number, JsonObject>;
}

/** The items a layer holds at tag `v`, in authored order. */
export function contentsAt(items: VersionedItem[], v: number): TagItem[] {
	return items
		.filter((i) => i.from <= v && (i.to === undefined || i.to >= v))
		.map((i) => {
			let detail = i.detail;
			for (const [ver, d] of Object.entries(i.changes ?? {}).sort((a, b) => Number(a[0]) - Number(b[0])))
				if (Number(ver) <= v) detail = d;
			return { kind: i.kind, key: i.key, label: i.label, detail };
		});
}

const itemId = (i: Pick<TagItem, 'kind' | 'key'>) => `${i.kind}\u0000${i.key}`;

/** What changed from `before` to `after`, keyed by (kind, key): added, changed (detail differs), removed. */
export function diffItems(before: TagItem[], after: TagItem[]): TagDiffRow[] {
	const a = new Map(before.map((i) => [itemId(i), i]));
	const b = new Map(after.map((i) => [itemId(i), i]));
	const rows: TagDiffRow[] = [];
	for (const [id, i] of b) {
		const old = a.get(id);
		if (!old)
			rows.push({ kind: i.kind, key: i.key, label: i.label, change: 'added', before: null, after: i.detail });
		else if (old.detail !== i.detail)
			rows.push({
				kind: i.kind,
				key: i.key,
				label: i.label,
				change: 'changed',
				before: old.detail,
				after: i.detail,
			});
	}
	for (const [id, i] of a)
		if (!b.has(id))
			rows.push({
				kind: i.kind,
				key: i.key,
				label: i.label,
				change: 'removed',
				before: i.detail,
				after: null,
			});
	const order = { added: 0, changed: 1, removed: 2 } as const;
	return rows.sort(
		(x, y) => order[x.change] - order[y.change] || x.kind.localeCompare(y.kind) || x.key.localeCompare(y.key),
	);
}

/** Deterministic 12-hex content hash of a tag's items (FNV-1a over two seeds; a stand-in for sha256). */
export function contentHash(items: TagItem[]): string {
	const text = JSON.stringify(items.map((i) => [i.kind, i.key, i.label, i.detail]));
	const fnv = (seed: number) => {
		let h = seed >>> 0;
		for (let i = 0; i < text.length; i++) {
			h ^= text.charCodeAt(i);
			h = Math.imul(h, 0x01000193) >>> 0;
		}
		return h.toString(16).padStart(8, '0');
	};
	return `${fnv(0x811c9dc5)}${fnv(0x01000193).slice(0, 4)}`;
}

/**
 * Applies untagged revisions as tag `v` (the Package step): `add` starts an item at v (or revives it), `patch`
 * changes its detail from v on, `tombstone` ends it at v-1. Mutates and returns `items`.
 */
export function applyDrafts(items: VersionedItem[], drafts: DraftRevision[], v: number): VersionedItem[] {
	for (const d of drafts) {
		const live = items.find(
			(i) => i.kind === d.item.kind && i.key === d.item.key && (i.to === undefined || i.to >= v),
		);
		if (d.op === 'add' && !live) items.push({ ...d.item, from: v });
		else if ((d.op === 'add' || d.op === 'patch') && live) {
			live.changes = { ...(live.changes ?? {}), [v]: d.item.detail };
			live.label = d.item.label;
		} else if (d.op === 'tombstone' && live) live.to = v - 1;
	}
	return items;
}

// ---------------------------------------------------------------------------------------------------------------
// Object state machines

export const isBackendEvent = (verb: string) => verb.startsWith('backend:');

/**
 * Per operation verb of the state machine: the states it may run from, and the `invalid_state:<s>` error for every
 * other state ("complete by construction", 11 §6.1). Backend events are not operations and yield no errors.
 */
export function verbStates(def: Pick<ObjectDef, 'states' | 'transitions'>): VerbStates[] {
	const verbs = [...new Set(def.transitions.map((t) => t.verb).filter((v) => !isBackendEvent(v)))];
	return verbs.map((verb) => {
		const from = new Set(def.transitions.filter((t) => t.verb === verb).map((t) => t.from_status));
		const allowed = def.states.map((s) => s.status).filter((s) => from.has(s));
		return {
			verb,
			allowed_from: allowed,
			invalid_states: def.states.map((s) => s.status).filter((s) => !from.has(s)),
		};
	});
}

/** `invalid_state:<s>` codes for one `object.verb` (empty when the verb is not a state transition). */
export function invalidStatesFor(def: Pick<ObjectDef, 'states' | 'transitions'>, verb: string): string[] {
	return (verbStates(def).find((v) => v.verb === verb)?.invalid_states ?? []).map(
		(s) => `invalid_state:${s}`,
	);
}

/** Guard rules on the transitions of one verb (`ineligible:<rule>` errors). */
export function guardRulesFor(transitions: ObjectTransition[], verb: string): string[] {
	return [...new Set(transitions.filter((t) => t.verb === verb && t.guard_rule).map((t) => t.guard_rule!))];
}

// ---------------------------------------------------------------------------------------------------------------
// Operations

export const ATTR_KEYS: (keyof OperationAttrs)[] = [
	'modality',
	'is_derive',
	'identity_level',
	'side_effect',
	'idempotency',
	'input_fields',
	'output_fields',
	'confirm_back',
	'verify_read',
	'builtin',
	'op_kind',
	'guard_rules',
];

export const opKey = (object: string, verb: string, variant: string) =>
	`${object}.${verb}${variant === '*' ? '' : `{${variant}}`}`;

/** The system errors every connector-routed operation can end in (doc 10 §3.1); built-ins run in agent. */
const SYSTEM_ERRORS = ['upstream_unavailable', 'timeout', 'rate_limited', 'connector_auth_failed'];

/**
 * The error list an operation can return, generated from its attributes, the object's state machine and its rules
 * (11 §3.2): user-facing outcomes first, then the escalating system errors.
 */
export function errorStatesFor(
	a: OperationAttrs,
	verb: string,
	def: ObjectDef | undefined,
	extra: string[],
): string[] {
	const out: string[] = [];
	if (verb === 'read' || verb === 'verify') out.push('not_found');
	if (verb === 'list') out.push('ambiguous');
	if (a.identity_level !== '0') out.push('identity_required');
	if (a.side_effect !== 'R') out.push('invalid_input', 'conflict');
	if (a.idempotency === 'key+dup') out.push('duplicate');
	out.push(...extra);
	const full = def ? `${def.object}.${verb}` : verb;
	if (def) out.push(...invalidStatesFor(def, full));
	const rules = [...a.guard_rules, ...(def ? guardRulesFor(def.transitions, full) : [])];
	out.push(...[...new Set(rules)].map((r) => `ineligible:${r}`));
	if (!a.builtin) out.push(...SYSTEM_ERRORS);
	if (a.side_effect !== 'R' && !a.builtin) out.push('unknown_outcome');
	return [...new Set(out)];
}

/**
 * Resolves `operation_def` rows as the release cut does: a variant row is merged with its `*` row column by column
 * (NULL = inherit). Rows come back in catalogue order, each `*` row followed by its variants.
 */
export function resolveOperations(rows: OperationDefRow[], objects: ObjectDef[]): OperationRow[] {
	const out: OperationRow[] = [];
	const bases = rows.filter((r) => r.variant === '*');
	for (const base of bases) {
		const group = [
			base,
			...rows.filter((r) => r.variant !== '*' && r.object === base.object && r.verb === base.verb),
		];
		for (const r of group) {
			const attrs = { ...(base.attrs as OperationAttrs), ...r.attrs };
			const overridden = r.variant === '*' ? [] : ATTR_KEYS.filter((k) => k in r.attrs);
			const inherited = r.variant === '*' ? [] : ATTR_KEYS.filter((k) => !(k in r.attrs));
			const def = objects.find((o) => o.object === r.object);
			out.push({
				...attrs,
				key: opKey(r.object, r.verb, r.variant),
				object: r.object,
				verb: r.verb,
				variant: r.variant,
				discriminator: r.discriminator,
				overridden,
				inherited,
				description: r.description,
				retail: r.retail,
				stages: r.stages,
				error_states: errorStatesFor(attrs, r.verb, def, [...base.extra_errors, ...r.extra_errors]),
			});
		}
	}
	return out;
}

/** The success JSON an operation returns, per the convention (gen:36, 11 §3.2). */
export function successExample(op: OperationRow, status: string | null): Record<string, unknown> {
	return {
		operation: op.key,
		outcome: 'ok',
		object: op.object,
		id: `<${op.object}_id>`,
		status: status ?? '<canonical status after the operation>',
		data: Object.fromEntries(op.output_fields.map((f) => [f, '…'])),
		reference: op.side_effect === 'R' ? null : '<confirmation / case / transaction ref>',
		effective_at: '2026-10-18T10:42:00Z',
		next_steps: op.side_effect === 'R' ? [] : ['<what the customer should expect>'],
	};
}

/** The error JSON for one code (gen:37, 11 §3.2); `invalid_state:*` and `ineligible:*` carry their suffix. */
export function errorExample(op: OperationRow, code: string, defs: ErrorStateDef[]): Record<string, unknown> {
	const [family, detail] = code.includes(':') ? code.split(':', 2) : [code, null];
	const def = defs.find((d) => d.code === family);
	return {
		operation: op.key,
		outcome: 'error',
		error: {
			code: family,
			...(family === 'ineligible' ? { rule: detail } : {}),
			...(family === 'invalid_state' ? { status: detail } : {}),
			reason: def?.description ?? family,
			retryable: def?.retryable ?? false,
			user_facing: def?.user_facing ?? false,
		},
	};
}

/** URL segment for a layer key: `industry_dept/retail/customer_service` → `industry_dept.retail.customer_service`. */
export const layerSlug = (layer: string) => layer.replaceAll('/', '.');
export const slugLayer = (slug: string) => decodeURIComponent(slug).replaceAll('.', '/');
export const tagHref = (layer: string, v: number) => `/platform/templates/${layerSlug(layer)}/v${v}`;

// ---------------------------------------------------------------------------------------------------------------
// Layer composition (P7): a job composes base → industry → base department (D116) → department → job; later layers
// override earlier ones.

const SCOPE_RANK: Record<TemplateScope, number> = {
	base: 0,
	industry: 1,
	base_dept: 2,
	industry_dept: 3,
	job: 4,
};

type StackLayer = { layer_key: string; scope: TemplateScope; industry: string | null };

/** Does `lower` sit under `upper` in a job's composition? Base under everything; a D116 base department under every
 *  department and job head whatever the industry (never under an industry); else the same industry. */
const composesUnder = (lower: StackLayer, upper: StackLayer) =>
	SCOPE_RANK[lower.scope] < SCOPE_RANK[upper.scope] &&
	(lower.scope === 'base' ||
		lower.scope === 'base_dept' ||
		(lower.industry !== null && lower.industry === upper.industry));

/** The layers `layer` composes with from the bottom up, ending with itself (base → industry → department → job). */
export function layerStack<T extends StackLayer>(layers: T[], layer: T): T[] {
	return [...layers.filter((x) => composesUnder(x, layer)), layer].sort(
		(a, b) => SCOPE_RANK[a.scope] - SCOPE_RANK[b.scope] || a.layer_key.localeCompare(b.layer_key),
	);
}

/** The layers that compose on top of `layer` and so inherit what it contributes. */
export function composedOnBy<T extends StackLayer>(layers: T[], layer: T): T[] {
	return layers
		.filter((x) => composesUnder(layer, x))
		.sort((a, b) => SCOPE_RANK[a.scope] - SCOPE_RANK[b.scope] || a.layer_key.localeCompare(b.layer_key));
}

/** Where a pin stands against the tag being viewed: on it, behind it (gets it on re-point + cut), or past it. */
export function pinRelation(pinned: number, viewed: number): 'here' | 'behind' | 'ahead' {
	return pinned === viewed ? 'here' : pinned < viewed ? 'behind' : 'ahead';
}
