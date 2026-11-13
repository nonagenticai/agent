#!/usr/bin/env node
// B4 known-customer web chat, LOCAL acceptance: strata front for recreate.ts. The B8 fake (b8-fake-strata.mjs: stores,
// /api/query, providers) has no ingest write path, but `recreate.ts` re-syncs file sources through
// POST /api/ingest/jobs (crates/agent-api/src/strata/ingest.rs: multipart `store_id`, optional `source_id`, one `file`
// part per job -> 202 {"jobs":[{job_id,status}]}) and polls GET /api/ingest/jobs/{id}. This process answers those two
// (jobs complete at once, nothing is indexed) and forwards EVERYTHING else unchanged to the B8 fake.
//
// It also keeps the job lifecycle agent-api proxies (crates/agent-api/src/strata/ingest.rs, strata_ingest#11), per
// tenant: GET /api/ingest/jobs?store_id=&status=&limit= (newest first), POST …/{id}/cancel (running → cancelling →
// cancelled once the worker's step ends, CANCEL_MS later; queued → cancelled; ended → 409 conflict), POST …/{id}/retry
// (failed → queued, else 409), DELETE …/{id} (running → 409 conflict; ended → 200 report, documents retained). Test
// hook (no auth, loopback only): POST /__ingest/seed {tenant, store_id, filename, status, steps?, delete_fails_once?}
// → 201 {id} adds a job as strata would hold it; `delete_fails_once` makes its first DELETE leave it `deleting` with
// 502 upstream_unavailable (object storage down), as strata_ingest does, and the next DELETE finish it.
//
// Env: KC_STRATA_PORT (required), KC_STRATA_UPSTREAM (required, e.g. http://127.0.0.1:9356).
import { createServer, request } from 'node:http';
import { createHash } from 'node:crypto';
import { randomUUID } from 'node:crypto';
import { bearerOf, verifyBundleToken } from './ps-bundle.mjs';
// opt-in (platform-stack): the paths answered HERE (ingest jobs) need a valid bundle token for strata-ingest/strata-rag or a
// ps-strata-* static token; everything forwarded is judged by the B8 fake
const BUNDLE_SECRET = process.env.PS_BUNDLE_SECRET ?? '';

const PORT = Number(process.env.KC_STRATA_PORT ?? 0);
const UP = new URL(process.env.KC_STRATA_UPSTREAM ?? 'http://invalid');
if (!PORT || !process.env.KC_STRATA_UPSTREAM) throw new Error('KC_STRATA_PORT and KC_STRATA_UPSTREAM are required');
const jobs = new Map(); // id -> strata job view (+ `_tenant`, `_cancelAt`, `_deleteFails`)
const CANCEL_MS = Number(process.env.KC_STRATA_CANCEL_MS ?? 2500);
const ENDED = ['completed', 'failed', 'rejected', 'cancelled'];
/** The tenant a request's token pins (bundle token or `ps-strata-<uuid>`); null = unknown. */
const tenantOf = (req) => {
	const b = bearerOf(req);
	if (!b) return null;
	return (BUNDLE_SECRET ? (verifyBundleToken(BUNDLE_SECRET, b, 'strata-ingest') ?? verifyBundleToken(BUNDLE_SECRET, b, 'strata-rag')) : null) ?? /^ps-strata-([0-9a-f-]{36})$/.exec(b)?.[1] ?? null;
};
/** A job as strata answers it now: a cancelling job is cancelled once its step has ended. */
const settle = (j) => {
	if (j.status === 'cancelling' && Date.now() >= j._cancelAt) {
		j.status = 'cancelled';
		j.updated_at = new Date().toISOString();
		for (const st of j.steps) if (st.status === 'running' || st.status === 'pending') st.status = st.status === 'running' ? 'completed' : 'skipped';
	}
	return j;
};
const view = (j) => {
	const { _tenant, _cancelAt, _deleteFails, _was, ...v } = settle(j);
	return v;
};
const jobOf = (req, id) => {
	const j = jobs.get(id);
	return j && (!j._tenant || j._tenant === tenantOf(req)) ? j : null;
};
const send = (res, status, body) => {
	res.writeHead(status, { 'content-type': 'application/json' });
	res.end(JSON.stringify(body));
};
const field = (text, name) => new RegExp(`name="${name}"\\r?\\n\\r?\\n([^\\r\\n]*)`).exec(text)?.[1] ?? null;

createServer(async (req, res) => {
	const path = new URL(req.url, 'http://x').pathname;
	const chunks = [];
	for await (const c of req) chunks.push(c);
	const raw = Buffer.concat(chunks);
	if (req.method === 'POST' && path === '/__ingest/seed') {
		const b = JSON.parse(raw.toString('utf8') || '{}');
		const id = randomUUID();
		const at = new Date().toISOString();
		const steps = (b.steps ?? []).map((st) => ({ kind: st.kind, status: st.status ?? 'pending', attempt: st.attempt ?? 1, provider: st.provider ?? null, model: st.model ?? null, error: st.error ?? null }));
		jobs.set(id, { id, store_id: b.store_id, status: b.status ?? 'running', created_at: at, updated_at: at, document_ids: b.document_ids ?? [], steps, meta: { source_id: `upload:${randomUUID()}` }, source: { kind: 'upload', filename: b.filename ?? 'upload.pdf' }, _tenant: b.tenant ?? null, _deleteFails: !!b.delete_fails_once });
		return send(res, 201, { id });
	}
	if (BUNDLE_SECRET && /^\/api\/ingest\/jobs/.test(path)) {
		const b = bearerOf(req);
		if (!verifyBundleToken(BUNDLE_SECRET, b) && !/^ps-strata-/.test(b)) return send(res, 401, { error: { code: 'unauthorized', message: 'unauthorized' } });
	}
	if (req.method === 'POST' && path === '/api/ingest/jobs') {
		const text = raw.toString('latin1');
		const files = (text.match(/name="file"/g) ?? []).length;
		const store_id = field(text, 'store_id');
		const source_id = field(text, 'source_id');
		const out = [];
		// index every text file part through the B8 fake's JSON ingest, so a search returns what was uploaded
		const ctype = String(req.headers['content-type'] ?? '');
		const boundary = /boundary=("?)([^";]+)\1/.exec(ctype)?.[2];
		const parts = boundary ? text.split(`--${boundary}`) : [];
		for (const part of parts) {
			const head = /^\r?\n?Content-Disposition:[^\r\n]*name="file"[^\r\n]*filename="([^"]*)"/i.exec(part);
			if (!head || !store_id) continue;
			const bodyAt = part.indexOf('\r\n\r\n');
			const content = Buffer.from(part.slice(bodyAt + 4).replace(/\r\n$/, ''), 'latin1').toString('utf8');
			if (!content.trim() || content.includes('\u0000') || content.startsWith('%PDF')) continue;
			await new Promise((ok) => {
				const body = JSON.stringify({ text: content, source: head[1], title: head[1].replace(/\.[^.]+$/, ''), source_id: source_id ?? undefined });
				const h = { ...req.headers, 'content-type': 'application/json', 'content-length': Buffer.byteLength(body) };
				const u = request({ host: UP.hostname, port: UP.port, path: `/api/stores/${store_id}/documents`, method: 'POST', headers: h }, (r) => { r.resume(); r.on('end', ok); });
				u.on('error', ok);
				u.end(body);
			});
		}
		for (let i = 0; i < Math.max(files, 1); i++) {
			const id = randomUUID();
			const now = new Date().toISOString();
			jobs.set(id, { id, store_id, status: 'completed', created_at: now, updated_at: now, document_ids: [randomUUID()], steps: [], meta: { source_id }, source: { kind: 'upload' }, _tenant: tenantOf(req) });
			out.push({ job_id: id, status: 'queued' });
		}
		return send(res, 202, { jobs: out });
	}
	const one = /^\/api\/ingest\/jobs\/([0-9a-f-]{36})$/.exec(path);
	const notFound = () => send(res, 404, { error: { code: 'not_found', message: 'not found' } });
	const conflict = (message) => send(res, 409, { error: { code: 'conflict', message } });
	if (req.method === 'GET' && one) {
		const j = jobOf(req, one[1]);
		return j ? send(res, 200, view(j)) : notFound();
	}
	if (req.method === 'DELETE' && one) {
		const j = jobOf(req, one[1]);
		if (!j) return notFound();
		settle(j);
		if (['queued', 'running', 'cancelling'].includes(j.status)) return conflict('conflict: the job is still running; cancel it first');
		if (j._deleteFails) {
			j._deleteFails = false;
			j._was = j.status;
			j.status = 'deleting';
			j.updated_at = new Date().toISOString();
			return send(res, 502, { error: { code: 'upstream_unavailable', message: 'upstream unavailable: object storage did not remove a staged file' } });
		}
		jobs.delete(j.id);
		return send(res, 200, { job_id: j.id, status: j._was ?? j.status, deleted_jobs: 1, deleted_objects: 1, document_ids: j.document_ids, documents: 'retained' });
	}
	const act = /^\/api\/ingest\/jobs\/([0-9a-f-]{36})\/(cancel|retry)$/.exec(path);
	if (req.method === 'POST' && act) {
		const j = jobOf(req, act[1]);
		if (!j) return notFound();
		settle(j);
		if (act[2] === 'retry') {
			if (j.status !== 'failed') return conflict('conflict: only a failed job can be retried');
			j.status = 'queued';
		} else {
			if (ENDED.includes(j.status) || j.status === 'deleting') return conflict('conflict: the job has ended');
			if (j.status === 'running') {
				j.status = 'cancelling';
				j._cancelAt = Date.now() + CANCEL_MS;
			} else if (j.status !== 'cancelling') j.status = 'cancelled';
		}
		j.updated_at = new Date().toISOString();
		return send(res, 200, view(j));
	}
	// documents of a store (run detail) and the source prune: nothing is indexed here, so both are empty
	if (req.method === 'DELETE' && /^\/api\/sources\/[^/]+\/documents$/.test(path)) return send(res, 200, { deleted: 0, kept: 0, stores: [], refused_stores: [] });
	if (req.method === 'GET' && path === '/api/ingest/jobs') {
		const q = new URL(req.url, 'http://x').searchParams;
		const tenant = tenantOf(req);
		const limit = Math.min(200, Math.max(1, Number(q.get('limit') ?? 50) || 50));
		const list = [...jobs.values()]
			.reverse()
			.filter((j) => !j._tenant || j._tenant === tenant)
			.map(settle)
			.filter((j) => (!q.get('store_id') || j.store_id === q.get('store_id')) && (!q.get('status') || j.status === q.get('status')))
			.slice(0, limit)
			.map(view);
		return send(res, 200, { jobs: list, next: null });
	}
	const up = request({ host: UP.hostname, port: UP.port, path: req.url, method: req.method, headers: req.headers }, (r) => {
		res.writeHead(r.statusCode ?? 502, r.headers);
		r.pipe(res);
	});
	up.on('error', () => send(res, 502, { error: 'upstream_down' }));
	up.end(raw);
}).listen(PORT, '127.0.0.1', () => console.log(`kc-fake-strata on 127.0.0.1:${PORT} -> ${UP.origin}`));
