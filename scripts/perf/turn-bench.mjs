#!/usr/bin/env node
// Stage 0 turn-latency driver. It reuses the tenant, widget key and customer identities the
// platform-stack smoke journey wrote to journey.json; it does not author a tenant.
//
//   PS_STATE=target/local/platform-stack/journey.json PS_EDGE=http://localhost:8199 \
//   PERF_CONCURRENCY=4 PERF_TURNS=10 node scripts/perf/turn-bench.mjs
//
// The fake LLM has a fixed latency, so a change in these numbers is the app's own overhead.
import { createHmac, randomBytes, randomUUID } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';

const STATE = process.env.PS_STATE || 'target/local/platform-stack/journey.json';
const EDGE = (process.env.PS_EDGE || 'http://localhost:8199').replace(/\/+$/, '');
// The web-chat routes enforce the binding's origin allow-list; the browser sends the page origin.
const EDGE_ORIGIN = new URL(EDGE).origin.replace('127.0.0.1', 'localhost');
const TURNS = Number(process.env.PERF_TURNS || 10);
const CONCURRENCY = Number(process.env.PERF_CONCURRENCY || 1);
const WARMUP = Number(process.env.PERF_WARMUP || 1);
const TIMEOUT_MS = Number(process.env.PERF_TIMEOUT_MS || 180000);
const TEXT = process.env.PERF_TEXT || 'Where is my parcel?';
// The edge limits 30 messages/10 min per client IP and trusts `cf-connecting-ip` (what Cloudflare
// sends in production). Give each worker a distinct TEST-NET-3 client so one run measures the
// platform, not the flood guard. The product's limits are unchanged.
const IP_OFFSET = Number(process.env.PERF_IP_OFFSET || 0);
const clientIp = (w) => `203.0.113.${((IP_OFFSET + w) % 250) + 1}`;

const state = JSON.parse(readFileSync(STATE, 'utf8'));
for (const k of ['widget_key', 'binding', 'id_kid', 'id_secret', 'issuer', 'customers']) {
	if (!state[k]) throw new Error(`${STATE} has no ${k}`);
}
if (!Array.isArray(state.customers) || state.customers.length === 0) throw new Error(`${STATE} has no customers`);

const b64 = (o) => Buffer.from(JSON.stringify(o)).toString('base64url');
function assertionFor(cust) {
	const now = Math.floor(Date.now() / 1000);
	const head = b64({ alg: 'HS256', typ: 'JWT', kid: state.id_kid });
	const claims = b64({ iss: state.issuer, aud: state.binding, sub: String(cust.customer_id), email: cust.email,
		name: cust.name, iat: now, exp: now + 120, jti: randomUUID().replace(/-/g, '') });
	return `${head}.${claims}.${createHmac('sha256', state.id_secret).update(`${head}.${claims}`).digest('base64url')}`;
}

async function post(path, body, headers = {}) {
	const t0 = performance.now();
	const r = await fetch(`${EDGE}${path}`, {
		method: 'POST',
		headers: { 'content-type': 'application/json', origin: EDGE_ORIGIN, ...headers },
		body: JSON.stringify(body),
		signal: AbortSignal.timeout(TIMEOUT_MS),
	});
	const text = await r.text();
	return { status: r.status, text, ms: performance.now() - t0 };
}

async function worker(cust, turns, latencies, w) {
	const client = { 'cf-connecting-ip': clientIp(w) };
	// One visitor per session: a new visitor id per message spends the 10/hour new-visitor budget.
	const visitor = randomBytes(12).toString('hex');
	const s = await post('/api/webchat/sessions', { widget_key: state.widget_key, customer_assertion: assertionFor(cust) }, client);
	if (s.status !== 201) throw new Error(`session ${cust.customer_id} -> ${s.status}: ${s.text.slice(0, 300)}`);
	const { session_token: token } = JSON.parse(s.text);
	if (!token) throw new Error(`session ${cust.customer_id}: no session_token`);
	const auth = { authorization: `Bearer ${token}`, ...client };
	let conv = null;

	for (let i = 0; i < WARMUP + turns; i++) {
		const body = {
			widget_key: state.widget_key,
			// The web-chat abuse guard blocks a session that repeats one text; a short unique
			// suffix keeps the same intent on every turn without tripping it.
			visitor_id: visitor,
			messages: [{ role: 'user', text: `${TEXT} (check ${i + 1})` }],
			...(conv ? { conversation_id: conv.id, token: conv.token } : {}),
		};
		const r = await post('/api/webchat/messages', body, auth);
		if (r.status !== 200) throw new Error(`turn ${i} (${cust.customer_id}) -> ${r.status}: ${r.text.slice(0, 300)}`);
		const j = JSON.parse(r.text);
		if (!j.conversation_id || !j.token || !String(j.text ?? '').trim()) {
			throw new Error(`turn ${i} (${cust.customer_id}): incomplete answer ${r.text.slice(0, 300)}`);
		}
		conv = { id: j.conversation_id, token: j.token };
		if (i >= WARMUP) latencies.push(r.ms);
	}
}

const pct = (xs, q) => {
	const a = [...xs].sort((x, y) => x - y);
	return a[Math.min(a.length - 1, Math.max(0, Math.ceil(q * a.length) - 1))];
};

const started = performance.now();
const latencies = [];
await Promise.all(Array.from({ length: CONCURRENCY }, (_, w) =>
	worker(state.customers[w % state.customers.length], TURNS, latencies, w)));
const wall = performance.now() - started;

const result = {
	at: new Date().toISOString(),
	edge: EDGE,
	concurrency: CONCURRENCY,
	turns_per_worker: TURNS,
	measured_turns: latencies.length,
	wall_ms: Math.round(wall),
	turns_per_sec: Number((latencies.length / (wall / 1000)).toFixed(2)),
	p50_ms: Math.round(pct(latencies, 0.5)),
	p95_ms: Math.round(pct(latencies, 0.95)),
	p99_ms: Math.round(pct(latencies, 0.99)),
	min_ms: Math.round(Math.min(...latencies)),
	max_ms: Math.round(Math.max(...latencies)),
	mean_ms: Math.round(latencies.reduce((a, b) => a + b, 0) / latencies.length),
};

console.log(JSON.stringify(result, null, 2));
if (process.env.PERF_OUT) writeFileSync(process.env.PERF_OUT, JSON.stringify(result, null, 2) + '\n');
