#!/usr/bin/env node
// B8 local acceptance: a stand-in for strata_rag (STRATA_RAG_URL) and strata_ingest (STRATA_INGEST_URL).
//
// A6 runs a validation cut that answers through the built-in `knowledge.answer`, which queries strata_rag; with no
// Strata the step is 503. One server answers both base URLs (the routes do not overlap on the paths B8 uses).
// Mirrors crates/agent-api/tests/common/strata_fake.rs (routes, bodies, error codes), state kept in memory:
//   GET  /health                               -> 200 {"status":"ok"}
//   GET|POST /api/stores, GET|PATCH|DELETE /api/stores/{id}   per-tenant store tree (409 duplicate_name)
//   GET|PUT|DELETE /api/stores/{id}/providers, GET|PUT /api/tenant/providers, POST /api/tenant/providers/test
//   POST /api/query                            -> deterministic cited hits (keyword match over the corpus below)
//   GET  /api/platform/packs|exclusions        -> empty lists (platform knowledge has no packs here)
//   GET  /api/ingest/jobs                      -> empty list
//   /api/eval/*                                -> the tenant eval (ps-fake-eval.mjs), + POST /__eval/fail-next
// Anything else answers an empty 404, as strata does for an unknown path.
//
// Auth: `Authorization: Bearer <token>`; the token pins one tenant (B8_STRATA_TOKENS, comma-separated
// `<tenant-uuid>:<token>`, the same value the API gets in STRATA_RAG_TOKENS / STRATA_INGEST_TOKENS). Unknown or
// missing -> 401 `unauthorized`. `X-Tenant-Id` naming another tenant -> 403 `tenant_mismatch`.
//
// Corpus: customer-service knowledge for the retail demo. No text asks a customer for an order number, id,
// postcode, email or reference (owner KYC ruling 2026-10-26): the support flows work from the signed-in account.
//
// Env: B8_STRATA_PORT (required), B8_STRATA_TOKENS (required).
import { createServer } from 'node:http';
import { createHash, randomUUID } from 'node:crypto';
import { verifyBundleToken } from './ps-bundle.mjs';
import { handleEval, handleEvalHook } from './ps-fake-eval.mjs';
const BUNDLE_SECRET = process.env.PS_BUNDLE_SECRET ?? ''; // platform-stack: per-tenant bundle tokens (strata-rag / strata-ingest) pin the tenant they were minted for

const PORT = Number(process.env.B8_STRATA_PORT ?? 0);
if (!PORT) throw new Error('B8_STRATA_PORT is required');
const TOKENS = new Map(); // token -> tenant
for (const entry of (process.env.B8_STRATA_TOKENS ?? '').split(',').map((s) => s.trim()).filter(Boolean)) {
	const i = entry.indexOf(':');
	if (i > 0) TOKENS.set(entry.slice(i + 1), entry.slice(0, i).toLowerCase());
}
if (TOKENS.size === 0) throw new Error('B8_STRATA_TOKENS is required (<tenant-uuid>:<token>,...)');
// The first listed tenant is the platform owner (strata STRATA_PLATFORM_OWNER): it alone writes /api/platform/*.
const OWNER = [...TOKENS.values()][0];
// Tokens shaped `ps-strata-<tenant-uuid>` are accepted without a restart (platform-stack harness: a tenant created by
// signup gets its pair in the API env; this keeps the fake's in-memory stores alive across that restart).
const tenantOfToken = (t) => TOKENS.get(t) ?? (BUNDLE_SECRET ? (verifyBundleToken(BUNDLE_SECRET, t, 'strata-rag') ?? verifyBundleToken(BUNDLE_SECRET, t, 'strata-ingest')) : null) ?? /^ps-strata-([0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12})$/.exec(t)?.[1];

// state: documents (store id -> [doc]), platform packs (key@version -> pack), exclusions (tenant -> [prefix])
const docs = new Map();
const packs = new Map();
const exclusions = new Map();
const STOP = new Set('the a an of to and or is are in on for it my me you your i we be can do how what when where with at by this that from as'.split(' '));
const words = (t) => String(t).toLowerCase().split(/[^a-z0-9]+/).filter((w) => w.length > 2 && !STOP.has(w));

const uuidOf = (seed) => {
	const h = createHash('sha256').update(seed).digest('hex');
	return `${h.slice(0, 8)}-${h.slice(8, 12)}-5${h.slice(13, 16)}-8${h.slice(17, 20)}-${h.slice(20, 32)}`;
};

const CORPUS = [
	{
		title: 'Returns and refunds',
		source: 'returns-policy.md',
		keys: ['return', 'refund', 'money back', 'send back', 'exchange'],
		text: 'You can return most items within 30 days of delivery, unused and in the original packaging. Start the return from your account under Orders; we email a prepaid label. Refunds go back to the original payment method within 5 working days of the parcel arriving at the warehouse.',
	},
	{
		title: 'Delivery times and tracking',
		source: 'delivery.md',
		keys: ['deliver', 'shipping', 'ship', 'track', 'parcel', 'arrive', 'late', 'order', 'status', 'where'],
		text: 'Standard delivery takes 2 to 4 working days, express delivery is next working day for orders placed before 14:00. Tracking for each of your orders is shown in your account under Orders and updates as the carrier scans the parcel.',
	},
	{
		title: 'Damaged or faulty items',
		source: 'faulty-items.md',
		keys: ['damage', 'broken', 'faulty', 'defect', 'wrong', 'missing', 'not working'],
		text: 'If an item arrives damaged, faulty or not as described, tell us what is wrong and add a photo in the chat. We offer a replacement or a full refund, and you do not need to pay for the return.',
	},
	{
		title: 'Cancelling or changing an order',
		source: 'cancellations.md',
		keys: ['cancel', 'change', 'address', 'modify', 'amend'],
		text: 'An order can be cancelled or its delivery address changed until it is handed to the carrier. After that, refuse the parcel or return it under the 30-day returns policy.',
	},
	{
		title: 'Payments and invoices',
		source: 'payments.md',
		keys: ['pay', 'invoice', 'receipt', 'card', 'charge', 'billing', 'vat'],
		text: 'We accept cards and bank transfer. Invoices and receipts are available in your account under Orders. A pending authorisation on your card is released automatically if the order is cancelled.',
	},
	{
		title: 'Contacting a person',
		source: 'handover.md',
		keys: ['human', 'agent', 'person', 'complain', 'speak', 'manager', 'call'],
		text: 'You can ask for a colleague at any time. Support is staffed Monday to Friday, 08:00 to 18:00; outside those hours we pick the conversation up the next working day.',
	},
];
const FALLBACK = CORPUS[1];

// tenant -> Map(storeId -> view)
const stores = new Map();
const treeOf = (tenant) => {
	if (!stores.has(tenant)) stores.set(tenant, new Map());
	return stores.get(tenant);
};
const pathOf = (tree, s) => {
	const out = [];
	for (let c = s; c; c = c.parent_id ? tree.get(c.parent_id) : null) out.unshift(c.name);
	return out;
};
const providersView = () => ({
	embed: { provider: 'fake', model: 'BAAI/bge-m3', dims: 1024 },
	rerank: { provider: 'fake', model: 'fake-rerank' },
	chat: { provider: 'fake', model: 'fake-chat' },
});

function hitsFor(query, tenantStores, k) {
	const q = String(query).toLowerCase();
	const scored = CORPUS.map((d) => ({ d, n: d.keys.filter((w) => q.includes(w)).length }));
	let picked = scored.filter((s) => s.n > 0).sort((a, b) => b.n - a.n).slice(0, k).map((s) => s.d);
	if (picked.length === 0) picked = [FALLBACK];
	return picked.map((d, i) => {
		const leaf = tenantStores[i % Math.max(tenantStores.length, 1)];
		return {
			chunk_id: uuidOf(`chunk:${d.source}`),
			document_id: uuidOf(`doc:${d.source}`),
			store_path: leaf ? leaf.path : ['b8-fake'],
			layer: 'tenant',
			pack: null,
			ord: 0,
			text: d.text,
			page_start: null,
			page_end: null,
			source: d.source,
			title: d.title,
			scores: { dense: +(0.9 - i * 0.05).toFixed(3), lexical: +(0.8 - i * 0.05).toFixed(3), rerank: +(0.95 - i * 0.05).toFixed(3) },
			source_id: null,
			source_run: null,
			metadata: {},
		};
	});
}

function send(res, status, body) {
	if (body === undefined) {
		res.writeHead(status);
		return res.end();
	}
	const data = JSON.stringify(body);
	res.writeHead(status, { 'content-type': 'application/json', 'content-length': Buffer.byteLength(data) });
	res.end(data);
}
const fail = (res, status, code, message) => send(res, status, { error: { code, message } });

async function readJson(req) {
	const chunks = [];
	for await (const c of req) chunks.push(c);
	const raw = Buffer.concat(chunks).toString('utf8');
	try {
		return raw ? JSON.parse(raw) : {};
	} catch {
		return {};
	}
}

const server = createServer(async (req, res) => {
	try {
		const url = new URL(req.url, 'http://x');
		const seg = url.pathname.split('/').filter(Boolean);
		if (url.pathname === '/health') return send(res, 200, { status: 'ok' });
		if (url.pathname.startsWith('/__eval/') && handleEvalHook(req, res, url, await readJson(req), send)) return;

		const bearer = /^Bearer (.+)$/.exec(req.headers.authorization ?? '')?.[1];
		const tenant = bearer ? tenantOfToken(bearer) : undefined;
		if (!tenant) return fail(res, 401, 'unauthorized', 'unauthorized');
		const hdr = req.headers['x-tenant-id'];
		if (hdr && String(hdr).toLowerCase() !== tenant) return fail(res, 403, 'tenant_mismatch', 'tenant mismatch');

		const m = req.method;
		const b = ['POST', 'PUT', 'PATCH'].includes(m) ? await readJson(req) : {};
		const tree = treeOf(tenant);
		const view = (s) => ({
			id: s.id,
			parent_id: s.parent_id,
			kind: s.kind,
			name: s.name,
			description: s.description,
			token_count: s.kind === 'leaf' ? 120 : 0,
			chunk_count: s.kind === 'leaf' ? (docs.get(s.id) ?? []).length : 0,
			children: [...tree.values()].filter((c) => c.parent_id === s.id).map((c) => c.id),
		});

		if (seg[0] !== 'api') return send(res, 404);

		// ---- tenant eval (strata src/api/eval_api.rs): ps-fake-eval.mjs, scored through this fake's /api/query ranking ----
		const evalCtx = {
			tenant,
			actor: req.headers['x-actor-id'] ?? null,
			tree,
			pathOf,
			hitsFor,
			corpus: CORPUS.map((d) => ({ ...d, document_id: uuidOf(`doc:${d.source}`), chunk_id: uuidOf(`chunk:${d.source}`) })),
			readable: (id) => tree.has(id),
			send,
			fail,
		};
		if (await handleEval(req, res, url, b, evalCtx)) return;

		// ---- platform knowledge (strata src/api/platform.rs): owner-only writes, tenant reads of published packs ----
		const ownerTree = treeOf(OWNER);
		const packOf = (key, version) => packs.get(`${key}@${version}`);
		const excluded = (key) => (exclusions.get(tenant) ?? []).some((p) => key === p || key.startsWith(`${p}/`));
		const readable = (pk) => tenant === OWNER || (pk.published_at && !excluded(pk.key));
		const packView = (pk) => ({ key: pk.key, version: pk.version, root_id: pk.root_id, published_at: pk.published_at, retired_at: pk.retired_at });
		const subtree = (root) => {
			const out = [];
			const walk = (sid) => {
				const st = ownerTree.get(sid);
				if (!st) return;
				out.push(st);
				for (const k of [...ownerTree.values()].filter((x) => x.parent_id === sid)) walk(k.id);
			};
			walk(root);
			return out;
		};
		const packOfStore = (storeId) => {
			for (const pk of packs.values()) if (subtree(pk.root_id).some((x) => x.id === storeId)) return pk;
			return null;
		};
		if (seg[1] === 'platform' && seg[2] === 'packs') {
			if (m === 'GET' && seg.length === 3) return send(res, 200, { packs: [...packs.values()].filter((pk) => readable(pk) && (tenant === OWNER || !pk.retired_at)).map(packView) });
			if (m === 'POST' && seg.length === 3) {
				if (tenant !== OWNER) return fail(res, 403, 'forbidden', 'forbidden');
				const root = ownerTree.get(b.root_id);
				if (!root) return fail(res, 404, 'not_found', `not found: store ${b.root_id}`);
				if (root.parent_id) return fail(res, 422, 'invalid_target', 'a pack root must be a top-level store');
				if (!b.key || !Number.isInteger(b.version)) return fail(res, 422, 'invalid_input', 'invalid input: key, version');
				if (packOf(b.key, b.version) || [...packs.values()].some((x) => x.root_id === b.root_id)) return fail(res, 409, 'duplicate_name', 'duplicate name');
				const pk = { key: b.key, version: b.version, root_id: b.root_id, published_at: null, retired_at: null };
				packs.set(`${b.key}@${b.version}`, pk);
				return send(res, 201, packView(pk));
			}
			const pk = packOf(decodeURIComponent(seg[3] ?? ''), Number(seg[4]));
			const tail = seg[5];
			if (pk && m === 'POST' && (tail === 'publish' || tail === 'retire')) {
				if (tenant !== OWNER) return fail(res, 403, 'forbidden', 'forbidden');
				if (tail === 'publish') pk.published_at ??= new Date().toISOString();
				else {
					if (!pk.published_at) return fail(res, 422, 'invalid_target', 'a draft cannot be retired');
					pk.retired_at ??= new Date().toISOString();
				}
				return send(res, 200, packView(pk));
			}
			if (m === 'POST' && (tail === 'publish' || tail === 'retire') && !pk) return fail(res, 404, 'not_found', 'not found: pack');
			if (m === 'GET' && tail === 'stores') {
				if (!pk || !readable(pk)) return fail(res, 404, 'not_found', 'not found: pack');
				const sts = subtree(pk.root_id);
				return send(res, 200, {
					pack: `${pk.key}@${pk.version}`,
					root_id: pk.root_id,
					stores: sts.map((st) => ({ id: st.id, parent_id: st.parent_id, kind: st.kind, name: st.name, path: pathOf(ownerTree, st), embed_model: 'BAAI/bge-m3@1024', chunk_count: (docs.get(st.id) ?? []).length })),
				});
			}
		}
		if (seg[1] === 'platform' && seg[2] === 'exclusions') {
			if (m === 'GET') return send(res, 200, { exclusions: (exclusions.get(tenant) ?? []).map((key_prefix) => ({ key_prefix })) });
			if (m === 'PUT') {
				const t = b.tenant_id ?? tenant;
				exclusions.set(t, [...new Set([...(exclusions.get(t) ?? []), b.key_prefix])]);
				return send(res, 200, { key_prefix: b.key_prefix });
			}
			if (m === 'DELETE') {
				const t = url.searchParams.get('tenant_id') ?? tenant;
				exclusions.set(t, (exclusions.get(t) ?? []).filter((x) => x !== url.searchParams.get('key_prefix')));
				return send(res, 204);
			}
		}
		// ---- documents (JSON ingest, listing, delete) ----
		const docStore = (sid) => tree.get(sid) ?? (tenant !== OWNER ? null : ownerTree.get(sid));
		if (seg[1] === 'stores' && seg[3] === 'documents' && seg.length === 4) {
			const st = docStore(seg[2]);
			if (!st) return fail(res, 404, 'not_found', `not found: store ${seg[2]}`);
			if (m === 'GET') {
				const items = (docs.get(st.id) ?? []).slice().reverse().map(({ text, ...d }) => d);
				return send(res, 200, { items, next: null });
			}
			if (m === 'POST') {
				if (st.kind !== 'leaf') return fail(res, 422, 'invalid_target', 'documents go into a leaf store');
				const pk = packOfStore(st.id);
				if (pk?.published_at) return fail(res, 409, 'pack_published', 'pack is published; its subtree is frozen');
				if (!String(b.text ?? '').trim()) return fail(res, 422, 'invalid_input', 'invalid input: text must not be empty');
				if (pk) {
					if (!b.provenance) return fail(res, 422, 'invalid_input', 'provenance is required in a platform store');
					if (!['CC-BY-4.0', 'CC0-1.0', 'public-domain', 'MIT', 'Apache-2.0', 'CC-BY-SA-4.0'].includes(b.provenance.license)) return fail(res, 422, 'invalid_input', `licence ${b.provenance.license} is not allowed`);
				}
				const hash = createHash('sha256').update(b.text).digest('hex');
				const list = docs.get(st.id) ?? [];
				const have = list.find((d) => d.content_hash === hash);
				if (have) return send(res, 200, { status: 'existing', document: { ...have, text: undefined } });
				const d = { id: randomUUID(), store_id: st.id, source: b.source ?? 'api', title: b.title ?? null, content_hash: hash, token_count: words(b.text).length, chunk_count: 1, source_id: b.source_id ?? null, source_run: b.source_run ?? null, metadata: b.metadata ?? {}, created_at: new Date().toISOString(), text: b.text };
				list.push(d);
				docs.set(st.id, list);
				const { text, ...row } = d;
				return send(res, 201, { status: 'created', document: row });
			}
		}
		if (seg[1] === 'documents' && seg[2] && m === 'DELETE') {
			for (const [sid, list] of docs) if (list.some((d) => d.id === seg[2]) && docStore(sid)) {
				docs.set(sid, list.filter((d) => d.id !== seg[2]));
				return send(res, 204);
			}
			return fail(res, 404, 'not_found', 'not found: document');
		}
		const [, a, id, c] = seg;

		if (a === 'stores' && !id) {
			if (m === 'GET') return send(res, 200, { stores: [...tree.values()].map(view) });
			if (m === 'POST') {
				if (!b.name || !['router', 'leaf'].includes(b.kind)) return fail(res, 422, 'invalid_input', 'invalid input: name, kind');
				if (b.parent_id && !tree.has(b.parent_id)) return fail(res, 404, 'not_found', `not found: store ${b.parent_id}`);
				const dup = [...tree.values()].some((s) => s.parent_id === (b.parent_id ?? null) && s.name === b.name);
				if (dup) return fail(res, 409, 'duplicate_name', 'duplicate name');
				const s = { id: randomUUID(), parent_id: b.parent_id ?? null, kind: b.kind, name: b.name, description: b.description ?? '' };
				tree.set(s.id, s);
				return send(res, 201, view(s));
			}
		}
		if (a === 'stores' && id && !c) {
			const s = tree.get(id);
			if (!s) return fail(res, 404, 'not_found', `not found: store ${id}`);
			if (m === 'GET') return send(res, 200, view(s));
			if (m === 'PATCH') {
				if (typeof b.name === 'string') s.name = b.name;
				if (typeof b.description === 'string') s.description = b.description;
				return send(res, 200, view(s));
			}
			if (m === 'DELETE') {
				const kids = [...tree.values()].filter((x) => x.parent_id === id);
				if (kids.length && url.searchParams.get('recursive') !== 'true') return fail(res, 409, 'has_children', 'has children');
				const drop = (x) => {
					for (const k of [...tree.values()].filter((y) => y.parent_id === x)) drop(k.id);
					tree.delete(x);
				};
				drop(id);
				return send(res, 204);
			}
		}
		if (a === 'stores' && id && c === 'providers') {
			if (!tree.has(id)) return fail(res, 404, 'not_found', `not found: store ${id}`);
			return send(res, 200, { store_id: id, embed_model: 'BAAI/bge-m3@1024', ...providersView() });
		}
		if (a === 'tenant' && id === 'providers' && !c && ['GET', 'PUT'].includes(m)) return send(res, 200, providersView());
		if (a === 'tenant' && id === 'providers' && c === 'test' && m === 'POST') {
			return send(res, 200, { ok: true, capability: b.capability ?? 'embed', model: 'fake', latency_ms: 3, detail: 'fake strata' });
		}
		if (a === 'query' && m === 'POST') {
			if (!String(b.query ?? '').trim()) return fail(res, 422, 'invalid_input', 'invalid input: query must not be empty');
			const named = b.leaf_ids ?? b.root_ids ?? (b.root_id ? [b.root_id] : null);
			const ownerTree2 = treeOf(OWNER);
			const find = (sid) => (tree.has(sid) ? [tree.get(sid), tree, false] : (() => { const o = ownerTree2.get(sid); const pk = o && [...packs.values()].find((p) => p.published_at && p.root_id && (function w(x) { return x === p.root_id || (ownerTree2.get(x)?.parent_id && w(ownerTree2.get(x).parent_id)); })(sid)); return pk ? [o, ownerTree2, pk] : null; })());
			// a named root means every leaf below it
			const expand = (sid) => { const f = find(sid); if (!f) return []; const [st, t] = f; return st.kind === 'leaf' ? [sid] : [...t.values()].filter((x) => x.parent_id === sid).flatMap((x) => expand(x.id)); };
			const leaves = named ? named.flatMap(expand) : [...tree.values()].filter((s) => s.kind === 'leaf').map((s) => s.id);
			const qw = new Set(words(b.query));
			const pool = [];
			for (const l of leaves) {
				const f = find(l);
				if (!f) continue;
				const [st, t, pk] = f;
				for (const d of docs.get(l) ?? []) {
					const dw = words(`${d.title ?? ''} ${d.text}`);
					const n = dw.filter((w) => qw.has(w)).length;
					pool.push({ d, st, t, pk, n });
				}
			}
			const k = Math.min(Number(b.top_k ?? b.limit ?? 3) || 3, 8);
			let results;
			if (pool.length) {
				// uploaded text is what answers; best lexical overlap first (all documents when nothing overlaps)
				results = pool.sort((x, y) => y.n - x.n).slice(0, k).map(({ d, st, t, pk, n }, i) => ({
					chunk_id: uuidOf(`chunk:${d.id}`), document_id: d.id,
					store_path: pk ? [pk.key, ...pathOf(t, st)] : pathOf(t, st), layer: pk ? 'platform' : 'tenant', pack: pk ? `${pk.key}@${pk.version}` : null,
					ord: 0, text: d.text, page_start: null, page_end: null, source: d.source, title: d.title,
					scores: { dense: +(0.9 - i * 0.05).toFixed(3), lexical: +Math.min(1, n / Math.max(qw.size, 1)).toFixed(3), rerank: +(0.95 - i * 0.05).toFixed(3) },
					source_id: d.source_id, source_run: d.source_run, metadata: d.metadata ?? {},
				}));
			} else {
				const withPath = leaves.map(find).filter(Boolean).map(([st, t]) => ({ ...st, path: pathOf(t, st) }));
				results = hitsFor(b.query, withPath, Math.min(k, 5));
			}
			return send(res, 200, {
				results,
				routing: { mode_used: b.leaf_ids ? null : (b.route_mode ?? 'hybrid'), leaves, laya_consulted: false },
				timings_ms: { embed: 3, route: 1, search: 2, rerank: 4, total: 11 },
				providers: { embed: { provider: 'fake', model: 'BAAI/bge-m3' } },
				timings_by_model: [],
			});
		}
		if (a === 'ingest' && id === 'jobs' && m === 'GET') return send(res, 200, { jobs: [] });
		return send(res, 404);
	} catch (e) {
		console.error('[b8-fake-strata]', e);
		send(res, 500, { error: { code: 'internal', message: String(e) } });
	}
});
server.listen(PORT, '127.0.0.1', () => console.log(`[b8-fake-strata] listening on 127.0.0.1:${PORT}`));
