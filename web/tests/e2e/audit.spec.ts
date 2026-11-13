// Inert-control audit (frontend-ui-audit, Task 12) on the 10 densest pages. Opt-in: AUDIT=1 scripts/local/web-e2e.sh --reuse
// audit --project=desktop (the real stack). For every enabled button in <main>, reload the page, click the button by locator, and
// record whether anything happened (DOM of <main> or a drawer changed, URL changed, a dialog or toast appeared).
// Also lists pointer-cursor elements with no role/href ("looks clickable, is not"). Writes
// test-results/audit/<page>.json; a human reviews the "no effect" rows (native form validation also shows there).
import { mkdirSync, writeFileSync } from 'node:fs';
import { expect, test, type Page } from '@playwright/test';
import { J as journey, signIn, type Who } from './stack';

// On the REAL local platform stack (scripts/local/web-e2e.sh): tenant pages as the smoke tenant's admin on its job,
// platform pages as the super admin. A detail page is the first item its list page links to (the stack's own data).
const job = () => `/admin/jobs/${journey().job}`;
const PAGES: { name: string; who: Who; url?: () => string; list?: () => string; prefix?: () => string }[] = [
	{ name: 'release', who: 'admin', url: () => `${job()}/release` },
	{ name: 'route-matrix-J6', who: 'admin', url: () => `${job()}/operations` },
	{ name: 'case-detail-J20', who: 'admin', list: () => `${job()}/cases`, prefix: () => `${job()}/cases/` },
	{ name: 'providers-T6', who: 'admin', url: () => '/admin/providers' },
	{
		name: 'journey-designer-P8',
		who: 'sa',
		list: () => '/platform/journeys',
		prefix: () => '/platform/journeys/',
	},
	{ name: 'compile-P5', who: 'sa', url: () => '/platform/compile' },
	// The general stack need not have run a generator. Audit its real empty/list controls here;
	// generator detail and its write sequence have dedicated B8 browser acceptance.
	{ name: 'generator-P18', who: 'sa', url: () => '/platform/generator' },
	{ name: 'tenant-detail-P2', who: 'sa', url: () => `/platform/tenants/${journey().tenant}` },
	{
		name: 'object-detail-P10',
		who: 'sa',
		list: () => '/platform/objects',
		prefix: () => '/platform/objects/',
	},
	{
		name: 'connector-template-P17',
		who: 'sa',
		list: () => '/platform/connector-templates',
		prefix: () => '/platform/connector-templates/',
	},
];

/** The page's URL: fixed, or the first detail link its list page shows. */
async function target(page: Page, p: (typeof PAGES)[number]): Promise<string> {
	if (p.url) return p.url();
	await open(page, p.list!());
	const href = await page
		.locator(`main a[href^="${p.prefix!()}"]`)
		.first()
		.getAttribute('href', { timeout: 10_000 })
		.catch(() => null);
	expect(href, `${p.list!()} links to no ${p.prefix!()}… item on this stack`).toBeTruthy();
	return href!;
}

async function open(page: Page, url: string) {
	await page.goto(url);
	await page.locator('main h1').first().waitFor();
	await page
		.getByTestId('load-loading')
		.first()
		.waitFor({ state: 'detached', timeout: 8000 })
		.catch(() => {});
	await page.waitForTimeout(500);
}
const snapshot = (page: Page) =>
	page.evaluate(() => {
		const parts = [document.querySelector('main')?.innerHTML ?? ''];
		document
			.querySelectorAll('dialog[open], [role=dialog], .toast, [role=alert]')
			.forEach((d) => parts.push(d.outerHTML));
		return parts.join('|');
	});

test.skip(!process.env.AUDIT, 'opt-in audit');
test.setTimeout(600_000);
for (const p of PAGES)
	test(`audit ${p.name}`, async ({ page }) => {
		const name = p.name;
		await signIn(page, p.who, '/');
		const url = await target(page, p);
		let dialogs = 0;
		page.on('dialog', (d) => {
			dialogs++;
			void d.dismiss();
		});
		await open(page, url);
		const buttons = page.locator('main button:visible:enabled');
		const labels = await buttons.evaluateAll((bs) =>
			bs.map((b) =>
				(b.getAttribute('aria-label') || b.textContent || '').trim().replace(/\s+/g, ' ').slice(0, 60),
			),
		);
		const suspects = await page.evaluate(() =>
			[...document.querySelectorAll('main *')]
				.filter((el) => {
					const s = getComputedStyle(el);
					return (
						s.cursor === 'pointer' &&
						!el.closest('a[href],button,label,summary,select,input,[role=button],[role=tab],[tabindex]')
					);
				})
				.map(
					(el) =>
						`${el.tagName.toLowerCase()}.${el.className}: ${(el.textContent || '').trim().slice(0, 40)}`,
				),
		);
		const rows: { i: number; label: string; effect: string }[] = [];
		// State-changing controls belong to the action journeys. This generic audit must not
		// suspend its own tenant, accept routes, or publish an incomplete offering.
		const mutation =
			/^(turn on|turn off|act in|reissue|suspend|resume|archive|delete|remove|offboard|approve|go live|publish|unpublish|accept|save|create|retry|rotate|revoke|reset|discard|restore|enable|disable|send|take|resolve|assign|release|run now|start run)\b/i;
		for (let i = 0; i < labels.length; i++) {
			if (mutation.test(labels[i]!)) {
				rows.push({ i, label: labels[i]!, effect: 'requires-action-journey' });
				continue;
			}
			await open(page, url);
			const occurrence = labels.slice(0, i).filter((label) => label === labels[i]).length;
			const candidates = page.locator('main button:visible:enabled');
			const matches = await candidates.evaluateAll(
				(bs, label) =>
					bs
						.map((b, n) => ({
							n,
							label: (b.getAttribute('aria-label') || b.textContent || '')
								.trim()
								.replace(/\s+/g, ' ')
								.slice(0, 60),
						}))
						.filter((b) => b.label === label)
						.map((b) => b.n),
				labels[i]!,
			);
			const index = matches[occurrence];
			const b = candidates.nth(index ?? -1);
			if (index === undefined || (await b.count()) === 0) {
				rows.push({ i, label: labels[i]!, effect: 'gone-after-reload' });
				continue;
			}
			const before = await snapshot(page);
			const u0 = page.url();
			const d0 = dialogs;
			await b
				.click({ timeout: 3000 })
				.catch((e) => rows.push({ i, label: labels[i]!, effect: `click-failed ${String(e).slice(0, 80)}` }));
			await page.waitForTimeout(700);
			const after = await snapshot(page);
			const effect =
				page.url() !== u0 ? 'url' : dialogs > d0 ? 'dialog' : after !== before ? 'dom' : 'NO EFFECT';
			rows.push({ i, label: labels[i]!, effect });
		}
		mkdirSync('test-results/audit', { recursive: true });
		writeFileSync(`test-results/audit/${name}.json`, JSON.stringify({ url, rows, suspects }, null, 1));
	});
