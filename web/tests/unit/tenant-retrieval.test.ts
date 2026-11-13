// Retrieval eval maths, golden-question CSV and run diff (pure).
import { describe, expect, it } from 'vitest';
import type {
	CompletedEvalRun,
	EvalModeReport,
	EvalRun,
	GoldenQuestionInput,
} from '../../src/lib/api/types/retrieval';
import {
	FIELD_MAX_CHARS,
	diffRuns,
	evalReadiness,
	normalizeQuestion,
	notReadyLines,
	parseCsv,
	parseGoldenCsv,
	parseIds,
	questionPatch,
	questionProblems,
	rankOf,
	resolveLeaf,
	verdictOf,
	recallAt,
} from '../../src/lib/tenant/retrieval';

describe('retrieval eval maths', () => {
	it('rank and recall@k', () => {
		expect(rankOf([{ document: 'a' }, { document: 'b' }], 'b')).toBe(2);
		expect(rankOf([{ document: 'a' }], 'z')).toBeNull();
		const ranks = [1, 3, 7, null];
		expect(recallAt(ranks, 1)).toBe(0.25);
		expect(recallAt(ranks, 5)).toBe(0.5);
		expect(recallAt(ranks, 10)).toBe(0.75);
		expect(recallAt([], 5)).toBe(0);
	});

	it('a question needs at least one expectation; leaf and document are optional (strata_rag QuestionInput)', () => {
		expect(questionProblems({ query: 'q' })[0]).toMatch(/no expectation/);
		expect(questionProblems({ query: 'q', expected_leaf: 'policy.payment' })[0]).toMatch(/no expectation/);
		expect(questionProblems({ query: 'q', expected_text: 'moss green' })).toEqual([]);
		expect(questionProblems({ query: 'q', expected_chunk_ids: ['c1'] })).toEqual([]);
		expect(questionProblems({ query: 'q', expected_document_ids: ['d1'] })).toEqual([]);
		expect(questionProblems({ query: '  ', expected_text: 'x' })).toContain('query is empty');
		expect(questionProblems({ query: 'q', expected_text: 'x'.repeat(FIELD_MAX_CHARS + 1) })[0]).toMatch(
			/expected_text is over 2000/,
		);
		expect(
			questionProblems({ query: 'q', expected_chunk_ids: ['00000000-0000-0000-0000-000000000000'] }),
		).toContain('expected_chunk_ids holds the nil uuid');
		expect(
			normalizeQuestion({ query: ' q ', expected_document: ' ', expected_document_ids: ['a', 'a', 'b'] }),
		).toEqual({
			query: 'q',
			expected_document: null,
			expected_leaf: null,
			expected_document_ids: ['a', 'b'],
			expected_chunk_ids: [],
			expected_text: null,
		});
		expect(parseIds('a; b | c  d;a')).toEqual(['a', 'b', 'c', 'd']);
	});

	it('the edit form sends a PATCH: changed fields only, a cleared field as null, a cleared id list as []', () => {
		const before = { query: 'q', expected_document: 'd', expected_leaf: 'l', expected_chunk_ids: ['c1'] };
		expect(questionPatch(before, { ...before })).toEqual({});
		expect(
			questionPatch(before, { ...before, expected_leaf: '', expected_text: 'new', expected_chunk_ids: [] }),
		).toEqual({ expected_leaf: null, expected_text: 'new', expected_chunk_ids: [] });
	});

	it('expected_leaf resolves by id, key path or unique key; eval_not_ready lists what is missing', () => {
		const leaves = [
			{ id: 's1', key: 'refunds', path: ['policy', 'refunds'] },
			{ id: 's2', key: 'faq', path: ['policy', 'faq'] },
			{ id: 's3', key: 'faq', path: ['help', 'faq'] },
		];
		expect(resolveLeaf('s1', leaves)).toEqual({ id: 's1' });
		expect(resolveLeaf('help/faq', leaves)).toEqual({ id: 's3' });
		expect(resolveLeaf('refunds', leaves)).toEqual({ id: 's1' });
		expect(resolveLeaf('faq', leaves)).toMatchObject({ error: expect.stringContaining('several') });
		const docs = [{ id: 'd1', store_id: 's1', title: 'Refund timing', source: 'drive/policies/refunds.pdf' }];
		const q = (x: GoldenQuestionInput) => normalizeQuestion(x);
		expect(
			evalReadiness(
				[
					q({ query: 'a', expected_document: 'refunds.pdf', expected_leaf: 'policy/refunds' }),
					q({ query: 'b', expected_document: 'Nope', expected_leaf: 'nope' }),
					q({ query: 'c', expected_document_ids: ['d1', 'd9'], expected_chunk_ids: ['c9'] }),
				],
				leaves,
				docs,
				new Set(['c1']),
			),
		).toEqual([
			'question 2: expected_leaf `nope` is not a leaf store in the evaluated tree',
			'question 2: no document `Nope` in the evaluated tree',
			'question 3: document d9 is not in the evaluated tree',
			'question 3: chunk c9 is not in the evaluated tree',
		]);
		expect(evalReadiness([], [], [], new Set())).toEqual(['the evaluated tree has no leaf stores']);
		expect(notReadyLines('eval not ready: question 1: a\nquestion 2: b')).toEqual([
			'question 1: a',
			'question 2: b',
		]);
	});

	it('verdict: PASS when every routed mode’s recall@5 is at least flat’s, PASS without flat', () => {
		const r = (mode: EvalModeReport['mode'], r5: number): EvalModeReport => ({
			mode,
			n: 1,
			recall_at_1: 0,
			recall_at_5: r5,
			recall_at_10: 1,
			leaf_accuracy: null,
			laya_consulted: 0,
			laya_fallbacks: 0,
		});
		expect(verdictOf([r('hybrid', 0.8), r('flat', 0.8)])).toBe('PASS');
		expect(verdictOf([r('hybrid', 0.8), r('embed', 0.5), r('flat', 0.6)])).toBe('FAIL');
		expect(verdictOf([r('embed', 0.1)])).toBe('PASS');
	});
});

describe('golden question CSV', () => {
	it('parses quoted cells with commas, escaped quotes and CRLF', () => {
		expect(parseCsv('a,"b, c","say ""hi"""\r\n1,2,3')).toEqual([
			['a', 'b, c', 'say "hi"'],
			['1', '2', '3'],
		]);
	});

	it('takes strata_rag’s columns in any order, ids split on ; | space, and reports every bad line', () => {
		const csv = [
			'expected_leaf,query,expected_document,expected_text,expected_chunk_ids',
			'policy.payment,"How long does a refund take, roughly?",Refund timing,,',
			',Where is my parcel?,,tracking link,',
			'policy.returns,,Returns policy 2026,,',
			'policy.returns,Only a leaf?,,,',
			'',
			',By chunk?,,,c1; c2|c3',
			'policy.returns,how long does a refund take?,Returns policy 2026,,',
			',where is my parcel?,,x,',
		].join('\n');
		const r = parseGoldenCsv(csv, { existing: ['How long does a refund take?'] });
		expect(r.rows.map((x) => x.query)).toEqual([
			'How long does a refund take, roughly?',
			'Where is my parcel?',
			'By chunk?',
		]);
		expect(r.rows[1]).toMatchObject({
			expected_leaf: null,
			expected_document: null,
			expected_text: 'tracking link',
		});
		expect(r.rows[2]!.expected_chunk_ids).toEqual(['c1', 'c2', 'c3']);
		expect(r.errors.map((e) => e.line)).toEqual([4, 5, 8, 9]);
		expect(r.errors[0]!.message).toContain('query is empty');
		expect(r.errors[1]!.message).toContain('no expectation');
		expect(r.errors[2]!.message).toContain('already in the set');
		expect(r.errors[3]!.message).toContain('earlier line');
	});

	it('refuses a header without query or without an expectation column, and one that would pass the 200 cap', () => {
		expect(parseGoldenCsv('question,answer\nx,y').errors[0]).toMatchObject({ line: 1 });
		expect(parseGoldenCsv('query,expected_leaf\nx,y').errors[0]!.message).toMatch(/expectation column/);
		const existing = Array.from({ length: 199 }, (_, i) => `q${i}`);
		const r = parseGoldenCsv('query,expected_text\na,b\nc,d', { existing });
		expect(r.rows).toHaveLength(2);
		expect(r.errors[0]).toMatchObject({ line: 0 });
		expect(r.errors[0]!.message).toContain('201');
		expect(parseGoldenCsv('query,expected_document\n').errors[0]!.message).toMatch(/no question/);
	});
});

describe('eval run diff', () => {
	const run = (id: string, qs: string[], r5: number, fb: number): CompletedEvalRun<EvalRun> => ({
		id,
		source: 'tenant',
		root_id: null,
		modes: ['hybrid'],
		beam: 3,
		min_score: 0.35,
		status: 'completed',
		error: null,
		progress: { done: qs.length, total: qs.length },
		verdict: 'PASS',
		started_at: '2026-10-18T00:00:00Z',
		finished_at: '2026-10-18T00:00:01Z',
		started_by: 'x',
		took_ms: 1000,
		questions: qs.map((query) => ({ id: null, ...normalizeQuestion({ query, expected_document: 'd' }) })),
		reports: [
			{
				mode: 'hybrid',
				n: qs.length,
				recall_at_1: 0.5,
				recall_at_5: r5,
				recall_at_10: 0.9,
				leaf_accuracy: 0.8,
				laya_consulted: qs.length,
				laya_fallbacks: fb,
			},
		],
	});

	it('gives per-mode metric deltas and the questions added and removed', () => {
		const d = diffRuns(run('a', ['q1', 'q2'], 0.6, 1), run('b', ['q2', 'q3', 'q4'], 0.85, 0));
		expect(d.modes.map((m) => m.mode)).toEqual(['hybrid']);
		const m = Object.fromEntries(d.modes[0]!.metrics.map((x) => [x.key, x]));
		expect(m.recall_at_5).toMatchObject({ base: 0.6, next: 0.85, delta: 0.25 });
		expect(m.laya_fallbacks!.delta).toBe(-1);
		expect(m.laya_consulted!.delta).toBe(1);
		expect(d.added).toEqual(['q3', 'q4']);
		expect(d.removed).toEqual(['q1']);
	});

	it('a null leaf accuracy has no delta', () => {
		const a = run('a', ['q1'], 0.6, 0);
		const b = run('b', ['q1'], 0.6, 0);
		b.reports[0]!.leaf_accuracy = null;
		const m = Object.fromEntries(diffRuns(a, b).modes[0]!.metrics.map((x) => [x.key, x]));
		expect(m.leaf_accuracy).toMatchObject({ next: null, delta: null });
	});
});
