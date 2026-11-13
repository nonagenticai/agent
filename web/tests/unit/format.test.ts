import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { citation, duration, durationBetween, fmt, pages, sentence } from '../../src/lib/format';

describe('fmt', () => {
	it('formats like the design', () => {
		expect(fmt.n(1234)).toBe('1,234');
		expect(fmt.pct(0.884)).toBe('88.4%');
		expect(fmt.ms(320)).toBe('320 ms');
		expect(fmt.ms(4210)).toBe('4.2 s');
		expect(fmt.k(3090)).toBe('3.1k');
		expect(fmt.n(null)).toBe('—');
	});
});

describe('citation', () => {
	const hit = { title: 'Returns policy', source: 'policies/returns.pdf', page_start: 2, page_end: 3 };
	it('names the title and the page span', () => {
		expect(citation(hit)).toBe('Returns policy · p. 2–3');
	});
	it('names a single page once', () => {
		expect(citation({ ...hit, page_start: 2, page_end: 2 })).toBe('Returns policy · p. 2');
		expect(pages(4, null)).toBe('p. 4');
	});
	it('shows no pages when the document has none', () => {
		expect(citation({ ...hit, page_start: null, page_end: null })).toBe('Returns policy');
		expect(pages(null, null)).toBe('');
	});
	it('falls back to the source label when there is no title', () => {
		expect(citation({ ...hit, title: null })).toBe('policies/returns.pdf · p. 2–3');
		expect(citation({ ...hit, title: '  ' })).toBe('policies/returns.pdf · p. 2–3');
	});
	it('shows only the pages when there is neither a title nor a source label', () => {
		expect(citation({ ...hit, title: null, source: '' })).toBe('p. 2–3');
		expect(citation({ ...hit, title: '', source: '  ', page_end: 2 })).toBe('p. 2');
		expect(citation({ ...hit, title: null, source: '', page_start: null, page_end: null })).toBe('');
	});
});

describe('duration: the one human duration every page shows', () => {
	it('humanises minutes: <1 min, min, h + min under a day, d + h from a day on', () => {
		expect(duration(0)).toBe('<1 min');
		expect(duration(0.4)).toBe('<1 min');
		expect(duration(1)).toBe('1 min');
		expect(duration(59)).toBe('59 min');
		expect(duration(60)).toBe('1 h');
		expect(duration(95)).toBe('1 h 35 min');
		expect(duration(1164)).toBe('19 h 24 min');
		expect(duration(1440)).toBe('1 d');
		// The browser pass: an ingest job "took 2121 min".
		expect(duration(2121)).toBe('1 d 11 h');
		expect(duration(3000)).toBe('2 d 2 h');
		expect(duration(-50)).toBe('50 min');
		expect(duration(null)).toBe('—');
		expect(duration(Number.NaN)).toBe('—');
	});

	it('durationBetween reads two ISO stamps; empty while the end is missing', () => {
		expect(durationBetween('2026-10-17T10:00:00Z', '2026-10-17T10:00:40Z')).toBe('<1 min');
		expect(durationBetween('2026-10-17T10:00:00Z', '2026-10-17T10:14:00Z')).toBe('14 min');
		expect(durationBetween('2026-10-17T00:00:00Z', '2026-10-18T11:21:00Z')).toBe('1 d 11 h');
		expect(durationBetween('2026-10-17T10:00:00Z', null)).toBe('');
		// A clock skew never reads as a negative duration.
		expect(durationBetween('2026-10-17T10:05:00Z', '2026-10-17T10:00:00Z')).toBe('<1 min');
	});

	it('the ingest "took", the case "waiting", the SLA chips and the audit gap all use it', async () => {
		const { took } = await import('../../src/lib/tenant/ingest-T8');
		const { duration: caseDuration } = await import('../../src/lib/runtime/J19-caseload');
		const { dur } = await import('../../src/lib/journey/J21Handover');
		expect(took('2026-10-17T00:00:00Z', '2026-10-18T11:21:00Z')).toBe('1 d 11 h');
		expect(took('2026-10-17T00:00:00Z', null)).toBe('');
		expect(caseDuration).toBe(duration);
		expect(dur(2121)).toBe('1 d 11 h');
		expect(readFileSync('src/routes/admin/audit/+page.svelte', 'utf8')).toMatch(/\$\{duration\(m\)\}/);
	});
});

describe('sentence', () => {
	it('ends a message with exactly one full stop', () => {
		expect(sentence('object storage did not answer')).toBe('object storage did not answer.');
		expect(sentence('already said. ')).toBe('already said.');
		expect(sentence('dangling; ')).toBe('dangling.');
		expect(sentence('  ')).toBe('');
	});
});
