// B1 T21 (D79 view-as, D88 roles), on desktop and phone, on the REAL local platform stack (scripts/local/web-e2e.sh):
// the smoke tenant's tenant admin holds every tenant role; from the avatar menu ("View as…") they narrow to Support
// agent. The banner names the view; design pages turn read-only (writes disabled with the missing role), handover
// actions stay usable, the Support manager's policy does not; Exit restores everything. The staff agent (Support agent
// only) is offered only that role. The view-as row is the API's (it survives a reload), so every test clears it after.
import { expect, test, type Page } from '@playwright/test';
import { Api, J, signIn, type Who } from './stack';

const JOB = () => `/admin/jobs/${J().job}`;

/** Sign `who` in on `path`. */
async function signInAs(page: Page, who: Who, path: string) {
	await signIn(page, who, path);
}

// a failed test must not leave anyone viewing as a narrower role for the specs after it
test.afterEach(async () => {
	for (const who of ['admin', 'agent', 'sa'] as const) {
		const a = await Api.as(who);
		await a.del('/api/me/view-as');
		await a.dispose();
	}
});

function watchConsole(page: Page): string[] {
	const errors: string[] = [];
	page.on('console', (m) => {
		if (m.type() === 'error') errors.push(m.text());
	});
	page.on('pageerror', (e) => errors.push(`${String(e)} @ ${page.url()}\n${e.stack ?? ''}`));
	page.on('response', (r) => {
		if (r.status() >= 400) errors.push(`${r.status()} ${r.request().method()} ${new URL(r.url()).pathname}`);
	});
	return errors;
}

/** Navigate (the view-as row is the server's: it holds across the load). */
async function go(page: Page, path: string) {
	await page.goto(path);
	await expect(page.locator('main h1').first()).toBeVisible();
}

/** The "View as…" entry: in the user menu on desktop (under Developer for a super admin), in the nav drawer on a phone. */
async function openViewAs(page: Page) {
	await expect(page.locator('main h1').first()).toBeVisible();
	const menu = page.getByTestId('user-menu');
	if (await menu.isVisible()) await menu.click();
	else await page.getByRole('button', { name: 'Open navigation' }).click();
	const open = page.getByTestId('view-as-open');
	const dev = page.getByTestId('developer-toggle');
	if (!(await open.isVisible()) && (await dev.count())) await dev.click();
	await open.click();
	await expect(page.getByTestId('view-as-panel')).toBeVisible();
}

test('view as Support agent: design read-only, handover usable; Exit restores', async ({ page }) => {
	const errors = watchConsole(page);
	await signInAs(page, 'admin', `${JOB()}/ontology`);
	const addEntity = page.getByRole('button', { name: 'Add entity' });
	await expect(addEntity).toBeEnabled();
	await expect(page.getByTestId('view-as-banner')).toHaveCount(0);

	await openViewAs(page);
	const panel = page.getByTestId('view-as-panel');
	const boxes = panel.getByRole('checkbox');
	await expect(boxes).toHaveCount(8);
	// the tenant admin holds every role, so every box starts ticked; keep only Support agent.
	for (const box of await boxes.all()) {
		const name = (await box.getAttribute('value')) ?? '';
		if (name !== 'support_agent' && (await box.isChecked())) await box.uncheck();
	}
	await panel.getByTestId('view-as-apply').click();
	await expect(panel).toBeHidden();

	const banner = page.getByTestId('view-as-banner');
	await expect(banner).toContainText('Viewing as: Support agent');
	await expect(banner.getByRole('button', { name: 'Exit' })).toBeVisible();

	// Design pages: read-only, with the role the write needs.
	await expect(addEntity).toBeDisabled();
	await expect(addEntity).toHaveAttribute('title', 'Needs the Job designer role');
	await go(page, `${JOB()}/rules`);
	await expect(page.getByText(/Setting values or passages: needs the job designer role/i)).toBeVisible();

	// Handover: the queue itself has no work controls (that is the workspace); the SLA policy is the Support manager's.
	await go(page, `${JOB()}/handover`);
	await expect(page.getByTestId('take-next')).toHaveCount(0);
	await expect(page.getByTestId('queue-roles-note')).toHaveCount(0);
	await expect(page.getByTestId('save-policy')).toBeDisabled();
	await expect(page.getByTestId('save-policy')).toHaveAttribute('title', 'Needs the Support manager role');
	await expect(banner).toContainText('Viewing as: Support agent');

	// Exit: held roles back, banner gone, design writes enabled again.
	await banner.getByRole('button', { name: 'Exit' }).click();
	await expect(page.getByTestId('view-as-banner')).toHaveCount(0);
	await expect(page.getByTestId('save-policy')).toBeEnabled();
	await go(page, `${JOB()}/ontology`);
	await expect(page.getByRole('button', { name: 'Add entity' })).toBeEnabled();
	expect(errors).toEqual([]);
});

test('view-as offers only the roles held (the staff agent: Support agent)', async ({ page }) => {
	await signInAs(page, 'agent', '/admin/users');
	await openViewAs(page);
	const panel = page.getByTestId('view-as-panel');
	await expect(panel.getByRole('checkbox')).toHaveCount(8);
	await expect(panel.getByRole('checkbox', { name: /Support agent/ })).toBeEnabled();
	await expect(panel.getByRole('checkbox', { name: /Job designer/ })).toBeDisabled();
	await expect(panel.getByRole('checkbox', { name: /Tenant admin/ })).toBeDisabled();
});

test('view as: Exit stays reachable in the Developer submenu (super admin)', async ({ page }) => {
	test.skip((page.viewportSize()?.width ?? 0) <= 860, 'desktop menu');
	// a super admin acts in one tenant at a time: they pick the smoke's tenant first
	await signInAs(page, 'sa', `${JOB()}/ontology`);
	await page.getByRole('textbox', { name: 'Or enter a tenant id' }).fill(J().tenant);
	await page.getByRole('button', { name: 'Act in tenant' }).click();
	await expect(page.locator('main h1').first()).toBeVisible();
	await openViewAs(page);
	const panel = page.getByTestId('view-as-panel');
	await expect(panel.getByRole('checkbox')).toHaveCount(8);
	for (const box of await panel.getByRole('checkbox').all()) {
		if ((await box.getAttribute('value')) !== 'support_agent' && (await box.isChecked())) await box.uncheck();
	}
	await panel.getByTestId('view-as-apply').click();
	await expect(panel).toBeHidden();
	await expect(page.getByTestId('view-as-banner')).toContainText('Viewing as: Support agent');
	await page.getByTestId('user-menu').click();
	await page.getByTestId('developer-toggle').click();
	await page.getByTestId('view-as-leave').click();
	await expect(page.getByTestId('view-as-banner')).toHaveCount(0);
});
