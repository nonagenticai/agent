// B2 T17 (D80) on the REAL local platform stack (scripts/local/web-e2e.sh): the tenant admin of the smoke's tenant, on
// its published job. The Draft card of the release page lists the kinds still outside releases from the server's own
// `draftSummary.pending_kinds` (agent-api `design::PENDING`), the release page is live (no preview banner) and its Test
// card reads the live validation routes; a live B2 page (business rules) has no banner and reads over HTTP.
import { expect, test, type Page } from '@playwright/test';
import { Api, J, signIn } from './stack';

const job = () => J().job;

/** Every `/api` call the page makes, as "VERB /path". */
function recordApi(page: Page): string[] {
	const calls: string[] = [];
	page.on('request', (r) => {
		const u = new URL(r.url());
		if (u.pathname.startsWith('/api/')) calls.push(`${r.method()} ${u.pathname}`);
	});
	return calls;
}

test('the Draft card lists the kinds not yet part of releases; the live page has no banner and its Test card reads the live validation routes', async ({
	page,
}) => {
	const admin = await Api.as('admin');
	const draft = await admin.get(`/api/jobs/${job()}/draft`);
	await admin.dispose();
	expect(draft.status, JSON.stringify(draft.body).slice(0, 300)).toBe(200);
	const pendingKinds: string[] = draft.body?.pending_kinds ?? [];
	const calls = recordApi(page);
	await signIn(page, 'admin', `/admin/jobs/${job()}/release`);
	await expect(page.locator('main h1').first()).toBeVisible();
	await expect.poll(() => calls).toContain(`GET /api/jobs/${job()}/draft`);
	const pending = page.getByTestId('pending-kinds');
	if (pendingKinds.length) {
		// what the server says is still outside releases is what the card says, in words
		await expect(pending).toHaveText(/^Not yet part of releases: .+\.$/);
	} else {
		await expect(pending).toHaveCount(0);
	}
	await expect.poll(() => calls).toContain(`GET /api/jobs/${job()}/validation`);
	await expect.poll(() => calls).toContain(`GET /api/jobs/${job()}/validation/draft`);
	await expect(page.getByTestId('not-live-yet')).toHaveCount(0);
	await expect(page.getByTestId('preview-banner')).toHaveCount(0);
});

test('the tools page is live: it reads the job’s routes over HTTP with no sample-data banner', async ({
	page,
}) => {
	const calls = recordApi(page);
	await signIn(page, 'admin', `/admin/jobs/${job()}/tools`);
	await expect(page.locator('main h1').first()).toBeVisible();
	await expect.poll(() => calls.some((c) => c.startsWith(`GET /api/jobs/${job()}/`))).toBe(true);
	await expect(page.getByTestId('preview-banner')).toHaveCount(0);
	await expect(page.getByTestId('preview-banner-release')).toHaveCount(0);
});

test('a live B2 page (business rules) reads over HTTP with no preview banner', async ({ page }) => {
	const calls = recordApi(page);
	await signIn(page, 'admin', `/admin/jobs/${job()}/rules`);
	await expect(page.locator('main h1').first()).toBeVisible();
	await expect.poll(() => calls).toContain(`GET /api/jobs/${job()}/rules`);
	await expect(page.getByTestId('preview-banner')).toHaveCount(0);
});
