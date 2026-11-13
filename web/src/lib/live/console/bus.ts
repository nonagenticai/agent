// Everything the console derives from the envelopes it receives (from cb:web/src/lib/console/bus.ts@45ee128,
// re-based on the operation runtime of doc 12 §3: interaction → customer-bus `request.qualified` → orchestrator →
// `op.call` on `bus:op` → operation runner → `op.result` on `bus:op:results` → orchestrator → `request.resolved`).
// Op envelopes are mirrored to the console as ids + outcome codes only (K50): no inputs, no customer data.
import type { BusEvent, LiveAgent } from '$lib/api/types/live';
import { busKey, direction } from './store.svelte';

export const EVENTS = ['request.qualified', 'request.resolved', 'op.call', 'op.result'] as const;
export type BusEventName = (typeof EVENTS)[number];

/** `order-support.returns.request.qualified` → `request.qualified`; `op.call` → `op.call`. */
export function eventOf(topic: string): BusEventName | null {
	return EVENTS.find((e) => topic === e || (topic.endsWith(`.${e}`) && topic.length > e.length + 1)) ?? null;
}
/** The context-space part of a topic, with its trailing dot: `order-support.returns.`. */
export function contextOf(topic: string): string {
	const e = eventOf(topic);
	return e && topic !== e ? topic.slice(0, topic.length - e.length) : '';
}

const obj = (v: unknown): Record<string, unknown> =>
	v !== null && typeof v === 'object' && !Array.isArray(v) ? (v as Record<string, unknown>) : {};
const str = (v: unknown) => (typeof v === 'string' || typeof v === 'number' ? String(v) : '');
const words = (v: unknown) => str(v).replaceAll('_', ' ').replaceAll(':', ' · ');

/** One-line summary of the payload. */
export function summary(e: BusEvent): string {
	const p = obj(e.envelope.payload);
	switch (eventOf(e.envelope.topic)) {
		case 'request.qualified':
			return [str(p.activity), str(p.channel), str(p.env)].filter(Boolean).join(' · ');
		case 'request.resolved':
			return [str(p.outcome), str(p.reason)].filter(Boolean).join(' · ');
		case 'op.call': {
			const attempt = Number(p.attempt) || 1;
			return `${str(p.operation)} · ${str(p.env)}${attempt > 1 ? ` · retry ${attempt - 1}` : ''}${p.reason ? ` · ${str(p.reason)}` : ''}`;
		}
		case 'op.result':
			return [
				str(p.operation),
				str(p.code),
				str(p.instance_id),
				p.latency_ms !== undefined ? `${str(p.latency_ms)} ms` : '',
			]
				.filter(Boolean)
				.join(' · ');
		default:
			return '';
	}
}

/** Plain-language sentence for an event. Technical ids stay in the row below it. */
export function plain(e: BusEvent): string {
	const p = obj(e.envelope.payload);
	switch (eventOf(e.envelope.topic)) {
		case 'request.qualified':
			return `Customer request understood as ${words(p.activity) || 'a request'} (${str(p.channel) || 'channel'}, ${str(p.env) || 'prod'}); handed to the orchestrator.`;
		case 'request.resolved': {
			const outcome = str(p.outcome);
			const what =
				outcome === 'completed'
					? 'completed'
					: outcome === 'not_eligible'
						? 'closed as not eligible'
						: outcome === 'handed_over'
							? 'handed over to a colleague'
							: outcome === 'failed'
								? 'failed'
								: words(outcome) || 'resolved';
			return `Case ${what}${str(p.reason) ? `: ${str(p.reason)}` : ''}.`;
		}
		case 'op.call':
			if (e.stream === 'bus:dead')
				return `Call ${str(p.call_id)} to ${str(p.operation)} was dead-lettered (${str(p.reason) || 'unprocessable'}).`;
			return `Orchestrator asked the runner to call ${str(p.operation)}${Number(p.attempt) > 1 ? ` (retry ${Number(p.attempt) - 1})` : ''}.`;
		case 'op.result': {
			const st = str(p.state);
			if (st === 'ok') return `${str(p.operation)} answered by ${str(p.instance_id)}.`;
			if (st === 'unknown_outcome')
				return `${str(p.operation)} outcome unknown (${str(p.code)}): it may have happened; nothing is retried.`;
			return `${str(p.operation)} failed on ${str(p.instance_id)}: ${str(p.code) || 'error'}.`;
		}
		default:
			return '';
	}
}

export interface EventLink {
	kind: 'case' | 'operation' | 'system';
	label: string;
	href: string;
}
/** Where an event's own data names a case, an operation or a connection system. Nothing is guessed. */
export function links(e: BusEvent): EventLink[] {
	const p = obj(e.envelope.payload);
	const job = encodeURIComponent(e.envelope.job);
	const out: EventLink[] = [];
	const caseId = str(p.case_id);
	if (caseId)
		out.push({ kind: 'case', label: caseId, href: `/admin/jobs/${job}/cases/${encodeURIComponent(caseId)}` });
	const op = str(p.operation);
	if (op)
		out.push({
			kind: 'operation',
			label: op,
			href: `/admin/jobs/${job}/operation-calls?operation=${encodeURIComponent(op)}`,
		});
	const inst = str(p.instance_id);
	if (inst && !['builtin', 'agent_cases', 'strata_rag'].includes(inst))
		out.push({ kind: 'system', label: inst, href: '/admin/connections/systems' });
	return out;
}

/** The case an event belongs to, or ''. */
export const caseOf = (e: BusEvent): string =>
	str(obj(e.envelope.payload).case_id) || e.envelope.correlation_id;
/** The env a case runs in, when the entry says. */
export const envOf = (e: BusEvent): string => str(obj(e.envelope.payload).env);

/** A failure on the bus: a failed or unknown op result, a failed resolution, or a dead letter. */
export function isBad(e: BusEvent): boolean {
	const p = obj(e.envelope.payload);
	const ev = eventOf(e.envelope.topic);
	return (
		e.stream === 'bus:dead' ||
		(ev === 'op.result' && p.state !== 'ok') ||
		(ev === 'request.resolved' && p.outcome === 'failed')
	);
}

const REPLY: Partial<Record<BusEventName, BusEventName[]>> = {
	'request.qualified': ['request.resolved'],
	'op.call': ['op.result'],
};
function matchId(e: BusEvent): string {
	const p = obj(e.envelope.payload);
	switch (eventOf(e.envelope.topic)) {
		case 'request.qualified':
		case 'request.resolved':
			return str(p.case_id) || e.envelope.correlation_id;
		case 'op.call':
		case 'op.result':
			return str(p.call_id);
		default:
			return '';
	}
}

export type ItemStatus = 'run' | 'ok' | 'fail';
export interface WorkItem {
	key: string;
	event: BusEvent;
	reply: BusEvent | null;
	status: ItemStatus;
}

/** Every forward event with its answer (if one arrived). `run` means nothing has answered it yet. */
export function workItems(events: BusEvent[]): WorkItem[] {
	const replies = new Map<string, BusEvent>();
	for (const e of events) {
		const ev = eventOf(e.envelope.topic);
		const id = matchId(e);
		if (!ev || !id || e.stream === 'bus:dead') continue;
		for (const [down, ups] of Object.entries(REPLY)) if (ups!.includes(ev)) replies.set(`${down}|${id}`, e);
	}
	const out: WorkItem[] = [];
	for (const e of events) {
		const ev = eventOf(e.envelope.topic);
		if (!ev || !REPLY[ev] || e.stream === 'bus:dead') continue;
		const reply = replies.get(`${ev}|${matchId(e)}`) ?? null;
		out.push({ key: busKey(e), event: e, reply, status: !reply ? 'run' : isBad(reply) ? 'fail' : 'ok' });
	}
	return out;
}

export type AgentStatus = 'working' | 'waiting' | 'ready' | 'backlog' | 'offline' | 'unknown';
export interface AgentCard {
	id: string;
	name: string;
	kind: LiveAgent['kind'];
	view: LiveAgent | null;
	status: AgentStatus;
	line: string;
	items: WorkItem[];
}

/** The runtime's agents (doc 12 §2.1, §3.1), used until the backlog's metrics arrive. */
export const CATALOGUE: Pick<LiveAgent, 'id' | 'name' | 'kind'>[] = [
	{ id: 'interaction', name: 'Interaction (edge)', kind: 'front' },
	{ id: 'orchestrator', name: 'Orchestrator', kind: 'coordination' },
	{ id: 'runner', name: 'Operation runner', kind: 'runner' },
	{ id: 'post_gate', name: 'Post-gate (R1 fact check)', kind: 'gate' },
];

/**
 * Agent cards for the strip. `working` (it consumes a forward event nobody answered yet), `waiting` (it published
 * one still unanswered), then from the metrics: `backlog` (warn), `ready` (consumers), `offline`, `unknown`.
 */
export function agentCards(events: BusEvent[], views: LiveAgent[] | null): AgentCard[] {
	const items = workItems(events);
	const list =
		views && views.length
			? views.map((v) => ({ id: v.id, name: v.name, kind: v.kind, view: v as LiveAgent | null }))
			: CATALOGUE.map((c) => ({ ...c, view: null as LiveAgent | null }));
	return list.map(({ id, name, kind, view }) => {
		const mine = items.filter((w) => w.event.consumer_group === id || w.event.envelope.from === id);
		const consuming = mine.filter((w) => w.status === 'run' && w.event.consumer_group === id);
		const publishing = mine.filter((w) => w.status === 'run' && w.event.envelope.from === id);
		const last = [...events].reverse().find((e) => e.consumer_group === id || e.envelope.from === id);
		let status: AgentStatus;
		if (consuming.length) status = 'working';
		else if (publishing.length) status = 'waiting';
		else if (!view) status = 'unknown';
		else if (view.status === 'warn') status = 'backlog';
		else if (view.consumers > 0) status = 'ready';
		else status = 'offline';
		let line = id === 'post_gate' ? 'checks replies against knowledge (no bus traffic)' : '—';
		if (last) {
			const out = last.envelope.from === id;
			const s = summary(last);
			line = `${out ? '→' : '←'} ${eventOf(last.envelope.topic) ?? last.envelope.topic}${s ? ` · ${s}` : ''}`;
		}
		return { id, name, kind, view, status, line, items: mine };
	});
}

/** `HH:MM:SS.mmm` in local time. */
export function clock(ms: number): string {
	const d = new Date(ms);
	if (Number.isNaN(d.getTime())) return '';
	const p = (n: number, w = 2) => String(n).padStart(w, '0');
	return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}.${p(d.getMilliseconds(), 3)}`;
}

/** What the bus shows about one case: latest step, outcome once resolved, first and terminal times. */
export interface Activity {
	caseId: string;
	events: number;
	step: string;
	stepAt: number | null;
	outcome: string | null;
	bad: boolean;
	startedAt: number | null;
	endedAt: number | null;
}
export function activity(events: BusEvent[], caseFilter = ''): Activity | null {
	const caseId = caseFilter || events.at(-1)?.envelope.correlation_id || '';
	if (!caseId) return null;
	const mine = events.filter((e) => e.envelope.correlation_id === caseId);
	const last = mine.at(-1);
	const resolved = mine.findLast((e) => eventOf(e.envelope.topic) === 'request.resolved') ?? null;
	const times = mine.map((e) => e.envelope.published_at_ms).filter((t) => Number.isFinite(t));
	return {
		caseId,
		events: mine.length,
		step: last ? plain(last) || summary(last) || last.envelope.topic : '',
		stepAt: last?.envelope.published_at_ms ?? null,
		outcome: resolved ? plain(resolved) || summary(resolved) || 'resolved' : null,
		bad: resolved ? isBad(resolved) : false,
		startedAt: times.length ? Math.min(...times) : null,
		endedAt: resolved?.envelope.published_at_ms ?? null,
	};
}

/** Compact duration: `42s`, `3m 05s`, `2h 07m`, `1d 04h`. Negative spans read as 0s. */
export function elapsed(ms: number): string {
	const s = Math.max(0, Math.floor(ms / 1000));
	const p = (n: number) => String(n).padStart(2, '0');
	if (s < 60) return `${s}s`;
	const m = Math.floor(s / 60);
	if (m < 60) return `${m}m ${p(s % 60)}s`;
	const h = Math.floor(m / 60);
	if (h < 24) return `${h}h ${p(m % 60)}m`;
	return `${Math.floor(h / 24)}d ${p(h % 24)}h`;
}

/** The console's filters: case, env, free text (operation, code, activity), failures only. */
export interface ConsoleFilter {
	caseId: string;
	env: '' | 'prod' | 'test';
	text: string;
	badOnly: boolean;
}
/** Env is stamped on request.qualified and op.call; results inherit it through their case. */
export function matches(e: BusEvent, f: ConsoleFilter, envOfCase: Map<string, string>): boolean {
	if (f.caseId && e.envelope.correlation_id !== f.caseId) return false;
	if (f.badOnly && !isBad(e)) return false;
	if (f.env && (envOf(e) || envOfCase.get(e.envelope.correlation_id) || '') !== f.env) return false;
	const q = f.text.trim().toLowerCase();
	if (q && !`${e.envelope.topic} ${e.envelope.correlation_id} ${summary(e)}`.toLowerCase().includes(q))
		return false;
	return true;
}
export { direction };
