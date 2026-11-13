// J24 Buses & topics — the job's message path (edge → pre-gate/qualifier → customer topic → orchestrator → worker
// topics ⇄ tool topics → results → resolved → post-gate → reply) as fixture data, plus the pure helpers the page uses
// to read it. Shapes in api/types/runtime.ts (FlowHop, TopicInfo, BusFlow). Sources: spec 03 §1.3 (bus mechanics,
// MAXLEN ~100 000, 5 deliveries → bus:dead), §2.8 (target lifecycle with the edge), §3.1/§3.2 (pre-/post-gate points),
// §2.10 (durable telegram:out); D54 (test env reads the draft). Owned by the J24 page. Tests: runtime-J24.test.ts.
import type {
	BusFlow,
	DeadLetter,
	Env,
	FlowHop,
	GroupInfo,
	StreamInfo,
	TopicInfo,
} from '$lib/api/types/runtime';
import { groupHealth } from './labels';

/** `XADD MAXLEN ~ 100000` on every stream (03 §1.3). */
export const MAXLEN = 100_000;
/** Deliveries before a consumer gives up and moves the entry to `bus:dead` (03 §1.3). */
export const MAX_DELIVERIES = 5;
/** Window the rates are averaged over. */
export const WINDOW_MIN = 5;

interface TopicMeta {
	events: string[];
	producers: string[];
	/** msgs/min in production; test traffic is staff test chats only. */
	prod: number;
	test: number;
	testNote?: string;
}

/** Who writes which stream, what it carries and how busy it is (fixture rates, last 5 min). */
function metaOf(key: string): TopicMeta {
	const p = key.split(':');
	if (p[1] === 'customer')
		return p[2] === 'order-support'
			? {
					events: ['request.qualified', 'request.resolved'],
					producers: ['agent-edge', 'orchestrator'],
					prod: 17.4,
					test: 1.2,
				}
			: {
					events: ['request.qualified', 'request.resolved'],
					producers: ['agent-edge', 'orchestrator'],
					prod: 3.8,
					test: 0.2,
				};
	if (p[1] === 'worker' && p[3] === 'results')
		return { events: ['work.succeeded', 'work.failed'], producers: ['workers'], prod: 26.6, test: 1.4 };
	if (p[1] === 'worker') {
		const r: Record<string, [number, number]> = {
			knowledge: [6.2, 0.4],
			order: [9.8, 0.6],
			delivery: [3.1, 0.2],
			payment: [1.2, 0],
			case: [2.9, 0.2],
			rules: [3.4, 0],
		};
		const [prod, test] = r[p[3] ?? ''] ?? [1, 0];
		return { events: ['work.requested'], producers: ['orchestrator'], prod, test };
	}
	if (p[1] === 'tool') {
		const r: Record<string, [number, number, string?]> = {
			shopify: [14.2, 0.8, 'Test runs call shopify-test.'],
			frappe: [
				3.9,
				0,
				'No test-env Frappe instance: write steps are not exercised in test (route “write unverified”).',
			],
			stripe: [0.6, 0, 'Test runs call stripe-checkout-test.'],
		};
		const [prod, test, testNote] = r[p[2] ?? ''] ?? [1, 0];
		return {
			events: ['tool.call', 'tool.response'],
			producers: ['workers', 'tool-executor'],
			prod,
			test,
			testNote,
		};
	}
	if (p[1] === 'dead') return { events: ['dead.letter'], producers: ['any consumer'], prod: 0, test: 0 };
	return { events: [], producers: [], prod: 0, test: 0 };
}

const round = (n: number) => Math.round(n * 10) / 10;
const total = (ts: TopicInfo[], f: (t: TopicInfo) => boolean) =>
	round(ts.filter(f).reduce((a, t) => a + t.rate_per_min, 0));

const RANK = { idle: 0, ok: 1, warn: 2, err: 3 } as const;
const worst = (xs: FlowHop['status'][]): FlowHop['status'] =>
	xs.reduce<FlowHop['status']>((a, b) => (RANK[b] > RANK[a] ? b : a), 'ok');

/** Health of a set of consumer groups: idle when nobody consumes, else the worst group. */
export function groupsStatus(groups: GroupInfo[]): FlowHop['status'] {
	if (groups.length === 0) return 'ok';
	if (groups.every((g) => g.consumers === 0)) return 'idle';
	return worst(groups.map(groupHealth));
}

/** Build the J24 flow for one environment from the job's streams and dead letters. */
export function buildBusFlow(
	env: Env,
	tenant: string,
	job: string,
	streams: StreamInfo[],
	dead: DeadLetter[],
): BusFlow {
	const test = env === 'test';
	const live = streams.some((s) => s.groups.some((g) => g.consumers > 0));
	const topics: TopicInfo[] = streams.map((s) => {
		const m = metaOf(s.key);
		const rate = live ? (test ? m.test : m.prod) : 0;
		return {
			key: s.key,
			bus: s.bus,
			events: m.events,
			producers: m.producers,
			// Test streams are short-lived: staff test chats only, nothing waits in them.
			groups: test
				? s.groups.map((g) => ({ ...g, consumers: Math.min(g.consumers, 1), pending: 0, lag: 0 }))
				: s.groups.map((g) => ({ ...g })),
			rate_per_min: rate,
			length: test ? (s.bus === 'dead' ? 0 : Math.max(0, Math.round(s.length / 60))) : s.length,
			maxlen: MAXLEN,
			dead: test ? 0 : dead.filter((d) => d.dead.stream === s.key).length,
			note: test ? m.testNote : undefined,
		};
	});
	const of = (f: (t: TopicInfo) => boolean) => topics.filter(f);
	const keys = (f: (t: TopicInfo) => boolean) => of(f).map((t) => t.key);
	const groupsNamed = (pred: (name: string) => boolean) =>
		topics.flatMap((t) => t.groups.filter((g) => pred(g.name)));
	const st = (pred: (name: string) => boolean): FlowHop['status'] =>
		live ? groupsStatus(groupsNamed(pred)) : 'idle';
	const topicSt = (f: (t: TopicInfo) => boolean): FlowHop['status'] =>
		live ? worst(of(f).map((t) => groupsStatus(t.groups))) : 'idle';

	const isCust = (t: TopicInfo) => t.bus === 'customer';
	const isWork = (t: TopicInfo) => t.bus === 'worker' && !t.key.endsWith(':results');
	const isResults = (t: TopicInfo) => t.key.endsWith(':results');
	const isTool = (t: TopicInfo) => t.bus === 'tool';
	const k = test ? 1 / 14 : 1;
	const r = (n: number) => (live ? round(n * k) : 0);
	const custRate = total(topics, isCust);
	// The customer topic carries both directions: ≈ 55 % qualified (in), 45 % resolved (out).
	const qualified = round(custRate * 0.55);
	const resolved = round(custRate - qualified);

	const hops: FlowHop[] = [
		{
			id: 'channel-in',
			phase: 'in',
			kind: 'channel',
			label: test ? 'Test chat (staff)' : 'Channels → edge',
			does: test
				? 'Staff test chats against the draft. Nothing reaches a customer.'
				: 'Telegram webhook and staff chat land on agent-edge: deduplicated, logged, then drained through the licence gate.',
			streams: [],
			events: ['customer message'],
			agents: ['agent-edge'],
			rate_per_min: r(14.6),
			p95_ms: live ? 40 : null,
			exits: test ? [] : [{ label: 'refused by licence gate', per_min: 0, tone: 'ok' }],
			link: test ? { href: '/chat', label: 'Test chat' } : { href: '/admin/channels', label: 'Channels' },
			status: live ? 'ok' : 'idle',
		},
		{
			id: 'pre-gate',
			phase: 'in',
			kind: 'gate',
			label: 'Pre-gate + qualifier',
			does: 'P1 message gate (abuse, out of scope, language) → qualify with the LLM → P2 request gate confirms before publish.',
			streams: [],
			events: [],
			agents: ['qualifier'],
			rate_per_min: r(14.6),
			p95_ms: live ? 1900 : null,
			exits: [
				{ label: 'asked the customer for more', per_min: r(1.9), tone: 'ok' },
				{ label: 'deflected (out of scope)', per_min: r(1.0), tone: 'warn' },
			],
			link: { href: '/gates', label: 'Gates' },
			status: st((n) => n === 'qualifier'),
		},
		{
			id: 'qualified',
			phase: 'in',
			kind: 'topic',
			label: 'request.qualified',
			does: 'One entry per qualified request; the case id is the correlation id from here on.',
			streams: keys(isCust),
			events: ['request.qualified'],
			agents: [],
			rate_per_min: qualified,
			p95_ms: null,
			status: topicSt(isCust),
		},
		{
			id: 'orchestrator',
			phase: 'work',
			kind: 'agent',
			label: 'Orchestrator · procedure',
			does: test
				? 'Opens the case, picks the DRAFT procedure version and plans its steps (case deadline 3,600 s).'
				: 'Opens the case, picks the published procedure of the live release and plans its steps (case deadline 3,600 s).',
			streams: [],
			events: [],
			agents: ['orchestrator'],
			rate_per_min: qualified,
			p95_ms: live ? 120 : null,
			link: { href: '/procedures', label: 'Procedures' },
			status: st((n) => n === 'orchestrator'),
		},
		{
			id: 'work',
			phase: 'work',
			kind: 'topic',
			label: 'work.requested',
			does: 'One stream per worker; each step of the procedure becomes one entry.',
			streams: keys(isWork),
			events: ['work.requested'],
			agents: [],
			rate_per_min: total(topics, isWork),
			p95_ms: null,
			status: topicSt(isWork),
		},
		{
			id: 'workers',
			phase: 'work',
			kind: 'agent',
			label: 'Workers',
			does: 'Run one step each: read knowledge, check a rule, or call a system through the tool topics.',
			streams: [],
			events: [],
			agents: groupsNamed((n) => n.startsWith('worker-')).map((g) => g.name),
			rate_per_min: total(topics, isWork),
			p95_ms: live ? 640 : null,
			link: { href: '/agents', label: 'Agents' },
			status: st((n) => n.startsWith('worker-')),
		},
		{
			id: 'tools',
			phase: 'work',
			kind: 'topic',
			label: 'tool.call ⇄ tool.response',
			does: test
				? 'Round trip to the tool executor, which calls the TEST-env systems.'
				: 'Round trip to the tool executor, which calls your systems (Shopify, Frappe, Stripe).',
			streams: keys(isTool),
			events: ['tool.call', 'tool.response'],
			agents: ['tool-executor'],
			rate_per_min: total(topics, isTool),
			p95_ms: live ? 820 : null,
			loop: true,
			link: { href: '/operation-calls', label: 'Operation calls' },
			status: live ? worst([topicSt(isTool), st((n) => n === 'tool-executor')]) : 'idle',
		},
		{
			id: 'results',
			phase: 'work',
			kind: 'topic',
			label: 'work.succeeded / work.failed',
			does: 'Each finished step reports back; the orchestrator plans the next step or resolves the case.',
			streams: keys(isResults),
			events: ['work.succeeded', 'work.failed'],
			agents: [],
			rate_per_min: total(topics, isResults),
			p95_ms: null,
			exits: [{ label: 'step failed', per_min: r(1.2), tone: 'warn' }],
			status: topicSt(isResults),
		},
		{
			id: 'resolved',
			phase: 'out',
			kind: 'topic',
			label: 'request.resolved',
			does: 'The orchestrator publishes the outcome and its facts on the customer topic.',
			streams: keys(isCust),
			events: ['request.resolved'],
			agents: [],
			rate_per_min: resolved,
			p95_ms: null,
			exits: [{ label: 'handed over to staff', per_min: r(0.6), tone: 'warn' }],
			link: { href: '/handover', label: 'Handover queue' },
			status: topicSt(isCust),
		},
		{
			id: 'post-gate',
			phase: 'out',
			kind: 'gate',
			label: 'Post-gate (R1)',
			does: 'Fact-checks the composed reply against the facts it was built from; fails closed to the escalation text.',
			streams: [],
			events: [],
			agents: ['post-gate'],
			rate_per_min: resolved,
			p95_ms: live ? 850 : null,
			exits: [{ label: 'failed closed → escalation text', per_min: r(0.3), tone: 'err' }],
			link: { href: '/gates', label: 'Gates' },
			status: st((n) => n === 'post-gate'),
		},
		{
			id: 'reply',
			phase: 'out',
			kind: 'channel',
			label: test ? 'Reply → test chat' : 'Reply → customer',
			does: test
				? 'The reply appears in the staff test chat; the case is recorded as env=test with the draft revision.'
				: 'Interaction agent appends the reply; Telegram via the durable telegram:out stream (acked after 2xx), staff chat via SSE.',
			streams: [],
			events: ['reply'],
			agents: ['interaction'],
			rate_per_min: resolved,
			p95_ms: live ? 300 : null,
			link: { href: '/cases', label: 'Cases' },
			status: st((n) => n === 'interaction'),
		},
	];

	return {
		env,
		prefix: `s:${tenant}:${job}:${test ? 'test:' : ''}`,
		reads: test ? 'draft' : 'live',
		window_min: WINDOW_MIN,
		hops: streams.length ? hops : [],
		topics,
		dead_letters: test ? 0 : dead.length,
	};
}

// ---- page helpers ---------------------------------------------------------------------------------------------------

/** `14.6/min`, `0.4/min`, `—` when nothing flows. */
export function rate(n: number): string {
	if (!n) return '—';
	return `${n >= 10 ? Math.round(n) : n.toFixed(1)}/min`;
}

/** How long the MAXLEN cap keeps history at the current rate: `≈ 4 days`, `≈ 11 h`, or `until replayed` for bus:dead. */
export function retention(t: TopicInfo): string {
	if (t.bus === 'dead') return 'until replayed or discarded';
	if (!t.rate_per_min) return `last ~${(t.maxlen / 1000).toFixed(0)}k entries`;
	const min = t.maxlen / t.rate_per_min;
	if (min >= 2 * 24 * 60) return `≈ ${Math.round(min / 1440)} days`;
	return `≈ ${Math.max(1, Math.round(min / 60))} h`;
}

export const PHASES: { id: FlowHop['phase']; label: string; sub: string }[] = [
	{ id: 'in', label: 'In', sub: 'customer message → qualified request' },
	{ id: 'work', label: 'Work', sub: 'the procedure runs its steps' },
	{ id: 'out', label: 'Out', sub: 'outcome → checked reply' },
];

/** Readable name for a producer / process id on this page. */
export function procName(id: string): string {
	if (id === 'agent-edge') return 'Edge (qualifier)';
	if (id === 'workers') return 'Workers';
	if (id === 'any consumer') return 'Any consumer';
	if (id === 'tool-executor') return 'Tool executor';
	if (id === 'orchestrator') return 'Orchestrator';
	return id;
}

/** What to do with a dead letter, by reason code (03 §1.3 retry/DLQ). */
export const DEAD_ADVICE: Record<string, { what: string; then: string; tone: 'err' | 'warn' | 'muted' }> = {
	gave_up: {
		what: 'Delivered again and again and failed every time (usually the system behind it was down).',
		then: 'Fix or wait for the system, then Replay.',
		tone: 'warn',
	},
	decode_error: {
		what: 'The consumer could not read the message (a version skew during a roll-out).',
		then: 'Replay once all agents run the same version.',
		tone: 'muted',
	},
	handler_panic: {
		what: 'The consumer crashed while handling it.',
		then: 'Inspect the payload; replay after the fix ships.',
		tone: 'warn',
	},
	scope_mismatch: {
		what: 'Stamped for another tenant or job: it must never be handled here.',
		then: 'Discard. Never replay.',
		tone: 'err',
	},
};

/** Dead letters grouped by reason code, most dangerous first. */
export function deadByReason(dead: DeadLetter[]): { code: string; n: number }[] {
	const order = ['scope_mismatch', 'gave_up', 'handler_panic', 'decode_error'];
	const m = new Map<string, number>();
	for (const d of dead) m.set(d.dead.reason_code, (m.get(d.dead.reason_code) ?? 0) + 1);
	return [...m.entries()].map(([code, n]) => ({ code, n })).sort((a, b) => rank(a.code) - rank(b.code));
	function rank(code: string) {
		const i = order.indexOf(code);
		return i < 0 ? order.length : i;
	}
}
