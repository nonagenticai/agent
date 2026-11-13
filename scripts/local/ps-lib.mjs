// ps-lib.mjs - shared helpers for the platform-stack headless journeys (platform-stack-smoke.mjs, platform-stack-b4.mjs).
// Env/config (lazy `env` getters: importing never throws), the retrying fetch, note/gap/step/assert bookkeeping,
// identities (`who`, `J` are SHARED objects: mutate, never reassign), product-API and edge clients, the demo_retail
// connector template, demo-backends helpers, layer authoring and the customer web-chat driver.
// finish() prints the summary and returns the exit code; the caller owns process.exit.
import { createHmac, randomBytes, randomUUID } from 'node:crypto';

const req = (k) => {
	const v = process.env[k];
	if (!v) throw new Error(`${k} is required (run via platform-stack.sh)`);
	return v.replace(/\/$/, '');
};
const lazy = {};
for (const [name, key] of Object.entries({ API: 'PS_API', EDGE: 'PS_EDGE', IDP: 'PS_IDP', KCADMIN: 'PS_KCADMIN', DEMO_URL: 'PS_DEMO_URL', DEMO_TOKEN: 'PS_DEMO_TOKEN', DEMO_DIRECT: 'PS_DEMO_DIRECT', SELF: 'PS_SELF', LLM_URL: 'PS_LLM_URL', LLM_MODEL: 'PS_LLM_MODEL' }))
	Object.defineProperty(lazy, name, { enumerable: true, get: () => req(key) });
export const START_MS = Date.now();
export const RUN = Date.now().toString(36);
export const env = Object.defineProperties(lazy, {
	UNTIL: { enumerable: true, get: () => Number(process.env.PS_UNTIL ?? 6) },
	STATE: { enumerable: true, get: () => process.env.PS_STATE ?? '' },
	ANSWER_MS: { enumerable: true, get: () => Number(process.env.PS_ANSWER_TIMEOUT_MS ?? 120000) },
});

export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
// every call retries a connection-level failure (the harness restarts the API/daemons once after signup); the URL is named if it never returns
const rawFetch = globalThis.fetch;
globalThis.fetch = async (url, init) => {
	for (let n = 0; ; n++) {
		try {
			return await rawFetch(url, init);
		} catch (e) {
			if (n >= 60) throw new Error(`fetch ${init?.method ?? 'GET'} ${url} failed after 120 s: ${e.cause?.code ?? e.message}`);
			if (process.env.PS_TRACE) console.log(`      retry ${n + 1}: ${init?.method ?? 'GET'} ${url} ${e.cause?.code ?? e.message}`);
			await sleep(2000);
		}
	}
};

export const results = [];
export const gaps = [];
export const note = (m) => console.log(`      ${m}`);
export const gap = (code, why) => {
	if (!gaps.includes(code)) gaps.push(code);
	return new Error(`PRODUCT GAP: ${code} - ${why}`);
};
export async function step(n, title, fn) {
	if (n > env.UNTIL) return;
	try {
		await fn();
		console.log(`PASS  step ${n}: ${title}`);
		results.push([n, true]);
	} catch (e) {
		console.log(`FAIL  step ${n}: ${title}\n      ${String(e.message ?? e).split('\n').join('\n      ')}${e.cause ? ` (cause ${e.cause.code ?? e.cause.message})` : ''}${process.env.PS_TRACE ? `\n${e.stack}` : ''}`);
		results.push([n, false]);
	}
}
export const assert = (c, m) => {
	if (!c) throw new Error(m);
};

// ---- identities: sa is a persona of the fake IdP; ta/ag are the identities the product made (Keycloak-admin ids) ----
export const who = { sa: { persona: 'sa' } };
export const tokens = {};
export async function mint(w) {
	if (tokens[w]) return tokens[w];
	const id = who[w];
	assert(id, `identity ${w} is not known yet`);
	const q = new URLSearchParams({ persona: id.persona });
	if (id.sub) q.set('sub', id.sub);
	if (id.email) q.set('email', id.email);
	const r = await fetch(`${env.IDP}/__mint?${q}`);
	assert(r.ok, `fake IdP mint ${w} -> ${r.status}`);
	return (tokens[w] = (await r.json()).access_token);
}
export async function api(w, method, path, body, headers = {}) {
	const h = { authorization: `Bearer ${await mint(w)}`, 'content-type': 'application/json', ...headers };
	// the API may be restarting (harness-tenant): retry connection errors for up to 120 s, naming the URL if it never returns
	let r;
	for (let n = 0; ; n++) {
		try {
			r = await fetch(`${env.API}${path}`, { method, headers: h, body: body === undefined ? undefined : JSON.stringify(body) });
			break;
		} catch (e) {
			if (n >= 60) throw new Error(`fetch ${method} ${env.API}${path} failed after 120 s: ${e.cause?.code ?? e.message}`);
			await sleep(2000);
		}
	}
	const text = await r.text();
	let json = null;
	try {
		json = text ? JSON.parse(text) : null;
	} catch {
		/* not json */
	}
	return { status: r.status, json, text };
}
export const ok = (r, what, accept = [200, 201, 202, 204]) => {
	assert(accept.includes(r.status), `${what} -> ${r.status} ${r.text.slice(0, 400)}`);
	return r.json;
};
export const itemsOf = (j) => (Array.isArray(j) ? j : (j?.items ?? j?.data ?? j?.rows ?? []));
export async function kcUserByEmail(email) {
	const st = await (await fetch(`${env.KCADMIN}/__state`)).json();
	return (st.users ?? []).find((u) => String(u.email).toLowerCase() === email.toLowerCase()) ?? null;
}

// ---- edge (the customer door; same-origin Origin like the demo page) ------------------------------------------------
export async function edge(method, path, body, extra = {}) {
	const edgeOrigin = new URL(env.EDGE).origin.replace('127.0.0.1', 'localhost');
	const r = await fetch(`${env.EDGE}${path}`, { method, headers: { 'content-type': 'application/json', origin: edgeOrigin, ...extra }, body: body === undefined ? undefined : JSON.stringify(body) });
	const text = await r.text();
	let json = null;
	try {
		json = text ? JSON.parse(text) : null;
	} catch {
		/* not json */
	}
	return { status: r.status, json, text };
}

export const J = { tenant: null, job: null, template: null, widgetKey: null, binding: null, idKid: null, idSecret: null };
export const opKey = (o) => `${o.object}.${o.verb}${o.variant && o.variant !== '*' ? `{${o.variant}}` : ''}`;

// ---- the tenant's external system: demo-backends (a customer REST endpoint) and the platform connector template for it ----
// fd7543d6 ships no connector template and no demo-backends crate. The platform admin authors `demo_retail` through the
// product routes (POST /api/platform/connector-templates with the ops in the body, then POST .../publish: T1 replays the
// fixtures). Only the ops demo-backends serves are authored; the job's other operations stay unrouted and readiness names them.
export const TEMPLATE = { key: 'demo_retail', version: '1' };
const ERRFIX = [
	{ name: 'not found', outcome: 'not_found', status: 404, body: { error: { code: 'not_found' } } },
	{ name: 'expired credential', outcome: '401 -> connector_auth_failed', status: 401, body: { error: 'Unauthorized' } },
	{ name: 'upstream 5xx', outcome: 'upstream_unavailable', status: 503, body: { error: 'Unavailable' } },
	{ name: 'upstream timeout', outcome: 'timeout', status: 0, body: null },
];
export function tplOp(object, verb, variant, side, method, path, request, maps, okBody) {
	return {
		object, verb, variant, side_effect: side, op_kind: 'standard', method, path, request,
		field_map: maps.map(([p, field]) => ({ path: p, field, transform: 'none' })), status_map: [],
		fault_map: [
			{ code: 'timeout', system: 'fault=timeout' },
			{ code: 'upstream_unavailable', system: 'fault=5xx' },
			{ code: 'not_found', system: 'HTTP 404' },
			{ code: 'connector_auth_failed', system: 'HTTP 401' },
		],
		pagination: null, idempotency: side === 'R' ? { kind: 'nat' } : { kind: 'dup_search', on: 'case ref' }, verify_read: null, timeout_ms: 5000,
		fixtures: [{ name: `${object}.${verb} ok`, outcome: 'ok', status: 200, body: okBody }, ...ERRFIX],
	};
}
export function templateOps() {
	// paths and shapes are demo-backends' own (crates/demo-backends/src/api: customers.rs, orders.rs, shipments.rs)
	const ord = { order_id: 'RET-01', customer_id: 'C-1', placed_at: '2026-10-01T00:00:00Z', item: 'Rug', status: 'shipped', total: { amount: '49.00', currency: 'EUR' }, delivery_type: 'home', payment_method_ref: 'card', lines: [{ sku: 'RUG', qty: 1 }] };
	const ship = { id: 'SHP-1', order_id: 'RET-01', status: 'in_transit', eta: '2026-10-05T00:00:00Z', carrier: 'DHL', tracking: 'SHP-1' };
	const caseBody = { case_id: 'CS-50101', case_type: 'handover', status: 'open', due_at: '2026-10-28T00:00:00Z', queue: 'Customer care', order_id: null, subject: 'x' };
	const caseMaps = [['$.case_id', 'case_id'], ['$.status', 'status'], ['$.due_at', 'due_at']];
	const caseReq = (type, extra) => `{"type":"${type}","subject":"{{canonical.subject}}"${extra},"ref":"{{step.idempotency_key}}"}`;
	const problem = '{"order_id":"{{canonical.order_id}}","problem":"{{canonical.problem}}"}';
	return [
		tplOp('party', 'read', '*', 'R', 'GET', '/customers/{{canonical.party_id}}', '', [['$.name', 'name'], ['$.email', 'email'], ['$.phone', 'phone'], ['$.postal_address', 'postal_address']], { name: 'Ann', email: 'ann@example.test', phone: '+31000', postal_address: { line1: 'x', city: 'y' } }),
		tplOp('party', 'list', '*', 'R', 'GET', '/customers?email={{canonical.email}}', '', [['$.customers[*].party_id', 'party_id'], ['$.customers[*].roles', 'roles']], { customers: [{ party_id: 'C-1', roles: ['customer'] }] }),
		tplOp('order', 'list', '*', 'R', 'GET', '/orders?customer_id={{canonical.party_id}}', '', [['$.orders[*].order_id', 'order_id'], ['$.orders[*].status', 'status'], ['$.orders[*].placed_at', 'placed_at'], ['$.orders[*].total', 'total'], ['$.orders[*].item', 'items']], { orders: [ord] }),
		tplOp('order', 'read', '*', 'R', 'GET', '/orders/{{canonical.order_id}}', '', [['$.status', 'status'], ['$.lines', 'lines'], ['$.total', 'total'], ['$.delivery_type', 'delivery_type'], ['$.payment_method_ref', 'payment_method_ref'], ['$.customer_id', 'customer_id']], ord),
		tplOp('delivery', 'read', '*', 'R', 'GET', '/shipments?order_id={{canonical.order_id}}', '', [['$.items[*].status', 'status'], ['$.items[*].carrier', 'carrier'], ['$.items[*].id', 'tracking_ref']   /* demo-backends serves GET /shipments/{shipment_id} only; its `tracking` field (3SDEMO..) is not resolvable, so the reference handed to delivery.read{by_tracking_ref} is the shipment id */, ['$.items[*].eta', 'expected_window']], { items: [ship] }),
		// the built-in track_parcel plan requires delivery.read{by_tracking_ref} routed (orchestrator.rs TRACK_PARCEL_READ); demo-backends GET /shipments/{id} (the shipment id is the tracking ref)
		tplOp('delivery', 'read', 'by_tracking_ref', 'R', 'GET', '/shipments/{{canonical.tracking_ref}}', '', [['$.status', 'status'], ['$.carrier', 'carrier'], ['$.eta', 'expected_window'], ['$.events', 'events']], { shipment_id: 'SHP-1', order_id: 'RET-01', status: 'in_transit', carrier: 'DHL', eta: '2026-10-05T00:00:00Z', events: [{ seq: 1, at: '2026-10-02 10:00', location: 'Hub', description: 'Departed' }] }),
		tplOp('delivery', 'report_problem', '*', 'W', 'POST', '/shipments/problems', problem.replace('canonical.order_id', 'canonical.delivery_id'), [['$.id', 'case_id'], ['$.status', 'status']], { id: 'SP-3001', status: 'reported' }),
		tplOp('delivery', 'report_problem', 'by_order', 'W', 'POST', '/shipments/problems', problem, [['$.id', 'case_id'], ['$.status', 'status']], { id: 'SP-3001', status: 'reported' }),
		// cases and returns: demo-backends api/cases.rs, returns.rs (source of bin-1650, commit 6724ef09)
		tplOp('case', 'read', '*', 'R', 'GET', '/cases/{{canonical.case_id}}', '', [['$.status', 'status'], ['$.case_type', 'case_type'], ['$.due_at', 'due_at'], ['$.queue', 'queue'], ['$.subject', 'title']], caseBody),
		tplOp('case', 'create', '*', 'W', 'POST', '/cases', caseReq('{{canonical.case_type}}', ',"order_id":"{{canonical.subject_object}}"'), caseMaps, caseBody),
		tplOp('case', 'create', 'handover', 'W', 'POST', '/cases', caseReq('handover', ''), caseMaps, caseBody),
		tplOp('case', 'create', 'complaint', 'W', 'POST', '/cases', caseReq('complaint', ',"order_id":"{{canonical.subject_object}}"'), caseMaps, caseBody),
		tplOp('case', 'create', 'return', 'W', 'POST', '/rmas', '{"order_id":"{{canonical.subject_object}}","reason":"return requested","ref":"{{step.idempotency_key}}"}', [['$.case_id', 'case_id'], ['$.status', 'status'], ['$.label', 'label'], ['$.deadline', 'deadline']], { rma_id: 'RMA-1', case_id: 'RMA-1', status: 'open', label: 'emailed', deadline: '2026-11-10' }),
		tplOp('return', 'list', '*', 'R', 'GET', '/returns?customer_id={{canonical.party_id}}', '', [['$.items[*].return_id', 'return_id'], ['$.items[*].order_id', 'order_id'], ['$.items[*].status', 'status'], ['$.items[*].created_at', 'created_at']], { items: [{ return_id: 'RMA-1', order_id: 'RET-01', status: 'open', created_at: '2026-10-02T00:00:00Z' }] }),
	];
}
export async function authorConnectorTemplate() {
	const body = {
		key: TEMPLATE.key, version: TEMPLATE.version, label: 'Demo Retail (REST)', vendor: 'Nonagentic', system_class: 'commerce', wave: 'v1',
		transport: 'rest', auth_kind: 'bearer_static', vendor_api_pin: 'demo-backends',
		auth_params: { static_headers: '{"X-Demo-Tenant":"{{tenant.id}}","X-Demo-Env":"{{system.env}}"}' },
		credential_schema: [{ name: 'service_token', label: 'Service token', secret: true }],
		ops: templateOps(),
	};
	const c = await api('sa', 'POST', '/api/platform/connector-templates', body);
	if (c.status === 409 && /template_exists/.test(c.text)) return note(`connector template ${TEMPLATE.key}@${TEMPLATE.version} exists already`);
	if (c.status !== 201) { gaps.push('connector_template_create'); throw new Error(`PRODUCT GAP: connector_template_create - POST /api/platform/connector-templates -> ${c.status} ${c.text.slice(0, 600)}`); }
	const pub = await api('sa', 'POST', `/api/platform/connector-templates/${TEMPLATE.key}/${TEMPLATE.version}/publish`, {});
	if (pub.status !== 200) { gaps.push('connector_template_publish'); throw new Error(`PRODUCT GAP: connector_template_publish - publish ${TEMPLATE.key}@${TEMPLATE.version} -> ${pub.status} ${pub.text.slice(0, 600)}`); }
}
// the tenant's external system has its OWN admin/seed routes (not platform routes): seed its partition for this tenant
export async function demoAdmin(method, path, body) {
	const r = await fetch(`${env.DEMO_DIRECT}${path}`, { method, headers: { authorization: `Bearer ${env.DEMO_TOKEN}`, 'x-demo-tenant': J.tenant, 'x-demo-env': 'prod', 'content-type': 'application/json' }, body: body === undefined ? undefined : JSON.stringify(body) });
	const text = await r.text();
	let json = null;
	try {
		json = text ? JSON.parse(text) : null;
	} catch {
		/* not json */
	}
	return { status: r.status, json, text };
}
// the customers of the external system (featured first); a known customer is {customer_id, email, name}
export const demoCustomers = async () => {
	const r = await demoAdmin('GET', '/customers?q=');
	assert(r.status === 200, `demo-backends GET /customers -> ${r.status} ${r.text.slice(0, 200)}`);
	return r.json?.customers ?? [];
};
// the tenant's logged-in site, simulated: an HS256 assertion signed with the binding's identity secret (conversations/webchat_identity.rs)
export function assertionFor(cust) {
	const b64 = (o) => Buffer.from(JSON.stringify(o)).toString('base64url');
	const now = Math.floor(Date.now() / 1000);
	const head = b64({ alg: 'HS256', typ: 'JWT', kid: J.idKid });
	const claims = b64({ iss: ISSUER, aud: J.binding, sub: String(cust.customer_id), email: cust.email, name: cust.name, iat: now, exp: now + 120, jti: randomUUID().replace(/-/g, '') });
	return `${head}.${claims}.${createHmac('sha256', J.idSecret).update(`${head}.${claims}`).digest('base64url')}`;
}
export const ISSUER = 'https://demo-shop.local';
// "seed-only" PRODUCT GAP.
export const slug = (layerKey) => layerKey.replace(/\//g, '.');
export async function ensureLayer(body, key) {
	const r = await api('sa', 'POST', '/api/layers', body);
	if (r.status === 409) return itemsOf((await api('sa', 'GET', '/api/layers')).json).find((l) => l.key === key);
	return ok(r, `POST /api/layers ${key}`, [201]);
}
export async function addRow(layer, kind, key, fields) {
	const r = await api('sa', 'POST', `/api/layers/${layer.id}/rows`, { kind, key, op: 'add', fields });
	if (r.status === 409 && /duplicate_add/.test(r.text)) return;   // a row cannot be re-authored in its own layer (a patch there is read as a patch of an EARLIER layer: compile_failed)
	ok(r, `POST /api/layers/{id}/rows ${kind} ${key}`, [200, 201]);
}
export const readiness = async () => ok(await api('ta', 'GET', `/api/jobs/${J.job}/release-readiness?of=draft`), 'GET release-readiness')?.blocking ?? [];
export const codes = (items) => items.map((i) => `${i.code}(${i.subject})`).join(', ') || 'none';
// The tenant's model is no ensureModel() any more: an unbound qualify goes to the inference-router under the tenant's own
// bundle token (providers/llm.rs tenant_router), minted at signup. 503 llm_inference_not_provisioned = the mint did not land.
export const llmGap = (r) => /llm_inference_not_provisioned/.test(r.text ?? '') && gap('llm_inference_not_provisioned', `the tenant has no inference-router licence: BUNDLE_MINTER_* / the fake minter / signup step app_licences did not provision it: ${String(r.text).slice(0, 200)}`);

// a known customer opens the web chat as the tenant's site would: signed assertion -> session -> first message
export async function customerChat(cust, what) {
	const page = await edge('GET', `/webchat/demo?key=${encodeURIComponent(J.widgetKey)}`);
	assert(page.status === 200, `demo shop page -> ${page.status}`);
	const s0 = await edge('POST', '/api/webchat/sessions', { widget_key: J.widgetKey, customer_assertion: assertionFor(cust) });
	ok(s0, `${what}: POST /api/webchat/sessions`, [201]);
	assert(s0.json?.identified === true, `${what}: the session is not identified`);
	const bearer = { authorization: `Bearer ${s0.json.session_token}` };
	const send = (text, extra = {}) => edge('POST', '/api/webchat/messages', { widget_key: J.widgetKey, visitor_id: randomBytes(12).toString('hex'), messages: [{ role: 'user', text }], ...extra }, bearer);
	const first = await Promise.race([send('Where is my parcel?'), sleep(env.ANSWER_MS).then(() => ({ status: 0, text: `no answer within ${env.ANSWER_MS} ms` }))]);
	const f = ok(first, `${what}: POST /api/webchat/messages`, [200]);
	assert(f?.conversation_id && f?.token, `${what}: no conversation in ${first.text.slice(0, 300)}`);
	assert(String(f.text ?? '').trim(), `${what}: empty first answer`);
	assert(!/\b(RET|ORD|SHP|PAY|CS)-\d+\b/.test(f.text), `${what}: the reply shows an internal id: ${f.text.slice(0, 160)}`);
	assert(!/order (number|no\b|id)|post ?code|zip code|e-?mail address|(customer|account) (id|number)|reference number/i.test(f.text), `${what}: the reply asks for a lookup key: ${f.text.slice(0, 160)}`);
	assert(/in transit|delivered|shipped|out for delivery|on its way|expected by|delayed|ready for pickup/i.test(f.text) && !/colleague/i.test(f.text), `${what}: "where is my parcel?" did not answer with the customer's real shipment status (a handover or a refusal): ${f.text.slice(0, 200)}`);
	note(`${what}: first reply: ${String(f.text).replace(/\s+/g, ' ').slice(0, 120)}`);
	return { id: f.conversation_id, token: f.token, bearer, send };
}

// prints the summary; returns the exit code (the caller does process.exit)
export function finish() {
	if (gaps.length) console.log(`\nPRODUCT GAP codes: ${gaps.join(', ')}`);
	const failed = results.filter(([, p]) => !p);
	console.log(failed.length ? `\nSMOKE FAILED: steps ${failed.map(([n]) => n).join(', ')}` : `\nSMOKE PASSED: ${results.length}/${results.length} steps`);
	return failed.length ? 1 : 0;
}
