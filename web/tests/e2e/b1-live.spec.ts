// B1 T18 on the REAL local platform stack (scripts/local/web-e2e.sh): the real boot — PKCE through the stack's IdP, the
// token, the live client against agent-api. The smoke tenant's admin sees the REAL members on /admin/users (no preview
// banner, no persona select); their write controls follow the live effective roles, and view-as goes over PUT/DELETE
// /api/me/view-as and survives a reload (the row is agent-api's). A super_admin reads the real tenant registry.
import { expect, test } from '@playwright/test';
import { J, clearViewAs, recordApi, signIn, viewAsOnly, watchConsole } from './stack';

test.afterEach(clearViewAs);

test('a tenant admin: live members, writes and view-as on the live roles', async ({ page }) => {
	const errors = watchConsole(page);
	const calls = recordApi(page);
	await signIn(page, 'admin', '/admin/users');

	// Real members from agent-api (the invited support agent among them), no preview banner, no persona select.
	await expect(page.getByText(J().agent.email).first()).toBeVisible();
	await expect(page.getByTestId('preview-banner')).toHaveCount(0);
	await expect(page.getByTestId('persona')).toHaveCount(0);
	expect(calls).toContain(`GET /api/tenants/${J().tenant}/users`);
	// The invite form is a tenant_admin write: open for the live roles.
	const email = page.getByRole('textbox', { name: 'E-mail' });
	await expect(email).toBeEnabled();

	// View as Support agent: PUT /api/me/view-as; the tenant_admin write turns disabled.
	await viewAsOnly(page, 'support_agent');
	await expect(page.getByTestId('view-as-banner')).toContainText('Support agent');
	expect(calls).toContain('PUT /api/me/view-as');
	await expect(email).toBeDisabled();

	// A reload keeps the view (the row lives on the server).
	await page.reload();
	await expect(page.getByTestId('view-as-banner')).toContainText('Support agent');
	await expect(email).toBeDisabled();

	// Exit: DELETE /api/me/view-as, the write is back.
	await page.getByTestId('view-as-exit').click();
	await expect(page.getByTestId('view-as-banner')).toHaveCount(0);
	expect(calls).toContain('DELETE /api/me/view-as');
	await expect(email).toBeEnabled();
	expect(errors).toEqual([]);
});

test('a super_admin: real tenants on the live registry, no persona select', async ({ page }) => {
	const errors = watchConsole(page);
	const calls = recordApi(page);
	await signIn(page, 'sa', '/platform/tenants');
	await expect(page.locator('main h1').first()).toBeVisible();
	await expect(page.getByTestId('persona')).toHaveCount(0);
	await expect(page.getByTestId('preview-banner')).toHaveCount(0);
	await expect.poll(() => calls).toContain('GET /api/tenants?include_archived=true');
	expect(calls).toContain('GET /api/tenants?view=portfolio&include_archived=true');
	// the smoke's tenant is in the registry the page reads
	await expect(page.locator(`a[href="/platform/tenants/${J().tenant}"]`).first()).toBeVisible();
	expect(errors).toEqual([]);
});
