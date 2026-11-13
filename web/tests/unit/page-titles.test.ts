// Every route's document title (`<title>` in +layout.svelte) comes from its pages.ts entry: an `also` route's own
// label, else the page title. The browser pass found `/admin/knowledge/eval` titled "Knowledge stores" and
// `…/backends` titled "Tools" because the layout used the owning page's title for every route of it.
import { existsSync, readFileSync, readdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { describe, expect, it } from 'vitest';
import { PAGES, pageForRoute, routesOf, titleForRoute } from '../../src/lib/pages';

const routes = PAGES.flatMap((p) =>
	routesOf(p)
		.filter((r) => !r.hash)
		.map((r) => ({ page: p, r })),
);

describe('page titles', () => {
	it('every route is titled by its own pages.ts entry', () => {
		expect(routes.length).toBeGreaterThan(61);
		for (const { page, r } of routes) {
			// A route two pages claim belongs to the first (J14 over J16); only that page titles it.
			if (pageForRoute(r.route) !== page) continue;
			expect(titleForRoute(r.route), r.route).toBe(r.label ?? page.title);
		}
	});

	it('the routes the browser pass caught', () => {
		expect(titleForRoute('/admin/knowledge/eval')).toBe('Retrieval eval');
		expect(titleForRoute('/admin/knowledge/stores')).toBe('Knowledge stores');
		expect(titleForRoute('/admin/jobs/[job]/backends')).toBe('Backend systems');
		expect(titleForRoute('/admin/jobs/[job]/tools')).toBe('Tools');
		expect(titleForRoute('/nowhere')).toBeUndefined();
		expect(titleForRoute(null)).toBeUndefined();
	});

	it('the layout titles the tab from titleForRoute', () => {
		const src = readFileSync('src/routes/+layout.svelte', 'utf8');
		const head = /<svelte:head>([\s\S]*?)<\/svelte:head>/.exec(src)?.[1] ?? '';
		expect(head).toContain('currentTitle');
		expect(src).toMatch(/currentTitle = \$derived\(titleForRoute\(routeId\)\)/);
	});

	it("a route's literal PageHead title equals its tab title", () => {
		const bad: string[] = [];
		for (const { r } of routes) {
			const file = `src/routes${r.route}/+page.svelte`;
			if (!existsSync(file)) continue;
			const dir = dirname(file);
			for (const f of readdirSync(dir).filter((x) => x.endsWith('.svelte'))) {
				const src = readFileSync(join(dir, f), 'utf8');
				for (const m of src.matchAll(/<PageHead\b[^>]*?\btitle="([^"{]*)"/g))
					if (m[1] !== titleForRoute(r.route))
						bad.push(`${r.route}: "${m[1]}" ≠ "${titleForRoute(r.route)}"`);
			}
		}
		expect(bad).toEqual([]);
	});
});
