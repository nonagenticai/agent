// Viewport check (owner: "mobile/tablet viewports must work"): every page route, the journey job (and a second job of the tenant if any), at the project's
// viewport (phone 390, tablet 768, tablet-landscape 1024). Per page it fails on
//   • page-level horizontal scroll: the document or the app's scrolling column (main.content) is wider than the
//     viewport;
//   • an element in the shell's main column (topbar + content) whose box passes the viewport's right edge and is not
//     inside an intentional horizontal scroller (an ancestor with overflow-x auto|scroll|hidden|clip that itself
//     fits). Only the outermost offender of a subtree is reported.
// Informational (never fails): scrollers that really scroll sideways without the shared affordance
// (.tbl-wrap / .tabs / .scroll-x / [data-scroll-x], which draw the edge fade); the JSON also lists the scrollers
// with the affordance that scroll at this width.
// Real stack (scripts/local/web-e2e.sh): every id comes from the live stack (viewport-routes.ts), each page is opened
// as the identity a person would (platform: super admin, /support: support agent, the rest: the journey tenant's admin).
// Also per page (the health checks pages.spec.ts made): no console error, a visible main h1, loading finished, no
// "Unavailable" callout. A failing page is named by id + URL. Routes with no object on the stack are listed under
// `skipped` in the JSON (and as a test annotation).
// Runs in the phone, tablet and tablet-landscape projects (playwright.config.ts). One browser page per chunk, pages visited in sequence. Report: test-results/viewports/<project>-<chunk>.json.
import { expect, test, type Page } from '@playwright/test';
import { mkdirSync, writeFileSync } from 'node:fs';
import { J, signIn, watchConsole, type Who } from './stack';
import { resolveViewportTargets, type Skipped, type Target } from './viewport-routes';

const CHUNKS = 6;

interface Offender {
	sel: string;
	px: number;
	text: string;
}
interface Finding {
	pageScroll: number;
	offenders: Offender[];
	bare: Offender[];
	/** Intentional scrollers (with the affordance) that do scroll sideways at this width: info for the report. */
	scrolling: Offender[];
}

/** Runs in the page. Keep it self-contained (serialised by Playwright). */
function detect(): Finding {
	const vw = document.documentElement.clientWidth;
	const content = document.querySelector<HTMLElement>('main.content');
	const pageScroll = Math.max(
		document.documentElement.scrollWidth - vw,
		content ? content.scrollWidth - content.clientWidth : 0,
	);
	const root = document.querySelector<HTMLElement>('.shell > .main') ?? document.body;
	const AFFORDANCE = '.tbl-wrap, .tabs, .scroll-x, [data-scroll-x]';
	const describe = (el: Element): Offender['sel'] => {
		const cls = [...el.classList].filter((c) => !c.startsWith('svelte-')).slice(0, 2);
		const own = el.getAttribute('data-testid');
		const tidEl = own ? null : el.closest('[data-testid]');
		const tid = own ?? (tidEl && root.contains(tidEl) ? tidEl.getAttribute('data-testid') : null);
		return `${el.tagName.toLowerCase()}${cls.map((c) => '.' + c).join('')}${tid ? ` [${own ? '' : 'in '}${tid}]` : ''}`;
	};
	const snippet = (el: Element) => (el.textContent ?? '').replace(/\s+/g, ' ').trim().slice(0, 40);
	// The nearest ancestor that clips el sideways. An absolutely positioned box (e.g. an sr-only span) escapes every
	// overflow ancestor below its containing block, so a scroller only clips it when the scroller is positioned.
	const clipperOf = (el: Element): Element | null => {
		let escapes = getComputedStyle(el).position === 'absolute';
		for (let a = el.parentElement; a && a !== root; a = a.parentElement) {
			if (a === content) return null;
			const cs = getComputedStyle(a);
			const contains = !escapes || cs.position !== 'static' || cs.transform !== 'none';
			if (contains && cs.overflowX !== 'visible') return a;
			if (contains) escapes = false;
		}
		return null;
	};
	const offenders: Offender[] = [];
	const bare: Offender[] = [];
	const scrolling: Offender[] = [];
	const reported: Element[] = [];
	for (const el of root.querySelectorAll('*')) {
		const cs = getComputedStyle(el);
		if (cs.position === 'fixed' || cs.visibility === 'hidden') continue;
		const ox = cs.overflowX;
		if (
			(ox === 'auto' || ox === 'scroll') &&
			el !== content &&
			el.scrollWidth > el.clientWidth + 1 &&
			el.closest('[aria-hidden="true"]') === null
		)
			(el.matches(AFFORDANCE) ? scrolling : bare).push({
				sel: describe(el),
				px: el.scrollWidth - el.clientWidth,
				text: snippet(el),
			});
		const r = el.getBoundingClientRect();
		if ((r.width === 0 && r.height === 0) || r.right <= vw + 1) continue;
		if (reported.some((p) => p.contains(el))) continue;
		if (clipperOf(el)) continue;
		reported.push(el);
		offenders.push({ sel: describe(el), px: Math.round(r.right - vw), text: snippet(el) });
	}
	return { pageScroll, offenders, bare, scrolling };
}

/** Open `t` and collect what a person would call broken besides layout: console errors, no h1, stuck loading, Unavailable. */
async function health(page: Page, errors: string[], t: Target): Promise<string[]> {
	const bad: string[] = [];
	const at = `${t.id} ${t.url}`;
	errors.length = 0;
	await page.goto(t.url);
	// isVisible() never waits: wait for the heading the way a person waits for the page
	const h1 = await page
		.locator('main h1')
		.first()
		.waitFor({ state: 'visible', timeout: 8000 })
		.then(() => true)
		.catch(() => false);
	if (!h1) bad.push(`${at}: no visible main h1`);
	const stuck = await page
		.getByTestId('load-loading')
		.first()
		.waitFor({ state: 'detached', timeout: 15_000 })
		.then(() => false)
		.catch(() => true);
	if (stuck) bad.push(`${at}: still loading (load-loading never went away)`);
	if ((await page.locator('.callout.err strong', { hasText: /^Unavailable$/ }).count()) > 0)
		bad.push(`${at}: shows the Unavailable callout`);
	await page.waitForTimeout(250);
	for (const e of errors) bad.push(`${at}: console error ${e.slice(0, 200)}`);
	return bad;
}

test.describe('viewports', () => {
	for (let c = 0; c < CHUNKS; c++) {
		test(`pages chunk ${c + 1}/${CHUNKS}`, async ({ browser }, info) => {
			const { targets, skipped } = await resolveViewportTargets();
			const chunk = targets.filter((_, i) => i % CHUNKS === c);
			// each page may wait 8s (h1) + 15s (loading) on a bad day; stay far under the 30-minute browser lease
			test.setTimeout(60_000 + chunk.length * 30_000);
			if (skipped.length)
				info.annotations.push({
					type: 'skipped-routes',
					description: skipped.map((s) => `${s.id} ${s.route} [${s.job}]: ${s.reason}`).join('\n'),
				});
			const use = info.project.use;
			const vw = use.viewport!.width;
			const rows: (Target & Finding & { health: string[] })[] = [];
			for (const who of ['sa', 'admin', 'agent'] as Who[]) {
				const mine = chunk.filter((t) => t.who === who);
				if (!mine.length) continue;
				// one context per identity: the sign-in is the context's own session
				const ctx = await browser.newContext({
					baseURL: use.baseURL,
					viewport: use.viewport,
					userAgent: use.userAgent,
					isMobile: use.isMobile,
					hasTouch: use.hasTouch,
					deviceScaleFactor: use.deviceScaleFactor,
				});
				try {
					const page = await ctx.newPage();
					const errors = watchConsole(page);
					await signIn(page, who, '/');
					for (const t of mine) {
						const bad = await health(page, errors, t);
						rows.push({ ...t, health: bad, ...(await page.evaluate(detect)) });
					}
				} finally {
					await ctx.close();
				}
			}
			mkdirSync('test-results/viewports', { recursive: true });
			writeFileSync(
				`test-results/viewports/${info.project.name}-${c + 1}.json`,
				JSON.stringify({ viewport: vw, rows, skipped: skipped satisfies Skipped[] }, null, 1),
			);
			const lines: string[] = [];
			for (const r of rows) {
				lines.push(...r.health);
				if (r.pageScroll > 1)
					lines.push(`${r.id} ${r.url} @${vw}: PAGE scrolls sideways by ${r.pageScroll}px`);
				for (const o of r.offenders) lines.push(`${r.id} ${r.url} @${vw}: ${o.sel} +${o.px}px “${o.text}”`);
			}
			const info2 = rows.flatMap((r) =>
				r.bare.map((b) => `${r.id} ${r.url} @${vw}: scroller w/o affordance ${b.sel}`),
			);
			if (info2.length) info.annotations.push({ type: 'bare-scrollers', description: info2.join('\n') });
			expect(lines.join('\n'), `${lines.length} viewport problems at ${vw}px`).toBe('');
		});
	}
});

// Review Focus 4: at phone width the switchers and the user menu live in the off-canvas drawer.
test('phone: switchers and user menu are inside the drawer, no horizontal scroll', async ({ page }) => {
	test.skip((page.viewportSize()?.width ?? 0) > 860, 'phone/tablet-portrait only');
	await signIn(page, 'admin', `/admin/jobs/${encodeURIComponent(J().job)}`);
	await expect(page.locator('main h1').first()).toBeVisible();
	// Closed: neither switchers nor the user menu are in the top bar.
	await expect(page.getByTestId('agent-switcher')).toHaveCount(0);
	await expect(page.getByTestId('user-menu')).toHaveCount(0);
	await expect(page.locator('.topbar').getByTestId('persona')).toHaveCount(0);
	await page.getByRole('button', { name: 'Open navigation' }).click();
	const drawer = page.getByRole('dialog');
	await expect(drawer.getByTestId('agent-switcher')).toBeVisible();
	// a tenant admin sees their workspace; only a super_admin gets the Change button (ScopeSwitcher)
	await expect(drawer.getByTestId('scope-switcher')).toBeVisible();
	await expect(drawer.getByTestId('tenant-button')).toHaveCount(0);
	await expect(drawer.getByTestId('theme-toggle')).toBeVisible();
	await expect(drawer.getByTestId('sign-out')).toBeVisible();
	const over = await page.evaluate(
		() => document.documentElement.scrollWidth - document.documentElement.clientWidth,
	);
	expect(over).toBeLessThanOrEqual(0);
	await page.keyboard.press('Escape');
	await expect(drawer).toBeHidden();
});
