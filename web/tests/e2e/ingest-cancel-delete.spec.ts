// T8 ingest job cancel + delete (strata_ingest#11) on the REAL local platform stack (scripts/local/web-e2e.sh): a
// running job is cancelled cooperatively (cancelling until the worker finishes its step), only an ended job can be
// deleted, and the confirm says indexed documents stay. The jobs live in the stack's strata_ingest (kc-fake-strata),
// seeded on the smoke tenant's own store through its test hook; agent-api proxies every read and write.
import { expect, test, type Page } from '@playwright/test';
import { Api, J, signIn } from './stack';

const STEPS_RUNNING = [
	{ kind: 'fetch', status: 'completed' },
	{ kind: 'ocr_pages', status: 'running', provider: 'openai_compatible', model: 'local-llm' },
	{ kind: 'chunk', status: 'pending' },
	{ kind: 'embed', status: 'pending' },
	{ kind: 'index', status: 'pending' },
];
const STEPS_DONE = STEPS_RUNNING.map((s) => ({ ...s, status: 'completed' }));

/** The strata store behind the smoke tenant's first knowledge store. */
async function strataStore(): Promise<string> {
	const api = await Api.as('admin');
	const r = await api.get('/api/knowledge/stores');
	await api.dispose();
	expect(r.status, JSON.stringify(r.body).slice(0, 300)).toBe(200);
	const id = (r.body as { items: { strata_store_id: string | null }[] }).items.find(
		(s) => s.strata_store_id,
	)?.strata_store_id;
	expect(id, 'the smoke tenant has a store bound in strata').toBeTruthy();
	return id!;
}

async function seed(
	page: Page,
	job: { filename: string; status: string; steps: object[]; delete_fails_once?: boolean },
): Promise<string> {
	const r = await page.request.post(`${process.env.PS_E2E_INGEST}/__ingest/seed`, {
		data: { tenant: J().tenant, store_id: await strataStore(), document_ids: [crypto.randomUUID()], ...job },
	});
	expect(r.status()).toBe(201);
	return ((await r.json()) as { id: string }).id;
}

function watchConsole(page: Page): string[] {
	const errors: string[] = [];
	page.on('console', (m) => {
		if (m.type() === 'error') errors.push(m.text());
	});
	page.on('pageerror', (e) => errors.push(String(e)));
	return errors;
}

test('tenant_admin: cancel a running job (cancelling → cancelled), then delete it', async ({ page }) => {
	const errors = watchConsole(page);
	const other = await seed(page, { filename: 'delivery-zones.pdf', status: 'completed', steps: STEPS_DONE });
	const id = await seed(page, { filename: 'size-guide.pdf', status: 'running', steps: STEPS_RUNNING });
	await signIn(page, 'admin', `/admin/knowledge/jobs?id=${id}`);
	const drawer = page.getByRole('dialog');
	await expect(drawer.getByRole('list', { name: 'Steps by stage' })).toBeVisible();
	await expect(drawer.getByRole('button', { name: 'Delete' })).toBeDisabled();
	await drawer.getByRole('button', { name: 'Cancel job' }).click();
	const ask = drawer.getByRole('alertdialog', { name: 'Cancel job' });
	await expect(ask).toContainText('finishes and its result is kept');
	await ask.getByRole('button', { name: 'Cancel job' }).click();
	await expect(drawer.getByTestId('ingest-cancelling')).toBeVisible();
	await expect(drawer.getByRole('button', { name: 'Cancelling…' })).toBeDisabled();
	await expect(drawer.getByTestId('ingest-cancelling')).toBeHidden({ timeout: 10_000 });
	const del = drawer.getByRole('button', { name: 'Delete' });
	await expect(del).toBeEnabled();
	await del.click();
	const confirm = drawer.getByRole('alertdialog', { name: 'Delete job' });
	await expect(confirm).toContainText('Documents it already indexed stay');
	await confirm.getByRole('button', { name: 'Delete job' }).click();
	await expect(page.locator(`tr[data-job="${id}"]`)).toHaveCount(0);
	await expect(page.locator(`tr[data-job="${other}"]`)).toHaveCount(1);
	expect(errors).toEqual([]);
});

test('a member without the Knowledge manager role: cancel and delete stay visible but disabled', async ({
	page,
}) => {
	const id = await seed(page, { filename: 'size-guide.pdf', status: 'running', steps: STEPS_RUNNING });
	await signIn(page, 'agent', `/admin/knowledge/jobs?id=${id}`);
	const drawer = page.getByRole('dialog');
	for (const name of ['Cancel job', 'Delete']) {
		const b = drawer.getByRole('button', { name, exact: true });
		await expect(b).toBeDisabled();
		await expect(b).toHaveAttribute('title', 'Needs the Knowledge manager role');
	}
});

test('tenant_admin: a delete that cannot remove a staged file stays deleting; Retry delete finishes it', async ({
	page,
}) => {
	const errors = watchConsole(page);
	const other = await seed(page, { filename: 'crawl.html', status: 'completed', steps: STEPS_DONE });
	const id = await seed(page, {
		filename: 'old-faq.pdf',
		status: 'completed',
		steps: STEPS_DONE,
		delete_fails_once: true,
	});
	await signIn(page, 'admin', `/admin/knowledge/jobs?id=${id}`);
	const drawer = page.getByRole('dialog');
	await expect(drawer.getByRole('list', { name: 'Steps by stage' })).toBeVisible();
	await drawer.getByRole('button', { name: 'Delete', exact: true }).click();
	await drawer
		.getByRole('alertdialog', { name: 'Delete job' })
		.getByRole('button', { name: 'Delete job' })
		.click();
	// strata_ingest's object storage fails once: 502 upstream_unavailable, the job stays `deleting`.
	const left = drawer.getByTestId('ingest-delete-unfinished');
	await expect(left).toContainText('upstream_unavailable');
	await expect(left).toContainText('stays deleting');
	for (const name of ['Retry', 'Cancel job']) {
		await expect(drawer.getByRole('button', { name, exact: true })).toBeDisabled();
	}
	await page.keyboard.press('Escape');
	const status = page.getByRole('combobox', { name: 'Status' });
	await status.selectOption('deleting');
	await expect(page.locator(`tr[data-job="${id}"]`)).toHaveCount(1);
	await expect(page.locator(`tr[data-job="${other}"]`)).toHaveCount(0);
	await page.locator(`tr[data-job="${id}"]`).getByRole('button').click();
	await drawer.getByRole('button', { name: 'Retry delete' }).click();
	await expect(page.locator(`tr[data-job="${id}"]`)).toHaveCount(0);
	await status.selectOption('');
	await expect(page.locator(`tr[data-job="${other}"]`)).toHaveCount(1);
	await expect(page.locator(`tr[data-job="${id}"]`)).toHaveCount(0);
	// The 502 is an expected API answer, not a page error.
	expect(errors.filter((e) => !/502|upstream_unavailable/.test(e))).toEqual([]);
});
