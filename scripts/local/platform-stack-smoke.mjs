#!/usr/bin/env node
// Headless run of the OWNER's product journey against the running platform stack (scripts/local/platform-stack.sh
// smoke | onboard). Nothing is pre-built: no demo tenant, no recreate.ts. Every call is one a real UI makes.
//
//   0  platform admin (sa) authors the minimum platform offering on the EMPTY database
//        POST /api/layers, POST /api/layers/{id}/rows, POST /api/platform/knowledge/stores|documents|packs,
//        POST /api/platform/templates/{layer}/tags; GET /api/platform/connector-templates (seed-only gap check)
//   1a a customer signs up (intake service token) and sa approves it: POST /api/signup-approvals/intake, GET /api/signup-approvals,
//      POST /api/signup-approvals/{id}/approve -> tenant + tenant_admin identity + seat. No env edit, no restart.
//   1  tenant admin (ta, no extra roles) sets the company profile, then creates a job from a published template, else a BLANK job
//        GET/PUT /api/tenant/profile, GET /api/job-templates, POST /api/jobs {template,...} | {key,label,industry,departments}, PATCH /api/jobs/{job}
//   2  knowledge + procedure: platform packs, own documents, an authored procedure
//        GET /api/knowledge/platform-packs, GET/PUT /api/jobs/{job}/knowledge-kinds[/{stage}/{kind}],
//        POST /api/knowledge/stores, POST /api/knowledge/stores/{id}/upload,
//        POST /api/jobs/{job}/procedures, POST .../procedures/{name}/versions/{v}/status
//   3  integration: connector instances from the demo_retail template pointed at demo-backends, credential, contract
//      tests, routes, demo data
//        GET /api/connections/catalogue, POST /api/connections/systems, PUT .../{id}/credential,
//        POST /api/jobs/{job}/demo/reset, GET /api/jobs/{job}/operations, PUT .../operations/{op}/routes/{env},
//        POST .../systems/{id}/contract-tests
//   4  channels: web chat binding (+ identity secret) and the staff side
//        POST /api/jobs/{job}/channel-bindings, POST .../{id}/identity-secret, POST /api/tenant/invites, GET /api/me,
//        PUT /api/jobs/{job}/staff
//   5  test (staff test chat, Test env) and publish, readiness cleared through the product API only
//        POST /api/jobs/{job}/conversations {env:test}, POST .../conversations/{id}/messages,
//        GET /api/jobs/{job}/release-readiness?of=draft, POST .../releases, POST .../releases/{id}/apply
//   6  customer chats (known customer via the channel's identity mechanism), asks for a person, staff replies
//        edge: POST /webchat/demo/assertion, POST /api/webchat/sessions, POST /api/webchat/messages,
//        GET /api/webchat/conversations/{id}/messages; api: GET .../console/conversations, POST .../take,
//        POST .../conversations/{id}/messages {kind:reply}
//
// A blocker the product cannot resolve through its own API is reported "PRODUCT GAP: <code> - why", never worked
// around. Exit 0 = every step passed. PS_UNTIL=5 stops after publish (platform-stack.sh onboard).
// Env (set by platform-stack.sh): PS_API PS_EDGE PS_IDP PS_KCADMIN PS_DEMO_URL PS_DEMO_TOKEN PS_SELF PS_UNTIL PS_STATE
// PS_LLM_URL PS_LLM_MODEL PS_ANSWER_TIMEOUT_MS.
import { execFileSync } from 'node:child_process';
import { createHmac, randomBytes, randomUUID } from 'node:crypto';
import { writeFileSync, existsSync, readFileSync } from 'node:fs';
import { env, START_MS, RUN, sleep, results, gaps, note, gap, step, assert, who, tokens, mint, api, ok, itemsOf, kcUserByEmail, edge, J, opKey, TEMPLATE, tplOp, templateOps, authorConnectorTemplate, demoAdmin, demoCustomers, assertionFor, ISSUER, slug, ensureLayer, addRow, readiness, codes, llmGap, customerChat, finish } from './ps-lib.mjs';

const { API, EDGE, IDP, KCADMIN, DEMO_URL, DEMO_TOKEN, SELF, LLM_URL, LLM_MODEL, UNTIL, STATE, ANSWER_MS } = env; // eager: the smoke fails fast without the harness env

// distinctive phrases of the two knowledge sources: the platform pack (step 0) and the tenant's own uploaded document (step 2)
const PLATFORM_PHRASE = 'bluebird-seal pouch';
const OWN_PHRASE = 'tulip-wrap eco box';
const KNOWLEDGE_Q = 'How long does shipping take and how is it packed?';
// ===== step 0: the minimum platform offering, authored by the platform admin through the product ============================
// The database is EMPTY after migrations (no seeded layers, job templates, connector templates, ontology or knowledge).
// Everything below is a real platform-authoring route called as sa. Content that has no authoring route is a
await step(0, 'platform admin authors the minimum platform offering (layers, job template, platform knowledge); empty DB assumed', async () => {
	const base = await ensureLayer({ scope: 'base', label: 'base' }, 'base');
	const industry = await ensureLayer({ scope: 'industry', industry: 'retail', label: 'industry/retail' }, 'industry/retail');
	const dept = await ensureLayer({ scope: 'base_dept', department: 'customer_service', label: 'base_dept/customer_service' }, 'base_dept/customer_service');
	const indDept = await ensureLayer({ scope: 'industry_dept', industry: 'retail', department: 'customer_service', label: 'industry_dept/retail/customer_service', areas: ['logistics', 'handover'] }, 'industry_dept/retail/customer_service'); // areas live on the industry_dept layer: the release target's options.areas
	const job = await ensureLayer({ scope: 'job', job: 'customer_care_agent', label: 'job/customer_care_agent', template_selection: { industry: 'retail', departments: ['industry_dept/retail/customer_service'], profile: 'retail_ecommerce', areas: ['logistics'], dimensions: { channel: ['web-chat'] } } }, 'job/customer_care_agent');
	assert(base && industry && dept && indDept && job, 'a platform layer could not be created or found');
	// the built-in track_parcel plan runs only when the template DECLARES the procedure `order_status` (orchestrator.rs procedure_for_space/builtin_declared; else no_procedure -> handover out_of_scope)
	await addRow(base, 'agent_role', 'procedure', { label: 'Procedure', used_by: 'Authored procedures', max_side_effect: 'P', enabled: true, grants: [{ object: '*', verb: '*', variant: null, modalities: ['observe', 'request', 'do'], derive_from_activity: true }] });
	await addRow(base, 'job', 'customer_care_agent', { label: 'Customer care agent', summary: 'Answers retail customers.' });   // a job-headed template compiles only when a base layer defines the job
	await addRow(job, 'procedure', 'order_status@1', { label: 'Order status', description: 'Tell the known customer where their parcel is.', steps: 1 });
	await addRow(indDept, 'context_space', 'retail:logistics:track_parcel', { label: 'Where is my parcel', description: 'The customer wants the carrier tracking status of a parcel that has shipped.', area: 'logistics', activity: 'support:track_parcel', procedure: 'order_status@1', knowledge_kinds: [], slots: ['tracking_ref'] });
	await addRow(indDept, 'activity', 'support:track_parcel', { label: 'Track a parcel', description: 'Tell the customer where their parcel is.', area: 'logistics', actor: 'customer', operations: [{ op: 'order.list', required: true }, { op: 'order.read', required: true }, { op: 'delivery.read', required: true }, { op: 'delivery.read', variant: 'by_tracking_ref', required: true }] });
	// GET /api/jobs/{job}/operations and PUT .../routes read operation_expectation rows (connectors/routes_store.rs expected()),
	// NOT the operations embedded in an activity (readiness reads those): both must be authored
	{
		const REQUIRED = new Set(['order.list', 'order.read', 'delivery.read']);
		const byKey = new Map();
		for (const t of templateOps()) {
			const k = `${t.object}.${t.verb}`;
			const e = byKey.get(k) ?? { variants: [] };
			if (t.variant !== '*') e.variants.push(t.variant);
			byKey.set(k, e);
		}
		for (const [k, e] of byKey) await addRow(indDept, 'operation_expectation', k, { label: k, summary: `${REQUIRED.has(k) ? 'required' : 'optional'}${e.variants.length ? ` {${e.variants.join(', ')}}` : ''}` });
	}
	await addRow(indDept, 'stage_knowledge_kind', 'support|shipping', { label: 'Shipping', description: 'How parcels are shipped and tracked', required: false });
	await addRow(indDept, 'context_space', 'retail:logistics:shipping_info', { label: 'Shipping information', description: 'The customer asks how long shipping takes, how parcels are packed or what the shipping policy is.', area: 'logistics', activity: 'support:shipping_info', knowledge_kinds: ['shipping'], slots: [] });
	// Knowledge is a pure procedure builtin; this known activity deliberately has no explicit knowledge grant.
	await addRow(indDept, 'activity', 'support:shipping_info', { label: 'Shipping advice', description: 'Answer shipping policy questions.', area: 'logistics', actor: 'customer', operations: [] });
	await addRow(indDept, 'context_space', 'retail:logistics:order_overview', { label: 'Order overview', description: 'The customer asks for an overview of all their orders.', area: 'logistics', activity: 'support:order_overview', knowledge_kinds: [], slots: [] });
	await addRow(indDept, 'activity', 'support:order_overview', { label: 'List orders', description: 'List the known customer\'s orders.', area: 'logistics', actor: 'customer', operations: [{ op: 'order.list', required: false }] });
	await addRow(dept, 'context_space', 'customer_service:handover:handover_to_person', { label: 'Hand over to a person', description: 'The customer asks to speak to a person.', area: 'handover', knowledge_kinds: [], slots: ['handover_reason'] });
	// the catalogue (objects + operations) the job and the connector template use: API-only authoring, base layer first
	{
		const contract = JSON.parse(readFileSync(env.PS_OPDEFS ?? process.env.PS_OPDEFS, 'utf8'));
		const opsToAuthor = templateOps();
		const contractDef = (o, v, variant) => contract.find((d) => d.object === o && d.verb === v && d.variant === variant) ?? contract.find((d) => d.object === o && d.verb === v && d.variant === '*');
		// an op's input/output names must be fields of its object (422 unknown_field) and the runtime keeps only DECLARED outputs: author the union of what the authored ops declare
		const objFields = new Map();
		for (const t of opsToAuthor) { const d = contractDef(t.object, t.verb, t.variant); const s = objFields.get(t.object) ?? new Set(); for (const f of [...(d?.input_fields ?? []), ...(d?.output_fields ?? [])]) s.add(f); objFields.set(t.object, s); }
		const objs = [['party', 'Party'], ['order', 'Order'], ['delivery', 'Delivery'], ['case', 'Case'], ['return', 'Return'], ['payment', 'Payment']];
		for (const [o, label] of objs) {
			const fields = [...(objFields.get(o) ?? [])].map((field) => ({ field, data_type: 'json', description: `platform-stack ${o}.${field}` }));
			const r = await api('sa', 'POST', '/api/platform/objects', { object: o, layer_key: 'base', label, tier: 'base', description: `platform-stack ${label}`, fields });
			if (![201, 409].includes(r.status)) throw gap('objects_authoring', `POST /api/platform/objects ${o} -> ${r.status} ${r.text.slice(0, 300)}`);
		}
		// identity levels come from the contract (opdefs.json): an op may not be authored below its contract level (422 identity_level_below_contract)
		const contractLevel = (o, v, variant) => { const hit = contract.find((d) => d.object === o && d.verb === v && d.variant === variant) ?? contract.find((d) => d.object === o && d.verb === v && d.variant === '*'); return String(hit?.identity_level ?? '1'); };
		const seen = new Set();
		const ops = opsToAuthor.sort((a, b) => (a.variant === '*' ? 0 : 1) - (b.variant === '*' ? 0 : 1));
		for (const t of ops) {
			const k = `${t.object}.${t.verb}.${t.variant}`;
			if (seen.has(k)) continue;
			seen.add(k);
			const star = t.variant === '*';
			const body = { object: t.object, verb: t.verb, ...(star ? {} : { variant: t.variant }), layer_key: 'base', modality: t.side_effect === 'R' ? 'observe' : 'do', op_kind: 'standard', identity_level: contractLevel(t.object, t.verb, t.variant), side_effect: t.side_effect, confirm_back: t.side_effect !== 'R', idempotency: t.side_effect === 'R' ? 'nat' : 'key', description: `platform-stack ${k}`, input_fields: contractDef(t.object, t.verb, t.variant)?.input_fields ?? [], output_fields: contractDef(t.object, t.verb, t.variant)?.output_fields ?? [] };
			const r = await api('sa', 'POST', '/api/platform/operations', body);
			if (![201, 409].includes(r.status)) throw gap('operations_authoring', `POST /api/platform/operations ${k} -> ${r.status} ${r.text.slice(0, 300)}`);
		}
	}
	// platform knowledge: a strata store the platform owns, with a curated document, published as a pack
	const store = ok(await api('sa', 'POST', '/api/platform/knowledge/stores', { name: `ps-shipping-${RUN}`, kind: 'leaf', description: 'Platform shipping knowledge' }), 'POST /api/platform/knowledge/stores', [200, 201]);
	ok(await api('sa', 'POST', `/api/platform/knowledge/stores/${store.id}/documents`, { text: `Platform standard: every parcel leaves the carrier hub in a sealed ${PLATFORM_PHRASE} and tracking is available from the moment it ships.`, title: 'Shipping basics', topic_keys: ['shipping'], provenance: { source_url: 'https://platform-stack.example.test/shipping', license: 'CC-BY-4.0', retrieved_at: new Date().toISOString() } }), 'POST platform knowledge document', [200, 201, 202]);
	ok(await api('sa', 'POST', '/api/platform/knowledge/packs', { key: `platform/shipping-${RUN}`, version: 1, store_id: store.id }), 'POST platform pack', [200, 201]);
	ok(await api('sa', 'POST', `/api/platform/knowledge/packs/${encodeURIComponent(`platform/shipping-${RUN}`)}/1/publish`), 'publish platform pack', [200, 201, 202]);
	// the business model (profile): the release target lists only DB profiles (compile.rs db_options); a job created from the template
	// carries profile retail_ecommerce, and PUT /api/jobs/{job}/target 422s "Unknown profile" unless the platform authored it
	{
		const ax = await api('sa', 'POST', '/api/axes', { key: 'sector', label: 'Sector', description: 'platform-stack', cardinality: 'one', object: 'Case' });
		if (![200, 201, 409].includes(ax.status)) { gaps.push('profile_authoring:axis'); note(`PRODUCT GAP: profile_authoring - POST /api/axes -> ${ax.status} ${ax.text.slice(0, 200)}`); }
		else {
			const v = await api('sa', 'POST', '/api/axes/sector/values', { key: 'retail', label: 'Retail' });
			if (![200, 201, 409].includes(v.status)) note(`axis value -> ${v.status} ${v.text.slice(0, 160)}`);
			const pf = await api('sa', 'PUT', '/api/profiles/retail_ecommerce', { label: 'Retail e-commerce', industry: 'retail', description: 'platform-stack', selection: { sector: ['retail'] } });
			if (![200, 201].includes(pf.status)) { gaps.push('profile_authoring:profile'); note(`PRODUCT GAP: profile_authoring - PUT /api/profiles/retail_ecommerce -> ${pf.status} ${pf.text.slice(0, 200)}`); }
		}
	}
	// geography: the platform offers a region vocabulary (dimension 'region') and a region sub-layer of the retail industry; the TENANT chooses
	let regionLayer = null;
	{
		const dims = await api('sa', 'GET', '/api/dimensions');
		if (!itemsOf(dims.json).some((d) => d.key === 'region')) {
			const d = await api('sa', 'POST', '/api/dimensions', { key: 'region', label: 'Region', description: 'platform-stack' });
			if (![200, 201, 409].includes(d.status)) { gaps.push('region_vocabulary_authoring'); note(`PRODUCT GAP: region_vocabulary_authoring - POST /api/dimensions region -> ${d.status} ${d.text.slice(0, 200)}`); }
		}
		for (const [k, label] of [['de', 'Germany'], ['be', 'Belgium']]) {
			const dv = await api('sa', 'POST', '/api/dimensions/region/values', { key: k, label });
			if (![200, 201, 409].includes(dv.status)) { gaps.push('region_vocabulary_authoring'); note(`PRODUCT GAP: region_vocabulary_authoring - POST /api/dimensions/region/values ${k} -> ${dv.status} ${dv.text.slice(0, 200)}`); }
		}
		regionLayer = await ensureLayer({ scope: 'industry', industry: 'retail', region: 'de', label: 'industry/retail/region/de' }, 'industry/retail/region/de');
		// languages: the platform defines no authoring route (the four-language list is a constant, release/settings.rs:58), so there is nothing to author
	}
	// tag every family so a tenant can create a job from the job head
	for (const l of [base, industry, dept, indDept, job]) {
		const t = await api('sa', 'POST', `/api/platform/templates/${slug(l.key)}/tags`, { note: 'platform-stack' });
		if (t.status === 409) continue; // nothing_to_tag
		if (t.status === 422) throw new Error(`RESOLVE FAILED tag ${l.key}: ${t.text.slice(0, 400)} (this script authors only layers + two context spaces; objects, operations, procedures, job settings and the department archetype are not authored here)`);
		ok(t, `tag ${l.key}`, [200, 201]);
	}
	// a second, throwaway job template: step 1 retires and restores it (migration 0228) and checks the tenant's catalogue each time
	{
		const tl = await ensureLayer({ scope: 'industry_dept', industry: 'retail', department: 'throwaway', label: 'industry_dept/retail/throwaway', areas: ['logistics'], template_selection: { industry: 'retail', departments: ['industry_dept/retail/throwaway'], profile: 'retail_ecommerce', areas: ['logistics'], dimensions: { channel: ['web-chat'] } } }, 'industry_dept/retail/throwaway');
		assert(tl, 'the throwaway job layer could not be created or found');
		// a template needs its own department hierarchy and at least one row to tag
		const tb = await ensureLayer({ scope: 'base_dept', department: 'throwaway', label: 'base_dept/throwaway' }, 'base_dept/throwaway');
		await addRow(tb, 'context_space', 'throwaway:handover:handover_to_person', { label: 'Hand over to a person', description: 'The customer asks to speak to a person.', area: 'handover', knowledge_kinds: [], slots: [] });
		await addRow(tl, 'context_space', 'retail:logistics:throwaway_info', { label: 'Throwaway', description: 'Throwaway template space.', area: 'logistics', knowledge_kinds: [], slots: [] });
		for (const l of [tb]) { const t0 = await api('sa', 'POST', `/api/platform/templates/${slug(l.key)}/tags`, { note: 'platform-stack throwaway' }); if (![200, 201, 409].includes(t0.status)) throw new Error(`tag ${l.key} -> ${t0.status} ${t0.text.slice(0, 300)}`); }
		const tt = await api('sa', 'POST', `/api/platform/templates/${slug(tl.key)}/tags`, { note: 'platform-stack throwaway' });
		let tag = tt.json?.tag ?? (tt.json?.version ? `v${tt.json.version}` : 'v1');
		if (tt.status === 409) {   // re-run on a shared stack: already tagged (an aborted earlier run may have left it retired): use the latest tag and restore it
			const catalog = await api('sa', 'GET', '/api/platform/templates');
			const family = (catalog.json?.layers ?? []).find((x) => x.layer_key === tl.key);
			const latest = [...(family?.tags ?? [])].sort((x, y) => y.version - x.version)[0];
			tag = latest ? `v${latest.version}` : null;
			if (!tag) throw new Error(`tag throwaway ${tl.key} -> 409 and no existing tag found in the catalogue (status ${catalog.status})`);
			await api('sa', 'POST', `/api/platform/templates/${slug(tl.key)}/tags/${tag}/restore`);   // 4xx when it was not retired: harmless
		} else if (![200, 201].includes(tt.status)) throw new Error(`tag throwaway ${tl.key} -> ${tt.status} ${tt.text.slice(0, 300)}`);
		J.throwaway = { slug: slug(tl.key), tag };
	}
	// channel vocabulary (dimension_value): the release target's options.dimensions.channel
	{
		const dims = await api('sa', 'GET', '/api/dimensions');
		if (!itemsOf(dims.json).some((d) => d.key === 'channel')) {
			const d = await api('sa', 'POST', '/api/dimensions', { key: 'channel', label: 'Channel', description: 'platform-stack' });
			if (![200, 201, 409].includes(d.status)) { gaps.push('dimension_authoring'); note(`PRODUCT GAP: dimension_authoring - POST /api/dimensions channel -> ${d.status} ${d.text.slice(0, 200)}`); }
		}
		const dv = await api('sa', 'POST', '/api/dimensions/channel/values', { key: 'web-chat', label: 'Web chat' });
		if (![200, 201, 409].includes(dv.status)) note(`dimension value web-chat -> ${dv.status} ${dv.text.slice(0, 160)}`);
	}
	await authorConnectorTemplate();
	note(`layers (with areas), channel vocabulary, connector template ${TEMPLATE.key}@${TEMPLATE.version}, platform pack published`);
});

// ===== step 0/1: tenant onboarding by the platform admin, then the tenant admin's job ======================================
await step(1, 'customer signs up and sa approves; the new tenant admin creates a job from a platform template (TJ)', async () => {
	const email = `ps-admin-${RUN}@platform-stack.example.test`;
	// the product's customer path: the website/chat intake files the signup with its service token, sa approves it
	const intakeTok = (await (await fetch(`${IDP}/__mint?persona=intake`)).json()).access_token;
	const filed = await fetch(`${API}/api/signup-approvals/intake`, { method: 'POST', headers: { authorization: `Bearer ${intakeTok}`, 'content-type': 'application/json' }, body: JSON.stringify({ email, name: 'Tessa Admin', company: `Platform Stack ${RUN}`, message: 'platform-stack smoke', source: 'web', locale: 'en' }) });
	const filedText = await filed.text();
	if (filed.status === 404) throw gap('signup_dark', 'POST /api/signup-approvals/intake is 404: EARLY_ACCESS_SIGNUP_ENABLED is not on in this stack (run up again)');
	assert(filed.status === 202, `POST /api/signup-approvals/intake -> ${filed.status} ${filedText.slice(0, 300)}`);
	const queue = itemsOf(ok(await api('sa', 'GET', '/api/signup-approvals?status=pending'), 'GET /api/signup-approvals'));
	const row = queue.find((x) => JSON.stringify(x).toLowerCase().includes(email));
	assert(row?.id, `the signup for ${email} is not in the pending queue: ${JSON.stringify(queue).slice(0, 300)}`);
	const made = ok(await api('sa', 'POST', `/api/signup-approvals/${row.id}/approve`, { residency: 'eu' }), 'POST /api/signup-approvals/{id}/approve', [200, 201, 202]);
	J.tenant = made?.tenant?.id;
	assert(J.tenant, `no tenant id in ${JSON.stringify(made).slice(0, 300)}`);
	assert(made?.seated === true, `the approval did not seat the requester (steps ${JSON.stringify(made?.steps).slice(0, 300)})`);
	const kc = await kcUserByEmail(email);
	assert(kc?.id, `the fake Keycloak admin holds no user for ${email}: the approval did not create the tenant_admin identity`);
	who.ta = { persona: 'ta', sub: kc.id, email };
	note(`signup ${row.id} approved -> tenant ${J.tenant}; tenant_admin ${email}`);
	// LOCAL-HARNESS ONLY (not product behaviour): in prod the customer's external system is public https. The tenant goes into
	// CONNECTOR_PRIVATE_TARGETS once (loopback demo-backends) and api/daemons restart. The tenant's LLM and strata access are NOT
	// written: signup provisioning mints its bundle tokens (BUNDLE_MINTER_* -> the fake licences).
	execFileSync(SELF, ['harness-tenant', J.tenant], { stdio: 'inherit' });
	// the admin's first sign-in seats the membership; the tenant becomes usable once provisioning + licence gate 2 pass
	const until = Date.now() + 90000;
	let last = '';
	while (Date.now() < until) {
		const me = await api('ta', 'GET', '/api/me');
		const jobs = me.status === 200 ? await api('ta', 'GET', '/api/jobs') : me;
		last = `${jobs.status} ${jobs.text.slice(0, 160)}`;
		if (jobs.status === 200) break;
		await sleep(2000);
	}
	assert(last.startsWith('200'), `the new tenant admin cannot use the tenant after 90 s (last: ${last})`);
	// tenant_admin only, no extra role grants (D127). Every failure on this path is a b7-lane gap, never worked around.
	const b7 = (r, what, accept = [200, 201]) => {
		if (accept.includes(r.status)) return r.json;
		const code = r.json?.code ?? r.status;
		gaps.push(`b7:${code}`);
		throw new Error(`PRODUCT GAP (b7 lane): ${code} - ${what} -> ${r.status} ${r.text.slice(0, 300)}`);
	};
	// 1. the company profile (D124), first
	const prof = b7(await api('ta', 'GET', '/api/tenant/profile'), 'GET /api/tenant/profile');
	const opts = prof?.options ?? {};
	const industry = (opts.industries ?? []).find((i) => i.key === 'retail')?.key ?? opts.industries?.[0]?.key;
	if (!industry) throw gap('b7:no_industries_published', 'the profile offers no industry: no industry layer is tagged on this platform (D128)');
	const deps = (opts.departments ?? []).filter((d) => d.industry === industry);
	const dep = deps.find((d) => /customer_service/.test(d.key)) ?? deps[0];
	if (!dep) throw gap('b7:no_departments_published', `no department of ${industry} is tagged on this platform`);
	const regionKeys = (opts.regions ?? []).map((r) => r.key);
	const region = regionKeys.includes('de') ? 'de' : regionKeys[0] ?? null; // the tenant's own choice in this run
	if (!region) { gaps.push('b7:no_regions_offered'); note('PRODUCT GAP: no_regions_offered - GET /api/tenant/profile options.regions is empty'); }
	J.profile = { industry, departments: [dep.key], region };
	b7(await api('ta', 'PUT', '/api/tenant/profile', { ...J.profile, expected_updated_at: prof.updated_at ?? null }), 'PUT /api/tenant/profile');
	// 2. templates (may be []), 3. a job from a template when one is published, else a BLANK job on the layers
	const cat = await api('ta', 'GET', '/api/job-templates');
	assert(cat.status !== 404, 'GET /api/job-templates is 404: feat/tenant-job-design is not in these binaries');
	const templates = (b7(cat, 'GET /api/job-templates')?.templates ?? []).filter((x) => !JSON.stringify(x).includes('throwaway'));
	// retire/restore of a published template (migration 0228): the tenant's catalogue drops it while retired and gets it back
	{
		const T = J.throwaway;
		const has = (c) => (c.json?.templates ?? []).some((x) => JSON.stringify(x).includes('throwaway'));
		assert(T && has(cat), `the published throwaway template is not in the tenant's GET /api/job-templates: ${cat.text.slice(0, 300)}`);
		const base = `/api/platform/templates/${T.slug}/tags/${T.tag}`;
		ok(await api('sa', 'POST', `${base}/retire`), `retire throwaway ${T.tag}`, [200, 201]);
		const afterRetire = await api('ta', 'GET', '/api/job-templates');
		assert(!has(afterRetire), `retired throwaway template still offered: ${afterRetire.text.slice(0, 300)}`);
		ok(await api('sa', 'POST', `${base}/restore`), `restore throwaway ${T.tag}`, [200, 201]);
		const afterRestore = await api('ta', 'GET', '/api/job-templates');
		assert(has(afterRestore), `restored throwaway template is not back: ${afterRestore.text.slice(0, 300)}`);
		note('template retire/restore: published -> retired (absent from tenant GET /api/job-templates) -> restored (present)');
	}
	let job;
	// region/languages at create: sent when the route accepts them; a refusal is a PRODUCT GAP and the job is created without them (D132 adds them)
	const createJob = async (body, what) => {
		if (body.template) return b7(await api('ta', 'POST', '/api/jobs', body), what, [201]); // a template job takes no region at create (routes/jobs.rs:392 blank-only); it inherits the company region (D124.3), checked below
		let r = await api('ta', 'POST', '/api/jobs', { ...body, ...(region ? { region } : {}), languages: ['de', 'en'] });
		if (r.status !== 201) {
			const why = `${r.status} ${r.text.slice(0, 160)}`;
			r = await api('ta', 'POST', '/api/jobs', { ...body, ...(region ? { region } : {}) });
			if (r.status === 201) { gaps.push('job_create_no_languages'); note(`PRODUCT GAP: job_create_no_languages - POST /api/jobs {languages} -> ${why} (crates/agent-api/src/routes/jobs.rs:392 body has no languages); set via PUT /api/jobs/{job}/languages instead`); }
			else { const why2 = `${r.status} ${r.text.slice(0, 160)}`; r = await api('ta', 'POST', '/api/jobs', body); if (r.status === 201 && region) { gaps.push('job_create_region_refused'); note(`PRODUCT GAP: job_create_region_refused - POST /api/jobs {region:${region}} -> ${why2}`); } }
		}
		return b7(r, what, [201]);
	};
	if (templates.length) {
		J.template = templates.find((x) => x.recommended) ?? templates.find((x) => /customer_care/.test(x.key)) ?? templates[0];
		job = await createJob({ template: J.template.template, key: `ps_${RUN}`, label: 'Platform stack job' }, 'POST /api/jobs (from template)');
		note(`job ${job.key} created FROM TEMPLATE ${J.template.template} (${templates.length} offered)`);
	} else {
		job = await createJob({ key: `ps_${RUN}`, label: 'Platform stack job', industry, departments: [dep.key], profile: 'retail_ecommerce' }, 'POST /api/jobs (blank)');
		note(`job ${job.key} created BLANK (no published template) on ${industry}/${dep.key}`);
	}
	J.job = job.key;
	b7(await api('ta', 'PATCH', `/api/jobs/${J.job}`, { label: 'Platform stack job (renamed)' }), 'PATCH /api/jobs/{job}', [200]);
	// languages served: the tenant's job designer sets them on the job (PUT /api/jobs/{job}/languages)
	{
		const lg = await api('ta', 'PUT', `/api/jobs/${J.job}/languages`, { languages: ['de', 'en'] });
		if (lg.status === 200) note(`job languages ${JSON.stringify(lg.json?.draft)}`);
		else { gaps.push('job_languages_refused'); note(`PRODUCT GAP: job_languages_refused - PUT /api/jobs/{job}/languages -> ${lg.status} ${lg.text.slice(0, 200)} (crates/agent-api/src/release/settings.rs:52)`); }
		const g = await api('ta', 'GET', `/api/jobs/${J.job}/target`);
		const sr = g.json?.selection?.region;
		if (region && sr !== region) { gaps.push('job_region_not_on_selection'); note(`PRODUCT GAP: job_region_not_on_selection - job selection.region=${JSON.stringify(sr)}, expected ${region} (GET /api/jobs/{job}/target)`); }
	}
	note(`profile ${industry}/${dep.key} region ${region}; job ${J.job}`);
});

// ===== step 2: knowledge (platform-offered + own) and an authored procedure ===============================================
async function bindOwnKnowledge(stage, kind) {
	const key = `ps-${kind.replace(/[^a-z0-9]+/gi, '-').toLowerCase()}`.slice(0, 50);
	const stores = itemsOf(ok(await api('ta', 'GET', '/api/knowledge/stores'), 'GET /api/knowledge/stores'));
	let leaf = stores.find((x) => x.key === key);
	if (!leaf) leaf = ok(await api('ta', 'POST', '/api/knowledge/stores', { key, label: `Own ${kind}`, kind: 'leaf' }), 'POST /api/knowledge/stores', [201]);
	ok(await api('ta', 'PUT', `/api/jobs/${J.job}/knowledge-kinds/${encodeURIComponent(stage)}/${encodeURIComponent(kind)}`, { store: leaf.path ?? leaf.key }), `PUT knowledge-kinds/${stage}/${kind}`);
	// store creation enqueues an outbox store_ensure (a worker creates it in strata_rag): wait for it, then upload
	const until = Date.now() + 90000;
	let r;
	for (let n = 0; ; n++) {
		const form = new FormData();
		form.append('language', 'en');
		form.append('file', new Blob([`# ${kind}\n\nOur shop promise: every order arrives in a ${OWN_PHRASE} within 2 working days. Returns are free within 30 days of delivery.\n`], { type: 'text/markdown' }), `${key}.md`);
		r = await fetch(`${API}/api/knowledge/stores/${leaf.id}/upload`, { method: 'POST', headers: { authorization: `Bearer ${await mint('ta')}` }, body: form });
		const body = await r.clone().text();
		if (r.status === 409 && /provision_pending/.test(body)) {   // the signup step app_licences mints the bundle tokens: wait for it
			if (Date.now() > until) throw gap('strata_provision_pending', `knowledge upload still 409 provision_pending after 90 s (app_licences / store provisioning incomplete): ${body.slice(0, 200)}`);
			await sleep(2000);
			continue;
		}
		if (r.status !== 409 || !/store_not_bound/.test(body)) break;
		if (n === 2) await api('ta', 'POST', '/api/knowledge/stores/ensure');
		if (Date.now() > until) throw gap('store_not_bound', `store ${key} is still not bound in strata 90 s after creation (store_ensure outbox did not run): ${body}`);
		await sleep(2000);
	}
	assert(r.ok, `upload to ${key} -> ${r.status} ${(await r.text()).slice(0, 300)}`);
}
// the tenant admin's release-target page: pick profile, area and channel from the offered options
async function pickTarget() {
	let target = ok(await api('ta', 'GET', `/api/jobs/${J.job}/target`), 'GET target');
	{
		const sel = target?.selection ?? {};
		const opts = target?.options ?? {};
		const dept = (sel.departments ?? [])[0];
		const areas = (opts.areas ?? []).filter((a) => !dept || a.industry_dept === dept || a.industry_dept === `industry_dept/${sel.industry}/${dept}`);
		const chans = (opts.dimensions?.channel?.values ?? []).map((v) => v.key);
		const channel = chans.includes('web-chat') ? 'web-chat' : chans[0];
		if (!sel.areas?.length || !sel.dimensions?.channel?.length) {
			// options.areas are the `areas` of the tagged industry_dept layers (step 0 authored logistics + handover): pick from the options, never a constant
		areas.sort((x, y) => Number(y.key === 'logistics') - Number(x.key === 'logistics'));
		if (!areas.length || !channel) throw gap('release_target_options', `the target offers no area/channel to pick (profiles ${JSON.stringify(opts.profiles).slice(0,200)}; options keys ${Object.keys(opts)}; target keys ${Object.keys(target ?? {})}; areas ${JSON.stringify(opts.areas).slice(0, 200)}; selection ${JSON.stringify(sel).slice(0, 300)}; channels ${chans.join(',')}, dept ${dept})`);
			if (!sel.profile) { const pk = opts.profiles?.[0]?.key; if (pk) { sel.profile = pk; gaps.push('job_template_no_profile'); note(`PRODUCT GAP: job_template_no_profile - the job created from the template has selection.profile null (GET /api/jobs/{job}/target); the tenant admin picks ${pk} from options.profiles`); } }
			const next = { ...sel, areas: sel.areas?.length ? sel.areas : [areas[0].key], dimensions: { ...(sel.dimensions ?? {}), channel: sel.dimensions?.channel?.length ? sel.dimensions.channel : [channel] } };
			const put = await api('ta', 'PUT', `/api/jobs/${J.job}/target`, { selection: next, expected_updated_at: target?.selection_updated_at ?? null });
			if (put.status === 422) throw gap('release_target_unsatisfiable', `the cut needs >= 1 area + 1 channel (bundle_incomplete) but PUT /api/jobs/{job}/target -> 422 ${put.text.slice(0, 200)}; options.profiles=${JSON.stringify(opts.profiles).slice(0, 80)} options.areas=${(opts.areas ?? []).length} (target options are DB-sourced, areas never)`);
			ok(put, 'PUT target (area + channel)');
			target = ok(await api('ta', 'GET', `/api/jobs/${J.job}/target`), 'GET target');
			note(`release target: area ${next.areas[0]}, channel ${next.dimensions.channel[0]}`);
		}
	}
	return target;
}
await step(2, 'platform knowledge bound, own documents uploaded, own procedure authored and published', async () => {
	assert(J.job, 'step 1 made no job');
	const packs = await api('ta', 'GET', '/api/knowledge/platform-packs');
	const offered = packs.status === 200 ? itemsOf(packs.json) : [];
	note(`platform packs offered: ${offered.length}${packs.status === 200 ? '' : ` (status ${packs.status})`}`);
	const kinds = ok(await api('ta', 'GET', `/api/jobs/${J.job}/knowledge-kinds`), 'GET knowledge-kinds');
	const rows = kinds?.rows ?? [];
	for (const row of rows.slice(0, 12)) {
		const pin = (offered.find((p) => String(p.key ?? p.pin).includes(`${row.kind}-${RUN}`)) ?? offered.find((p) => String(p.key ?? p.pin).includes(row.kind)))?.pin; // this run's pack first: earlier runs left theirs on the shared platform
		if (pin) ok(await api('ta', 'PUT', `/api/jobs/${J.job}/knowledge-kinds/${encodeURIComponent(row.stage)}/${encodeURIComponent(row.kind)}`, { packs: [pin] }), `PUT packs ${row.kind}`);
		if (!row.store) await bindOwnKnowledge(row.stage, row.kind);
	}
	note(`${rows.length} knowledge kinds on the job; own documents uploaded for the unbound ones`);
	let spaces = itemsOf(ok(await api('ta', 'GET', `/api/jobs/${J.job}/context-spaces`), 'GET context-spaces'));
	if (!spaces.length) { note('no context space yet: the tenant admin sets the release target (profile, area, channel) first'); await pickTarget(); spaces = itemsOf(ok(await api('ta', 'GET', `/api/jobs/${J.job}/context-spaces`), 'GET context-spaces')); }
	// a procedure may only use operations of its space's activity grant: take a space that names an activity
	const space = spaces.find((s) => s.activity && /order_overview$/.test(s.key)) ?? spaces.find((s) => s.activity && /order_status|track_parcel|faq|return/.test(s.key)) ?? spaces.find((s) => s.activity) ?? spaces[0];
	if (!space) throw gap('no_context_space', 'the job has no context space to author a procedure for');
	note(`context space ${space.key} (activity ${space.activity || 'none'}; ${spaces.filter((s) => s.activity).length}/${spaces.length} spaces name an activity)`);
	const existing = itemsOf(ok(await api('ta', 'GET', `/api/jobs/${J.job}/procedures`), 'GET procedures'));
	const version = 1 + Math.max(0, ...existing.map((p) => Number(p.version ?? 0)));
	const step1 = (operation, variant) => ({ step_number: 1, phase: 0, name: `Run ${operation}`, operation, variant, modality: 'request', side_effect: 'R', identity_level: '0', confirm_back: false, gate: false, args: operation === 'order.list' ? { party_id: '$party_id' } : {} });
	let proc = { name: `ps_${RUN}_procedure`, version, context_space: space.key, description: 'Look up the customer\'s orders (platform stack).', steps: [step1('order.list', null)] };
	let c = await api('ta', 'POST', `/api/jobs/${J.job}/procedures`, proc);
	if (c.status === 422 || c.status === 409) throw gap(`procedure_${c.json?.code ?? c.json?.error?.code ?? c.status}`, `a tenant cannot author this procedure through the product API: ${c.text.slice(0, 300)}`);
	let created = ok(c, 'POST procedures', [200, 201]);
	const grant = created?.activity_grant ?? [];
	if ((created?.outside_grant ?? []).length && grant.length) {
		const m = /^([a-z_]+)\.([a-z_]+)(?:\{(.*)\})?$/.exec(grant[0]);
		const v2 = version + 1;
		proc = { ...proc, version: v2, steps: [step1(m[1] + '.' + m[2], m[3] && m[3] !== '*' ? m[3] : null)] };
		c = await api('ta', 'POST', `/api/jobs/${J.job}/procedures`, proc);
		created = ok(c, `POST procedures (op from grant ${grant[0]})`, [200, 201]);
	}
	const pv = proc.version;
	for (const to of ['CANARY', 'PUBLISHED']) {   // DRAFT -> CANARY -> PUBLISHED (procedures.rs next_status)
		const pub = await api('ta', 'POST', `/api/jobs/${J.job}/procedures/${encodeURIComponent(proc.name)}/versions/${pv}/status`, { to });
		if (pub.status >= 400) throw gap(`procedure_${to.toLowerCase()}_${pub.json?.error?.code ?? pub.status}`, `cannot move the authored procedure to ${to} as tenant_admin: ${pub.status} ${pub.text.slice(0, 300)} (space ${space.key}, activity ${space.activity || 'none'}; activity_grant: ${JSON.stringify(grant)})`);
	}
	note(`procedure ${proc.name}@${pv} on ${space.key} published`);
 const knowledge = { name: `ps_${RUN}_knowledge`, version: 1, context_space: 'retail:logistics:shipping_info', description: 'Answer from this space’s platform and tenant knowledge.', steps: [{...step1('knowledge.answer', null), name:'tenant_knowledge_answer', modality:'observe'}] };
 const authored = ok(await api('ta', 'POST', `/api/jobs/${J.job}/procedures`, knowledge), 'POST tenant knowledge procedure', [200,201]);
 assert(!(authored.outside_grant??[]).length, 'knowledge builtin is outside the known activity grant');
 for (const to of ['CANARY','PUBLISHED']) ok(await api('ta','POST',`/api/jobs/${J.job}/procedures/${knowledge.name}/versions/1/status`,{to}), `publish tenant knowledge procedure ${to}`);
 note(`tenant knowledge procedure ${knowledge.name}@1 published with no explicit knowledge grant`);
});

// ===== step 3: the integration =============================================================================================
await step(3, 'integration: demo_retail systems (test stub + prod at demo-backends), credential, demo data, routes, contract tests', async () => {
	assert(J.job, 'step 1 made no job');
	const cat = itemsOf(ok(await api('ta', 'GET', '/api/connections/catalogue'), 'GET /api/connections/catalogue'));
	const tpl = cat.filter((t) => t.key === 'demo_retail').sort((a, b) => Number(b.version) - Number(a.version))[0];
	if (!tpl) throw gap('no_connector_template', `the platform catalogue holds no demo_retail template (${cat.map((t) => t.key).join(', ') || 'empty'})`);
	for (const inst of [
		{ id: 'demo-retail-stub', label: 'Demo Retail (test)', template_key: tpl.key, template_version: tpl.version, env: 'test', stub: false, base_url: DEMO_URL }   /* the test env reads the same demo-backends (faults act only on env test): a stub returns one fixture for every customer */,
		{ id: 'demo-retail', label: 'Demo Retail backends', template_key: tpl.key, template_version: tpl.version, env: 'prod', stub: false, base_url: DEMO_URL },
	]) {
		const c = await api('ta', 'POST', '/api/connections/systems', inst);
		if (c.status === 422 && /invalid_target|invalid_url/.test(c.text)) throw gap('connector_private_target', `a new tenant cannot point an instance at demo-backends (${DEMO_URL}): non-public targets need a tenant-listed CONNECTOR_PRIVATE_TARGETS entry (netguard.rs check_url): ${c.text.slice(0, 200)}`);
		ok(c, `POST /api/connections/systems ${inst.id}`, [201]);
	}
	for (const id of ['demo-retail', 'demo-retail-stub']) ok(await api('ta', 'PUT', `/api/connections/systems/${id}/credential`, { service_token: DEMO_TOKEN }), `PUT credential ${id}`);
	// demo data: agent-api has no demo/reset route any more (fd7543d6). The tenant's external system seeds its own partition for
	// this tenant through ITS admin route (not a platform call).
	{
		const seed = await demoAdmin('POST', '/admin/reset', { seed: 1 });
		assert(seed.status === 200, `demo-backends POST /admin/reset (tenant ${J.tenant}) -> ${seed.status} ${seed.text.slice(0, 200)}`);
		note(`external system seeded for the tenant: ${(await demoCustomers()).length} customers`);
	}
	const ops = ok(await api('ta', 'GET', `/api/jobs/${J.job}/operations`), 'GET operations')?.ops ?? [];
	note(`GET operations: ${ops.length} expected (${ops.map((o) => opKey(o) + (o.required ? '*' : '')).join(', ').slice(0, 300)})`);
	let routed = 0;
	for (const o of ops.filter((x) => x.required)) {
		const op = opKey(o);
		for (const [env, instance] of [['test', 'demo-retail-stub'], ['prod', 'demo-retail']]) {
			const r = await api('ta', 'PUT', `/api/jobs/${J.job}/operations/${encodeURIComponent(op)}/routes/${env}`, { target: { kind: 'instance', instance_id: instance } });
			if (r.status === 422 && /not_implemented/.test(r.text)) continue; // the template does not implement it on that system: readiness will say
			ok(r, `PUT route ${op}@${env}`);
			if (env === 'prod') {
				const t = await api('ta', 'POST', '/api/connections/systems/demo-retail/contract-tests', { op, prod_write_ok: o.side_effect !== 'R' });
				if (t.status === 404 && /operation_not_implemented/.test(t.text)) {
					gaps.push(`contract_test:${op}`);
					note(`PRODUCT GAP: contract_test:${op} - POST .../contract-tests -> 404 operation_not_implemented although PUT route@prod succeeded: ${t.text.slice(0, 160)}`);
				} else ok(t, `contract test ${op}`);
			}
			routed++;
		}
	}
	note(`template ${tpl.key}@${tpl.version}; ${routed} routes set (required ops: ${ops.filter((x) => x.required).length})`);
});

// ===== step 4: channels ====================================================================================================
const agentEmail = `ps-agent-${RUN}@platform-stack.example.test`;
await step(4, 'channels: customer web chat (identity secret) and the staff side (agent invited, in the job staff team)', async () => {
	assert(J.job, 'step 1 made no job');
	const b = ok(
		await api('ta', 'POST', `/api/jobs/${J.job}/channel-bindings`, {
			kind: 'web_chat',
			side: 'customer',
			label: 'Website chat',
			settings: { display: { title: 'Platform stack shop', greeting: 'Hi! How can we help?', colour: '#1f6f5c' }, identity: { mode: 'hs256', issuer: ISSUER, session_hours: 8 }, limits: { sessions_per_ip_hour: 1000, msgs_per_ip_10m: 1000 } },
		}),
		'POST channel-bindings (web_chat)',
		[200, 201],
	);
	J.widgetKey = b.external_id;
	assert(J.widgetKey, `no widget key in the binding ${JSON.stringify(b).slice(0, 200)}`);
	J.binding = b.id;
	note(`binding ${b.id} env ${b.env ?? '?'} (D130: a new customer binding starts in test)`);
	const sec = ok(await api('ta', 'POST', `/api/jobs/${J.job}/channel-bindings/${b.id}/identity-secret`, {}), 'POST identity-secret');
	J.idKid = sec?.kid;
	J.idSecret = sec?.secret;
	assert(J.idKid && J.idSecret, 'identity-secret answered no {kid, secret}');
	ok(await api('ta', 'POST', '/api/tenant/invites', { email: agentEmail, roles: ['support_agent'], display_name: 'Platform Stack Agent' }), 'POST /api/tenant/invites (staff agent)', [200, 201, 202]);
	const kc = await kcUserByEmail(agentEmail);
	if (!kc?.id) throw gap('invite_identity', 'the invite created no identity in the (fake) Keycloak admin');
	who.ag = { persona: 'ag', sub: kc.id, email: agentEmail };
	ok(await api('ag', 'GET', '/api/me'), 'GET /api/me as the invited agent (first sign-in seats the membership)');
	const staff = await api('ta', 'PUT', `/api/jobs/${J.job}/staff`, { members: [kc.id], routing: { mode: 'off' } });
	if (staff.status === 422 || staff.status === 409) throw gap(`staff_${staff.json?.code ?? staff.status}`, `the invited agent cannot join the job staff team: ${staff.text.slice(0, 300)}`);
	ok(staff, 'PUT /api/jobs/{job}/staff');
	note(`widget key ${J.widgetKey}; agent ${agentEmail} on the staff team`);
});

async function askKnowledge(c, what) {
	const r = ok(await c.send(KNOWLEDGE_Q, { conversation_id: c.id, token: c.token }), `${what}: POST knowledge question`, [200]);
	const text = String(r?.text ?? '');
	note(`${what} knowledge question: "${KNOWLEDGE_Q}" -> "${text.replace(/\s+/g, ' ').slice(0, 400)}"`);
	assert(text.includes(OWN_PHRASE), `${what}: the answer has no text of the tenant's own uploaded document ("${OWN_PHRASE}"): ${text.slice(0, 300)}`);
	assert(text.includes(PLATFORM_PHRASE), `${what}: the answer has no text of the platform pack ("${PLATFORM_PHRASE}"): ${text.slice(0, 300)}`);
 const core = c.token.slice(0,c.token.lastIndexOf('.')).split('~')[0];
 const conv = ok(await api('ta','GET',`/api/jobs/${J.job}/conversations/${encodeURIComponent(core)}`), 'read knowledge conversation');
 const caseId=conv.conversation?.case_id;
 assert(caseId, `${what}: knowledge question has no case`);
 const detail=ok(await api('ta','GET',`/api/jobs/${J.job}/cases/${encodeURIComponent(caseId)}`), 'read knowledge case');
 assert(detail.case.status==='resolved', `${what}: knowledge case is ${detail.case.status}`);
 assert((detail.timeline??[]).some(e=>e.op==='knowledge.answer' && e.step?.step==='tenant_knowledge_answer' && e.step?.procedure===`ps_${RUN}_knowledge@1`), `${what}: no authored knowledge step in operation timeline: ${JSON.stringify(detail.timeline).slice(0,1800)}`);
}
// ===== step 5: test, readiness through the product API, publish =============================================================
let testChat = null;
const PROBE = { 'probe.case_id': 'CS-50001', 'probe.order_id': 'RET-01', 'probe.subject_object': 'RET-01', 'probe.tracking_ref': 'SHP-9101' }; // seeded demo-backends ids (seed/catalogue.rs); a made-up 'C-1' is a 404 not_found = red
// probe.<field> -> a test identifier of the demo-backends dataset
const resolvers = {
	async slot_not_deployed(item) {
		const slot = item.subject.split('.').pop();
		const cur = ok(await api('ta', 'GET', '/api/providers/slots'), 'GET /api/providers/slots');
		const row = itemsOf(cur?.slots ?? cur).find((x) => x.slot === slot || x.key === slot);
		ok(await api('ta', 'PUT', `/api/providers/slots/${slot}`, { kind: ({ ocr: 'mistral_ocr', rerank: 'cohere' })[slot] ?? 'openai_compatible', base_url: LLM_URL, model: LLM_MODEL, secret: 'local-inert', expected_version: row?.version ?? 0 }), `PUT /api/providers/slots/${slot}`);
	},
	async knowledge_unbound(item) {
		const rest = item.subject.slice(item.subject.indexOf(':') + 1);
		const [stage, kindLang] = rest.split('|');
		const kind = (kindLang ?? '').split(':')[0];
		if (!stage || !kind) throw gap('knowledge_unbound', `cannot read stage|kind from ${item.subject}`);
		await bindOwnKnowledge(stage, kind);
	},
	async route_not_ready(item) {
		const at = item.subject.lastIndexOf('@');
		const op = item.subject.slice(0, at);
		const env = item.subject.slice(at + 1);
		// the agent runs these in-process (resolve.rs is_builtin_op): the route is { kind: 'builtin' }, never an instance
		const builtin = /^(knowledge\.(search|answer)|credential\.verify|rule\.evaluate)(\{.*\})?$/.test(op);
		const target = builtin ? { kind: 'builtin' } : { kind: 'instance', instance_id: env === 'prod' ? 'demo-retail' : 'demo-retail-stub' };
		const pr = await api('ta', 'PUT', `/api/jobs/${J.job}/operations/${encodeURIComponent(op)}/routes/${env}`, { target });
		if (pr.status === 422 && /built-in/.test(pr.text)) throw gap(`builtin_route_not_ready:${op}`, `readiness blocks on route_not_ready(${op}@${env}) but PUT .../routes/${env} -> 422 ${pr.text.slice(0, 220)}`);
		ok(pr, `PUT route ${op}@${env}`);
		if (env === 'prod' && !builtin) {
			{
				const inst = ok(await api('ta', 'GET', '/api/connections/systems/demo-retail'), 'GET system')?.instance ?? {};
				const have = inst.env_ids ?? {};
				if (Object.entries(PROBE).some(([k, v]) => have[k] !== v)) ok(await api('ta', 'PATCH', '/api/connections/systems/demo-retail', { env_ids: { ...have, ...PROBE } }), 'PATCH env_ids (probe ids)');
			}
			let t = await api('ta', 'POST', '/api/connections/systems/demo-retail/contract-tests', { op, prod_write_ok: true });
			if (t.status === 404 && /operation_not_implemented/.test(t.text)) throw gap(`contract_test:${op}`, `contract test -> 404 ${t.text.slice(0, 160)}`);
			ok(t, `contract test ${op}`);
			// a probe input the instance does not hold yet: set the named env id (the tenant admin's own setting), then run again
			for (let k = 0; k < 4 && t.json?.code === 'probe_input_missing'; k++) {
				const idName = /env id .(probe\.[a-z_]+)/i.exec(t.json.note ?? '')?.[1];
				if (!idName) break;
				const inst = ok(await api('ta', 'GET', '/api/connections/systems/demo-retail'), 'GET system')?.instance ?? {};
				const have = inst.env_ids ?? {};
				ok(await api('ta', 'PATCH', '/api/connections/systems/demo-retail', { env_ids: { ...have, [idName]: PROBE[idName] ?? 'C-1' } }), `PATCH env_ids ${idName}`);
				t = await api('ta', 'POST', '/api/connections/systems/demo-retail/contract-tests', { op, prod_write_ok: true });
				ok(t, `contract test ${op} (after ${idName})`);
			}
			if (t.json?.outcome && t.json.outcome !== 'green') throw gap(`contract_${t.json.outcome}:${op}`, `POST .../contract-tests ${op} on demo-retail (prod) -> outcome ${t.json.outcome} code ${t.json.code} note ${t.json.note}; the route stays not ready (demo-backends serves no /cases, only template paths it implements)`);
		}
	},
	async hierarchy_incomplete() {
		const t = ok(await api('ta', 'GET', `/api/jobs/${J.job}/target`), 'GET target');
		const sel = t?.selection ?? {};
		const ind = J.template?.industry ?? J.profile?.industry;
		const dps = J.template?.departments?.length ? J.template.departments : J.profile?.departments;
		if (ind) sel.industry = ind;
		if (dps?.length) sel.departments = dps;
		sel.dimensions = { ...(sel.dimensions ?? {}), channel: ['web-chat'] };
		ok(await api('ta', 'PUT', `/api/jobs/${J.job}/target`, { selection: sel, expected_updated_at: t?.selection_updated_at ?? null }), 'PUT target (selection)');
	},
};
await step(5, 'test chat answers in the Test env; readiness is cleared through the product API; the job is published', async () => {
	assert(J.job, 'step 1 made no job');
	// test: the staff test chat on the draft (Test env)
	const custs = await demoCustomers();
	if (!custs.length) throw gap('no_test_customers', 'the external system holds no customer for this tenant (admin/reset seeded none)');
	J.cust = custs[0];
	let conv;
	for (const end = Date.now() + 120000; ; await sleep(3000)) {   // 503 llm_inference_not_provisioned: the bundle mint may still be landing
		conv = await api('ta', 'POST', `/api/jobs/${J.job}/conversations`, { env: 'test', customer_email: J.cust.email });
		if (conv.status === 201 || !/llm_inference_not_provisioned/.test(conv.text) || Date.now() > end) break;
	}
	llmGap(conv);
	conv = ok(conv, 'POST conversations (test)', [201]);
	ok(await api('ta', 'POST', `/api/jobs/${J.job}/conversations/${conv.conversation_id}/messages`, { text: 'Where is my parcel?' }), 'POST test message', [200, 202]);
	let reply = '';
	for (const end = Date.now() + ANSWER_MS; Date.now() < end && !reply; await sleep(2000)) {
		const t = ok(await api('ta', 'GET', `/api/jobs/${J.job}/conversations/${conv.conversation_id}`), 'GET test transcript');
		reply = (t?.messages ?? []).filter((m) => m.role !== 'customer' && m.role !== 'user' && m.kind !== 'note').map((m) => m.text ?? '').join(' ').trim();
	}
	assert(reply, `the Test env gave no reply within ${ANSWER_MS} ms`);
	note(`test chat reply: ${reply.replace(/\s+/g, ' ').slice(0, 120)}`);
	// the customer door on the TEST binding (D130: env test answers with the draft, before any release)
	testChat = await customerChat(J.cust, 'customer on the test binding');
	// knowledge use on the TEST binding: the answer must carry text of the tenant's own document AND of the platform pack.
	await askKnowledge(testChat, 'test binding');
	// readiness
	let blocking = await readiness();
	note(`blocking at start: ${codes(blocking)}`);
	for (let round = 1; round <= 5 && blocking.length; round++) {
		for (const item of blocking) {
			const fn = resolvers[item.code];
			if (!fn) {
				gap(item.code, `no product call resolves ${item.code}(${item.subject})`);
				continue;
			}
			try {
				await fn(item);
				note(`resolved ${item.code}(${item.subject})`);
			} catch (e) {
				if (String(e.message).startsWith('PRODUCT GAP')) { note(String(e.message).slice(0, 600)); continue; }
				throw new Error(`RESOLVE FAILED ${item.code}(${item.subject}): ${e.message}`);
			}
		}
		blocking = await readiness();
		note(`blocking after round ${round}: ${codes(blocking)}`);
	}
	if (blocking.length) {
		for (const c of new Set(blocking.map((b) => b.code))) if (!gaps.includes(c)) gaps.push(c);
		throw new Error(`PRODUCT GAP: ${[...new Set(blocking.map((b) => b.code))].join(', ')} - still blocking after the product calls above: ${codes(blocking)}`);
	}
	// the release target: a retail bundle needs >= 1 area and >= 1 channel in the selection (compile.rs violations no_area/no_channel);
	// the tenant admin picks them on the release target page (PUT /api/jobs/{job}/target), readiness does not ask for them
	const target = await pickTarget();
	const cut = await api('ta', 'POST', `/api/jobs/${J.job}/releases`, { apply: false, selection_seen_at: target?.selection_updated_at ?? null });
	const rel = ok(cut, 'POST releases', [200, 201])?.release;
	assert(rel?.id, `no release id in ${cut.text.slice(0, 300)}`);
	const ap = await api('ta', 'POST', `/api/jobs/${J.job}/releases/${encodeURIComponent(rel.id)}/apply`);
	assert(ap.status !== 409, `apply blocked after readiness was clear (409): ${ap.text.slice(0, 300)}`);
	ok(ap, `apply ${rel.id}`);
	note(`release ${rel.id} applied on ${J.job}`);
});

if (STATE && J.job) {
	// the web e2e (scripts/local/web-e2e.sh) plays the tenant's site with these: the binding's identity key + secret sign a
	// known customer's assertion (assertionFor), the customers are the tenant's own external system's (mode 0600)
	const customers = J.binding && J.idSecret ? (await demoCustomers().catch(() => [])).slice(0, 8).map((c) => ({ customer_id: c.customer_id, email: c.email, name: c.name })) : [];
	writeFileSync(STATE, JSON.stringify({ tenant: J.tenant, job: J.job, widget_key: J.widgetKey, binding: J.binding, id_kid: J.idKid, id_secret: J.idSecret, issuer: ISSUER, customers, admin: who.ta && { sub: who.ta.sub, email: who.ta.email }, agent: who.ag && { sub: who.ag.sub, email: who.ag.email } }, null, 1), { mode: 0o600 });
}
if (UNTIL <= 5 && J.job && who.ta) {
	console.log(`\nLogins (browser DevTools on ${new URL(IDP).hostname}, then reload ${process.env.PS_WEB ?? 'the web UI'}):`);
	console.log(`  tenant admin: document.cookie='b8_persona=ta; path=/'; document.cookie='b8_sub=${who.ta.sub}; path=/'; document.cookie='b8_email=${encodeURIComponent(who.ta.email)}; path=/'`);
	if (who.ag) console.log(`  staff agent : document.cookie='b8_persona=ag; path=/'; document.cookie='b8_sub=${who.ag.sub}; path=/'; document.cookie='b8_email=${encodeURIComponent(who.ag.email)}; path=/'`);
	console.log(`  customer web chat: ${EDGE}/webchat/demo?key=${J.widgetKey}  (pick a customer; no login)`);
}

// ===== step 6: publish the binding (D130), live customer chat -> handover -> staff reply ==================================
let chat = null;
await step(6, 'the web chat binding is published after the applied release; a known customer chats live, asks for a person, the staff agent replies and the customer sees it', async () => {
	assert(J.widgetKey && J.binding, 'step 4 made no widget key / binding');
	assert(who.ag, 'step 4 made no staff agent');
	// D130: a customer binding starts in env=test; publish moves it to prod and needs an applied release (409 no_release otherwise)
	const pub = await api('ta', 'POST', `/api/jobs/${J.job}/channel-bindings/${J.binding}/publish`, {});
	if (pub.status === 409 && /no_release/.test(pub.text)) throw gap('binding_no_release', `publish -> 409 no_release although step 5 applied a release: ${pub.text.slice(0, 200)}`);
	const pb = ok(pub, 'POST channel-bindings/{id}/publish', [200, 201]);
	note(`binding published${pb?.env ? ` (env ${pb.env})` : ''}`);
	chat = await customerChat(J.cust, 'live customer');
	const send = chat.send;
	const bearer = chat.bearer;
	await askKnowledge(chat, 'live');
	await send('I would like to speak to a person please.', { conversation_id: chat.id, token: chat.token });
	const findRow = async () => itemsOf(ok(await api('ag', 'GET', `/api/jobs/${J.job}/console/conversations`), 'GET console/conversations')).find((r) => r.channel === 'web_chat' && (r.last_message?.at_ms ?? 0) >= START_MS - 5000);
	let row = null;
	for (const end = Date.now() + 60000; Date.now() < end; await sleep(1500)) {
		row = await findRow();
		if (row?.owner?.owner && row.owner.owner !== 'agent') break;
	}
	assert(row?.owner?.owner && row.owner.owner !== 'agent', `the conversation did not hand over by itself within 60 s (owner ${JSON.stringify(row?.owner)}): the job needs a handover_to_person space and the LLM must pick it`);
	note(`handed over from the conversation; owner ${row.owner.owner}`);
	const reply = `Hello, this is a colleague from the team (platform-stack ${RUN}).`;
	const base = `/api/jobs/${J.job}/conversations/${encodeURIComponent(row.conversation_id)}`;
	ok(await api('ag', 'POST', `${base}/take`, { expected_owner: row.owner.owner }), 'POST take');
	ok(await api('ag', 'POST', `${base}/messages`, { text: reply, kind: 'reply' }), 'POST reply');
	let seen = false;
	let last = '';
	for (const end = Date.now() + 30000; Date.now() < end && !seen; await sleep(1500)) {
		const h = await edge('GET', `/api/webchat/conversations/${encodeURIComponent(chat.id)}/messages?token=${encodeURIComponent(chat.token)}`, undefined, bearer);
		last = `${h.status} ${h.text.slice(0, 160)}`;
		seen = h.status === 200 && h.text.includes(reply);
	}
	assert(seen, `the staff reply is not in the customer's history after 30 s (last: ${last})`);
	note('staff reply visible to the customer');
});

process.exit(finish());
