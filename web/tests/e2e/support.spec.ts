// SW T12 acceptance E2E: the staff workspace (/support/{job}/c/{conversationId}) on the REAL local platform stack
// (scripts/local/web-e2e.sh), one test per acceptance criterion of the spec. Every conversation is a real one: a known
// customer of the tenant opens the job's published web chat (the edge, with the tenant site's signed assertion), and
// asks for a person, so the conversation is in the staff queue. The staff agent is the identity the tenant admin invited
// onto the job's staff team; the racing colleague is a second one the spec invites. Web chat is the channel the stack
// publishes, so the per-channel checks run on it.
import { expect, test, type Page } from '@playwright/test';
import {
	Api,
	asIdentity,
	inviteColleague,
	pngOf,
	J,
	queuedConversation,
	signIn,
	type CustomerChat,
	type Customer,
} from './stack';

const ZONE = 'Europe/Amsterdam';
test.use({ timezoneId: ZONE });

const job = () => J().job;
const url = (conv: string) => `/support/${job()}/c/${conv}`;
let next = 0;
/** The tenant's customers in turn: every test talks to a fresh conversation. */
const someone = (): Customer => J().customers[next++ % J().customers.length]!;

const bar = (page: Page) => page.getByRole('toolbar', { name: 'Conversation actions' });
const chip = (page: Page) => page.locator('.conv-col').getByTestId('owner-chip');
const composer = (page: Page) => page.getByLabel('Reply to customer');
const transcript = (page: Page) => page.getByTestId('transcript');

/** Take the open conversation (waits for the pane to know its owner first). */
async function take(page: Page, from: RegExp = /Unassigned|Agent/) {
	await expect(chip(page)).toHaveText(from);
	const btn = bar(page).getByRole('button', { name: 'Take' });
	await expect(btn).toBeEnabled();
	await btn.click();
	await expect(chip(page)).toHaveText(/You/);
}

/** Wait until every send in the transcript is confirmed. */
async function settled(page: Page) {
	await expect(transcript(page).getByText('Sending…')).toHaveCount(0);
}

/** The staff agent opens `chat`'s conversation in the workspace. */
async function open(page: Page, chat: CustomerChat, size = { width: 1280, height: 720 }) {
	await page.setViewportSize(size);
	await signIn(page, 'agent', url(chat.id));
	await expect(transcript(page)).toBeVisible();
	await expect(chip(page)).toBeVisible();
}

/** Navigate, and wait until the page's support stream is connected (frames sent before it would be missed). */
async function gotoLive(page: Page, path: string) {
	const stream = page.waitForResponse((r) => new URL(r.url()).pathname === '/api/support/events', {
		timeout: 30_000,
	});
	await page.goto(path);
	expect((await stream).status(), 'support stream').toBe(200);
}

/** The case a conversation's row and pages show: its newest. */
async function caseOf(chat: CustomerChat): Promise<string> {
	const ag = await Api.as('agent');
	const c = await ag.get(`/api/jobs/${job()}/conversations/${chat.id}`);
	await ag.dispose();
	const ids: string[] = c.body?.case_ids ?? [];
	expect(ids.length, `the conversation has a case: ${JSON.stringify(c.body).slice(0, 300)}`).toBeGreaterThan(
		0,
	);
	// every customer turn is its own case; the newest (here the handover's) is the one the conversation's row shows
	return ids[ids.length - 1]!;
}

test.describe('desktop', () => {
	test.skip(({ isMobile }) => isMobile, 'desktop layouts; the phone project has its own test below');

	test('newest message and composer visible at 1280x720', async ({ page }) => {
		const chat = await queuedConversation(someone());
		await open(page, chat);
		// the newest row of the transcript and the composer, without scrolling the page or the transcript by hand
		const newest = transcript(page).locator('.line, .sys').last();
		await expect(newest).toBeVisible();
		await expect(newest).toBeInViewport({ ratio: 1 });
		await expect(composer(page)).toBeInViewport({ ratio: 1 });
		// and it stays so when a message lands (the transcript follows the newest)
		await chat.say('Newest: are you still there?');
		await expect(transcript(page).getByText('Newest: are you still there?')).toBeInViewport({ ratio: 1 });
		await expect(composer(page)).toBeInViewport({ ratio: 1 });
	});

	// Acceptance: no message bubble is clipped at any width >= 1024. At 1200+ the customer pane is inline; below that it
	// is a drawer behind the Customer button, so 1024 is checked with it closed and open.
	for (const width of [1024, 1280, 1440, 1920, 2560]) {
		for (const ctx of width < 1200 ? (['collapsed', 'open'] as const) : (['inline'] as const)) {
			test(`no bubble clipped at ${width} (context ${ctx})`, async ({ page }) => {
				const chat = await queuedConversation(someone());
				await chat.say('x'.repeat(400));
				await chat.say('A long message with several words that should wrap inside the bubble. '.repeat(8));
				await chat.say('Here is the tracking page I was sent: https://example.com/' + 'path/'.repeat(60));
				await open(page, chat, { width, height: 900 });
				if (ctx === 'open') {
					await bar(page).getByRole('button', { name: 'Customer' }).click();
					await expect(page.getByLabel('Context', { exact: true })).toBeVisible();
				} else if (ctx === 'collapsed') {
					await expect(page.getByLabel('Context', { exact: true })).toHaveCount(0);
				}
				await expect(transcript(page).getByText('Here is the tracking page I was sent')).toBeVisible();
				const pane = await transcript(page).boundingBox();
				expect(pane).not.toBeNull();
				const bubbles = transcript(page).locator('.bubble');
				const n = await bubbles.count();
				expect(
					n,
					'the opening, the answers, the request for a person and the 3 long ones',
				).toBeGreaterThanOrEqual(5);
				for (let i = 0; i < n; i++) {
					const b = await bubbles.nth(i).boundingBox();
					expect(b, `bubble ${i} at ${width}`).not.toBeNull();
					expect(b!.x, `bubble ${i} left edge at ${width}`).toBeGreaterThanOrEqual(pane!.x - 0.5);
					expect(b!.x + b!.width, `bubble ${i} right edge at ${width}`).toBeLessThanOrEqual(
						pane!.x + pane!.width + 0.5,
					);
					// nothing inside is clipped by the bubble itself (overflow) or sticks out of its right edge
					const inner = await bubbles.nth(i).evaluate((el) => {
						const right = el.getBoundingClientRect().right;
						return {
							overflow: el.scrollWidth - el.clientWidth,
							out: [...el.querySelectorAll('*')].filter((d) => d.getBoundingClientRect().right > right + 0.5)
								.length,
						};
					});
					expect(inner.overflow, `bubble ${i} scrollWidth vs clientWidth at ${width}`).toBeLessThanOrEqual(1);
					expect(inner.out, `bubble ${i} descendants past its right edge at ${width}`).toBe(0);
				}
				// the transcript itself does not scroll sideways
				const sw = await transcript(page).evaluate((el) => ({ s: el.scrollWidth, c: el.clientWidth }));
				expect(sw.s, `transcript scrollWidth vs clientWidth at ${width}`).toBeLessThanOrEqual(sw.c);
			});
		}
	}

	test('owner identical on workspace, conversations, console, case', async ({ page }) => {
		const chat = await queuedConversation(someone());
		const caseId = await caseOf(chat);
		await open(page, chat);
		const caseHeld = async () => {
			await page.goto(`/admin/jobs/${job()}/cases/${caseId}`);
			return page
				.getByText(/Held by [^.]+\.|Nobody has taken it yet\./)
				.first()
				.innerText();
		};
		const convRow = async () => {
			await page.goto(`/admin/jobs/${job()}/conversations`);
			const row = page.locator('tbody tr').filter({ hasText: caseId });
			await expect(row).toHaveCount(1);
			return row.getByTestId('owner-chip').innerText();
		};
		const consoleChip = async () => {
			await page.goto('/console');
			await page.locator(`[data-testid="console-conversation"][data-case="${caseId}"]`).click();
			return page.getByTestId('console-control').getByTestId('owner-chip').innerText();
		};

		// before: nobody has it, on all four surfaces
		await expect(chip(page)).toHaveText(/Unassigned/);
		expect(await convRow()).toMatch(/Unassigned/);
		expect(await consoleChip()).toMatch(/Unassigned/);
		expect(await caseHeld()).toBe('Nobody has taken it yet.');

		// the agent takes it in the workspace
		await page.goto(url(chat.id));
		await take(page, /Unassigned/);
		expect(await convRow()).toMatch(/You/);
		expect(await consoleChip()).toMatch(/You/);
		expect(await caseHeld()).toMatch(/^Held by .+\.$/);

		// and hands it back: the agent has it again everywhere
		await page.goto(url(chat.id));
		await expect(chip(page)).toHaveText(/You/);
		await page.keyboard.press('h');
		await page
			.getByLabel('Note for the agent — what should it do next?')
			.fill('Customer is verified, carry on.');
		await page
			.getByRole('form', { name: 'Hand back to the agent' })
			.getByRole('button', { name: 'Hand back' })
			.click();
		await expect(chip(page)).toHaveText(/Agent/);
		expect(await convRow()).toMatch(/Agent/);
		expect(await consoleChip()).toMatch(/Agent/);
	});

	test('inbound message appears without reload', async ({ page }) => {
		const chat = await queuedConversation(someone());
		await open(page, chat);
		const text = 'Is anyone there? I still have not heard back.';
		const t0 = Date.now();
		await chat.say(text);
		await expect(transcript(page).getByText(text)).toBeVisible({ timeout: 5000 });
		expect(Date.now() - t0).toBeLessThan(5000);
		// no reload: the earlier lines are the same page
		await expect(transcript(page).getByText('I would like to speak to a person, please.')).toBeVisible();

		// and an answer on a case that waited on the customer moves it to the top of Mine (when it is not the open one)
		const reply = async (text: string) => {
			await composer(page).fill(text);
			await page.getByRole('button', { name: 'Send', exact: true }).click();
			await expect(transcript(page).getByText(text)).toBeVisible();
			await settled(page);
		};
		const other = await queuedConversation(someone());
		await take(page, /Unassigned/);
		await reply('Checking your refund now.');
		await page.goto(url(other.id));
		await take(page, /Unassigned/);
		await reply('Let me check where your parcel is.'); // both now wait on the customer
		await gotoLive(page, url(chat.id));
		await page.getByRole('tab', { name: /^Mine/ }).click();
		await expect(page.getByRole('tab', { name: /^Mine/ })).toHaveAttribute('aria-selected', 'true');
		const rows = page.locator('a[data-conversation]');
		// both are in Mine (earlier tests' conversations may be there too)
		await expect(rows.locator(`xpath=self::*[@data-conversation="${chat.id}"]`)).toHaveCount(1);
		expect((await other.say('Thanks, any news?')).status).toBe(200);
		await expect(rows.first()).toHaveAttribute('data-conversation', other.id);
		await gotoLive(page, url(other.id));
		expect((await chat.say('Hello? Still waiting.')).status).toBe(200);
		await expect(rows.first()).toHaveAttribute('data-conversation', chat.id);
	});

	test('attach a photo and send it, see it and the customer photo as thumbnails (SW-9)', async ({ page }) => {
		const chat = await queuedConversation(someone());
		await open(page, chat);
		await take(page, /Unassigned/);
		const png = pngOf();
		await page
			.getByTestId('attach-input')
			.setInputFiles({ name: 'receipt.png', mimeType: 'image/png', buffer: png });
		await expect(page.getByTestId('chips').getByText('receipt.png')).toBeVisible();
		await expect(page.getByText('Uploading…')).toHaveCount(0);
		await composer(page).fill('Here is your receipt.');
		await page.getByRole('button', { name: 'Send', exact: true }).click();
		await expect(page.getByTestId('chips')).toHaveCount(0);
		await expect(transcript(page).getByText('Here is your receipt.')).toBeVisible();
		await settled(page);
		await expect(transcript(page).getByRole('img', { name: 'receipt.png' })).toBeVisible();
		// the customer's photo arrives without a reload
		const sent = await chat.sendFile({ name: 'damage.png', mimeType: 'image/png', buffer: png });
		expect(sent.status, sent.body).toBe(200);
		await expect(transcript(page).getByRole('img', { name: 'damage.png' })).toBeVisible();
		await transcript(page).getByRole('button', { name: 'Open photo damage.png' }).click();
		const dlg = page.getByRole('dialog', { name: 'damage.png' });
		await expect(dlg.getByRole('link', { name: /Download/ })).toBeVisible();
		await page.keyboard.press('Escape');
		await expect(dlg).toHaveCount(0);
	});

	test('double take — one owner', async ({ page }) => {
		const colleague = await inviteColleague('Daan Mulder');
		const chat = await queuedConversation(someone());
		await open(page, chat);
		await expect(chip(page)).toHaveText(/Unassigned/);
		// the colleague takes it a moment before us (two people within the same second)
		const theirs = await asIdentity('ag', colleague);
		const t = await theirs.post(`/api/jobs/${job()}/conversations/${chat.id}/take`, {
			expected_owner: 'queue',
		});
		expect(t.status, `colleague's Take: ${JSON.stringify(t.body).slice(0, 200)}`).toBe(200);
		// our Take now finds it taken (or the live update already showed it): either way one owner
		await bar(page)
			.getByRole('button', { name: 'Take' })
			.click({ timeout: 3000 })
			.catch(() => {});
		// exactly one owner: the colleague; we see who and are read-only
		await expect(chip(page)).toHaveText(/Daan Mulder/);
		await expect(composer(page)).toBeDisabled();
		await expect(composer(page)).toHaveAttribute('placeholder', /Daan Mulder is answering this conversation/);
		await expect(bar(page).getByRole('button', { name: 'Resolve' })).toHaveCount(0);
		await expect(page.getByRole('button', { name: 'Send', exact: true })).toBeDisabled();
		// the other surfaces say the same
		const caseId = await caseOf(chat);
		await page.goto(`/admin/jobs/${job()}/conversations`);
		await expect(page.locator('tbody tr').filter({ hasText: caseId }).getByTestId('owner-chip')).toHaveText(
			/Daan Mulder/,
		);
		await theirs.dispose();
	});

	test('note never shows in customer web chat', async ({ page }) => {
		const chat = await queuedConversation(someone());
		const note = 'Internal: customer is a repeat refund claimer';
		const reply = 'Reply on web chat: we are on it.';
		await open(page, chat);
		await take(page, /Unassigned/);
		await page.getByRole('button', { name: 'Note', exact: true }).click();
		await page.getByLabel('Internal note').fill(note);
		await page.getByRole('button', { name: 'Add note' }).click();
		await expect(transcript(page).getByText(note)).toBeVisible();
		await expect(transcript(page).getByText('Visible to staff only')).toBeVisible();
		await composer(page).fill(reply);
		await page.getByRole('button', { name: 'Send', exact: true }).click();
		await expect(transcript(page).getByText(reply)).toBeVisible();
		await settled(page);
		// the customer's own chat window got the reply and never the note
		const seen = await chat.history();
		const texts = seen.map((m) => m.text);
		expect(texts).toContain(reply);
		expect(texts.join('\n')).not.toContain('repeat refund claimer');
	});

	test('take send resolve by keyboard only', async ({ page }) => {
		const chat = await queuedConversation(someone());
		await open(page, chat);
		await expect(chip(page)).toHaveText(/Unassigned/);
		await page
			.locator('body')
			.click({ position: { x: 1, y: 1 } })
			.catch(() => {});
		await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
		await page.keyboard.press('t');
		await expect(chip(page)).toHaveText(/You/);
		await page.keyboard.press('r');
		await expect(composer(page)).toBeFocused();
		await page.keyboard.type('Thanks for waiting. The refund is on its way.');
		await page.keyboard.press('ControlOrMeta+Enter');
		await expect(transcript(page).getByText('Thanks for waiting. The refund is on its way.')).toBeVisible();
		await expect(composer(page)).toHaveValue('');
		// leave the textarea, then E opens Resolve; reach its submit with Tab and press Enter
		await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
		await page.keyboard.press('e');
		const dialog = page.getByRole('form', { name: 'Resolve this conversation' });
		await expect(dialog).toBeVisible();
		let reached = false;
		for (let i = 0; i < 25 && !reached; i++) {
			reached = await page.evaluate(() => {
				const a = document.activeElement as HTMLElement | null;
				return (
					!!a && a.tagName === 'BUTTON' && /^Resolve$/.test(a.innerText.trim()) && a.closest('form') !== null
				);
			});
			if (!reached) await page.keyboard.press('Tab');
		}
		expect(reached, 'Tab reaches the Resolve button inside the dialog').toBe(true);
		await page.keyboard.press('Enter');
		await expect(dialog).toBeHidden();
		await expect(chip(page)).toHaveText(/Agent/);
	});

	test('one zone label per screen', async ({ page }) => {
		const zones = async () => {
			const text = await page.evaluate(() => document.body.innerText);
			expect(text, 'no zone abbreviation next to a time').not.toMatch(/\b(CEST|CET|GMT|UTC)\b/);
			return text.split(ZONE).length - 1;
		};
		const chat = await queuedConversation(someone());
		const caseId = await caseOf(chat);
		await open(page, chat);
		await expect(page.getByText(/^Times in /)).toBeVisible();
		expect(await zones(), 'workspace').toBe(1);
		await page.goto(`/admin/jobs/${job()}/conversations`);
		await expect(page.locator('tbody tr').first()).toBeVisible();
		expect(await zones(), 'conversations').toBe(1);
		await page.goto(`/admin/jobs/${job()}/handover`);
		await expect(page.locator('main h1').first()).toBeVisible();
		expect(await zones(), 'handover queue').toBe(1);
		await page.goto(`/admin/jobs/${job()}/cases/${caseId}`);
		await expect(page.getByTestId('case-story')).toBeVisible();
		expect(await zones(), 'case').toBe(1);
	});

	test('identified customer: the pane says who it is and never offers Link customer', async ({ page }) => {
		const cust = someone();
		const chat = await queuedConversation(cust);
		await open(page, chat);
		const block = page.getByTestId('customer-block');
		await expect(block.getByText(cust.name)).toBeVisible();
		await expect(block.getByTestId('identity-chip')).toHaveText('Verified · signed in');
		await expect(page.getByRole('button', { name: 'Link customer' })).toHaveCount(0);
		await page
			.getByRole('button', { name: 'More actions' })
			.click()
			.catch(() => {});
		await expect(page.getByRole('menuitem', { name: 'Link customer' })).toHaveCount(0);
	});

	test('unidentified visitor: the pane says so and offers the staff pick', async ({ page }) => {
		const chat = await queuedConversation(null, 'Hello, I have a question about my order.');
		await open(page, chat);
		const block = page.getByTestId('customer-block');
		await expect(block.getByTestId('not-signed-in')).toContainText('Not signed in');
		await block.getByRole('button', { name: 'Link customer' }).click();
		await expect(block.getByPlaceholder('Search customers by name')).toBeVisible();
	});

	test('no raw ids outside details', async ({ page }) => {
		const RAW = [
			/\b[BD]\d{1,3}\b/, // decision codes: B4, D64
			/\bwc[:_]/,
			/\buser:/,
			/\bconv-[0-9a-f]/, // conversation ids
			/\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b/, // uuid
			/\bm-\d{10,}/, // message ids
		];
		for (let i = 0; i < 2; i++) {
			const chat = await queuedConversation(someone());
			await open(page, chat);
			if (i === 1) await take(page, /Unassigned/);
			const open_ = await page
				.locator('details[data-details]')
				.evaluate((d) => (d as HTMLDetailsElement).open);
			expect(open_, 'Details starts closed').toBe(false);
			const visible = await page.evaluate(() => {
				const bits = [document.body.innerText];
				for (const el of document.querySelectorAll('[title],[aria-label],[placeholder]'))
					for (const a of ['title', 'aria-label', 'placeholder']) {
						const v = el.getAttribute(a);
						if (v) bits.push(v);
					}
				return bits.join('\n');
			});
			for (const re of RAW) expect(visible, `${re}`).not.toMatch(re);
			// the ids are there, in Details
			await page.locator('details[data-details] summary').click();
			await expect(page.locator('details[data-details]')).toContainText(/\w/);
		}
	});
});

test.describe('phone', () => {
	test.skip(
		({ isMobile }) => !isMobile,
		'one pane at a time below 768 px (the full journey is sw6-narrow.spec.ts)',
	);

	test('phone shows inbox then conversation', async ({ page }) => {
		const chat = await queuedConversation(someone());
		await signIn(page, 'agent', '/support');
		const inbox = page.getByRole('region', { name: 'Inbox' });
		await expect(inbox).toBeVisible();
		await page.getByRole('tab', { name: /^Unassigned/ }).click();
		await expect(page.locator(`a[data-conversation="${chat.id}"]`)).toBeVisible();
		await expect(transcript(page)).toHaveCount(0);
		await expect(composer(page)).toHaveCount(0);
		await page.locator(`a[data-conversation="${chat.id}"]`).click();
		await expect(page).toHaveURL(new RegExp(`${url(chat.id)}$`));
		await expect(transcript(page)).toBeVisible();
		await expect(composer(page)).toBeVisible();
		await expect(inbox).toBeHidden();
		// no horizontal scroll, and Inbox brings the list back
		expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
		await bar(page).getByRole('button', { name: 'Back to inbox' }).click();
		await expect(inbox).toBeVisible();
		await expect(transcript(page)).toHaveCount(0);
	});
});
