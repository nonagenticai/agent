// ps-fake-artifacts.mjs — the platform artifact_service for the local platform stack (conversation attachments,
// SW-9: crates/agent-api/src/attachments/artifacts.rs). In memory, one process, nothing persisted; it stands in for the
// in-cluster service the way the fake IdP / LLM / Strata stand in for theirs. The scan is "clean" for every file except
// one whose bytes contain the EICAR test string (422 {code: "malware"}), so the malware path stays testable.
//
//   POST   /projects/{p}/artifacts/upload?project_id={p}   multipart file + filename, content_type, kind, sensitivity, tags
//                                                          -> 201 {artifact_id}
//   GET    /projects/{p}/artifacts/{id}/download           -> the bytes (404 when unknown)
//   DELETE /projects/{p}/artifacts/{id}                    -> 204 (404 when unknown)
//   GET    /projects/{p}/artifacts?tags=a,b&limit=n         -> [{id, tags, filename, content_type, size}]
//   GET    /health                                          -> {status: "ok"}
// Every /projects call needs a Bearer (the tenant's licence token: any non-empty one here). Env: PS_ARTIFACTS_PORT.
import { createServer } from 'node:http';
import { randomUUID } from 'node:crypto';

const PORT = Number(process.env.PS_ARTIFACTS_PORT ?? 9457);
const EICAR = 'X5O!P%@AP[4\\PZX54(P^)7CC)7}$EICAR-STANDARD-ANTIVIRUS-TEST-FILE!$H+H*';
/** id -> {project, bytes, content_type, filename, tags} */
const store = new Map();

const json = (res, status, body) => {
	res.writeHead(status, { 'content-type': 'application/json' });
	res.end(JSON.stringify(body));
};
const read = (req) =>
	new Promise((ok, fail) => {
		const parts = [];
		req.on('data', (c) => parts.push(c));
		req.on('end', () => ok(Buffer.concat(parts)));
		req.on('error', fail);
	});

createServer(async (req, res) => {
	try {
		const u = new URL(req.url, 'http://127.0.0.1');
		if (u.pathname === '/health') return json(res, 200, { status: 'ok' });
		const m = u.pathname.match(/^\/projects\/([0-9a-f-]{36})\/artifacts(?:\/(upload|[0-9a-f-]{36})(\/download)?)?$/);
		if (!m) return json(res, 404, { detail: 'not found' });
		if (!/^Bearer \S+/.test(req.headers.authorization ?? '')) return json(res, 401, { detail: 'unauthorized' });
		const [, project, sub, download] = m;
		if (req.method === 'POST' && sub === 'upload') {
			const body = await read(req);
			const form = await new Request('http://x', { method: 'POST', headers: { 'content-type': req.headers['content-type'] }, body }).formData();
			const file = form.get('file');
			if (!file || typeof file === 'string') return json(res, 422, { detail: 'file is required' });
			const bytes = Buffer.from(await file.arrayBuffer());
			if (bytes.includes(Buffer.from(EICAR))) return json(res, 422, { code: 'malware', detail: 'EICAR test signature' });
			const id = randomUUID();
			const tags = String(form.get('tags') ?? '').split(',').map((t) => t.trim()).filter(Boolean);
			store.set(id, { project, bytes, content_type: String(form.get('content_type') ?? file.type), filename: String(form.get('filename') ?? file.name), tags });
			return json(res, 201, { artifact_id: id });
		}
		if (req.method === 'GET' && !sub) {
			const want = (u.searchParams.get('tags') ?? '').split(',').map((t) => t.trim()).filter(Boolean);
			const limit = Number(u.searchParams.get('limit') ?? 100);
			const rows = [...store.entries()]
				.filter(([, a]) => a.project === project && want.every((t) => a.tags.includes(t)))
				.slice(0, limit)
				.map(([id, a]) => ({ id, tags: a.tags, filename: a.filename, content_type: a.content_type, size: a.bytes.length }));
			return json(res, 200, rows);
		}
		const a = store.get(sub);
		if (!a || a.project !== project) return json(res, 404, { detail: 'artifact not found' });
		if (req.method === 'GET' && download) {
			res.writeHead(200, { 'content-type': a.content_type, 'content-length': a.bytes.length });
			return res.end(a.bytes);
		}
		if (req.method === 'DELETE' && !download) {
			store.delete(sub);
			res.writeHead(204);
			return res.end();
		}
		return json(res, 405, { detail: 'method not allowed' });
	} catch (e) {
		json(res, 500, { detail: String(e?.message ?? e) });
	}
}).listen(PORT, '127.0.0.1', () => console.log(`ps-fake-artifacts on 127.0.0.1:${PORT}`));
