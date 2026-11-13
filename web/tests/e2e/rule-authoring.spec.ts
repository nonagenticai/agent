// An arbitrary tenant-authored condition is saved through the console and evaluated by the real backend.
import { randomUUID } from 'node:crypto';
import { expect, test } from '@playwright/test';
import { J, ok, signIn, viaUi } from './stack';

test('tenant authors, tries, edits and removes a business rule', async ({ page }) => {
	const id = `CREDIT-${randomUUID().slice(0, 8).toUpperCase()}`;
	const errors: string[] = [];
	page.on('pageerror', (e) => errors.push(e.message));
	await signIn(page, 'admin', `/admin/jobs/${J().job}/rules`);
	await page.getByRole('button', { name: 'Create business rule' }).click();
	const drawer = page.getByRole('dialog', { name: 'Create business rule' });
	await drawer.getByLabel('Rule id', { exact: true }).fill(id);
	await drawer.getByLabel('Label', { exact: true }).fill('Console service credit');
	await drawer.getByLabel('Guarded operations').fill('case.create');
	await drawer.getByRole('button', { name: 'Add fact', exact: true }).click();
	await drawer.getByLabel('Fact 1 key').fill('order.total');
	await drawer.getByLabel('Fact 1 label').fill('Order total');
	await drawer.getByLabel('Fact 1 type').selectOption('number');
	await drawer.getByLabel('Condition fact', { exact: true }).selectOption('order.total');
	await drawer.getByLabel('Condition comparison').selectOption('lte');
	await drawer.getByLabel('Condition value', { exact: true }).fill('100');
	await drawer.getByLabel('Eligible reason', { exact: true }).fill('Within the service credit policy.');
	await drawer.getByLabel('Ineligible reason').fill('Above the service credit limit.');
	const created = ok(
		await viaUi(page, 'POST', /\/rules$/, () =>
			drawer.getByRole('button', { name: 'Save rule draft' }).click(),
		),
		'create arbitrary rule',
	);
	expect(created.tenant_authored).toBe(true);
	expect(created.evaluation.condition.op).toBe('lte');
	const card = page.locator(`[data-rule="${id}"]`);
	await expect(card).toBeVisible();
	const run = async (value: string, outcome: string) => {
		await card.getByLabel('Order total order.total').fill(value);
		const result = ok(
			await viaUi(page, 'POST', new RegExp(`/rules/${id}/evaluate$`), () =>
				card.getByRole('button', { name: 'Evaluate', exact: true }).click(),
			),
			'try rule',
		);
		expect(result.outcome).toBe(outcome);
		await expect(card.getByTestId('rule-try-result')).toContainText(
			outcome === 'eligible' ? 'Within the service credit policy.' : 'Above the service credit limit.',
		);
	};
	await run('75', 'eligible');
	await run('125', 'ineligible');
	await card.getByRole('button', { name: 'Edit definition' }).click();
	const editor = page.getByRole('dialog', { name: `Edit rule ${id}` });
	await expect(editor.getByLabel('Rule id')).toBeDisabled();
	await editor.getByLabel('Condition value', { exact: true }).fill('50');
	ok(
		await viaUi(page, 'PUT', new RegExp(`/rules/${id}/definition$`), () =>
			editor.getByRole('button', { name: 'Save rule draft' }).click(),
		),
		'edit definition',
	);
	await expect(editor).toBeHidden();
	await run('75', 'ineligible');
	await page.screenshot({ path: test.info().outputPath('authored-rule.png'), fullPage: true });
	await card.getByRole('button', { name: 'Edit definition' }).click();
	await editor.getByRole('button', { name: 'Remove rule', exact: true }).click();
	ok(
		await viaUi(page, 'DELETE', new RegExp(`/rules/${id}$`), () =>
			editor.getByRole('button', { name: 'Confirm removal' }).click(),
		),
		'remove rule',
	);
	await expect(card).toHaveCount(0);
	expect(errors).toEqual([]);
});
