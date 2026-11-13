// The platform administrator creates catalogue content through real controls, then tags/publishes it.
// PS_E2E_EMPTY_PLATFORM=1 additionally proves bootstrapping a platform with no layers or catalogue content.
import { randomUUID } from 'node:crypto';
import { expect, test } from '@playwright/test';
import { Api, ok, signIn, viaUi } from './stack';

test('platform authoring: create an object and operation, tag their layer, and publish a connector', async ({
	page,
}) => {
	const sa = await Api.as('sa');
	const errors: string[] = [];
	page.on('pageerror', (e) => errors.push(e.message));
	const objects = ok(await sa.get('/api/platform/objects'), 'read initial objects').objects;
	const empty = process.env.PS_E2E_EMPTY_PLATFORM === '1';
	if (empty) expect(objects).toEqual([]);
	const key = objects.some((o: { object: string }) => o.object === 'order')
		? `visit_${randomUUID().slice(0, 8)}`
		: 'order';
	const template = `console_${randomUUID().slice(0, 8)}`;
	await signIn(page, 'sa', '/platform/objects');
	if (empty) await expect(page.getByText('No objects in the catalogue yet.')).toBeVisible();
	await page.getByRole('button', { name: 'Create object', exact: true }).click();
	let drawer = page.getByRole('dialog', { name: 'Create object', exact: true });
	if (empty) {
		ok(
			await viaUi(page, 'POST', /^\/api\/layers$/, () =>
				drawer.getByRole('button', { name: 'Create base layer' }).click(),
			),
			'create base layer',
		);
	} else await drawer.getByLabel('Template layer').selectOption('base');
	await drawer.getByLabel('Object key').fill(key);
	await drawer.getByLabel('Label', { exact: true }).fill('Console authored order');
	expect(
		ok(
			await viaUi(page, 'POST', /^\/api\/platform\/objects$/, () =>
				drawer.getByRole('button', { name: 'Create object draft' }).click(),
			),
			'create object',
		).object,
	).toBe(key);
	await drawer.getByRole('link', { name: 'Edit fields and lifecycle' }).click();
	await page.getByRole('button', { name: 'Add field', exact: true }).click();
	const field = page.getByRole('dialog', { name: `Add a field to ${key}`, exact: true });
	await field.getByLabel('Field', { exact: true }).fill('status');
	await field.getByLabel('Type', { exact: true }).selectOption('string');
	ok(
		await viaUi(page, 'PUT', /\/fields\/status$/, () =>
			field.getByRole('button', { name: 'Save field' }).click(),
		),
		'save object field',
	);
	await expect(field).toBeHidden();

	await page.goto('/platform/operations');
	await page.getByRole('button', { name: 'Create operation', exact: true }).click();
	drawer = page.getByRole('dialog', { name: 'Create operation', exact: true });
	await drawer.getByLabel('Template layer').selectOption('base');
	await drawer.getByLabel('Object', { exact: true }).selectOption(key);
	await drawer.getByLabel('Verb', { exact: true }).fill('read');
	await expect(drawer.getByRole('button', { name: 'Create operation draft' })).toBeDisabled();
	await drawer.getByLabel('Minimum identity level').selectOption('2');
	if (key !== 'order') await drawer.getByLabel('Output fields (comma separated)').fill('status');
	const operation = ok(
		await viaUi(page, 'POST', /^\/api\/platform\/operations$/, () =>
			drawer.getByRole('button', { name: 'Create operation draft' }).click(),
		),
		'create operation',
	);
	expect(operation.key).toBe(`${key}.read`);
	expect(operation.output_fields).toContain('status');
	await drawer.getByRole('link', { name: 'tag the template layer' }).click();
	const base = page.locator('[data-layer="base"]');
	await base.getByRole('button', { name: /^Tag as/ }).click();
	const tagging = page.getByRole('dialog', { name: /^Tag base as/ });
	await tagging.getByLabel('Tag note').fill('Console authored catalogue');
	ok(
		await viaUi(page, 'POST', /\/api\/platform\/templates\/base\/tags$/, () =>
			tagging.getByRole('button', { name: /^Tag as/ }).click(),
		),
		'tag base',
	);
	await expect(tagging).toBeHidden();

	await page.goto('/platform/connector-templates');
	await page.getByRole('button', { name: 'Create connector template', exact: true }).click();
	drawer = page.getByRole('dialog', { name: 'Create connector template', exact: true });
	await drawer.getByLabel('Template key').fill(template);
	await drawer.getByLabel('Label', { exact: true }).fill('Console authored connector');
	ok(
		await viaUi(page, 'POST', /^\/api\/platform\/connector-templates$/, () =>
			drawer.getByRole('button', { name: 'Create connector template draft' }).click(),
		),
		'create connector template',
	);
	await drawer.getByRole('link', { name: 'Add operations, run contract tests and publish' }).click();
	await page.getByRole('button', { name: 'Add operation', exact: true }).click();
	const mapping = page.getByRole('dialog');
	await expect(mapping).toHaveAccessibleName('Add operation');
	await mapping.getByLabel('Object', { exact: true }).fill('order');
	await mapping.getByLabel('Verb', { exact: true }).fill('read');
	await mapping.getByLabel('Path', { exact: true }).fill('/orders/{{canonical.order_id}}');
	await mapping.getByLabel('System path 1', { exact: true }).fill('$.status');
	await mapping.getByLabel('Canonical field 1', { exact: true }).fill('status');
	await mapping.getByLabel('Recorded response fixtures (JSON array)').fill(
		JSON.stringify([
			{ name: 'ok', status: 200, outcome: 'ok', body: { status: 'processing' } },
			{ name: 'missing', status: 404, outcome: 'not_found', body: {} },
		]),
	);
	ok(
		await viaUi(page, 'PUT', /\/ops\/order.read$/, () =>
			mapping.getByRole('button', { name: 'Save operation' }).click(),
		),
		'save connector operation',
	);
	await expect(mapping).toBeHidden();
	const published = ok(
		await viaUi(page, 'POST', new RegExp(`/connector-templates/${template}/1/publish$`), () =>
			page.getByRole('button', { name: 'Publish @1', exact: true }).click(),
		),
		'publish connector template',
	);
	expect(published.status).toBe('active');
	expect(published.t1.status).toBe('green');
	expect(published.t1.note).toContain('2 fixtures');
	await expect(page.getByText('Immutable — active')).toBeVisible();
	expect(errors).toEqual([]);
	await page.screenshot({ path: test.info().outputPath('published-connector.png'), fullPage: true });
	await sa.dispose();
});
