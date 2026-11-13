// J6 "Your own operations" (D13/D28, 0246) on the REAL local platform stack (scripts/local/web-e2e.sh): the tenant
// admin's operations page offers the operations of the tenant's own connector templates ("Add from your
// connectors"), a job designer adds one to the job — it becomes a required operation of the draft — the draft rev
// moves, the op survives a reload (it is the API's, not client state), and removing it puts it back on offer. The
// tenant's own support agent — a job member with no job designer role — reads the same rows with Add / Remove
// disabled.
//
// Everything goes through the product's own routes: the page is /admin/jobs/{job}/operations, its data is
// GET /api/jobs/{job}/own-operations, the writes are PUT / DELETE on it. The smoke
// (scripts/local/platform-stack-smoke.mjs) builds no tenant-private template, so `setup` makes one through the same
// API the Templates page uses (POST /api/connections/templates, then PUT its ops). Its two ops are not in the
// platform catalogue, so each carries its own `def` — what an MCP / ERP / CRM tool looks like in this product. The
// template key and the op names are fixed: a rerun (or a failed earlier test) reuses the template and every test
// starts from "neither op is one of the job's yet", so there is exactly one row per op.
import { expect, test, type Page } from '@playwright/test';
import { Api, J, ok, rowsOf, signIn, viaUi, watchConsole } from './stack';

const job = () => J().job;
const jobPath = () => `/admin/jobs/${encodeURIComponent(job())}`;
/** `GET|PUT|DELETE /api/jobs/{job}/own-operations[/{op}]`, through the browser's own origin. */
const ownPath = (op?: string) => `/api/jobs/${encodeURIComponent(job())}/own-operations${op ? `/${op}` : ''}`;

const TEMPLATE = 'e2e_ownops';
/** The op the spec adds through the UI (unless the journey already offers one: see `setup`). */
const NOTES = 'support.notes_probe';
/** The op the role test adds through the API, so a Remove button exists to be seen disabled. */
const HISTORY = 'support.history_probe';
const OUR_OPS = [NOTES, HISTORY];

const escape = (s: string) => s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
/** A row's text as Playwright sees it. A plain string, never a `\\b` regex: a row's innerText joins its cells
 * without separators ("support.notes_probeR2 · Verified…"), so a trailing word boundary never matches. */
const opText = (op: string) => op;
/** The browser's write for one op, whatever the job key. */
const ownCall = (op: string) => new RegExp(`/own-operations/${escape(op)}$`);

const section = (page: Page) => page.getByTestId('j6-own-ops');
/** A table row of the section matching `text` (both tables live in the section; scope every lookup to it). */
const rowOf = (page: Page, text: string | RegExp) => section(page).getByRole('row').filter({ hasText: text });
const toast = (page: Page, text: string) => page.locator('.toast[role="status"]').filter({ hasText: text });

/** The job's draft revision, as the J6 chain shows it; every own-operation write answers the same number. */
async function draftRev(page: Page): Promise<number> {
	const cell = page.locator('ol.j6-chain li').nth(1).locator('span.mono');
	await expect(cell).toHaveText(/^\d+$/);
	return Number((await cell.textContent())!.trim());
}

/** The two ops of the tenant's own template: a `def` per op, because no platform catalogue knows them. */
const templateOps = () => [
	{
		object: 'support',
		verb: 'notes_probe',
		method: 'GET',
		path: '/v1/support/notes',
		request: '',
		field_map: [],
		status_map: [],
		idempotency: { kind: 'nat' },
		def: {
			side_effect: 'R',
			input_fields: ['thread_id'],
			output_fields: ['notes'],
			description: 'Read the notes this tenant keeps on a support thread.',
		},
	},
	{
		object: 'support',
		verb: 'history_probe',
		method: 'GET',
		path: '/v1/support/history',
		request: '',
		field_map: [],
		status_map: [],
		idempotency: { kind: 'nat' },
		def: {
			side_effect: 'R',
			input_fields: ['thread_id'],
			output_fields: ['events'],
			description: 'Read the history this tenant keeps on a support thread.',
		},
	},
];

/** The tenant template on a draft version, its ops exactly `templateOps()`; `@1` on a fresh tenant. */
async function ensureTemplate(admin: Api): Promise<string> {
	const mine = rowsOf(ok(await admin.get('/api/connections/templates'), 'list the tenant templates')).filter(
		(t: { key?: string }) => t.key === TEMPLATE,
	);
	let version = '1';
	if (!mine.length) {
		ok(
			await admin.post('/api/connections/templates', {
				key: TEMPLATE,
				label: 'E2E support tools',
				kind: 'custom',
				auth_kind: 'none',
				description: 'The tenant’s own support backend, for the J6 own-operations browser test.',
			}),
			`create ${TEMPLATE}@1`,
		);
	} else {
		const draft = mine.find((t: { status?: string }) => t.status === 'draft');
		if (draft) version = draft.version;
		else {
			// a published version never changes: carry it into a draft (the spec itself never activates it)
			const next = await admin.post(`/api/connections/templates/${TEMPLATE}/${mine[0].version}/drafts`, {});
			if (next.status === 201) version = next.body.version;
			else {
				const again = rowsOf(
					ok(await admin.get('/api/connections/templates'), 'list the tenant templates'),
				).find((t: { key?: string; status?: string }) => t.key === TEMPLATE && t.status === 'draft');
				expect(
					again,
					`no draft of ${TEMPLATE} to save: ${JSON.stringify(next.body).slice(0, 300)}`,
				).toBeTruthy();
				version = again.version;
			}
		}
	}
	ok(
		await admin.put(`/api/connections/templates/${TEMPLATE}/${version}`, {
			label: 'E2E support tools',
			description: 'The tenant’s own support backend, for the J6 own-operations browser test.',
			credential_schema: [],
			ops: templateOps(),
		}),
		`save ${TEMPLATE}@${version}`,
	);
	return version;
}

interface Setup {
	/** The op the UI test adds and removes: one the journey already offers, else the spec's own. */
	addOp: string;
	/** The op the role test adds through the API, so an own row exists next to an addable one. */
	archiveOp: string;
	template: string;
	version: string;
}

/** Make the page hold one op it can add (and one it can own), as the admin, through the API. Idempotent. */
async function setup(): Promise<Setup> {
	const admin = await Api.as('admin');
	try {
		// what the job could add today; the smoke builds no tenant template, so this is normally empty
		const before = ok(await admin.get(ownPath()), 'GET the job’s own operations');
		// prefer an op the journey itself offers, so the spec invents one only when the product has none
		const offered = (before.available ?? []).find(
			(a: { op: string; added: boolean; status: string }) =>
				!a.added && a.status !== 'retired' && !OUR_OPS.includes(a.op),
		);
		const version = await ensureTemplate(admin);
		for (const op of OUR_OPS)
			if ((before.ops ?? []).some((o: { op: string }) => o.op === op))
				ok(await admin.del(ownPath(op)), `remove the leftover ${op}`);
		const view = ok(await admin.get(ownPath()), 'GET the job’s own operations');
		for (const op of OUR_OPS) {
			const a = (view.available ?? []).find((x: { op: string }) => x.op === op);
			expect(a, `${op} is offered to the job: ${JSON.stringify(view).slice(0, 500)}`).toBeTruthy();
			expect(a.added, `${op} is not one of the job’s own operations yet`).toBe(false);
		}
		return { addOp: offered?.op ?? NOTES, archiveOp: HISTORY, template: TEMPLATE, version };
	} finally {
		await admin.dispose();
	}
}

// an own op left behind by a failed test is the API's: it outlives the page, so every test clears both
test.afterEach(async () => {
	const admin = await Api.as('admin');
	for (const op of OUR_OPS) {
		const r = await admin.del(ownPath(op));
		expect([200, 404], `remove ${op}: ${JSON.stringify(r.body).slice(0, 200)}`).toContain(r.status);
	}
	await admin.dispose();
});

test('renders the own operations section for the tenant admin, no console errors', async ({ page }) => {
	const errors = watchConsole(page);
	const own = page.waitForResponse(
		(r) => r.request().method() === 'GET' && new URL(r.url()).pathname === ownPath(),
		{ timeout: 60_000 },
	);
	await signIn(page, 'admin', `${jobPath()}/operations`);
	expect((await own).status(), 'GET the job’s own operations').toBe(200);

	const ops = section(page);
	await expect(ops).toBeVisible();
	// the resource has loaded when its offer list's heading is up; then it is one of the two honest states
	await expect(ops.getByRole('heading', { name: 'Add from your connectors' })).toBeVisible();
	// one of the two honest states: an own row to remove, or the empty note — whichever the load settles on
	await expect(
		ops
			.getByRole('button', { name: 'Remove', exact: true })
			.first()
			.or(ops.getByText('This job has no operations of its own yet.')),
	).toBeVisible();
	expect(errors).toEqual([]);
});

test('adds an own operation, keeps it across a reload and removes it again', async ({ page }) => {
	const { addOp } = await setup();
	await signIn(page, 'admin', `${jobPath()}/operations`);
	const ops = section(page);
	await expect(ops).toBeVisible();

	const add = rowOf(page, opText(addOp)).getByRole('button', { name: 'Add', exact: true });
	await expect(add).toBeEnabled();
	const before = await draftRev(page);

	const added = ok(await viaUi(page, 'PUT', ownCall(addOp), () => add.click()), `add ${addOp}`);
	await expect(toast(page, `Added ${addOp}.`)).toBeVisible();
	// "Your own operations" holds it now — only that table has Remove
	const remove = rowOf(page, opText(addOp)).getByRole('button', { name: 'Remove', exact: true });
	await expect(remove).toBeVisible();
	// the page showed the rev the API read before the write, and moved to the rev the write answered
	expect(Number(added.previous_rev), 'the page showed the pre-write draft rev').toBe(before);
	await expect.poll(() => draftRev(page)).toBe(Number(added.draft_rev));
	expect(Number(added.draft_rev), 'the draft moved').toBeGreaterThan(before);

	// the draft is the API's: after a reload the op is still one of the job's own
	await page.reload();
	const removeAfterReload = rowOf(page, opText(addOp)).getByRole('button', {
		name: 'Remove',
		exact: true,
	});
	await expect(removeAfterReload).toBeVisible();
	await expect.poll(() => draftRev(page)).toBe(Number(added.draft_rev));

	const removed = ok(
		await viaUi(page, 'DELETE', ownCall(addOp), () => removeAfterReload.click()),
		`remove ${addOp}`,
	);
	await expect(toast(page, `Removed ${addOp}.`)).toBeVisible();
	// gone from "Your own operations" (no Remove), back on offer (an Add row again)
	const row = rowOf(page, opText(addOp));
	await expect(row.getByRole('button', { name: 'Remove', exact: true })).toHaveCount(0);
	await expect(row.getByRole('button', { name: 'Add', exact: true })).toBeVisible();
	expect(Number(removed.previous_rev), 'the page showed the added rev').toBe(Number(added.draft_rev));
	await expect.poll(() => draftRev(page)).toBe(Number(removed.draft_rev));
	expect(Number(removed.draft_rev), 'the draft moved again').toBeGreaterThan(Number(added.draft_rev));
});

test('offers no own-operation writes to the staff agent, who holds no job designer role', async ({
	page,
}) => {
	const { addOp, archiveOp, template, version } = await setup();
	// one of the job's own operations, added through the API exactly as the Add button does
	const admin = await Api.as('admin');
	ok(await admin.put(ownPath(archiveOp), { template, version, identity_level: '2' }), `add ${archiveOp}`);
	await admin.dispose();

	// the tenant's support agent (support_agent only, on the job's staff team): the page and its data are a job
	// member's, the two writes need the Job designer role the agent does not have
	await signIn(page, 'agent', `${jobPath()}/operations`);
	const ops = section(page);
	await expect(ops).toBeVisible();
	const remove = rowOf(page, opText(archiveOp)).getByRole('button', { name: 'Remove', exact: true });
	const add = rowOf(page, opText(addOp)).getByRole('button', { name: 'Add', exact: true });
	await expect(remove).toBeDisabled();
	await expect(remove).toHaveAttribute('title', 'Needs the Job designer role');
	await expect(add).toBeDisabled();
	await expect(add).toHaveAttribute('title', 'Needs the Job designer role');
	await expect(ops.getByLabel(`Identity level for ${addOp}`, { exact: true })).toBeDisabled();
	// the rows are real, not an empty shell: the agent reads the own op and the template it came from
	await expect(rowOf(page, opText(archiveOp))).toContainText(`${template}@${version}`);
});
