// D70 (owner, 2026-10-19): every GPU service is an external provider. A customer tenant picks, per capability, a
// provider, a model, a URL and a token (write-only); nonagentic's own (platform-owned) tenant runs on the platform's
// internal services. The knowledge model card shows the tenant's own embed/rerank binding, never a platform-fixed model.
// On the REAL local platform stack (scripts/local/web-e2e.sh): the customer tenant is the smoke's (its tenant admin),
// the platform's own tenant is the harness's platform tenant, which the super admin acts in.
import { expect, test, type Page } from '@playwright/test';
import { signIn as signInAs } from './stack';

/** The super admin acts in the platform's own tenant (the tenant picker's "Platform" entry). */
async function inPlatformTenant(page: Page, path: string) {
	await signInAs(page, 'sa', path);
	// the platform's own tenant (D70), not the tenant-less platform view
	await page
		.getByTestId('tenant-picker')
		.getByRole('button', { name: /^Platform 5b1a7c10/ })
		.click();
}

function watchConsole(page: Page): string[] {
	const errors: string[] = [];
	page.on('console', (m) => {
		if (m.type() === 'error') errors.push(m.text());
	});
	page.on('pageerror', (e) => errors.push(String(e)));
	return errors;
}

test('customer tenant: slot drawer has provider, model, URL and a write-only token', async ({ page }) => {
	const errors = watchConsole(page);
	await signInAs(page, 'admin', '/admin/providers?tab=ingest');
	const table = page.getByTestId('slots-strata_ingest');
	// an empty slot opens on the provider's suggested model and URL (one project run per tenant fills it)
	const empty = !(await table.locator('tr[data-slot="vision"]').innerText()).includes('local-llm');
	await table.getByRole('button', { name: 'Vision' }).click();
	const provider = page.getByRole('combobox', { name: 'Provider', exact: true });
	await expect(provider.locator('option')).toHaveText(['OpenAI-compatible']);
	if (empty) {
		await expect(page.getByRole('combobox', { name: 'Model', exact: true })).toHaveValue(
			'pixtral-large-latest',
		);
		await expect(page.getByRole('textbox', { name: 'URL' })).toHaveValue('https://api.mistral.ai/v1');
	}
	// the stack's own OpenAI-compatible endpoint (the fake LLM serves `local-llm`): Save runs the live test against it
	await page.getByRole('combobox', { name: 'Model', exact: true }).selectOption({ label: 'Other model…' });
	await page.getByRole('textbox', { name: 'Model name' }).fill('local-llm');
	await page.getByRole('textbox', { name: 'URL' }).fill(process.env.PS_E2E_LLM!);
	const token = page.getByLabel('Token');
	await expect(token).toHaveAttribute('type', 'password');
	await token.fill('sk-e2e-vision');
	await page.getByRole('button', { name: 'Save', exact: true }).click();
	const row = table.locator('tr[data-slot="vision"]');
	await expect(row).toContainText('local-llm');
	await expect(row).not.toContainText('sk-e2e-vision');
	await row.getByRole('button', { name: 'Vision' }).click();
	await expect(page.getByLabel('Token')).toHaveValue('');
	await expect(page.getByLabel('Token')).toHaveAttribute(
		'placeholder',
		/Leave empty to keep the stored token/,
	);
	expect(errors).toEqual([]);
});

test('customer tenant: the model card shows its own embed/rerank providers, never a platform-fixed model', async ({
	page,
}) => {
	const errors = watchConsole(page);
	await signInAs(page, 'admin', '/admin/knowledge/stores');
	const card = page.getByTestId('t7-models');
	await expect(card).toBeVisible();
	await expect(card).not.toContainText('(internal)');
	await expect(page.getByText(/fixed by the platform/)).toHaveCount(0);
	expect(errors).toEqual([]);
});

test('the platform’s own tenant: LLM on the pgx vLLM, search on mlx_api', async ({ page }) => {
	const errors = watchConsole(page);
	await inPlatformTenant(page, '/admin/providers');
	await expect(page.getByTestId('llm-internal')).toContainText('pgx vLLM');
	await expect(page.getByRole('button', { name: 'Add provider' })).toHaveCount(0);
	await expect(page.getByTestId('llm-providers')).toContainText('platform service key');
	// Client-side navigation keeps the tenant the super admin acts in (a reload starts on the persona's own).
	await page.evaluate(() => {
		const a = document.createElement('a');
		a.href = '/admin/knowledge/stores';
		document.body.appendChild(a);
		a.click();
	});
	await expect(page.getByTestId('t7-models')).toContainText('mlx_api (internal)');
	await expect(page.getByTestId('t7-models-note')).toContainText('mlx_api');
	expect(errors).toEqual([]);
});

test('the platform’s own tenant: the slot drawer is read-only (no Save, no other model)', async ({
	page,
}) => {
	const errors = watchConsole(page);
	await inPlatformTenant(page, '/admin/providers');
	await expect(page.getByTestId('llm-internal')).toContainText('pgx vLLM');
	await page.getByRole('tab', { name: /Ingest/ }).click();
	const table = page.getByTestId('slots-strata_ingest');
	await table.getByRole('button', { name: 'Transcription' }).click();
	await expect(page.getByTestId('slot-platform-managed')).toContainText('mlx_api');
	const model = page.getByRole('combobox', { name: 'Model', exact: true });
	await expect(model).toBeDisabled();
	// the platform's one model for the slot, fixed
	await expect(model.locator('option')).toHaveCount(1);
	await expect(page.getByRole('combobox', { name: 'Provider', exact: true })).toBeDisabled();
	await expect(page.getByRole('button', { name: 'Save', exact: true })).toHaveCount(0);
	expect(errors).toEqual([]);
});
