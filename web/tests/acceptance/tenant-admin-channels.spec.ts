// Tenant-admin channel journey on the REAL local stack (no mocks in the browser). Runbook:
// docs/runbooks/acceptance-tenant-admin-channels.md. Run with `npx playwright test -c playwright.acceptance.config.ts`.
//
// A tenant admin signs in to the console, creates a job from a platform template, configures a web chat channel
// (allowed origin, known-customer identity, photos and PDFs for customers and staff), TRIES it as a known customer from
// the console (the admin names a customer of the tenant's system; core signs the sign-in proof server-side, as the
// tenant's site would, and the hosted test page opens as that customer), the customer sends a photo before the first
// message and gets the answer, staff see the photo in the support workspace and answer with a PDF, the admin publishes
// the channel (needs an applied release), the customer's tab works in Production, the admin switches customer
// attachments off (the open tab is refused in plain words and loses its paperclip, a new session has none) and an
// EICAR PDF is refused as malware.
//
// KYC rule (owner 2026-10-26): the customer types natural sentences only. No order number, id, postcode or email is
// ever typed by or asked of the customer. The "Customer reference from your system" field is the ADMIN's (staff) test
// input, and nothing else.
//
// Secrets: nothing is hardcoded and no secret is minted here. The identity secret is shown once by the console and only
// checked for its shape. The EICAR string is assembled at run time (never written to a file in the repository).
import { execFileSync } from 'node:child_process';
import { expect, request as pwRequest, test, type BrowserContext, type Page } from '@playwright/test';
import { Api, J, signIn as stackSignIn, type Who } from '../e2e/stack';

const env = (k: string, d = ''): string => process.env[k] ?? d;
/** The tenant's own site: an allowed origin of the binding (the snippet's home). The hosted test page needs none. */
const SITE = env('TAC_SITE_ORIGIN', 'http://localhost:5199');
/** The tenant and its people come from the platform-stack smoke's journey (PS_E2E_JOURNEY); TAC_TENANT overrides. */
const TENANT = () => env('TAC_TENANT') || J().tenant;
/** psql URL of the stack's database owner (read-only checks of what the product stored). Never printed. */
const OWNER_URL = env('TAC_OWNER_URL') || env('PS_E2E_OWNER_URL');
/** artifact_service fake of the stack, for the pre-flight check only. */
const ARTIFACTS = env('TAC_ARTIFACTS_URL') || env('PS_E2E_ARTIFACTS');
const EDGE = env('TAC_EDGE_URL') || env('PS_E2E_EDGE');

const ISSUER = 'https://shop.example';
/** The admin's test input: a customer of the tenant's own system (staff typing, never the customer). */
const customerRef = (): string => String(J().customers[0]!.customer_id);
/** The journey's real people: the tenant's admin (`ta`) and the support agent invited onto the job's staff (`st`). */
type Persona = 'ta' | 'st';
const WHO: Record<Persona, Who> = { ta: 'admin', st: 'agent' };

test.describe.configure({ mode: 'serial' });

// ---- fixtures built at run time ---------------------------------------------------------------------------------

/** A valid 1x1 RGBA PNG (every chunk CRC checked; the previous copy had a corrupt IDAT CRC and was rightly refused). */
const PNG = Buffer.from(
	'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==',
	'base64',
);

/** A minimal, valid, plain PDF (no active content). `content` is the one content stream, stored uncompressed. */
function tinyPdf(content = 'BT ET'): Buffer {
	const objs = [
		'<< /Type /Catalog /Pages 2 0 R >>',
		'<< /Type /Pages /Kids [3 0 R] /Count 1 >>',
		'<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R /Resources << >> >>',
		`<< /Length ${Buffer.byteLength(content)} >>\nstream\n${content}\nendstream`,
	];
	let out = '%PDF-1.4\n';
	const offsets: number[] = [];
	objs.forEach((o, i) => {
		offsets.push(Buffer.byteLength(out));
		out += `${i + 1} 0 obj\n${o}\nendobj\n`;
	});
	const xref = Buffer.byteLength(out);
	out += `xref\n0 ${objs.length + 1}\n0000000000 65535 f \n`;
	for (const off of offsets) out += `${String(off).padStart(10, '0')} 00000 n \n`;
	out += `trailer\n<< /Size ${objs.length + 1} /Root 1 0 R >>\nstartxref\n${xref}\n%%EOF\n`;
	return Buffer.from(out, 'latin1');
}

/**
 * The EICAR antivirus test string, assembled here so no file in the repository holds it. The agent re-encodes images
 * (which would drop it), so it travels as the one content stream of an otherwise valid PDF, stored byte-identical, and
 * ClamAV (storage_service) sees it when it unpacks the stream.
 */
function eicarPdf(): Buffer {
	const eicar = ['X5O!P%@AP[4\\PZX54(P^)7CC)7}$', 'EICAR-STANDARD-ANTIVIRUS-TEST-FILE!$H+H*'].join('');
	return tinyPdf(eicar);
}

// ---- helpers ----------------------------------------------------------------------------------------------------

interface Called {
	status: number;
	body: any;
}
const rowsOf = (b: any): any[] => (Array.isArray(b) ? b : (b?.items ?? []));
const text = (v: unknown) => JSON.stringify(v).slice(0, 500);

function ok(c: Called, label: string): any {
	expect(c.status, `${label}: ${text(c.body)}`).toBeGreaterThanOrEqual(200);
	expect(c.status, `${label}: ${text(c.body)}`).toBeLessThan(300);
	return c.body;
}

/** SQL predicate: rows of this tenant's job under test (the smoke's own job has chats of its own). */
/** SQL: a conversation's state `key` (A1: identity lives in `conversation_state`, not in `conversation` columns). */
const stateOf = (alias: string, key: string) =>
	`(SELECT s.value FROM conversation_state s WHERE s.tenant_id = ${alias}.tenant_id AND s.job_id = ${alias}.job_id AND s.conversation_id = ${alias}.conversation_id AND s.key = '${key}')`;

const mine = () =>
	`tenant_id = '${TENANT()}' AND job_id = (SELECT id FROM jobs WHERE tenant_id = '${TENANT()}' AND key = '${S.job}')`;

/** One psql statement as the database owner (read-only checks of what the product stored). */
function sql(statement: string): string {
	if (!OWNER_URL) throw new Error('PS_E2E_OWNER_URL is not set (run through scripts/local/web-e2e.sh)');
	return execFileSync('psql', [OWNER_URL, '-v', 'ON_ERROR_STOP=1', '-qtA', '-c', statement], {
		encoding: 'utf8',
	}).trim();
}

/** Sign in to the console as the journey's person through the real PKCE flow. */
const signIn = (page: Page, persona: Persona, path: string): Promise<void> =>
	stackSignIn(page, WHO[persona], path);

async function until<T>(
	label: string,
	read: () => Promise<T>,
	done: (v: T) => boolean,
	ms = 90_000,
): Promise<T> {
	const end = Date.now() + ms;
	for (;;) {
		const v = await read();
		if (done(v)) return v;
		if (Date.now() > end) throw new Error(`${label}: not reached in ${ms} ms; last: ${text(v)}`);
		await new Promise((r) => setTimeout(r, 1500));
	}
}

/**
 * A job created moments ago answers once an `interaction` process serves its TEST scope: until its heartbeat names the
 * scope the edge answers 503 and the widget asks the customer to send again. Wait on the product's own runtime view
 * (`GET /api/jobs/{job}/agents`: the interaction role's `test.rev` is set while a process serves the scope).
 */
async function testScopeServed(job: string): Promise<void> {
	const ta = await Api.as('admin');
	await until(
		`an interaction process serving the TEST scope of ${job}`,
		async () =>
			rowsOf((await ta.get(`/api/jobs/${job}/agents`)).body).find((a) => a.role === 'interaction') as
				{ test?: { rev?: number | null } } | undefined,
		(a) => a?.test?.rev !== null && a?.test?.rev !== undefined,
	);
}

/**
 * The integration step of the tenant admin's journey for a fresh job, as the platform-stack smoke does it through the
 * product API: every `route_not_ready` blocker of the draft is routed (in-process built-ins as `builtin`, the rest to
 * the tenant's own systems: the stub in TEST, the real one in Production) and each Production route gets its contract
 * test. Nothing else is cleared: every other go-live check still has to pass on its own.
 */
async function routeTheJob(job: string): Promise<void> {
	const ta = await Api.as('admin');
	const blocking = rowsOf(
		ok(await ta.get(`/api/jobs/${job}/release-readiness?of=draft`), 'release readiness')?.blocking,
	).filter((b) => b.code === 'route_not_ready');
	for (const b of blocking) {
		const subject = String(b.subject);
		const at = subject.lastIndexOf('@');
		const [op, env] = [subject.slice(0, at), subject.slice(at + 1)];
		const builtin = /^(knowledge\.(search|answer)|credential\.verify|rule\.evaluate)(\{.*\})?$/.test(op);
		const target = builtin
			? { kind: 'builtin' }
			: { kind: 'instance', instance_id: env === 'prod' ? 'demo-retail' : 'demo-retail-stub' };
		const r = await ta.put(`/api/jobs/${job}/operations/${encodeURIComponent(op)}/routes/${env}`, { target });
		expect(r.status, `route ${op}@${env}: ${text(r.body)}`).toBe(200);
		if (env === 'prod' && !builtin) {
			const t = await ta.post('/api/connections/systems/demo-retail/contract-tests', {
				op,
				prod_write_ok: true,
			});
			expect(t.status, `contract test ${op}: ${text(t.body)}`).toBe(200);
			expect((t.body as { outcome?: string })?.outcome, `contract test ${op}: ${text(t.body)}`).toBe('green');
		}
	}
	await ta.dispose();
}

// ---- shared state -----------------------------------------------------------------------------------------------

const S = {
	job: '',
	bindingId: '',
	widgetKey: '',
	conversation: '',
};

/** A customer's tab: the hosted test page opened by the console's "Try as this customer", in its own browser context. */
interface Tab {
	ctx: BrowserContext;
	page: Page;
}

/** The widget lives in an open shadow root: Playwright selectors pierce it. */
const widget = (p: Page) => ({
	launcher: p.locator('button.launcher'),
	panel: p.locator('.panel'),
	input: p.getByLabel('Message', { exact: true }),
	send: p.getByRole('button', { name: 'Send message' }),
	attach: p.locator('button.attach'),
	file: p.locator('input[type="file"]'),
	chip: p.locator('ul.chips li'),
	assistant: p.locator('.msg.assistant'),
	user: p.locator('.msg.user'),
});

async function openWidget(p: Page): Promise<void> {
	const w = widget(p);
	if (await w.panel.isHidden()) await w.launcher.click();
	await expect(w.panel).toBeVisible();
}

/** The customer types a natural sentence and waits for the agent's next answer. */
async function say(p: Page, sentence: string): Promise<void> {
	const w = widget(p);
	const before = await w.assistant.count();
	await w.input.fill(sentence);
	await w.send.click();
	await expect.poll(() => w.assistant.count(), { timeout: 120_000 }).toBeGreaterThan(before);
}

const channelsUrl = () => `/admin/jobs/${S.job}/channels`;
const bindingRow = (p: Page) => p.getByTestId('binding-row').filter({ hasText: 'Shop web chat' });

/**
 * The admin tries the TEST channel as a known customer, from the console: types the customer reference of the tenant's
 * system, presses "Try as this customer"; core signs the sign-in proof and the hosted test page opens in a new tab as that
 * customer. Returns that tab (it lives in `admin`'s browser context).
 */
async function tryAsCustomer(admin: Page): Promise<Page> {
	if (!(await admin.getByTestId('binding-panel').isVisible()))
		await bindingRow(admin).getByTestId('binding-snippet-toggle').click();
	await admin.getByTestId('webchat-try-customer').fill(customerRef());
	const opened = admin.waitForEvent('popup');
	await admin.getByTestId('webchat-try-as').click();
	const tab = await opened;
	await tab.waitForURL(/\/webchat\/demo\?key=/);
	return tab;
}

// ---- the journey ------------------------------------------------------------------------------------------------

test.beforeAll(async () => {
	expect(
		OWNER_URL && ARTIFACTS && EDGE,
		'PS_E2E_OWNER_URL, PS_E2E_ARTIFACTS, PS_E2E_EDGE (run through scripts/local/web-e2e.sh)',
	).toBeTruthy();
	// pre-flight: the attachments fake and agent-edge answer. The tenant, its offering, tenant admin and support agent
	// already exist: the platform-stack smoke made them through the product routes (journey.json).
	const probe = await pwRequest.newContext();
	expect((await probe.get(`${ARTIFACTS}/health`)).ok(), `artifact_service ${ARTIFACTS}/health`).toBeTruthy();
	expect((await probe.get(`${EDGE}/health`)).ok(), `agent-edge ${EDGE}/health`).toBeTruthy();
	await probe.dispose();
});

test('1 the tenant admin signs in and creates a job from a template', async ({ page }) => {
	await signIn(page, 'ta', '/admin/jobs');
	const ta = await Api.as('admin');
	// always a NEW job: the smoke's own job is already released, and step 7 needs one with no release yet
	const before = new Set(rowsOf(ok(await ta.get('/api/jobs'), 'jobs')).map((j) => j.key as string));
	await page.getByRole('button', { name: 'New job' }).click();
	const dialog = page.getByTestId('create-job');
	await expect(dialog).toBeVisible();
	if (await page.getByTestId('mode-template').isVisible()) await page.getByTestId('mode-template').click();
	await page.getByRole('button', { name: 'Create job' }).click();
	await expect(page.getByText(/Job .+ created/)).toBeVisible();
	const made = rowsOf(ok(await ta.get('/api/jobs'), 'jobs')).filter((j) => !before.has(j.key as string));
	expect(made.length, 'one new job exists').toBe(1);
	S.job = made[0].key as string;
	// the journey's support agent staffs it (as the smoke does for its own job)
	const put = await ta.put(`/api/jobs/${S.job}/staff`, {
		members: [J().agent.sub],
		routing: { mode: 'off' },
	});
	expect(put.status, `job staff: ${text(put.body)}`).toBe(200);
	await ta.dispose();
});

test('2 configure a web chat channel: origin, known customers, photos and PDFs for both sides', async ({
	page,
}) => {
	await signIn(page, 'ta', channelsUrl());
	await page.getByTestId('binding-kind').selectOption('web_chat');
	await page.getByTestId('binding-label').fill('Shop web chat');
	await page.getByTestId('binding-origins').fill(SITE);
	await page.getByTestId('binding-create').click();
	// a new web chat binding opens its widget panel
	const panel = page.getByTestId('binding-panel');
	await expect(panel).toBeVisible();
	await expect(page.getByTestId('widget-origins')).toHaveValue(SITE);
	await page.getByTestId('widget-attach-customer').check();
	await page.getByTestId('widget-attach-staff').check();
	await page.getByTestId('widget-identity-mode').selectOption('hs256');
	await page.getByTestId('widget-identity-issuer').fill(ISSUER);
	await page.getByTestId('widget-save').click();
	await expect(page.getByText(/widget saved/)).toBeVisible();
	// the secret is made after the mode is saved and shown once; the admin never needs it again: the console signs the
	// test customer's proof through core (step 4)
	const make = page.getByTestId('widget-identity-secret-make');
	await expect(make).toBeEnabled();
	await make.click();
	const once = page.getByTestId('widget-identity-secret-once');
	await expect(once).toBeVisible();
	const shown = (await page.getByTestId('widget-identity-secret-value').innerText()).trim();
	expect(
		shown.startsWith('ids_') && /Key id\s+\S+/.test(await once.innerText()),
		'secret and key id shown',
	).toBeTruthy();
	await page.getByTestId('widget-identity-secret-done').click();
	S.widgetKey = (await page.getByTestId('binding-snippet').innerText()).match(/wk_[a-z2-7]{24}/)?.[0] ?? '';
	expect(S.widgetKey, 'the widget key is in the snippet').toBeTruthy();
	S.bindingId = (await bindingRow(page).getAttribute('data-id')) ?? '';
	expect(S.bindingId).toBeTruthy();
});

test('3 the channel is in TEST: "Try it" opens the hosted test page and "Try as this customer" is offered', async ({
	page,
}) => {
	await signIn(page, 'ta', channelsUrl());
	const chip = bindingRow(page).getByTestId('binding-env');
	await expect(chip).toHaveAttribute('data-env', 'test');
	await expect(chip).toHaveText('Test');
	await bindingRow(page).getByTestId('binding-snippet-toggle').click();
	const link = page.getByTestId('webchat-try');
	await expect(link).toBeVisible();
	const href = (await link.getAttribute('href')) ?? '';
	expect(href).toContain(`/webchat/demo?key=${S.widgetKey}`);
	// the plain link is an anonymous visitor's page: it only has to load, and carries its same-origin bridge script
	const r = await page.request.get(href);
	expect(r.status(), `hosted test page ${href}`).toBe(200);
	const html = await r.text();
	expect(html).toContain('data-widget');
	expect(html).toContain('/webchat/demo.js');
	// HS256 and its secret exist, so the admin may try the channel as one of the tenant's customers
	await expect(page.getByText('Customer reference from your system')).toBeVisible();
	await expect(page.getByTestId('webchat-try-as')).toBeDisabled(); // nothing typed yet
	await expect(page.getByTestId('webchat-try-anon-note')).toHaveCount(0);
});

/** The customer's tab, kept open from step 4 to step 9: the session recovery of step 8 needs the same sessionStorage. */
let cust: Tab;

test('4 the admin tries the TEST channel as a known customer: a photo before the first message, and the answer', async ({
	browser,
}) => {
	const ctx = await browser.newContext();
	const admin = await ctx.newPage();
	await signIn(admin, 'ta', channelsUrl());
	await testScopeServed(S.job);
	const tab = await tryAsCustomer(admin);
	cust = { ctx, page: tab };
	const w = widget(tab);
	await openWidget(tab);
	// the paperclip works as soon as the chat is open, before any message: the photo waits as a chip
	await expect(w.attach).toBeVisible();
	await expect(w.attach).toBeEnabled();
	await w.file.setInputFiles({ name: 'damaged-parcel.png', mimeType: 'image/png', buffer: PNG });
	await expect(w.chip.locator('.name')).toHaveText('damaged-parcel.png');
	// the customer writes one natural sentence; the photo follows it on its own, with no extra step
	await say(tab, 'Hi, something I received yesterday looks damaged.');
	await expect(w.user.locator('button.thumb img').first()).toBeVisible({ timeout: 120_000 });
	await expect(w.chip).toHaveCount(0);
	// find the conversation for the staff steps (newest web chat of the job)
	const ta = await Api.as('admin');
	const conv = await until(
		'the web chat conversation',
		async () =>
			rowsOf((await ta.get(`/api/jobs/${S.job}/console/conversations`)).body).find(
				(c) => c.channel === 'web_chat',
			),
		(c) => !!c,
	);
	S.conversation = conv.conversation_id as string;
	// known customer: the proof core signed was accepted (identity source `assertion`), and the chat ran on the draft (TEST)
	expect(
		sql(
			`SELECT ${stateOf('conversation', 'identity_source')} || '/' || env FROM conversation WHERE ${mine()} AND conversation_id = '${S.conversation}'`,
		),
	).toBe('assertion/test');
	// bound to the customer reference the admin named, not to anything typed in the chat
	expect(
		sql(`SELECT customer_ref FROM web_chat_session WHERE ${mine()} ORDER BY created_at DESC LIMIT 1`),
	).toBe(customerRef());
	await ta.dispose();
});

test('5 staff see the photo in the support workspace and reply with a PDF', async ({ page }) => {
	expect(S.conversation, 'step 4 found the conversation').toBeTruthy();
	await signIn(page, 'st', `/support/${S.job}/c/${S.conversation}`);
	const tr = page.getByTestId('transcript');
	await expect(tr).toBeVisible();
	await expect(tr.getByTestId('attachment-photo').first()).toBeVisible();
	await page
		.getByRole('toolbar', { name: 'Conversation actions' })
		.getByRole('button', { name: 'Take' })
		.click();
	await expect(page.locator('.conv-col').getByTestId('owner-chip')).toHaveText(/You/);
	await page
		.getByTestId('attach-input')
		.setInputFiles({ name: 'care-instructions.pdf', mimeType: 'application/pdf', buffer: tinyPdf() });
	await expect(page.getByTestId('chips').getByText('care-instructions.pdf')).toBeVisible();
	await expect(page.getByText('Uploading…')).toHaveCount(0);
	await page.getByLabel('Reply to customer').fill('Thank you for the photo. Here is what to do next.');
	await page.getByRole('button', { name: 'Send', exact: true }).click();
	await expect(tr.getByText('Sending…')).toHaveCount(0);
	await expect(tr.getByTestId('attachment-pdf').first()).toBeVisible();
});

test('6 the customer sees the staff PDF in the widget, without reloading', async () => {
	await expect(
		cust.page.getByRole('button', { name: /Download care-instructions\.pdf/ }),
		'the staff PDF arrives in the open widget',
	).toBeVisible({ timeout: 60_000 });
});

test('7 publish needs an applied release: refused first, then the admin applies one and publishes', async ({
	page,
}) => {
	await signIn(page, 'ta', channelsUrl());
	// refused before any release: a plain reason, the channel stays in TEST
	await bindingRow(page).getByTestId('binding-publish').click();
	await expect(page.getByText(/no release yet/i)).toBeVisible();
	await expect(bindingRow(page).getByTestId('binding-env')).toHaveAttribute('data-env', 'test');

	// the console path of a tenant admin: select, approve, go live (/admin/jobs/{job}/release)
	await page.goto(`/admin/jobs/${S.job}/release`);
	await page.getByRole('button', { name: 'Edit selection' }).click();
	const editor = page.getByTestId('selection-editor');
	await expect(editor).toBeVisible();
	let changed = false;
	for (const id of ['sel-dim-area', 'sel-dim-channel']) {
		const boxes = editor.getByTestId(id).getByRole('checkbox');
		if ((await boxes.count()) && !(await boxes.first().isChecked())) {
			await boxes.first().check();
			changed = true;
		}
	}
	const region = editor.getByTestId('sel-dim-region').getByRole('radio');
	if ((await region.count()) && !(await region.first().isChecked())) {
		await region.first().check();
		changed = true;
	}
	// an already complete selection leaves Save disabled: nothing to save, close the drawer
	if (changed) await editor.getByRole('button', { name: 'Save selection' }).click();
	else await editor.getByRole('button', { name: 'Cancel' }).click();
	await expect(editor).toBeHidden();
	await routeTheJob(S.job);
	// D54: nothing here is test-chat testable (the selection, Production routes): the go-live checklist proves them,
	// so the release page lets the admin approve without a scenario run (this job's template has no scenarios)
	await page.goto(`/admin/jobs/${S.job}/release`);

	const approve = page.getByRole('button', { name: 'Approve', exact: true });
	await expect(approve, await blockersText(page)).toBeEnabled({ timeout: 60_000 });
	await approve.click();
	const goLive = page.getByRole('button', { name: /^Go live/ });
	await expect(goLive, await blockersText(page)).toBeEnabled({ timeout: 60_000 });
	await goLive.click();
	await expect(page.getByTestId('release-live')).toBeVisible({ timeout: 60_000 });

	await page.goto(channelsUrl());
	await bindingRow(page).getByTestId('binding-publish').click();
	const chip = bindingRow(page).getByTestId('binding-env');
	await expect(chip).toHaveAttribute('data-env', 'prod');
	await expect(chip).toHaveText('Live');
	// a live channel is not a test: the console no longer offers "Try as this customer"
	await bindingRow(page).getByTestId('binding-snippet-toggle').click();
	await expect(page.getByTestId('webchat-try-customer')).toHaveCount(0);
});

/** Why a release button is disabled, for the failure message (the page states it up front). */
async function blockersText(page: Page): Promise<string> {
	// a message for a failure only: never wait (innerText() waits up to the test timeout for an element that is absent)
	const now = async (id: string) => {
		const l = page.getByTestId(id);
		return (await l.count())
			? await l
					.first()
					.innerText({ timeout: 2000 })
					.catch(() => '')
			: '';
	};
	const b = await now('release-blockers');
	const r = await now('release-refusal');
	return `release page: ${b} ${r}`.slice(0, 600);
}

test('8 LIVE: the customer tab keeps its history; publishing ended its test session, the next message is Production', async () => {
	const w = widget(cust.page);
	const sessionsBefore = sql(`SELECT count(*) FROM web_chat_session WHERE ${mine()}`);
	await cust.page.reload();
	await openWidget(cust.page);
	// recovered from this tab's own storage: the conversation so far is still there, no new session was opened to show it
	await expect(w.user.first()).toBeVisible();
	expect(sql(`SELECT count(*) FROM web_chat_session WHERE ${mine()}`), 'reloading opens no session').toBe(
		sessionsBefore,
	);
	await say(cust.page, 'Now that you are live, can you tell me what happens with my damaged box?');
	// publishing moved the channel to Production and ended the test session (D130): the widget opened a new Production
	// session. The admin test proof is TEST-only and single-use, so this visitor is anonymous until their own site signs
	// them in (that path is covered by crates/agent-api/tests/webchat_sessions.rs, not by the console)
	const row = sql(
		`SELECT env || '/' || coalesce(${stateOf('conversation', 'identity_source')}, 'anonymous') FROM conversation WHERE ${mine()} AND channel = 'web_chat' ORDER BY started_at DESC LIMIT 1`,
	);
	expect(row.startsWith('prod/'), `the newest web chat conversation runs in Production: ${row}`).toBe(true);
	expect(row).not.toContain('assertion');
});

test('9 admin switches customer attachments OFF: the open widget is refused plainly and loses its paperclip, a new one has none', async ({
	page,
}) => {
	await signIn(page, 'ta', channelsUrl());
	await bindingRow(page).getByTestId('binding-snippet-toggle').click();
	await page.getByTestId('widget-attach-customer').uncheck();
	await page.getByTestId('widget-save').click();
	await expect(page.getByText(/widget saved/)).toBeVisible();

	// the tab that was open already still shows its paperclip (it has not asked again); the upload is refused in plain
	// words and the paperclip disappears at once
	const old = cust.page;
	const wo = widget(old);
	await expect(wo.attach).toBeVisible();
	await wo.file.setInputFiles({ name: 'another-photo.png', mimeType: 'image/png', buffer: PNG });
	await expect(
		old.getByText('Photos and PDFs are turned off for this chat. Please type your message instead.'),
	).toBeVisible();
	await expect(wo.attach).toBeHidden();
	await expect(wo.chip).toHaveCount(0);
	await cust.ctx.close();

	// back to TEST, so the admin can open a brand new known-customer session: it learns the setting at the start
	await bindingRow(page).getByTestId('binding-unpublish').click();
	await expect(bindingRow(page).getByTestId('binding-env')).toHaveAttribute('data-env', 'test');
	const fresh = await tryAsCustomer(page);
	await openWidget(fresh);
	await say(fresh, 'Hello again, I only want to ask a question this time.');
	await expect(widget(fresh).attach).toBeHidden();
});

test('10 EICAR is refused as malware by the attachments stack (customer attachments back ON)', async ({
	page,
}) => {
	await signIn(page, 'ta', channelsUrl());
	await bindingRow(page).getByTestId('binding-snippet-toggle').click();
	await page.getByTestId('widget-attach-customer').check();
	await page.getByTestId('widget-save').click();
	await expect(page.getByText(/widget saved/)).toBeVisible();

	// the channel is still in TEST after step 9: another known-customer session
	const tab = await tryAsCustomer(page);
	const w = widget(tab);
	await openWidget(tab);
	await say(tab, 'One more thing about my order, I have a document to show you.');
	await expect(w.attach).toBeVisible();
	await w.file.setInputFiles({ name: 'invoice.pdf', mimeType: 'application/pdf', buffer: eicarPdf() });
	await expect(tab.getByText("This file can't be accepted.")).toBeVisible({ timeout: 60_000 });
	await expect(w.chip).toHaveCount(0);
});
