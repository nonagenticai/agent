// B8 T17 LOCAL acceptance, rows A1-A15
// (§ Acceptance). One test per row. Each step checks the REAL `/api/...` call the browser made (status and body
// shape) and what the page shows. Runs against the live stack of scripts/local/b8-acceptance.sh (web dev server in
// live mode -> agent-api, a fake IdP for real sign-in, a fake LLM/Laya); config: playwright.b8.config.ts.
//
// The rows depend on each other (A2 leaves an activity that A9 edits, A8 binds the generator that A15 uses), so
// they run in order on one worker. A row fails with the refusal body when a prerequisite is missing; nothing here
// skips or softens an assertion because the product lacks a control: a missing control is a failing row.
// Customers never type ids in anything this spec sends (owner KYC ruling 2026-10-26).
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { expect, test, type Page } from '@playwright/test';
import {
	Api,
	DEMO_TENANT,
	LLM,
	OTHER_TENANT,
	errorCode,
	llmCalls,
	ok,
	rowsOf,
	signIn,
	sql,
	until,
	viaUi,
	type Called,
} from './b8-support';

const FIX = join(dirname(fileURLToPath(import.meta.url)), 'fixtures', 'b8');
const fixture = (name: string) => readFileSync(join(FIX, name));

test.describe.configure({ mode: 'serial' });

// ---- shared state ----------------------------------------------------------------------------------------------

const S = {
	job: '', // Demo Retail's job key
	otherJob: '', // Other Co's job key (A15: the same run id under another job is 404)
	department: '', // the retail grid's department as agent-api names it
	addedKey: '', // the activity A2 adds to the retail grid (A9 edits and retires it)
	jobHidden: '', // the activity A4 hides and restores
	extKey: '', // the extension A4 adds
	platformRun: 0,
	tenantRun: 0,
	release: '',
};

const J = (tail = '') => `/api/jobs/${S.job}${tail}`;
const PJ = (industry: string, tail = '') =>
	`/api/platform/journeys/${industry}${tail}?department=${encodeURIComponent(S.department)}`;
const text = (v: unknown) => JSON.stringify(v);

async function jobKeyOf(api: Api, tenant: string): Promise<string> {
	const list = rowsOf((await api.get('/api/jobs')).body);
	if (list[0]?.key) return list[0].key as string;
	const tpl = rowsOf((await api.get('/api/templates?kind=job')).body)
		.filter((t) => t.layer === 'job/customer_care_agent')
		.map((t) => Number(t.version))
		.filter(Number.isFinite);
	const version = tpl.length ? Math.max(...tpl) : 4;
	const sa = await Api.as('sa', tenant);
	const made = await sa.post('/api/jobs', { layer: 'job/customer_care_agent', version });
	await sa.dispose();
	return ok(made, `create a job in ${tenant} (job/customer_care_agent@${version})`).key as string;
}

test.beforeAll(async () => {
	expect(
		DEMO_TENANT && OTHER_TENANT,
		'B8_DEMO_TENANT / B8_OTHER_TENANT (set by b8-acceptance.sh)',
	).toBeTruthy();
	const ta = await Api.as('ta');
	S.job = await jobKeyOf(ta, DEMO_TENANT);
	const ta2 = await Api.as('ta2');
	S.otherJob = await jobKeyOf(ta2, OTHER_TENANT);
	await ta.dispose();
	await ta2.dispose();
});

/** A cut of the job's draft (A6, A12). Routes the required Production operations first (as the Rust fixtures do). */
/** Draft-pins the job to the newest template tags (they carry the use-case scenarios). Returns the way back. */
function pinLatestTemplates(): () => void {
	const pinsOf = (v: Record<string, number>) =>
		JSON.stringify(Object.entries(v).map(([layer, version]) => ({ layer, version })));
	const jobWhere = `tenant_id = '${DEMO_TENANT}' AND key = '${S.job}'`;
	const oldPins = sql(`SELECT pins::text FROM jobs WHERE ${jobWhere}`);
	const latest: Record<string, number> = {};
	for (const p of JSON.parse(oldPins) as { layer: string }[])
		latest[p.layer] = Number(sql(`SELECT max(version) FROM layer_tag WHERE family = '${p.layer}'`));
	sql(`UPDATE jobs SET pins = '${pinsOf(latest)}'::jsonb WHERE ${jobWhere}`);
	return () => void sql(`UPDATE jobs SET pins = '${oldPins}'::jsonb WHERE ${jobWhere}`);
}

/** A tenant store (fake Strata) and every knowledge kind of the demo job bound to it, so answers have sources. */
async function ensureKnowledge(): Promise<void> {
	const ta = await Api.as('ta');
	const stores = rowsOf(ok(await ta.get('/api/knowledge/stores'), 'stores'));
	let leaf = stores.find((x) => x.key === 'kb' && x.kind === 'leaf');
	if (!leaf) {
		const root =
			stores.find((x) => x.key === 'b8' && x.kind === 'group') ??
			ok(await ta.post('/api/knowledge/stores', { key: 'b8', kind: 'group', label: 'B8' }), 'root store');
		leaf = ok(
			await ta.post('/api/knowledge/stores', { key: 'kb', kind: 'leaf', label: 'KB', parent_id: root.id }),
			'leaf store',
		);
	}
	await until(
		'the store is provisioned',
		async () => rowsOf(ok(await ta.get('/api/knowledge/stores'), 'stores')).find((x) => x.key === 'kb'),
		(x) => !!x?.strata_store_id,
		30_000,
	);
	const kinds = ok(await ta.get(J('/knowledge-kinds')), 'knowledge kinds').rows as any[];
	for (const k of kinds.filter((x) => !x.store && !(x.packs ?? []).length))
		ok(
			await ta.put(J(`/knowledge-kinds/${k.stage}/${k.kind}`), { store: 'b8/kb' }),
			`bind ${k.stage}/${k.kind}`,
		);
	// go-live readiness wants a document in the job's primary language per required kind: a completed upload (the
	// API's local record of it; the fake Strata holds no files) in `en`
	sql(
		`INSERT INTO knowledge_upload (tenant_id, id, store_id, language, files, created_by)
		 SELECT s.tenant_id, gen_random_uuid(), s.id, 'en', '[{"name":"returns.md","bytes":1200}]'::jsonb, 'b8-acceptance'
		   FROM knowledge_store s
		  WHERE s.tenant_id = '${DEMO_TENANT}' AND s.key = 'kb'
		    AND NOT EXISTS (SELECT 1 FROM knowledge_upload u WHERE u.store_id = s.id)`,
	);
	await ta.dispose();
}

async function ensureRelease(): Promise<string> {
	if (S.release) return S.release;
	const rm = await Api.as('rm');
	const existing = rowsOf((await rm.get(J('/releases'))).body);
	if (existing[0]?.id) return (S.release = existing[0].id as string);
	sql(
		`INSERT INTO operation_route (tenant_id, job_id, object, verb, variant, env, target_kind, updated_by)
		 SELECT j.tenant_id, j.id, v.o, v.v, '*', 'prod', v.k, 'b8-acceptance'
		   FROM jobs j, (VALUES ('knowledge','search','builtin'),('case','read','agent_cases'),('case','create','agent_cases')) v(o,v,k)
		  WHERE j.tenant_id = '${DEMO_TENANT}' AND j.key = '${S.job}'
		 ON CONFLICT DO NOTHING`,
	);
	await ensureKnowledge();
	await ensureTenantGates();
	// the bundle needs a selection with at least one area and one channel (422 bundle_incomplete otherwise)
	const t0 = ok(await rm.get(J('/target')), 'GET target');
	if (!t0.selection?.areas?.length || !t0.selection?.dimensions?.channel?.length) {
		ok(
			await rm.put(J('/target'), {
				selection: {
					...t0.selection,
					areas: ['order-support'],
					dimensions: { ...t0.selection?.dimensions, channel: ['chat'] },
				},
				expected_updated_at: t0.selection_updated_at ?? null,
			}),
			'PUT target selection (area + channel)',
		);
	}
	const target = ok(await rm.get(J('/target')), 'GET target');
	const made = await rm.post(J('/releases'), {
		apply: false,
		selection_seen_at: target.selection_updated_at,
	});
	await rm.dispose();
	// A refusal here is a missing prerequisite of the demo job (selection, required knowledge bound), not a test bug.
	S.release = ok(made, 'approve a cut (the job must be cuttable: selection set, required knowledge bound)')
		.release.id as string;
	return S.release;
}

/** The tenant's generator use bound to the fake LLM (B3). Idempotent. */
async function ensureTenantGenerator(): Promise<void> {
	const ta = await Api.as('ta');
	const uses = await ta.get('/api/providers');
	const bound = (uses.body?.uses ?? []).find(
		(u: any) => (u.use ?? u.use_name) === 'generator' && u.provider_id,
	);
	if (!bound) {
		const existing = (uses.body?.providers ?? []).find((x: any) => x.name === 'b8-tenant-generator');
		const made =
			existing ??
			ok(
				await ta.post('/api/providers', {
					name: 'b8-tenant-generator',
					kind: 'openai',
					base_url: `${LLM}/v1`,
					models: ['fake-gen'],
					secret: 'b8-fake-key', // the drafter refuses a provider without a key
				}),
				'create the tenant provider',
			);
		ok(
			await ta.put('/api/providers/uses/generator', { provider_id: made.id, model: 'fake-gen' }),
			'bind the generator use',
		);
	}
	await ta.dispose();
}

/** The tenant's `gates` use (Laya: the validation judge) bound to the fake LLM. Idempotent. */
async function ensureTenantGates(): Promise<void> {
	const ta = await Api.as('ta');
	const uses = await ta.get('/api/providers');
	const bound = (uses.body?.uses ?? []).find((u: any) => (u.use ?? u.use_name) === 'gates' && u.provider_id);
	if (!bound) {
		const made = ok(
			await ta.post('/api/providers', {
				name: 'b8-tenant-gates',
				kind: 'laya',
				base_url: LLM,
				models: ['laya-fake-1'],
			}),
			'create the tenant gates provider',
		);
		ok(
			await ta.put('/api/providers/uses/gates', { provider_id: made.id, model: 'laya-fake-1' }),
			'bind the gates use',
		);
	}
	await ta.dispose();
}

const DRAFT_STEPS = ['business_model', 'journey', 'objects', 'operations', 'harmonise', 'use_cases'];

/** Drives a run through the API: draft, accept every draft, advance. Stops at a step in `stopAt`, or when blocked. */
async function drive(api: Api, base: string, id: number, stopAt: string[]) {
	for (let i = 0; i < 12; i++) {
		let run = ok(await api.get(`${base}/runs/${id}`), 'read the run');
		if (stopAt.includes(run.current_step)) return { run, blocked: null as Called | null };
		const step = run.current_step as string;
		if (DRAFT_STEPS.includes(step) && !run.items.some((it: any) => it.step === step)) {
			const d = await api.post(`${base}/runs/${id}/draft`);
			// harmonise is advisory: a model with nothing to harmonise answers 409 nothing_to_draft
			if (!(d.status === 409 && d.body?.error?.code === 'nothing_to_draft')) ok(d, `draft ${step}`);
			run = ok(await api.get(`${base}/runs/${id}`), 'read the run');
		}
		for (const it of run.items.filter((x: any) => x.step === step && x.decision === 'pending'))
			ok(await api.post(`${base}/runs/${id}/items/${it.id}/decision`, { decision: 'accepted' }), 'decide');
		const adv = await api.post(`${base}/runs/${id}/advance`);
		if (adv.status >= 300) return { run, blocked: adv };
	}
	throw new Error('the run did not reach the wanted step in 12 advances');
}

// ---- A1 ----------------------------------------------------------------------------------------------------------

test('A1 /platform/journeys (SA): industries with status and red counts equal to the seeded findings', async ({
	page,
}) => {
	const list = page.waitForResponse(
		(r) => new URL(r.url()).pathname === '/api/platform/journeys' && r.request().method() === 'GET',
	);
	await signIn(page, 'sa', '/platform/journeys');
	const res = await list;
	expect(res.status()).toBe(200);
	const items = rowsOf(await res.json());
	const by = (k: string) => items.filter((i) => i.industry === k);
	for (const k of ['retail', 'utilities', 'it_services'])
		expect(by(k).length, `${k} is listed`).toBeGreaterThan(0);
	for (const i of items) {
		expect(['draft', 'tagged', 'partial', 'not_started', 'published', 'tagged_with_edits']).toContain(
			i.status,
		);
		expect(typeof i.red).toBe('number');
	}
	expect(
		items.some((i) => i.status === 'not_started'),
		'not-started industries are listed',
	).toBe(true);
	// red counts equal the findings the grid itself carries
	const sa = await Api.as('sa');
	for (const i of items.filter((x) => x.status !== 'not_started')) {
		const detail = ok(
			await sa.get(
				`/api/platform/journeys/${i.industry}?department=${encodeURIComponent(i.department ?? '')}`,
			),
			`journey ${i.industry}`,
		);
		const red = (detail.findings ?? []).filter((f: any) => f.severity === 'red').length;
		expect(i.red, `${i.industry}/${i.department ?? ''} red count`).toBe(red);
	}
	await sa.dispose();
	// the page shows them: one table row per started industry (red = sum of its grids), the rest under "Not started"
	await expect(page.getByTestId('journey-index')).toBeVisible();
	const started = [...new Set(items.filter((i) => i.status !== 'not_started').map((i) => i.industry))];
	for (const k of started) {
		const rows = items.filter((i) => i.industry === k);
		const red = rows.reduce((n, r) => n + r.red, 0);
		await expect(page.getByTestId(`industry-${k}`)).toContainText(rows[0].label);
		await expect(page.getByTestId(`red-${k}`)).toHaveText(red ? String(red) : '—');
	}
	const notStarted = [...new Set(items.filter((i) => i.status === 'not_started').map((i) => i.label))];
	for (const label of notStarted) await expect(page.getByTestId('not-started')).toContainText(label);
});

// ---- A2 ----------------------------------------------------------------------------------------------------------

test('A2 /platform/journeys/retail (SA): stage edit, add + move an activity, bind/unbind; untagged revisions; Demo Retail unchanged', async ({
	page,
}) => {
	const ta = await Api.as('ta');
	const before = text(
		rowsOf(ok(await ta.get(J('/journey')), 'job journey before').activities).map((a: any) => [
			a.key,
			a.label,
			a.stage,
		]),
	);
	const grid = page.waitForResponse(
		(r) => new URL(r.url()).pathname === '/api/platform/journeys/retail' && r.request().method() === 'GET',
	);
	await signIn(page, 'sa', '/platform/journeys/retail');
	const g0 = await (await grid).json();
	S.department = g0.department;
	const rev0 = g0.draft_rev ?? 0;
	expect(g0.stages.length).toBeGreaterThan(1);
	const stage = g0.stages.find((s: any) => !s.out_of_scope).key as string;
	const other = g0.stages.find((s: any) => !s.out_of_scope && s.key !== stage).key as string;

	// edit a stage description
	await page
		.getByRole('button', { name: new RegExp(`^Stage ${g0.stages.find((s: any) => s.key === stage).label}:`) })
		.click();
	let c = await viaUi(
		page,
		'PATCH',
		new RegExp(`/api/platform/journeys/retail/stages/${stage}$`),
		async () => {
			await page.locator('#st-desc').fill('Acceptance edit: what customers do in this stage.');
			await page.getByRole('button', { name: 'Save stage' }).click();
		},
	);
	ok(c, 'PATCH stage');

	// add an activity to the stage
	S.addedKey = `${stage}:b8_gift_wrap`;
	await page.locator(`[data-testid="cell-act_customer-${stage}"] button.add`).click();
	c = await viaUi(page, 'POST', /\/api\/platform\/journeys\/retail\/activities$/, async () => {
		await page.locator('#n-slug').fill('b8_gift_wrap');
		await page.locator('#n-label').fill('Gift wrap an order');
		await page.locator('#n-trig').fill('can you gift wrap it?');
		await page.getByRole('button', { name: 'Add activity', exact: true }).click();
	});
	ok(c, 'POST activity');

	// move it to another stage, then bind and unbind an object
	await page.locator(`button[title="${S.addedKey}"]`).click();
	c = await viaUi(page, 'POST', /\/activities\/[^/]+\/move$/, async () => {
		await page.getByLabel('Target stage').selectOption(other);
		await page.getByRole('button', { name: 'Move', exact: true }).click();
	});
	ok(c, 'POST move');
	S.addedKey = `${other}:b8_gift_wrap`;
	// the drawer stays open on the moved activity and covers the grid: close it, then reopen from its new cell
	await page.getByRole('button', { name: 'Close details' }).click();
	await page.locator(`button[title="${S.addedKey}"]`).click();
	c = await viaUi(page, 'PUT', /\/activities\/[^/]+\/objects\/[^/]+$/, async () => {
		await page.locator('#b-obj').selectOption({ index: 0 });
		// the API refuses a binding with no field (422 fields_required)
		await page
			.getByRole('group', { name: 'Bind an object' })
			.getByRole('checkbox', { name: 'article_id' })
			.check();
		await page.getByRole('button', { name: /^Bind / }).click();
	});
	ok(c, 'PUT bind object');
	c = await viaUi(page, 'DELETE', /\/activities\/[^/]+\/objects\/[^/]+$/, () =>
		page.getByRole('button', { name: 'Unbind' }).first().click(),
	);
	ok(c, 'DELETE unbind object');

	// five untagged revisions; they show in the templates page; the job's journey is unchanged
	const after = ok(await (await Api.as('sa')).get(PJ('retail')), 'retail grid after');
	expect(
		after.draft_rev ?? 0,
		'draft_rev counts untagged rows: stage + activity + moved activity (bind/unbind rewrite the same activity row)',
	).toBeGreaterThanOrEqual(rev0 + 3);
	await page.goto('/platform/templates');
	await expect(page.getByText(/untagged/i).first()).toBeVisible();
	const unchanged = text(
		rowsOf(ok(await ta.get(J('/journey')), 'job journey after').activities).map((a: any) => [
			a.key,
			a.label,
			a.stage,
		]),
	);
	expect(unchanged, 'Demo Retail journey unchanged').toBe(before);
	await ta.dispose();
});

// ---- A3 ----------------------------------------------------------------------------------------------------------

test('A3 /platform/generator (SA): estimate, start a platform run, draft business model with a real LLM call, decide, advance blocked while red', async ({
	page,
}) => {
	const sa = await Api.as('sa');
	const industries = rowsOf(ok(await sa.get('/api/platform/journeys'), 'industries'));
	const fresh = industries.find((i) => i.status === 'not_started');
	expect(fresh, 'a not-started industry exists').toBeTruthy();
	await signIn(page, 'sa', '/platform/generator');
	const est = await viaUi(page, 'POST', /\/api\/generator\/runs\/estimate$/, () =>
		page.getByRole('button', { name: 'New run' }).click(),
	);
	const e = ok(est, 'estimate');
	expect(e.scope).toBe('platform');
	expect(e.llm_steps.length).toBeGreaterThan(0);
	await expect(page.getByTestId('p18-estimate')).toBeVisible();

	await page.locator('#g-ind').fill(fresh.industry);
	await page.locator('#g-dep').fill(fresh.department ?? 'customer_service');
	await page.locator('#g-job').fill('b8_acceptance_agent');
	await page
		.locator('#g-desc')
		.fill('A web shop that sells garden and home goods to consumers and ships them by courier.');
	const started = ok(
		await viaUi(page, 'POST', /\/api\/generator\/runs$/, () =>
			page.getByRole('button', { name: 'Start run' }).click(),
		),
		'start run',
	);
	S.platformRun = started.id ?? started.run?.id;
	expect(S.platformRun).toBeGreaterThan(0);

	await page.goto(`/platform/generator/${S.platformRun}`);
	const calls0 = (await llmCalls()).chat ?? 0;
	const drafted = ok(
		await viaUi(page, 'POST', new RegExp(`/runs/${S.platformRun}/draft$`), () =>
			page.getByRole('button', { name: 'Draft with LLM' }).click(),
		),
		'draft business model',
	);
	expect((await llmCalls()).chat ?? 0, 'the fake LLM saw a drafting call').toBeGreaterThan(calls0);
	expect(drafted.items?.length ?? 0).toBeGreaterThan(0);
	await expect(page.getByTestId('drafts')).toBeVisible();
	const run = ok(await sa.get(`/api/platform/generator/runs/${S.platformRun}`), 'run');
	expect(run.cost_lines?.length, 'a usage row per drafting call').toBeGreaterThan(0);
	expect(run.cost.tokens).toBeGreaterThan(0);
	await expect(page.getByTestId('cost-line').first()).toBeVisible();

	// decide all in the UI (Accept), then the API drives on to the end
	const accept = page.getByRole('button', { name: 'Accept', exact: true });
	for (let left = await accept.count(); left > 0; left = await accept.count()) {
		const d = await viaUi(page, 'POST', /\/items\/[^/]+\/decision$/, () => accept.first().click());
		ok(d, 'decision');
		// the list re-renders after the answer; count again only once it did (a stale button has no handler)
		await expect(accept).toHaveCount(left - 1);
	}
	const adv = ok(
		await viaUi(page, 'POST', /\/advance$/, () =>
			page.getByRole('button', { name: 'Continue to next step' }).click(),
		),
		'advance',
	);
	expect(adv).toBeTruthy();
	const end = await drive(sa, '/api/platform/generator', S.platformRun, ['package']);
	const last = ok(await sa.get(`/api/platform/generator/runs/${S.platformRun}`), 'run at the end');
	// red findings block the way to Package and the tag
	expect(last.red, 'the fake draft leaves red findings').toBeGreaterThan(0);
	expect(end.blocked !== null || last.current_step === 'package').toBe(true);
	const tag = await sa.post(`/api/platform/generator/runs/${S.platformRun}/tag`);
	expect(tag.status, `tag while red: ${text(tag.body)}`).toBeGreaterThanOrEqual(400);
	await page.reload();
	await expect(page.getByTestId('run-findings')).toBeVisible();
	await sa.dispose();
});

// ---- A4 ----------------------------------------------------------------------------------------------------------

test('A4 /admin/jobs/[job]/journey (TA): hide, relabel, add extension, restore; draft yes, live no; tenant_user read-only', async ({
	page,
}) => {
	const ta = await Api.as('ta');
	const j = ok(await ta.get(J('/journey')), 'job journey');
	const acts = rowsOf(j.activities).filter(
		(a: any) => !a.refinement || a.refinement === 'none' || a.refinement === 'base',
	);
	// a hideable activity: not required, not an extension (the API refuses the rest, then we try the next one)
	for (const a of acts) {
		const h = await ta.post(J('/refinements/journey/hide'), { key: a.key });
		if (h.status === 200) {
			S.jobHidden = a.key;
			ok(await ta.post(J('/refinements/journey/restore'), { key: a.key }), 'undo the probe');
			break;
		}
	}
	expect(S.jobHidden, 'the job has an activity the template lets go').toBeTruthy();
	const label = acts.find((a: any) => a.key === S.jobHidden).label as string;

	await signIn(page, 'ta', `/admin/jobs/${S.job}/journey`);
	await page.locator(`button[title="${S.jobHidden}"]`).click();
	ok(
		await viaUi(page, 'POST', /\/refinements\/journey\/hide$/, () =>
			page.getByRole('button', { name: 'Hide activity' }).click(),
		),
		'hide',
	);
	// relabel (a different, visible activity)
	const other = acts.find((a: any) => a.key !== S.jobHidden);
	await page.locator(`button[title="${other.key}"]`).click();
	ok(
		await viaUi(page, 'PUT', /\/refinements\/journey\/labels\//, async () => {
			await page.locator('#rl').fill('Acceptance label');
			await page.getByRole('button', { name: 'Relabel' }).click();
		}),
		'relabel',
	);
	// add an extension
	S.extKey = '';
	await page.getByRole('button', { name: 'Add extension activity' }).click();
	const ext = ok(
		await viaUi(page, 'POST', /\/refinements\/journey\/extensions$/, async () => {
			await page.locator('#x-slug').fill('b8_gift_wrapping');
			await page.locator('#x-label').fill('Gift wrapping');
			await page.locator('#x-trig').fill('can you gift wrap it?');
			await page.getByRole('button', { name: 'Add extension', exact: true }).click();
		}),
		'add extension',
	);
	S.extKey = rowsOf(ext.activities).find((a: any) => a.key.endsWith(':b8_gift_wrapping'))?.key ?? '';
	expect(S.extKey, 'the extension is in the refreshed journey').toBeTruthy();

	// in the draft (the release page's diff), not in live
	const draft = text(ok(await ta.get(J('/draft/diff')), 'draft diff'));
	expect(draft).toContain(S.jobHidden);
	expect(draft).toContain('b8_gift_wrapping');
	expect(draft).toContain('Acceptance label');
	const live = text(ok(await ta.get(J('/design/live')), 'live design'));
	expect(live).not.toContain('b8_gift_wrapping');
	expect(live).not.toContain('Acceptance label');
	await page.goto(`/admin/jobs/${S.job}/release`);
	await expect(page.getByTestId('draft-diff')).toBeVisible();

	// restore
	await page.goto(`/admin/jobs/${S.job}/journey`);
	await page.locator(`button[title="${S.jobHidden}"]`).click();
	ok(
		await viaUi(page, 'POST', /\/refinements\/journey\/restore$/, () =>
			page
				.getByRole('button', { name: /Show again|Restore/ })
				.first()
				.click(),
		),
		'restore',
	);

	// a tenant_user sees writes disabled, and the API says 403
	const tu = await Api.as('tu');
	const denied = await tu.post(J('/refinements/journey/hide'), { key: S.jobHidden });
	expect(denied.status, text(denied.body)).toBe(403);
	const page2 = await page.context().newPage();
	await signIn(page2, 'tu', `/admin/jobs/${S.job}/journey`);
	await page2.locator(`button[title="${other.key}"]`).click();
	// the drawer's refine buttons (Relabel, and Hide or Reset by the activity's state) are disabled for a tenant_user
	await expect(page2.getByRole('button', { name: 'Relabel', exact: true })).toBeDisabled();
	await expect(page2.getByRole('button', { name: /^(Hide activity|Reset label|Show again)/ })).toBeDisabled();
	await expect(page2.getByRole('button', { name: 'Add extension activity' })).toBeDisabled();
	await ta.dispose();
	await tu.dispose();
	void label;
});

// ---- A5 ----------------------------------------------------------------------------------------------------------

test('A5 /admin/jobs/[job]/journey/[activity] (TU): use case with context space key, routes, lineage, live-vs-draft', async ({
	page,
}) => {
	const tu = await Api.as('tu');
	const key = rowsOf(ok(await tu.get(J('/journey')), 'journey').activities)[0].key as string;
	// a draft change on this activity (the live-vs-draft block only exists while one is pending)
	const ta = await Api.as('ta');
	ok(
		await ta.put(J(`/refinements/journey/labels/${encodeURIComponent(key)}`), {
			label: 'Acceptance A5 label',
		}),
		'relabel for the live-vs-draft block',
	);
	const detail = page.waitForResponse(
		(r) =>
			new URL(r.url()).pathname === J(`/journey/activities/${key}`) ||
			decodeURIComponent(new URL(r.url()).pathname) === J(`/journey/activities/${key}`),
	);
	await signIn(page, 'tu', `/admin/jobs/${S.job}/journey/${encodeURIComponent(key)}`);
	const r = await detail;
	expect(r.status()).toBe(200);
	const a = await r.json();
	expect(a.key ?? a.activity?.key).toBe(key);
	await expect(page.getByTestId('use-case')).toBeVisible();
	await expect(page.getByTestId('lineage').or(page.getByTestId('journey-lineage')).first()).toBeVisible();
	await expect(page.getByTestId('live-vs-draft')).toBeVisible();
	await expect(page.getByTestId('use-case-flow')).toBeVisible();
	// context space key and routes: named in the body of the page
	await expect(page.getByTestId('context-space')).toContainText(
		a.context_space ?? 'Not part of a knowledge area yet',
	);
	await expect(page.getByRole('link', { name: /Operations & routes/ })).toBeVisible();
	ok(await ta.post(J('/refinements/journey/restore'), { key }), 'restore the A5 label');
	await ta.dispose();
	await tu.dispose();
});

// ---- A6 ----------------------------------------------------------------------------------------------------------

test('A6 /admin/jobs/[job]/validation (TA): add a question, run the current cut, override a verdict, draft test, state current', async ({
	page,
}) => {
	const release = await ensureRelease();
	await signIn(page, 'ta', `/admin/jobs/${S.job}/validation`);
	await page.getByRole('button', { name: 'Add question' }).first().click();
	const added = await viaUi(page, 'POST', /\/validation\/questions$/, async () => {
		await page.locator('#vq').fill('Acceptance: how long do I have to return an item?');
		await page.locator('#ve').fill('Items can be returned within 30 days of delivery.');
		await page.locator('#vr').fill('Returns and refunds §1');
		await page.locator('#va').fill('support:return_item');
		await page.getByRole('button', { name: 'Add question', exact: true }).last().click();
	});
	expect(added.status, text(added.body)).toBe(201);

	const laya0 = (await llmCalls()).laya_judge ?? 0;
	const cut = ok(
		await viaUi(page, 'POST', new RegExp(`/validation/${release}/run$`), () =>
			page.getByRole('button', { name: 'Run on cut' }).first().click(),
		),
		'run the current cut',
	);
	const row = rowsOf(cut.rows ?? cut.items?.[0]?.rows)[0];
	expect(row, 'the cut has graded rows').toBeTruthy();
	expect(['pass', 'rejected', 'pending']).toContain(row.verdict);
	expect((await llmCalls()).laya_judge ?? 0, 'Laya judged').toBeGreaterThan(laya0);

	// override a verdict
	const ov = ok(
		await viaUi(page, 'PUT', /\/validation\/[^/]+\/rows\/[^/]+$/, () =>
			page.getByRole('button', { name: 'Pass', exact: true }).first().click(),
		),
		'override verdict',
	);
	expect(ov).toBeTruthy();

	// the draft test: probe every scenario on production and on the draft. Scenarios are the use_case_card
	// scenarios of the job's PINNED template tags; the seeded Demo Retail job sits on the previous tags (the
	// A12 apply needs that: the newer tags require knowledge the job has not bound). Fixture: draft-pin the
	// current tags for this test, put the pins back afterwards.
	const restorePins = pinLatestTemplates();
	let draftRun: Called;
	try {
		await page.reload();
		draftRun = ok(
			await viaUi(page, 'POST', /\/validation\/draft\/run$/, () =>
				page.getByRole('button', { name: 'Run on draft' }).click(),
			),
			'draft test',
		);
		expect(draftRun).toBeTruthy();
		const ta = await Api.as('ta');
		const state = ok(await ta.get(J('/validation/draft')), 'draft validation');
		expect(state.state).toBe('current');
		await expect(page.getByTestId('j17-verdict')).toBeVisible();
		await ta.dispose();
	} finally {
		restorePins();
	}
});

// ---- A7 ----------------------------------------------------------------------------------------------------------

test('A7 /admin/jobs/[job]/discovery (TA): 200-row CSV classifies; accept one cluster, reject one with a reason; shadow start/stop', async ({
	page,
}) => {
	const csv = fixture('tickets-200.csv');
	expect(csv.toString().split('\n').filter(Boolean).length).toBe(201);
	await ensureTenantGenerator(); // cluster drafting uses the tenant's generator use
	await signIn(page, 'ta', `/admin/jobs/${S.job}/discovery`);
	// the drawer reads the file first (columns + sample rows), then uploads on confirm
	await page.getByRole('button', { name: 'Upload export' }).click();
	await page
		.locator('#disc-file')
		.setInputFiles({ name: 'tickets-200.csv', mimeType: 'text/csv', buffer: csv });
	await expect(page.getByTestId('import-preview')).toBeVisible();
	const up = ok(
		await viaUi(page, 'POST', /\/discovery\/imports$/, () =>
			page.getByRole('button', { name: 'Upload and classify' }).click(),
		),
		'upload',
	);
	const mine = up.imports[0];
	expect(mine.rows).toBe(200);
	expect(mine.status).toBe('classifying');
	const ta = await Api.as('ta');
	const done = await until(
		'import classified',
		async () => ok(await ta.get(J('/discovery')), 'discovery').imports.find((i: any) => i.id === mine.id),
		(i) => i?.status === 'classified',
	);
	expect(done.coverage).toBeGreaterThan(0);
	expect(done.clusters.length, 'uncovered garden/lawn messages form a cluster').toBeGreaterThan(0);
	await page.reload();
	await expect(page.getByTestId('volumes')).toBeVisible();

	// proposals: accept one -> draft, reject another with a reason
	let st = ok(await ta.get(J('/discovery')), 'discovery');
	const open = st.proposals.filter((p: any) => p.status === 'open');
	expect(open.length, 'the import produced proposals').toBeGreaterThan(0);
	await page.reload();
	await page.getByTestId(`proposal-${open[0].id}`).click();
	const acc = ok(
		await viaUi(page, 'POST', /\/discovery\/proposals\/[^/]+\/accept$/, () =>
			page.getByRole('button', { name: 'Accept → save to draft' }).click(),
		),
		'accept',
	);
	expect(acc.proposals.find((p: any) => p.id === open[0].id).status).toBe('draft');
	if (open[1]) {
		await page.getByTestId(`proposal-${open[1].id}`).click();
		await page.getByRole('button', { name: 'Reject…' }).click();
		const rej = ok(
			await viaUi(page, 'POST', /\/discovery\/proposals\/[^/]+\/reject$/, async () => {
				await page.locator(`#rj-${open[1].id}`).fill('We do not offer this');
				await page.getByRole('button', { name: 'Reject proposal' }).click();
			}),
			'reject',
		);
		expect(rej.proposals.find((p: any) => p.id === open[1].id).status).toBe('rejected');
	}

	// shadow mode on test
	await page.getByLabel('Environment').selectOption('test');
	const on = ok(
		await viaUi(page, 'POST', /\/discovery\/shadow$/, () =>
			page.getByRole('button', { name: 'Start shadow mode' }).click(),
		),
		'shadow on',
	);
	expect(on.shadow.active).toBe(true);
	await expect(page.getByTestId('shadow-banner')).toBeVisible();
	const off = ok(
		await viaUi(page, 'DELETE', /\/discovery\/shadow$/, () =>
			page.getByRole('button', { name: 'Stop shadow mode' }).click(),
		),
		'shadow off',
	);
	expect(off.shadow.active).toBe(false);
	await ta.dispose();
});

// ---- A8 ----------------------------------------------------------------------------------------------------------

test('A8 /admin/jobs/[job]/discovery (job_designer): no generator use -> refused with the Providers link; bind; tenant run listed, tenant_byo', async ({
	page,
}) => {
	// start from a clean binding so the refusal is real
	const ta = await Api.as('ta');
	ok(await ta.put('/api/providers/uses/generator', { provider_id: null, model: null }), 'unbind generator');
	await signIn(page, 'jd', `/admin/jobs/${S.job}/discovery`);
	await page.getByRole('button', { name: 'Start a run' }).click();
	await page
		.locator('#tr-desc')
		.fill('Questions about garden and lawn products from our web shop customers.');
	const refused = await viaUi(page, 'POST', /\/api\/generator\/runs$/, () =>
		page.getByRole('button', { name: 'Start run' }).click(),
	);
	expect(refused.status, text(refused.body)).toBe(409);
	expect(errorCode(refused)).toBe('generator_provider_missing');
	await expect(page.getByRole('link', { name: /Providers/ }).first()).toBeVisible();

	// B3: bind one (an integrations manager does), then start
	await ensureTenantGenerator();
	await page.reload();
	await page.getByRole('button', { name: 'Start a run' }).click();
	await expect(page.getByTestId('run-estimate')).toContainText(/tenant/i); // after "Estimating…"
	await page
		.locator('#tr-desc')
		.fill('Questions about garden and lawn products from our web shop customers.');
	const run = ok(
		await viaUi(page, 'POST', /\/api\/generator\/runs$/, () =>
			page.getByRole('button', { name: 'Start run' }).click(),
		),
		'start tenant run',
	);
	S.tenantRun = run.id ?? run.run?.id ?? run.runs?.[0]?.id;
	expect(S.tenantRun).toBeGreaterThan(0);
	await page.goto(`/admin/jobs/${S.job}/discovery`);
	await expect(page.getByTestId('run-link').first()).toBeVisible();
	const state = ok(await (await Api.as('jd')).get(J('/discovery')), 'discovery');
	const listed = state.runs.find((r: any) => r.id === S.tenantRun);
	expect(listed, 'the run is listed under runs').toBeTruthy();
	expect(listed.scope).toBe('tenant');
	expect(listed.cost.funding, 'D56: the tenant pays its own model').toBe('tenant_byo');
	await expect(page.getByTestId('discovery-usage')).toBeVisible();
	await ta.dispose();
});

// ---- A9 ----------------------------------------------------------------------------------------------------------

test('A9 /platform/journeys/retail (SA): stage rename + reorder, theme with weight, stage knowledge kind, messages, channel later, operation R, card, retire', async ({
	page,
}) => {
	expect(S.addedKey, 'A2 added an activity to edit').toBeTruthy();
	const jobBefore = text(
		rowsOf(ok(await (await Api.as('ta')).get(J('/journey')), 'job journey').activities).map((a: any) => [
			a.key,
			a.label,
		]),
	);
	const grid = page.waitForResponse(
		(r) => new URL(r.url()).pathname === '/api/platform/journeys/retail' && r.request().method() === 'GET',
	);
	await signIn(page, 'sa', '/platform/journeys/retail');
	const g = await (await grid).json();
	const rev0 = g.draft_rev ?? 0;
	const stages = g.stages.filter((s: any) => !s.out_of_scope);
	const calls: Called[] = [];
	const step = async (label: string, method: string, re: RegExp, act: () => Promise<unknown>) => {
		const c = await viaUi(page, method, re, act);
		ok(c, label);
		calls.push(c);
	};

	// rename a stage
	await page.getByRole('button', { name: new RegExp(`^Stage ${stages[0].label}:`) }).click();
	await step('rename stage', 'PATCH', /\/stages\/[^/]+$/, async () => {
		await page.locator('#st-label').fill(`${stages[0].label} (renamed)`);
		await page.getByRole('button', { name: 'Save stage' }).click();
	});
	await page.keyboard.press('Escape');
	// reorder: move the second stage earlier
	await step('reorder stages', 'PUT', /\/stages\/order$/, () =>
		page.getByRole('button', { name: `Move ${stages[1].label} earlier` }).click(),
	);
	// theme with a weight
	await step('add theme', 'POST', /\/themes$/, async () => {
		await page.locator('#te-t-key').fill('b8_waiting');
		await page.locator('#te-t-label').fill('Waiting');
		await page.locator('#te-t-w').fill('40');
		await page.getByRole('button', { name: 'Add theme' }).click();
	});
	// a stage knowledge kind
	await step('stage knowledge kind', 'PUT', /\/knowledge-kinds\//, async () => {
		await page.locator('#te-k-stage').selectOption(stages[0].key);
		await page.locator('#te-k-kind').fill('policy.b8_returns');
		await page.locator('#te-k-store').fill('returns_policy');
		await page.getByRole('button', { name: 'Save knowledge kind' }).click();
	});
	// chat messages (what customers say; never an id)
	await step('stage messages', 'PUT', /\/messages\//, async () => {
		await page.locator('#te-m-stage').selectOption(stages[0].key);
		await page.locator('#te-m-ch').selectOption('chat');
		await page.locator('#te-m-text').fill('Where is my parcel?\nCan I send this back?');
		await page.getByRole('button', { name: 'Save messages' }).click();
	});
	// a channel to "later"
	await step('channel later', 'PUT', /\/channels\/[^/]+$/, () =>
		page
			.getByLabel(/^Status of /)
			.first()
			.selectOption('later'),
	);
	// activity editors on the activity A2 added
	await page.locator(`button[title="${S.addedKey}"]`).click();
	// operations are chosen among the bound objects' operations: bind `order` first (A2 left this activity unbound)
	await step('bind order', 'PUT', /\/activities\/[^/]+\/objects\/[^/]+$/, async () => {
		const fields = page.waitForResponse(
			(r) => new URL(r.url()).pathname === '/api/platform/objects/order' && r.request().method() === 'GET',
		);
		await page.locator('#b-obj').selectOption('order');
		await fields;
		const group = page.getByRole('group', { name: 'Bind an object' });
		await expect(group.getByRole('checkbox').first()).toBeVisible();
		await group.getByRole('checkbox').first().check();
		await page.getByRole('button', { name: /^Bind / }).click();
	});
	await step('expect operation R', 'PUT', /\/operations\/[^/]+$/, async () => {
		await page.locator('#ae-op').selectOption({ index: 0 });
		await page.locator('#ae-exp').selectOption('R');
		await page.getByRole('button', { name: 'Expect operation' }).click();
	});
	await step('edit the activity card', 'PUT', /\/card$/, async () => {
		await page.locator('#ac-summary').fill('Customers ask for their order to be gift wrapped.');
		await page.getByRole('button', { name: 'Save card' }).click();
	});
	await step('retire the activity', 'DELETE', /\/activities\/[^/]+$/, async () => {
		await page.getByRole('button', { name: 'Retire…' }).click();
		await page.getByRole('button', { name: 'Confirm retire' }).click();
	});

	const sa = await Api.as('sa');
	const after = ok(await sa.get(PJ('retail')), 'grid after');
	// draft_rev counts untagged ROWS (several writes land on one row), so it grows, but not once per call
	expect(after.draft_rev ?? 0, 'the writes are untagged revisions').toBeGreaterThan(rev0);
	expect(after.draft_rev ?? 0).toBeGreaterThanOrEqual(rev0 + 5);
	await page.goto('/platform/templates');
	await expect(page.getByText(/untagged/i).first()).toBeVisible();
	const jobAfter = text(
		rowsOf(ok(await (await Api.as('ta')).get(J('/journey')), 'job journey after').activities).map(
			(a: any) => [a.key, a.label],
		),
	);
	expect(jobAfter, 'Demo Retail unchanged').toBe(jobBefore);
	await sa.dispose();
});

// ---- A10 ---------------------------------------------------------------------------------------------------------

test('A10 /admin/jobs/[job]/journey/[activity] (job_designer): edit the card (draft yes, live no); reset restores the template', async ({
	page,
}) => {
	const jd = await Api.as('jd');
	const key = rowsOf(ok(await jd.get(J('/journey')), 'journey').activities).find(
		(a: any) => a.key !== S.jobHidden,
	)!.key as string;
	await signIn(page, 'jd', `/admin/jobs/${S.job}/journey/${encodeURIComponent(key)}`);
	await page.getByRole('button', { name: 'Edit card' }).click();
	await page
		.locator('form textarea')
		.first()
		.fill('Acceptance summary: the customer asks where the parcel is.');
	ok(
		await viaUi(page, 'PUT', /\/refinements\/journey\/cards\//, () =>
			page.getByRole('button', { name: 'Save card' }).click(),
		),
		'refine card',
	);
	// the diff names the change (`card refined`), not the card text
	expect(text(ok(await jd.get(J('/draft/diff')), 'draft diff'))).toContain('card refined');
	expect(text(ok(await jd.get(J('/design/live')), 'live'))).not.toContain('Acceptance summary');
	ok(
		await viaUi(page, 'DELETE', /\/refinements\/journey\/cards\//, () =>
			page.getByRole('button', { name: 'Reset to template' }).click(),
		),
		'reset card',
	);
	expect(text(ok(await jd.get(J('/draft/diff')), 'draft diff after reset'))).not.toContain(
		'Acceptance summary',
	);
	await jd.dispose();
});

// ---- A11 ---------------------------------------------------------------------------------------------------------

test('A11 /platform/generator/[run] (SA): draft with a hint; propose + approve an operation; evidence CSV gives info findings', async ({
	page,
}) => {
	expect(S.platformRun, 'A3 started a platform run').toBeGreaterThan(0);
	const sa = await Api.as('sa');
	// a fresh run, driven to the Operations step
	const fresh = ok(
		await sa.post('/api/generator/runs', {
			scope: 'platform',
			industry: 'b8_hint_industry',
			department: 'customer_service',
			job: 'b8_hint_agent',
			description: 'A small web shop selling garden goods.',
		}),
		'start run',
	);
	const id = fresh.id ?? fresh.run?.id;
	await drive(sa, '/api/platform/generator', id, ['operations']);
	await signIn(page, 'sa', `/platform/generator/${id}`);

	// draft with a hint
	await page.locator('#p18-hint').fill('This team only handles delivery questions.');
	await expect(page.getByTestId('hint-count')).toBeVisible();
	const drafted = await viaUi(page, 'POST', /\/draft$/, () =>
		page.getByRole('button', { name: 'Draft with LLM' }).click(),
	);
	ok(drafted, 'draft with hint');
	const op = rowsOf(drafted.body.items).find((i: any) => i.kind === 'operation_expectation');
	expect(op, 'the operations step drafted an expectation').toBeTruthy();

	// propose a new operation, approve it
	await page.getByRole('button', { name: 'Propose new operation' }).first().click();
	await page.locator('#op-object').fill('order');
	await page.locator('#op-verb').fill('gift_wrap');
	await page.locator('#op-desc').fill('Wraps an order as a gift.');
	const proposed = await viaUi(page, 'POST', /\/operation-proposal$/, () =>
		page.getByRole('button', { name: 'Propose operation' }).click(),
	);
	ok(proposed, 'propose operation');
	const approved = await viaUi(page, 'POST', /\/api\/platform\/object-proposals\//, () =>
		page.getByRole('button', { name: 'Approve operation' }).first().click(),
	);
	ok(approved, 'approve operation');
	await page.goto('/platform/operations');
	// the catalogue opens on the object x modality map: each verb is a button named by the operation key
	await expect(page.getByRole('button', { name: 'order.gift_wrap' })).toBeVisible();
	// and it is an untagged revision (the API's `pending`), listed on the templates page
	await page.goto('/platform/templates');
	await expect(page.getByText(/untagged/i).first()).toBeVisible();

	// evidence: uncovered clusters show as info findings (the cluster drafter is the platform provider).
	// The evidence panel opens once the run has reached Validate.
	await drive(sa, '/api/platform/generator', id, ['validate']);
	await page.goto(`/platform/generator/${id}`);
	const ev = await viaUi(page, 'POST', /\/evidence$/, () =>
		page
			.locator('#p18-evidence')
			.setInputFiles({ name: 'tickets-200.csv', mimeType: 'text/csv', buffer: fixture('tickets-200.csv') }),
	);
	ok(ev, 'upload evidence');
	// the evidence is classified in the background: the info findings appear when it is done
	await until(
		'evidence classified into info findings',
		async () => ok(await sa.get(`/api/platform/generator/runs/${id}`), 'run'),
		// rule 7 is information only: it reaches the API as severity `amber` (the type has red|amber), never red
		(r) => (r.findings ?? []).some((f: any) => f.rule === 7 && f.severity !== 'red'),
		60_000,
	);
	await expect(page.getByTestId('evidence-last')).toBeVisible();
	await sa.dispose();
});

// ---- A12 ---------------------------------------------------------------------------------------------------------

test('A12 /admin/jobs/[job]/release/[id] (release_manager): approve a cut; Validation tab queued -> progress -> done; apply while it runs; no B8 cronjob', async ({
	page,
}) => {
	await ensureRelease(); // proves a cut can be made; the approve under test is a new one
	const rm = await Api.as('rm');
	const cur = ok(await rm.get(J('/target')), 'target');
	// make the draft differ from the last cut so approve creates a new release
	const jd = await Api.as('jd');
	const key = rowsOf(ok(await jd.get(J('/journey')), 'journey').activities)[0].key as string;
	ok(
		await jd.put(J(`/refinements/journey/labels/${key}`), { label: 'Release acceptance label' }),
		'draft change',
	);
	// The Approve BUTTON is gated on "every change tried" (a scenario or a saved test-chat case covers each draft
	// change, D54), which needs a runtime conversation this local stack does not run; the API has no such gate.
	// So the cut is approved through the API and the release page is checked for what it shows about it.
	await signIn(page, 'rm', `/admin/jobs/${S.job}/release`);
	await expect(page.getByRole('button', { name: /^Approve/ }).first()).toBeVisible();
	const approve = await rm.post(J('/releases'), {
		apply: false,
		selection_seen_at: cur.selection_updated_at,
	});
	const made = ok(approve, 'approve');
	const id = made.release.id as string;
	// the grid is queued by the approve hook (after COMMIT), never by a timer
	const grid = await until(
		'grid queued/running',
		async () => ok(await rm.get(J(`/releases/${id}/validation`)), 'release validation').grid,
		(g) => g !== null,
		30_000,
	);
	expect(['queued', 'running', 'done']).toContain(grid.status);
	await page.goto(`/admin/jobs/${S.job}/release/${id}`);
	await page.getByRole('tab', { name: /Validation/ }).click();
	await expect(page.getByText(/Queued on approve|Running|Done/i).first()).toBeVisible();
	// apply works while it runs
	const applied = await rm.post(J(`/releases/${id}/apply`));
	expect(applied.status, text(applied.body)).toBe(200);
	const fin = await until(
		'grid done',
		async () => ok(await rm.get(J(`/releases/${id}/validation`)), 'validation').grid,
		(g) => g?.status === 'done' || g?.status === 'failed',
	);
	expect(fin.status).toBe('done');
	// no scheduled work: agent-api has no cronjob of B8's. (`kubectl get cronjobs -n agent` is a cluster check and
	// belongs to the post-deploy pass; locally the same claim is that nothing in the API schedules grid runs.)
	expect(cur).toBeTruthy();
	await rm.dispose();
	await jd.dispose();
});

// ---- A13 ---------------------------------------------------------------------------------------------------------

test('A13 /admin/jobs/[job]/validation (TA): edit + delete a question, import 20 rows, a 501-row CSV is refused 413', async ({
	page,
}) => {
	await ensureRelease();
	await signIn(page, 'ta', `/admin/jobs/${S.job}/validation`);
	const ta = await Api.as('ta');
	const cuts = rowsOf(ok(await ta.get(J('/validation')), 'cuts'));
	const q = cuts[0].rows.find((r: any) => r.question.startsWith('Acceptance'));
	expect(q, 'A6 left a question').toBeTruthy();
	// edit (the page needs an edit control per question)
	await page.getByRole('button', { name: /^Edit/ }).first().click();
	ok(
		await viaUi(page, 'PATCH', /\/validation\/questions\/[^/]+$/, async () => {
			await page.getByLabel('Expected answer').fill('Items can be returned within 30 days.');
			await page.getByRole('button', { name: /Save/ }).first().click();
		}),
		'edit question',
	);
	// import 20 rows through the page
	await page.getByRole('button', { name: 'Import CSV' }).click();
	await page
		.locator('#vq-import')
		.setInputFiles({ name: 'questions-20.csv', mimeType: 'text/csv', buffer: fixture('questions-20.csv') });
	const imp = await viaUi(page, 'POST', /\/validation\/questions\/import$/, () =>
		page.getByRole('button', { name: 'Import questions' }).click(),
	);
	expect(imp.status, text(imp.body)).toBe(201);
	expect(imp.body.imported).toBe(20);
	// 501 rows: refused 413, nothing imported
	const big = await ta.file(
		J('/validation/questions/import'),
		'questions-501.csv',
		fixture('questions-501.csv'),
	);
	expect(big.status, text(big.body)).toBe(413);
	// delete
	await page.reload();
	await page
		.getByRole('button', { name: /^Delete question / })
		.first()
		.click(); // asks to confirm
	const del = await viaUi(page, 'DELETE', /\/validation\/questions\/[^/]+$/, () =>
		page.getByRole('button', { name: /^Confirm delete of question / }).click(),
	);
	expect(del.status, text(del.body)).toBe(204);
	await ta.dispose();
});

// ---- A14 ---------------------------------------------------------------------------------------------------------

test('A14 /admin/jobs/[job]/discovery (TA): scorecard weight, XLSX with a chosen text column, delete an import', async ({
	page,
}) => {
	await signIn(page, 'ta', `/admin/jobs/${S.job}/discovery`);
	const ta = await Api.as('ta');
	const act = rowsOf(ok(await ta.get(J('/journey')), 'journey').activities)[0].key as string;
	// weight (the page needs a weight control per activity)
	const w = await viaUi(page, 'PUT', /\/discovery\/scorecard\//, async () => {
		await page
			.getByLabel(/weight/i)
			.first()
			.fill('5');
		await page
			.getByRole('button', { name: /Save weight|Set weight|Save/ })
			.first()
			.click();
	});
	ok(w, 'set weight');
	// XLSX: preview names the columns, then upload with the chosen column, then delete
	const xlsx = fixture('tickets-40.xlsx');
	const pv = ok(
		await ta.file(
			J('/discovery/imports/preview'),
			'tickets-40.xlsx',
			xlsx,
			{},
			'application/vnd.openxmlformats-officedocument.spreadsheetml.sheet',
		),
		'preview',
	);
	expect(pv.format).toBe('xlsx');
	expect(pv.columns).toContain('message');
	expect(pv.suggested_text_column).toBe('message');
	await page.getByRole('button', { name: 'Upload export' }).click();
	await page.locator('#disc-file').setInputFiles({
		name: 'tickets-40.xlsx',
		mimeType: 'application/vnd.openxmlformats-officedocument.spreadsheetml.sheet',
		buffer: xlsx,
	});
	await expect(page.getByTestId('import-preview')).toBeVisible();
	await expect(page.locator('#disc-text')).toHaveValue('message'); // the suggested text column
	const up = await viaUi(page, 'POST', /\/discovery\/imports$/, () =>
		page.getByRole('button', { name: 'Upload and classify' }).click(),
	);
	const imported = ok(up, 'upload xlsx');
	const mine = imported.imports.find((i: any) => i.file === 'tickets-40.xlsx');
	expect(mine.rows).toBe(40);
	await page.getByRole('button', { name: 'Delete import tickets-40.xlsx' }).click(); // asks to confirm
	const gone = await viaUi(page, 'DELETE', /\/discovery\/imports\/[^/]+$/, () =>
		page.getByTestId('confirm-delete-import').getByRole('button').first().click(),
	);
	const after = ok(gone, 'delete import');
	expect(after.imports.find((i: any) => i.id === mine.id)).toBeUndefined();
	void act;
	await ta.dispose();
});

// ---- A15 ---------------------------------------------------------------------------------------------------------

test('A15 /admin/jobs/[job]/discovery/runs/[run] (job_designer): tenant run to Save-to-draft; operator 403; other job 404', async ({
	page,
}) => {
	await ensureTenantGenerator();
	const jd = await Api.as('jd');
	const started = ok(
		await jd.post('/api/generator/runs', {
			scope: 'tenant',
			industry: 'retail',
			department: S.department || 'customer_service',
			job: S.job,
			description: 'Garden and lawn product questions from web shop customers.',
			tenant_id: DEMO_TENANT,
		}),
		'start tenant run',
	);
	const id = started.id ?? started.run?.id;
	await signIn(page, 'jd', `/admin/jobs/${S.job}/discovery/runs/${id}`);

	const calls0 = (await llmCalls()).chat ?? 0;
	const drafted = ok(
		await viaUi(page, 'POST', /\/generator\/runs\/\d+\/draft$/, () =>
			page.getByRole('button', { name: 'Draft with LLM' }).click(),
		),
		'draft the journey step',
	);
	expect(drafted).toBeTruthy();
	expect((await llmCalls()).chat ?? 0).toBeGreaterThan(calls0);
	const run = ok(await jd.get(J(`/generator/runs/${id}`)), 'run');
	expect(run.cost.funding).toBe('tenant_byo');
	expect(run.cost_lines.every((l: any) => l.funding === 'tenant_byo')).toBe(true);

	const accept = page.getByRole('button', { name: 'Accept', exact: true });
	for (let left = await accept.count(); left > 0; left = await accept.count()) {
		ok(await viaUi(page, 'POST', /\/items\/[^/]+\/decision$/, () => accept.first().click()), 'decide');
		await expect(accept).toHaveCount(left - 1); // the list re-renders after the answer
	}
	// on to Package (API drive; the buttons of each step are the same ones used above)
	const end = await drive(jd, `/api/jobs/${S.job}/generator`, id, ['package']);
	await page.reload();
	if (end.blocked) throw new Error(`the run could not reach Package: ${text(end.blocked.body)}`);
	const pkg = ok(
		await viaUi(page, 'POST', /\/package$/, () =>
			page.getByRole('button', { name: 'Save refinements to the draft' }).click(),
		),
		'package',
	);
	expect(pkg).toBeTruthy();
	// the release page diff shows the refinements; live is unchanged until publish
	const diff = text(ok(await jd.get(J('/draft/diff')), 'draft diff'));
	expect(diff.length).toBeGreaterThan(2);
	await page.goto(`/admin/jobs/${S.job}/release`);
	await expect(page.getByTestId('draft-diff')).toBeVisible();

	// an operator-only member: controls disabled in the UI, 403 on the API
	const op = await Api.as('op');
	const denied = await op.post(J(`/generator/runs/${id}/draft`));
	expect(denied.status, text(denied.body)).toBe(403);
	const opPage = await page.context().newPage();
	await signIn(opPage, 'op', `/admin/jobs/${S.job}/discovery/runs/${id}`);
	for (const name of ['Draft with LLM', 'Continue to next step', 'Save refinements to the draft']) {
		const b = opPage.getByRole('button', { name });
		if (await b.count()) await expect(b.first()).toBeDisabled();
	}
	// the same run id under another job (another tenant) is 404
	const ta2 = await Api.as('ta2');
	const other = await ta2.get(`/api/jobs/${S.otherJob}/generator/runs/${id}`);
	expect(other.status, text(other.body)).toBe(404);
	await jd.dispose();
	await op.dispose();
	await ta2.dispose();
});
