// Task 12's gate, built with the skeleton: every spec 14 §2 page id is in the registry and every registry route
// has its +page.svelte (a placeholder until the area agent replaces it).
import { existsSync, readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';
import { PAGES, PAGE_IDS, routesOf } from '../../src/lib/pages';

const fileOf = (route: string) => `src/routes${route}/+page.svelte`;

describe('page registry covers spec 14 §2', () => {
	it('has exactly the 61 page ids of spec 14 §2 (L3, demo data, is gone) plus T14 (Company, D124), T15 (Identity procedures, A3), L4 (Talk to a job), L5 (Support) and P21 (Industries), each once', () => {
		const ids = PAGES.map((p) => p.id);
		expect(PAGE_IDS).toHaveLength(66);
		expect(new Set(ids).size).toBe(ids.length);
		expect([...ids].sort()).toEqual([...PAGE_IDS].sort());
	});

	it('every page route has a route file', () => {
		const missing = PAGES.flatMap((p) => routesOf(p).map((r) => [p.id, fileOf(r.route)] as const)).filter(
			([, f]) => !existsSync(f),
		);
		expect(missing).toEqual([]);
	});

	it('a tab page (hash) points at a route another page owns', () => {
		for (const p of PAGES.filter((x) => x.hash)) {
			expect(PAGES.some((o) => o.id !== p.id && o.route === p.route && !o.hash)).toBe(true);
		}
	});

	it('a placeholder names its own page id', () => {
		for (const p of PAGES)
			for (const r of routesOf(p)) {
				if (r.hash) continue;
				const src = readFileSync(fileOf(r.route), 'utf8');
				const m = /<PagePlaceholder id="([^"]+)"/.exec(src);
				if (m) expect(m[1], fileOf(r.route)).toBe(p.id);
			}
	});

	it('pages carry a milestone and a known area', () => {
		const areas = [
			'runtime',
			'tenant',
			'platform',
			'model',
			'connectors',
			'release',
			'journey',
			'ontology',
			'live',
		];
		for (const p of PAGES) {
			expect(['M1', 'M2', 'M3', 'M4']).toContain(p.milestone);
			expect(areas).toContain(p.area);
		}
	});
});
