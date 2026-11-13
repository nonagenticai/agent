// SW-6 acceptance E2E: the staff workspace on a phone (390x844) and a tablet (768x1024), on the REAL local platform
// stack (scripts/local/web-e2e.sh): the staff agent the tenant admin invited, and real web chat customers who asked for
// a person (so their conversations are Unassigned in the staff queue).
// Phone: one pane at a time, driven by the URL (inbox -> conversation -> customer sheet), "Back to inbox", the browser's
// Back, the draft kept across all of it. Tablet: inbox | conversation side by side, the customer as a drawer.
// Screenshots go to SW6_SHOTS (default: the system temp dir) for the visual pass.
import { expect, test, type Page } from '@playwright/test';
import { mkdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { J, queuedConversation, signIn as signInAs } from './stack';

const JOB = () => J().job;
let next = 3;
/** A new conversation in the staff queue (nobody has it): a known customer asked for a person. */
const queued = async () => (await queuedConversation(J().customers[next++ % J().customers.length]!)).id;
const SHOTS = process.env.SW6_SHOTS ?? join(tmpdir(), 'sw6');
mkdirSync(SHOTS, { recursive: true });

// Viewports are set per describe; the mobile-emulation project would only repeat the phone run.
test.skip(
	({ isMobile }) => isMobile,
	'sets its own viewports; the mobile-emulation project would only repeat the phone run',
);

/** The staff agent signs in on `path` (the inbox opens on Mine: `unassigned` switches to the queue). */
async function signIn(page: Page, path = '/support', unassigned = false) {
	await signInAs(page, 'agent', path);
	if (unassigned)
		await inbox(page)
			.getByRole('tab', { name: /^Unassigned/ })
			.click();
}

const bar = (page: Page) => page.getByRole('toolbar', { name: 'Conversation actions' });
const composer = (page: Page) => page.getByLabel('Reply to customer');
const inbox = (page: Page) => page.getByRole('region', { name: 'Inbox' });
const row = (page: Page, id: string) => page.locator(`a[data-conversation="${id}"]`);
const chip = (page: Page) => page.locator('.conv-col').getByTestId('owner-chip');

/** No horizontal page scroll. */
async function noOverflow(page: Page, where: string) {
	const w = await page.evaluate(() => ({
		scroll: document.scrollingElement!.scrollWidth,
		inner: window.innerWidth,
	}));
	expect(w.scroll, `horizontal overflow ${where}`).toBeLessThanOrEqual(w.inner);
}
async function shot(page: Page, name: string) {
	await page.screenshot({ path: join(SHOTS, `${name}.png`) });
}
/** Every named control is at least 44 x 44 px. */
async function touchTargets(
	page: Page,
	root: ReturnType<Page['locator']>,
	names: (string | RegExp)[],
	where: string,
) {
	for (const name of names) {
		const box = await root.getByRole('button', { name }).first().boundingBox();
		expect(box, `${where}: ${name}`).not.toBeNull();
		expect(box!.height, `${where}: ${name} height`).toBeGreaterThanOrEqual(43.5);
		expect(box!.width, `${where}: ${name} width`).toBeGreaterThanOrEqual(43.5);
	}
}
async function take(page: Page) {
	await expect(chip(page)).toHaveText(/Unassigned|Agent/);
	const btn = bar(page).getByRole('button', { name: 'Take' });
	await expect(btn).toBeEnabled();
	await btn.click();
	await expect(chip(page)).toHaveText(/You/);
	await expect(composer(page)).toBeEnabled();
}

test.describe('phone 390x844', () => {
	test.use({ viewport: { width: 390, height: 844 } });

	test('inbox -> conversation -> customer sheet -> back -> reopen: the draft is still there', async ({
		page,
	}) => {
		const WA = await queued();
		await signIn(page, '/support', true);
		await expect(inbox(page)).toBeVisible();
		await expect(row(page, WA)).toBeVisible();
		await expect(page.getByTestId('transcript')).toBeHidden();
		await expect(page.getByRole('button', { name: 'Back to inbox' })).toHaveCount(0);
		await noOverflow(page, 'inbox');
		await shot(page, 'phone-1-inbox');

		// select: the URL is the conversation's
		await row(page, WA).click();
		await expect(page).toHaveURL(new RegExp(`/support/${JOB()}/c/${WA}$`));
		await expect(page.getByTestId('transcript')).toBeVisible();
		await expect(inbox(page)).toBeHidden();
		const back = bar(page).getByRole('button', { name: 'Back to inbox' });
		await expect(back).toBeVisible();
		// top-left of the header, a 44 px target
		const [bb, barBox] = [await back.boundingBox(), await bar(page).boundingBox()];
		expect(bb!.x - barBox!.x).toBeLessThan(24);
		expect(bb!.y - barBox!.y).toBeLessThan(24);
		await take(page);
		await touchTargets(page, bar(page), ['Back to inbox', 'Customer', 'Resolve', 'More actions'], 'header');
		await touchTargets(page, page.locator('.cmp'), ['Reply', 'Note', 'Send'], 'composer');
		await noOverflow(page, 'conversation');

		// the composer and the newest message are on screen, the header stays put while the transcript scrolls
		await composer(page).fill('Let me look into that order for you');
		await expect(composer(page)).toBeInViewport({ ratio: 1 });
		await expect(page.getByRole('button', { name: 'Send' })).toBeInViewport({ ratio: 1 });
		const barY = (await bar(page).boundingBox())!.y;
		await page.getByTestId('transcript').evaluate((el) => (el.scrollTop = 0));
		expect((await bar(page).boundingBox())!.y).toBe(barY);
		await shot(page, 'phone-2-conversation-draft');

		// the customer: a full-screen sheet, labelled, with its own back
		const customer = bar(page).getByRole('button', { name: 'Customer' });
		await customer.click();
		const sheet = page.getByRole('dialog', { name: 'Customer' });
		await expect(sheet).toBeVisible();
		await expect(sheet).toHaveAttribute('aria-modal', 'true');
		const sb = (await sheet.boundingBox())!;
		expect(sb.width).toBeGreaterThanOrEqual(389);
		expect(sb.height).toBeGreaterThanOrEqual(843);
		await expect(page.getByLabel('Context', { exact: true })).toBeVisible();
		await expect(sheet.getByText('Loading…')).toHaveCount(0);
		await touchTargets(page, sheet, ['Back to conversation'], 'sheet');
		await noOverflow(page, 'customer sheet');
		await shot(page, 'phone-3-customer-sheet');
		await page.getByRole('button', { name: 'Back to conversation' }).click();
		await expect(sheet).toBeHidden();
		await expect(customer).toBeFocused();
		await expect(page).toHaveURL(new RegExp(`/support/${JOB()}/c/${WA}$`));
		await expect(composer(page)).toHaveValue('Let me look into that order for you');

		// the browser's Back closes the sheet too (it is a history entry)
		await customer.click();
		await expect(sheet).toBeVisible();
		await page.goBack();
		await expect(sheet).toBeHidden();
		await expect(page).toHaveURL(new RegExp(`/support/${JOB()}/c/${WA}$`));
		await expect(composer(page)).toHaveValue('Let me look into that order for you');

		// back to the list: selection kept, then the same conversation again with its draft
		await back.click();
		await expect(inbox(page)).toBeVisible();
		await expect(page).toHaveURL(/\/support$/);
		await expect(page.getByTestId('transcript')).toBeHidden();
		// WA is Mine now (taken): the Mine tab lists it, still marked as the one just left
		await inbox(page).getByRole('tab', { name: /Mine/ }).click();
		await expect(row(page, WA)).toHaveAttribute('aria-current', 'true');
		await noOverflow(page, 'inbox again');
		await shot(page, 'phone-4-inbox-selection-kept');
		await row(page, WA).click();
		await expect(composer(page)).toHaveValue('Let me look into that order for you');
		await noOverflow(page, 'conversation again');

		// and the browser's Back from the conversation returns to the list the same way
		await page.goBack();
		await expect(inbox(page)).toBeVisible();
		await inbox(page).getByRole('tab', { name: /Mine/ }).click();
		await expect(row(page, WA)).toHaveAttribute('aria-current', 'true');
	});

	test('a conversation opened by its link: Back to inbox goes to the list', async ({ page }) => {
		const WA = await queued();
		await signIn(page, `/support/${JOB()}/c/${WA}`);
		await expect(page.getByTestId('transcript')).toBeVisible();
		await bar(page).getByRole('button', { name: 'Back to inbox' }).click();
		await expect(page).toHaveURL(/\/support$/);
		await expect(inbox(page)).toBeVisible();
		await inbox(page)
			.getByRole('tab', { name: /^Unassigned/ })
			.click();
		await expect(row(page, WA)).toHaveAttribute('aria-current', 'true');
	});

	test('the inbox keeps its scroll position when you come back from a conversation', async ({ page }) => {
		// enough waiting conversations that the short phone's list scrolls
		const ids: string[] = [];
		for (let i = 0; i < 6; i++) ids.push(await queued());
		await page.setViewportSize({ width: 390, height: 470 });
		await signIn(page, '/support', true);
		const list = inbox(page).locator('.list');
		await expect(row(page, ids[ids.length - 1]!)).toBeVisible();
		const scrollable = await list.evaluate((el) => el.scrollHeight - el.clientHeight);
		expect(scrollable, 'the short phone makes the list scroll').toBeGreaterThan(40);
		await list.evaluate((el) => (el.scrollTop = el.scrollHeight));
		const top = await list.evaluate((el) => el.scrollTop);
		expect(top).toBeGreaterThan(0);
		const lastRow = list.locator('a[data-conversation]').last();
		await lastRow.click();
		await expect(page.getByTestId('transcript')).toBeVisible();
		await bar(page).getByRole('button', { name: 'Back to inbox' }).click();
		await expect(inbox(page)).toBeVisible();
		expect(await list.evaluate((el) => el.scrollTop)).toBe(top);
		// selection kept, and the keyboard is back on the row just left
		await expect(lastRow).toHaveAttribute('aria-current', 'true');
		await expect(lastRow).toBeFocused();
		await noOverflow(page, 'short inbox');
	});
});

test.describe('tablet 768x1024', () => {
	test.use({ viewport: { width: 768, height: 1024 } });

	test('inbox and conversation side by side; the customer is a drawer; the draft is kept', async ({
		page,
	}) => {
		const WA = await queued();
		const OTHER = await queued();
		await signIn(page, '/support', true);
		await expect(row(page, WA)).toBeVisible();
		await row(page, WA).click();
		await expect(page.getByTestId('transcript')).toBeVisible();
		// both panes at once, no back button
		await expect(inbox(page)).toBeVisible();
		await expect(page.getByRole('button', { name: 'Back to inbox' })).toHaveCount(0);
		await take(page);
		await composer(page).fill('A draft on the tablet');
		await touchTargets(page, bar(page), ['Customer', 'Resolve', 'More actions'], 'tablet header');
		await noOverflow(page, 'tablet conversation');
		await shot(page, 'tablet-1-side-by-side');

		const customer = bar(page).getByRole('button', { name: 'Customer' });
		await customer.click();
		const drawer = page.getByRole('dialog', { name: 'Customer' });
		await expect(drawer).toBeVisible();
		await expect(drawer).toHaveAttribute('aria-modal', 'true');
		await expect.poll(() => drawer.evaluate((el) => el.contains(document.activeElement))).toBe(true);
		// it does not cover the whole screen: the conversation shows beside it
		expect((await drawer.boundingBox())!.width).toBeLessThan(700);
		await expect(drawer.getByText('Loading…')).toHaveCount(0);
		await noOverflow(page, 'tablet drawer');
		await shot(page, 'tablet-2-customer-drawer');

		await page.keyboard.press('Escape');
		await expect(drawer).toBeHidden();
		await expect(customer).toBeFocused();
		await expect(composer(page)).toHaveValue('A draft on the tablet');

		// the close button, then the backdrop
		await customer.click();
		await drawer.getByRole('button', { name: 'Close customer panel' }).click();
		await expect(drawer).toBeHidden();
		await expect(customer).toBeFocused();
		await customer.click();
		await page.mouse.click(40, 500);
		await expect(drawer).toBeHidden();
		await expect(customer).toBeFocused();

		// another conversation and back: the draft is there
		const elsewhere = OTHER;
		await inbox(page)
			.getByRole('tab', { name: /^Unassigned/ })
			.click();
		await row(page, elsewhere).click();
		await expect(page).toHaveURL(new RegExp(`/c/${elsewhere}$`));
		await expect(composer(page)).not.toHaveValue('A draft on the tablet');
		await inbox(page).getByRole('tab', { name: /Mine/ }).click();
		await row(page, WA).click();
		await expect(composer(page)).toHaveValue('A draft on the tablet');

		// growing to the wide layout puts the customer inline; shrinking to a phone keeps the draft
		await page.setViewportSize({ width: 1280, height: 800 });
		await expect(page.getByLabel('Context', { exact: true })).toBeVisible();
		await expect(composer(page)).toHaveValue('A draft on the tablet');
		await page.setViewportSize({ width: 390, height: 844 });
		await expect(bar(page).getByRole('button', { name: 'Back to inbox' })).toBeVisible();
		await expect(composer(page)).toHaveValue('A draft on the tablet');
		await noOverflow(page, 'resized to phone');
	});
});
