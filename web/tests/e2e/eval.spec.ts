// Retrieval eval (T7) against strata_rag's tenant eval contract (PR #21), on desktop and phone, on the REAL local
// platform stack (scripts/local/web-e2e.sh): agent-api proxies the stack's strata (b8-fake-strata + ps-fake-eval.mjs),
// which scores over its own corpus with its search ranking. A question needs at least one expectation (document, text
// or ids; the leaf is optional), edits are PATCHes, a run shows its verdict and per-question ranks, and a set strata_rag
// cannot run is answered 409 `eval_not_ready` with every missing item listed (agent-api passes the lines as
// `problems`). Runs are async (202 + poll every 2 s): progress shows while one runs, it survives leaving the page, and
// a failed run shows its error. Every test starts from the same golden set.
import { expect, test, type Page } from '@playwright/test';
import { Api, signIn } from './stack';

/** The golden set each test starts from: four the corpus answers, one it cannot (a miss in every mode). */
const SET = [
	{ query: 'How long does a refund take?', expected_document: 'Returns and refunds' },
	{ query: 'When will my parcel arrive?', expected_document: 'Delivery times and tracking' },
	{ query: 'My item arrived broken, what now?', expected_text: 'add a photo in the chat' },
	{ query: 'Can I get an invoice for my card payment?', expected_document: 'payments.md' },
	{ query: 'Do you sell gift vouchers?', expected_document: 'Cancelling or changing an order' },
];
const MODES = 3; // strata_rag's default modes: embed, hybrid, flat

async function noRunning(api: Api) {
	await expect
		.poll(
			async () => {
				const r = await api.get('/api/knowledge/eval/runs?limit=5');
				return (r.body as { items?: { status: string }[] })?.items?.some((x) => x.status === 'running');
			},
			{ timeout: 30_000 },
		)
		.toBe(false);
}

test.beforeEach(async () => {
	const api = await Api.as('admin');
	await noRunning(api);
	const qs = await api.get('/api/knowledge/eval/questions');
	expect(qs.status, JSON.stringify(qs.body).slice(0, 300)).toBe(200);
	for (const q of (qs.body as { items: { id: string }[] }).items)
		expect((await api.del(`/api/knowledge/eval/questions/${q.id}`)).status).toBe(204);
	const imp = await api.post('/api/knowledge/eval/questions/import', { questions: SET });
	expect(imp.status, JSON.stringify(imp.body).slice(0, 300)).toBe(201);
	await api.dispose();
});

function watchConsole(page: Page): string[] {
	const errors: string[] = [];
	page.on('console', (m) => {
		if (m.type() === 'error') errors.push(m.text());
	});
	page.on('pageerror', (e) => errors.push(String(e)));
	return errors;
}

const noSideScroll = (page: Page) =>
	page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth + 1);

const scored = (n: number) => new RegExp(`of ${n * MODES} scored \\(${n} questions × ${MODES} modes\\)$`);

async function openEval(page: Page) {
	await signIn(page, 'admin', '/admin/knowledge/eval');
	await expect(page.getByTestId('ev-questions')).toContainText(SET[0]!.query);
}

test('eval: verdict and per-question ranks; a text-only question is added and scored', async ({ page }) => {
	const errors = watchConsole(page);
	await openEval(page);
	await page.getByRole('button', { name: 'Run eval' }).click();
	await expect(page.getByTestId('ev-progress')).toHaveText(scored(SET.length));
	await expect(page.getByTestId('ev-run')).toBeDisabled();
	await expect(page.getByTestId('ev-run')).toHaveAttribute('title', /one eval per tenant at a time/);
	await expect(page.getByTestId('ev-running')).toHaveCount(0, { timeout: 20_000 });
	await expect(page.getByTestId('ev-verdict')).toHaveText(/^(PASS|FAIL)$/);
	await expect(page.getByTestId('ev-reports')).toContainText('Flat (no routing)');
	const perQ = page.getByTestId('ev-run-questions');
	// the voucher question has no answer in the corpus: a miss in every mode
	await expect(perQ).toContainText(new RegExp(`1 of ${SET.length} missed in`));
	await perQ.locator('summary').click();
	await expect(perQ.getByRole('cell', { name: 'How long does a refund take?' })).toBeVisible();

	await page.getByRole('button', { name: 'Add question' }).click();
	const form = page.getByTestId('ev-form');
	await form.getByPlaceholder('How long does a refund take?').fill('Do you deliver on Saturday?');
	await expect(page.getByTestId('ev-form-problems')).toContainText('no expectation');
	await expect(form.getByRole('button', { name: 'Add to golden set' })).toBeDisabled();
	await form.getByPlaceholder('Words a good passage contains').fill('express delivery');
	await form.getByRole('button', { name: 'Add to golden set' }).click();
	await expect(page.getByTestId('ev-questions')).toContainText('Do you deliver on Saturday?');
	await page.getByRole('button', { name: 'Run eval' }).click();
	await expect(page.getByTestId('ev-progress')).toHaveText(scored(SET.length + 1));
	expect(await noSideScroll(page)).toBe(true);
	await expect(page.getByText(new RegExp(`· ${SET.length + 1} questions ·`))).toBeVisible({
		timeout: 20_000,
	});
	await expect(page.getByTestId('ev-running')).toHaveCount(0);
	await expect(page.getByTestId('ev-run')).toBeEnabled();
	expect(await noSideScroll(page)).toBe(true);
	expect(errors).toEqual([]);
});

test('eval: a running run stays visible after leaving the page and completes on return', async ({ page }) => {
	const errors = watchConsole(page);
	await openEval(page);
	await page.getByRole('button', { name: 'Run eval' }).click();
	await expect(page.getByTestId('ev-progress')).toHaveText(scored(SET.length));
	// Client-side navigation away and back: strata_rag keeps the run going.
	await page.locator('a[href="/admin/knowledge/stores#t7-playground"]').first().click();
	await expect(page).toHaveURL(/\/admin\/knowledge\/stores/);
	await page.goBack();
	await expect(page).toHaveURL(/\/admin\/knowledge\/eval/);
	await expect(page.getByTestId('ev-running')).toBeVisible();
	await expect(page.getByTestId('ev-run')).toBeDisabled();
	expect(await noSideScroll(page)).toBe(true);
	await expect(page.getByTestId('ev-running')).toHaveCount(0, { timeout: 20_000 });
	await expect(page.getByText(new RegExp(`· ${SET.length} questions ·`))).toBeVisible();
	expect(errors).toEqual([]);
});

test('eval: a run whose work fails shows the error and the next run may start', async ({ page, request }) => {
	const errors = watchConsole(page);
	await openEval(page);
	// the stack's strata stops its next run half-way (ps-fake-eval.mjs test hook)
	const hook = await request.post(`${process.env.PS_E2E_STRATA}/__eval/fail-next`, {
		data: { error: 'embed provider error' },
	});
	expect(hook.status()).toBe(204);
	await page.getByRole('button', { name: 'Run eval' }).click();
	await expect(page.getByTestId('ev-running')).toBeVisible();
	const failed = page.getByTestId('ev-failed');
	await expect(failed).toContainText('The last eval failed: embed provider error', { timeout: 20_000 });
	const total = SET.length * MODES;
	await expect(failed).toContainText(
		`stopped after ${Math.floor(total / 2)} of ${total} scored (${SET.length} questions × ${MODES} modes)`,
	);
	await expect(page.getByTestId('ev-run')).toBeEnabled();
	expect(await noSideScroll(page)).toBe(true);
	expect(errors).toEqual([]);
});

test('eval: a set strata_rag cannot run lists every missing leaf and document', async ({ page }) => {
	const errors = watchConsole(page);
	await openEval(page);
	const qs = page.getByTestId('ev-questions');
	await qs.getByRole('button', { name: 'Edit How long does a refund take?' }).click();
	const form = page.getByTestId('ev-form');
	await form.getByPlaceholder('Document title').fill('Not a document');
	await form.getByRole('button', { name: 'Save' }).click();
	await expect(qs.getByRole('row', { name: /How long does a refund take\?/ })).toContainText(
		'Not a document',
	);
	await page.getByRole('button', { name: 'Run eval' }).click();
	const box = page.getByTestId('ev-not-ready');
	await expect(box).toContainText('question 1: no document `Not a document` in the evaluated tree');
	expect(await noSideScroll(page)).toBe(true);
	// The 409 is reported as a toast too; it is a refusal, not a console error.
	expect(errors.filter((e) => !/409/.test(e))).toEqual([]);
});
