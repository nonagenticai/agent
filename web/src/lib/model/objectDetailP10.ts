// P10 Object detail — pure rules behind the page (owned by the P10 agent):
//   - objectContextFrom: where an object lives outside its own rows — the template layers that define or reference
//     it (base → industry → department → job, 11 §6.1/§6.2), the connector templates that hold it (and how their
//     system statuses map onto its canonical states, doc 10 §4), and the journey activities that read or write it;
//   - lifecycleLayout: a left→right layout of the state machine with terminal "exit" states in a bottom lane;
//   - stateFacts / verbMatrix: what the agent may do in a status and what it is refused (`invalid_state:<s>`).
import type { ConnectorInstance, ConnectorTemplate } from '$lib/api/types/connectors';
import type { IndustryJourney } from '$lib/api/types/journey';
import type {
	ObjectActivityTouch,
	ObjectContext,
	ObjectDef,
	ObjectLayerTouch,
	ObjectState,
	ObjectSystemHolder,
	ObjectTransition,
	TemplateScope,
} from '$lib/api/types/model';
import type { DraftRevision } from '$lib/api/types/model';
import { isBackendEvent, verbStates, type VersionedItem } from './logic';

/** A template layer as the context read needs it: its rows, tags and draft (the API's layer read supplies them). */
export interface LayerSeed {
	layer_key: string;
	scope: TemplateScope;
	label: string;
	items: VersionedItem[];
	tags: { version: number }[];
	draft: DraftRevision[];
}

const SCOPE_ORDER: Record<TemplateScope, number> = {
	base: 0,
	industry: 1,
	base_dept: 2,
	industry_dept: 3,
	job: 4,
};

/** Strips a variant: `case{return}` → `case`. */
export const baseObject = (key: string) => key.replace(/\{.*\}$/, '');

/** Does free text name the object (`order.cancel`, `object delivery`, `case{return}`)? */
export function mentions(text: string, object: string): boolean {
	const o = object.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
	return new RegExp(`(^|[^a-z_])(${o}\\.[a-z_]|${o}\\{|object ${o}\\b)`).test(text);
}

function latestVersion(l: LayerSeed): number | null {
	return l.tags.reduce<number | null>((a, t) => (a === null || t.version > a ? t.version : a), null);
}

export function objectContextFrom(
	object: string,
	src: {
		layers: LayerSeed[];
		templates: ConnectorTemplate[];
		instances: ConnectorInstance[];
		tenantName: (id: string) => string;
		industries: IndustryJourney[];
	},
): ObjectContext {
	// Template layers: the `object` item that defines it, and authored items that reference it.
	const layers: ObjectLayerTouch[] = [];
	for (const l of [...src.layers].sort((a, b) => SCOPE_ORDER[a.scope] - SCOPE_ORDER[b.scope])) {
		const pending = new Set(l.draft.map((d) => `${d.item.kind}\u0000${d.item.key}`));
		for (const i of l.items) {
			const defines = i.kind === 'object' && i.key === object;
			const refs = !defines && i.kind !== 'gate_question' && mentions(`${i.key} ${i.detail}`, object);
			if (!defines && !refs) continue;
			const changes = Object.entries(i.changes ?? {}).sort((a, b) => Number(a[0]) - Number(b[0]));
			layers.push({
				layer_key: l.layer_key,
				scope: l.scope,
				layer_label: l.label,
				latest_tag: latestVersion(l),
				role: defines ? 'defines' : 'references',
				kind: i.kind,
				key: i.key,
				label: i.label,
				detail: changes.length ? changes[changes.length - 1]![1] : i.detail,
				since: i.from,
				changed_in: changes.map(([v]) => Number(v)),
				removed_after: i.to ?? null,
				pending: pending.has(`${i.kind}\u0000${i.key}`),
			});
		}
		// A draft revision that adds the object (generator Package step) before any tag carries it.
		for (const d of l.draft)
			if (
				d.op === 'add' &&
				d.item.kind === 'object' &&
				d.item.key === object &&
				!l.items.some((i) => i.kind === 'object' && i.key === object)
			)
				layers.push({
					layer_key: l.layer_key,
					scope: l.scope,
					layer_label: l.label,
					latest_tag: latestVersion(l),
					role: 'defines',
					kind: 'object',
					key: object,
					label: d.item.label,
					detail: d.item.detail,
					since: (latestVersion(l) ?? 0) + 1,
					changed_in: [],
					removed_after: null,
					pending: true,
				});
	}

	// Connector templates: the newest version of each template that implements an op on the object.
	const newest = new Map<string, ConnectorTemplate>();
	for (const t of src.templates) {
		if (!t.ops.some((o) => o.object === object)) continue;
		const cur = newest.get(t.key);
		if (!cur || t.version.localeCompare(cur.version, undefined, { numeric: true }) > 0) newest.set(t.key, t);
	}
	const systems: ObjectSystemHolder[] = [...newest.values()].map((t) => {
		const ops = t.ops.filter((o) => o.object === object);
		const seen = new Set<string>();
		const status_map = ops
			.flatMap((o) => o.status_map)
			.filter((m) => !m.canonical.startsWith('invalid_state') && !seen.has(m.system) && seen.add(m.system));
		return {
			template_key: t.key,
			version: t.version,
			label: t.label,
			vendor: t.vendor,
			system_class: t.system_class,
			platform_connector: t.platform_connector,
			ops: ops.map((o) => `${o.object}.${o.verb}${o.variant === '*' ? '' : `{${o.variant}}`}`),
			status_map,
			instances: src.instances
				.filter((i) => i.template_key === t.key)
				.map((i) => ({
					id: i.id,
					label: i.label,
					tenant_id: i.tenant_id,
					tenant_name: src.tenantName(i.tenant_id),
					env: i.env,
					stub: i.stub,
				})),
		};
	});
	systems.sort((a, b) => b.status_map.length - a.status_map.length || b.ops.length - a.ops.length);

	// Journey activities that bind the object (as subject or context) or expect an operation on it.
	const activities: ObjectActivityTouch[] = [];
	for (const j of src.industries)
		for (const a of j.activities) {
			const bind = a.objects.find((o) => baseObject(o.object) === object);
			const ops = a.operations.filter((o) => baseObject(o.op.split('.')[0] ?? '') === object);
			if (!bind && !ops.length) continue;
			activities.push({
				industry: j.industry,
				industry_label: j.label,
				stage: a.stage,
				key: a.key,
				label: a.label,
				actor: a.actor,
				role: bind?.role ?? 'context',
				variant: bind && bind.object.includes('{') ? bind.object.slice(bind.object.indexOf('{')) : null,
				fields: bind?.fields ?? [],
				operations: ops.map((o) => ({ op: o.op, required: o.required })),
			});
		}

	return { object, layers, systems, activities };
}

// ---------------------------------------------------------------------------------------------------------------
// Lifecycle diagram layout

export const NODE_W = 136;
export const NODE_H = 40;
const GX = 104;
const GY = 30;
const PAD = 24;
/** Room above the first row for the loop of a verb that keeps the status. */
const TOP = 46;
const LANE_GAP = 46;

export interface LaidOutState {
	status: string;
	x: number;
	y: number;
	/** Terminal states reached from several states sit in the bottom "ways out" lane. */
	lane: 'main' | 'exit';
}

export function lifecycleLayout(states: ObjectState[], transitions: ObjectTransition[]) {
	const ids = states.map((s) => s.status);
	const moves = transitions.filter((t) => t.from_status !== t.to_status && ids.includes(t.to_status));
	const sources = (id: string) => [
		...new Set(moves.filter((t) => t.to_status === id).map((t) => t.from_status)),
	];
	const exit = new Set(
		states.filter((s) => s.terminal && sources(s.status).length >= 2).map((s) => s.status),
	);
	const incoming = new Set(moves.map((t) => t.to_status));
	const starts = states.filter((s) => s.initial || !incoming.has(s.status)).map((s) => s.status);
	const depth = new Map<string, number>();
	const queue = (starts.length ? starts : ids.slice(0, 1)).filter((s) => !exit.has(s));
	for (const s of queue) depth.set(s, 0);
	while (queue.length) {
		const cur = queue.shift()!;
		for (const t of moves)
			if (t.from_status === cur && !depth.has(t.to_status) && !exit.has(t.to_status)) {
				depth.set(t.to_status, depth.get(cur)! + 1);
				queue.push(t.to_status);
			}
	}
	const maxDepth = Math.max(0, ...depth.values());
	const cols = new Map<number, string[]>();
	for (const id of ids) {
		if (exit.has(id)) continue;
		const d = depth.get(id) ?? maxDepth + 1;
		cols.set(d, [...(cols.get(d) ?? []), id]);
	}
	const colX = (d: number) => PAD + d * (NODE_W + GX);
	const pos = new Map<string, LaidOutState>();
	for (const [d, col] of cols)
		col.forEach((id, i) => pos.set(id, { status: id, x: colX(d), y: TOP + i * (NODE_H + GY), lane: 'main' }));
	const rows = Math.max(1, ...[...cols.values()].map((c) => c.length));
	const laneY = TOP + rows * (NODE_H + GY) - GY + LANE_GAP;
	const taken = new Set<number>();
	for (const id of ids.filter((s) => exit.has(s))) {
		const xs = sources(id)
			.map((s) => pos.get(s)?.x)
			.filter((x): x is number => x !== undefined);
		let d = xs.length ? Math.round((xs.reduce((a, b) => a + b, 0) / xs.length - PAD) / (NODE_W + GX)) : 0;
		while (taken.has(d)) d++;
		taken.add(d);
		pos.set(id, { status: id, x: colX(d), y: laneY, lane: 'exit' });
	}
	const xs = [...pos.values()].map((p) => p.x);
	const ys = [...pos.values()].map((p) => p.y);
	return {
		pos,
		width: Math.max(...xs, 0) + NODE_W + PAD,
		height: Math.max(...ys, 0) + NODE_H + PAD,
		laneY: exit.size ? laneY : null,
	};
}

// ---------------------------------------------------------------------------------------------------------------
// What each status allows

export interface StateFacts {
	/** Operations the agent may run here, with where they lead. */
	allowed: { verb: string; to: string; guard: string | null }[];
	/** Operations of the machine refused here — each yields `invalid_state:<status>`. */
	refused: string[];
	/** System events (not agent operations) that move the object on from here. */
	events: { verb: string; to: string }[];
	/** How an object gets into this status. */
	arrivals: { verb: string; from: string }[];
}

export function stateFacts(def: Pick<ObjectDef, 'states' | 'transitions'>, status: string): StateFacts {
	const out = def.transitions.filter((t) => t.from_status === status);
	return {
		allowed: out
			.filter((t) => !isBackendEvent(t.verb))
			.map((t) => ({ verb: t.verb, to: t.to_status, guard: t.guard_rule })),
		refused: verbStates(def)
			.filter((v) => v.invalid_states.includes(status))
			.map((v) => v.verb),
		events: out.filter((t) => isBackendEvent(t.verb)).map((t) => ({ verb: t.verb, to: t.to_status })),
		arrivals: def.transitions
			.filter((t) => t.to_status === status && t.from_status !== status)
			.map((t) => ({ verb: t.verb, from: t.from_status })),
	};
}

/** Verb × state: the target status when the verb may run there, `null` when it is refused (`invalid_state`). */
export function verbMatrix(def: Pick<ObjectDef, 'states' | 'transitions'>) {
	return verbStates(def).map((v) => ({
		verb: v.verb,
		allowed_from: v.allowed_from,
		invalid_states: v.invalid_states,
		cells: def.states.map((s) => ({
			status: s.status,
			to: def.transitions.find((t) => t.verb === v.verb && t.from_status === s.status)?.to_status ?? null,
		})),
	}));
}

/** `backend:payment_received` → `payment received`; `order.cancel` → `cancel`. */
export const shortVerb = (verb: string) =>
	isBackendEvent(verb) ? verb.slice(8).replaceAll('_', ' ') : verb.split('.').slice(1).join('.');
