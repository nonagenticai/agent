// ps-fake-eval.mjs — strata_rag's tenant eval (rag #21, `src/api/eval_api.rs`) for the B8 fake strata, which mounts it
// (b8-fake-strata.mjs). The shapes are agent-api's proxy contract (crates/agent-api/src/strata/rag.rs § Eval) and the
// web's types (web/src/lib/api/types/retrieval.ts); scoring runs over the fake's own corpus through its `/api/query`
// ranking, so a rank here is the rank a search in the stack gets.
//
//   GET    /api/eval/questions              -> {items, next: null}
//   POST   /api/eval/questions              -> 201 Question; 422 invalid_input, 409 question_exists / too_many_questions
//   PATCH  /api/eval/questions/{id}         -> Question; 404 not_found, 422 invalid_input
//   DELETE /api/eval/questions/{id}         -> 204; 404 not_found
//   POST   /api/eval/questions/import       -> 201 {added, total}, all or nothing
//   POST   /api/eval {source, root_id, question_ids?|questions?, modes?, beam?, min_score?}
//                                           -> 202 running summary + Location; 409 eval_running, 422 invalid_input,
//                                              404 not_found, 409 eval_not_ready (`eval not ready: a\nb`)
//   GET    /api/eval/runs?limit=&cursor=    -> {items: [summary], next}, newest first
//   GET    /api/eval/runs/{id}              -> the run with `questions` and `results` ([] until completed)
//
// A run scores in the background like strata's: progress grows with wall time (RUN_MS for the whole run) whether or
// not anyone polls. Test hook (no auth, the fake listens on loopback only): `POST /__eval/fail-next {"error"}` makes
// the next run stop with that error half-way.
import { randomUUID } from 'node:crypto';

const MODES = ['embed', 'laya', 'hybrid', 'flat'];
const DEFAULT_MODES = ['embed', 'hybrid', 'flat'];
const MAX_QUESTIONS = 200;
const TOP_K = 10;
const RUN_MS = Number(process.env.PS_EVAL_RUN_MS ?? 4500);
const NIL = '00000000-0000-0000-0000-000000000000';

const state = new Map(); // tenant -> {questions: [], runs: []}
const of = (tenant) => {
	if (!state.has(tenant)) state.set(tenant, { questions: [], runs: [] });
	return state.get(tenant);
};
let failNext = null;

const now = () => new Date().toISOString();
const opt = (s) => {
	const t = typeof s === 'string' ? s.trim() : '';
	return t || null;
};
const ids = (v) => [...new Set((Array.isArray(v) ? v : []).map((x) => String(x).trim()).filter(Boolean))];
const normalized = (q) => ({
	query: typeof q.query === 'string' ? q.query.trim() : '',
	expected_document: opt(q.expected_document),
	expected_leaf: opt(q.expected_leaf),
	expected_document_ids: ids(q.expected_document_ids),
	expected_chunk_ids: ids(q.expected_chunk_ids),
	expected_text: opt(q.expected_text),
});
/** strata_rag `QuestionInput::validate`: the first problem, or null. */
function invalid(q) {
	if (!q.query) return 'query is empty';
	if ([...q.expected_document_ids, ...q.expected_chunk_ids].includes(NIL)) return 'an expected id is the nil uuid';
	if (!q.expected_document && !q.expected_text && !q.expected_document_ids.length && !q.expected_chunk_ids.length)
		return 'no expectation: give expected_document, expected_document_ids, expected_chunk_ids or expected_text';
	return null;
}
const sameQuery = (a, b) => a.toLowerCase() === b.toLowerCase();
const baseName = (s) => String(s).split(/[\\/]/).pop();
const norm = (s) => String(s).toLowerCase().replace(/\s+/g, ' ').trim();

/** The evaluated tree under `root` (a strata store id; null = the whole tenant tree): its leaves with name paths. */
function leavesUnder(tree, pathOf, root) {
	const inTree = (s) => {
		for (let c = s; c; c = c.parent_id ? tree.get(c.parent_id) : null) if (c.id === root) return true;
		return false;
	};
	return [...tree.values()]
		.filter((s) => s.kind === 'leaf' && (!root || inTree(s)))
		.map((s) => ({ id: s.id, name: s.name, path: pathOf(tree, s) }));
}
function resolveLeaf(spec, leaves) {
	const byId = leaves.find((l) => l.id === spec);
	if (byId) return { id: byId.id };
	if (spec.includes('/')) {
		const p = leaves.find((l) => l.path.join('/') === spec);
		return p ? { id: p.id } : { error: 'is not a leaf store in the evaluated tree' };
	}
	const named = leaves.filter((l) => l.name === spec);
	if (named.length === 1) return { id: named[0].id };
	return { error: named.length ? 'matches several leaf stores; use its path' : 'is not a leaf store in the evaluated tree' };
}
/** strata_rag `eval_not_ready` lines: no leaf, an unresolvable leaf, a document / id / chunk not in the tree. */
function readiness(qs, leaves, corpus) {
	const out = [];
	if (!leaves.length) out.push('the evaluated tree has no leaf stores');
	const docIds = new Set(corpus.map((d) => d.document_id));
	const chunkIds = new Set(corpus.map((d) => d.chunk_id));
	qs.forEach((q, i) => {
		const n = i + 1;
		if (q.expected_leaf) {
			const r = resolveLeaf(q.expected_leaf, leaves);
			if (r.error) out.push(`question ${n}: expected_leaf \`${q.expected_leaf}\` ${r.error}`);
		}
		const d = q.expected_document;
		if (d && leaves.length && !corpus.some((x) => d === x.title || d === x.source || d === baseName(x.source)))
			out.push(`question ${n}: no document \`${d}\` in the evaluated tree`);
		for (const id of q.expected_document_ids) if (!docIds.has(id)) out.push(`question ${n}: document ${id} is not in the evaluated tree`);
		for (const id of q.expected_chunk_ids) if (!chunkIds.has(id)) out.push(`question ${n}: chunk ${id} is not in the evaluated tree`);
	});
	return out;
}
const matches = (q, h) =>
	(q.expected_document && (q.expected_document === h.title || q.expected_document === h.source || q.expected_document === baseName(h.source))) ||
	(q.expected_text && norm(h.text).includes(norm(q.expected_text))) ||
	q.expected_document_ids.includes(h.document_id) ||
	q.expected_chunk_ids.includes(h.chunk_id);

/** Every question × mode scored now (the outcome is fixed at start and revealed as the run's clock advances). */
function score(qs, modes, leaves, hitsFor) {
	const results = [];
	const reports = modes.map((mode) => {
		let r1 = 0, r5 = 0, r10 = 0, leafN = 0, leafHits = 0;
		qs.forEach((q, index) => {
			const hits = hitsFor(q.query, leaves, TOP_K);
			const at = hits.findIndex((h) => matches(q, h));
			const rank = at < 0 ? null : at + 1;
			let leaf_hit = null;
			if (mode !== 'flat' && q.expected_leaf) {
				const want = resolveLeaf(q.expected_leaf, leaves);
				const top = hits[0];
				leaf_hit = !!(top && want.id && leaves.find((l) => l.id === want.id)?.path.join('/') === top.store_path.join('/'));
				leafN += 1;
				if (leaf_hit) leafHits += 1;
			}
			if (rank && rank <= 1) r1 += 1;
			if (rank && rank <= 5) r5 += 1;
			if (rank && rank <= 10) r10 += 1;
			results.push({ index, question_id: q.id ?? null, mode, rank, leaf_hit });
		});
		const n = qs.length;
		return {
			mode,
			n,
			recall_at_1: r1 / n,
			recall_at_5: r5 / n,
			recall_at_10: r10 / n,
			leaf_accuracy: leafN ? leafHits / leafN : null,
			laya_consulted: mode === 'laya' || mode === 'hybrid' ? n : 0,
			laya_fallbacks: 0,
		};
	});
	const flat = reports.find((r) => r.mode === 'flat');
	const verdict = !flat || reports.every((r) => r.mode === 'flat' || r.recall_at_5 >= flat.recall_at_5) ? 'PASS' : 'FAIL';
	return { results, reports, verdict };
}

/** The run as of now: running until its clock passes the end (or its failure point). */
function settle(run) {
	if (run.status !== 'running') return run;
	const total = run.progress.total;
	const elapsed = Date.now() - run._t0;
	const stopAt = run._fail ? Math.floor(total / 2) : total;
	const done = Math.min(stopAt, Math.floor((elapsed / RUN_MS) * total));
	run.progress = { done, total };
	if (done < stopAt) return run;
	run.finished_at = now();
	run.took_ms = elapsed;
	if (run._fail) {
		run.status = 'failed';
		run.error = run._fail;
	} else {
		run.status = 'completed';
		run.verdict = run._outcome.verdict;
		run.reports = run._outcome.reports;
		run.results = run._outcome.results;
	}
	return run;
}
const summary = (r) => {
	const { _t0, _fail, _outcome, questions, results, ...head } = r;
	return { ...head, question_count: questions.length };
};
const detail = (r) => {
	const { _t0, _fail, _outcome, ...rest } = r;
	return { ...rest, results: rest.status === 'completed' ? rest.results : [] };
};

/**
 * Answer one eval request, or return false when the path is not the eval API's. `ctx`: {tenant, actor, tree, pathOf,
 * hitsFor, corpus, readable(storeId), send, fail}.
 */
export async function handleEval(req, res, url, body, ctx) {
	const { send, fail } = ctx;
	const seg = url.pathname.split('/').filter(Boolean);
	if (seg[0] !== 'api' || seg[1] !== 'eval') return false;
	const m = req.method;
	const s = of(ctx.tenant);

	if (seg.length === 3 && seg[2] === 'questions') {
		if (m === 'GET') return send(res, 200, { items: s.questions, next: null }), true;
		if (m === 'POST') {
			const q = normalized(body);
			const bad = invalid(q);
			if (bad) return fail(res, 422, 'invalid_input', `invalid input: ${bad}`), true;
			if (s.questions.some((x) => sameQuery(x.query, q.query)))
				return fail(res, 409, 'question_exists', 'question exists: the same query is already a question'), true;
			if (s.questions.length >= MAX_QUESTIONS)
				return fail(res, 409, 'too_many_questions', `too many questions: at most ${MAX_QUESTIONS} per tenant`), true;
			const row = { id: randomUUID(), ...q, created_by: ctx.actor, created_at: now(), updated_at: now() };
			s.questions.push(row);
			return send(res, 201, row), true;
		}
	}
	if (seg.length === 4 && seg[2] === 'questions' && seg[3] === 'import' && m === 'POST') {
		const list = Array.isArray(body.questions) ? body.questions.map(normalized) : [];
		if (!list.length) return fail(res, 422, 'invalid_input', 'invalid input: questions is empty'), true;
		for (const [i, q] of list.entries()) {
			const bad = invalid(q);
			if (bad) return fail(res, 422, 'invalid_input', `invalid input: question ${i + 1}: ${bad}`), true;
			if (s.questions.some((x) => sameQuery(x.query, q.query)) || list.slice(0, i).some((x) => sameQuery(x.query, q.query)))
				return fail(res, 409, 'question_exists', `question exists: question ${i + 1} repeats a query`), true;
		}
		if (s.questions.length + list.length > MAX_QUESTIONS)
			return fail(res, 409, 'too_many_questions', `too many questions: at most ${MAX_QUESTIONS} per tenant`), true;
		for (const q of list) s.questions.push({ id: randomUUID(), ...q, created_by: ctx.actor, created_at: now(), updated_at: now() });
		return send(res, 201, { added: list.length, total: s.questions.length }), true;
	}
	if (seg.length === 4 && seg[2] === 'questions' && (m === 'PATCH' || m === 'DELETE')) {
		const i = s.questions.findIndex((x) => x.id === seg[3]);
		if (i < 0) return fail(res, 404, 'not_found', `not found: question ${seg[3]}`), true;
		if (m === 'DELETE') return s.questions.splice(i, 1), send(res, 204), true;
		const keys = ['query', 'expected_document', 'expected_leaf', 'expected_document_ids', 'expected_chunk_ids', 'expected_text'];
		const patch = Object.fromEntries(Object.entries(body).filter(([k]) => keys.includes(k)));
		if (!Object.keys(patch).length) return fail(res, 422, 'invalid_input', 'invalid input: nothing to change'), true;
		const next = normalized({ ...s.questions[i], ...patch });
		const bad = invalid(next);
		if (bad) return fail(res, 422, 'invalid_input', `invalid input: ${bad}`), true;
		if (s.questions.some((x, j) => j !== i && sameQuery(x.query, next.query)))
			return fail(res, 409, 'question_exists', 'question exists: the same query is already a question'), true;
		s.questions[i] = { ...s.questions[i], ...next, updated_at: now() };
		return send(res, 200, s.questions[i]), true;
	}

	if (seg.length === 2 && m === 'POST') {
		s.runs.forEach(settle);
		if (s.runs.some((r) => r.status === 'running'))
			return fail(res, 409, 'eval_running', 'eval running: an eval of this tenant is already running'), true;
		if (body.questions && body.question_ids)
			return fail(res, 422, 'invalid_input', 'invalid input: give questions or question_ids, not both'), true;
		const modes = body.modes ?? DEFAULT_MODES;
		if (!Array.isArray(modes) || !modes.length || modes.some((x) => !MODES.includes(x)))
			return fail(res, 422, 'invalid_input', `invalid input: modes must be some of ${MODES.join(', ')}`), true;
		let qs;
		if (Array.isArray(body.questions)) {
			qs = body.questions.map((q) => ({ id: null, ...normalized(q) }));
			for (const [i, q] of qs.entries()) {
				const bad = invalid(q);
				if (bad) return fail(res, 422, 'invalid_input', `invalid input: question ${i + 1}: ${bad}`), true;
			}
		} else if (Array.isArray(body.question_ids)) {
			qs = [];
			for (const id of body.question_ids) {
				const q = s.questions.find((x) => x.id === id);
				if (!q) return fail(res, 404, 'not_found', `not found: question ${id}`), true;
				qs.push(q);
			}
		} else {
			if (!s.questions.length)
				return fail(res, 409, 'eval_not_ready', 'eval not ready: this tenant has no eval questions; add some with POST /api/eval/questions'), true;
			qs = s.questions;
		}
		if (!qs.length) return fail(res, 422, 'invalid_input', 'invalid input: no questions to evaluate'), true;
		if (qs.length > MAX_QUESTIONS)
			return fail(res, 422, 'too_many_questions', `too many questions: at most ${MAX_QUESTIONS} in one eval`), true;
		const root = body.root_id ?? null;
		if (root && !ctx.readable(root)) return fail(res, 404, 'not_found', `not found: store ${root}`), true;
		const leaves = leavesUnder(ctx.tree, ctx.pathOf, root);
		const problems = readiness(qs, leaves, ctx.corpus);
		if (problems.length) return fail(res, 409, 'eval_not_ready', `eval not ready: ${problems.join('\n')}`), true;
		const snapshot = qs.map((q) => ({
			id: q.id ?? null,
			query: q.query,
			expected_document: q.expected_document,
			expected_leaf: q.expected_leaf,
			expected_document_ids: q.expected_document_ids,
			expected_chunk_ids: q.expected_chunk_ids,
			expected_text: q.expected_text,
		}));
		const run = {
			id: randomUUID(),
			source: Array.isArray(body.questions) ? 'inline' : 'tenant',
			root_id: root,
			modes,
			beam: body.beam ?? 3,
			min_score: body.min_score ?? 0.35,
			status: 'running',
			error: null,
			progress: { done: 0, total: snapshot.length * modes.length },
			verdict: null,
			started_by: ctx.actor,
			started_at: now(),
			finished_at: null,
			took_ms: null,
			reports: null,
			questions: snapshot,
			results: [],
			_t0: Date.now(),
			_fail: failNext,
			_outcome: score(snapshot, modes, leaves, ctx.hitsFor),
		};
		failNext = null;
		s.runs.unshift(run);
		res.setHeader('location', `/api/eval/runs/${run.id}`);
		return send(res, 202, summary(run)), true;
	}
	if (seg.length === 3 && seg[2] === 'runs' && m === 'GET') {
		s.runs.forEach(settle);
		const limit = Math.min(100, Math.max(1, Number(url.searchParams.get('limit') ?? 20) || 20));
		const from = Number(url.searchParams.get('cursor') ?? 0) || 0;
		const page = s.runs.slice(from, from + limit).map(summary);
		return send(res, 200, { items: page, next: from + limit < s.runs.length ? String(from + limit) : null }), true;
	}
	if (seg.length === 4 && seg[2] === 'runs' && m === 'GET') {
		const r = s.runs.find((x) => x.id === seg[3]);
		if (!r) return fail(res, 404, 'not_found', `not found: eval run ${seg[3]}`), true;
		return send(res, 200, detail(settle(r))), true;
	}
	return fail(res, 404, 'not_found', `not found: ${url.pathname}`), true;
}

/** `POST /__eval/fail-next {"error"}`: the next run stops half-way with that error. */
export function handleEvalHook(req, res, url, body, send) {
	if (url.pathname !== '/__eval/fail-next' || req.method !== 'POST') return false;
	failNext = typeof body.error === 'string' && body.error ? body.error : 'embed provider error';
	send(res, 204);
	return true;
}
