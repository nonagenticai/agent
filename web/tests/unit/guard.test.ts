// The live-client guard (B1 T17). Nothing in src/ reaches the network or Keycloak except two files —
// src/lib/auth/session.ts (keycloak-js sign-in) and src/lib/api/http-client.ts (fetch) — and every request goes
// through a route that a live method names:
//   (1) fetch( / EventSource( only in http-client.ts, Keycloak only in session.ts;
//   (2) every req('VERB', `/api/…`) (and req.stream / req.form) in src/lib/api/http/*.ts is a literal that matches a
//       ROUTES template;
//   (3) every ROUTES key is an area method and each of its routes (query stripped) is in that method's
//       doc comment in areas/*.ts;
//   (4) the methods httpB1 ∪ httpInteraction ∪ httpB2 ∪ httpB3 ∪ httpB7 ∪ httpB12 ∪ httpB4 ∪ httpB4Journey ∪ httpPk ∪ httpB5 ∪ httpB6 ∪ httpB8 ∪ httpSupport (∪ later httpBn) implement are exactly the ROUTES
//       keys.
import { readdirSync, readFileSync, statSync } from 'node:fs';
import { join } from 'node:path';
import { describe, expect, it } from 'vitest';
import { emptySelection } from '../../src/lib/api/types/core';
import { isRouted, type Verb } from '../../src/lib/api/http-client';
import { ROUTES } from '../../src/lib/api/http/routes';
import { httpB1 } from '../../src/lib/api/http/b1';
import { httpB7 } from '../../src/lib/api/http/b7';
import { httpInteraction } from '../../src/lib/api/http/interaction';
import { httpB12 } from '../../src/lib/api/http/b12';
import { httpB2 } from '../../src/lib/api/http/b2';
import { httpB3 } from '../../src/lib/api/http/b3';
import { httpB4 } from '../../src/lib/api/http/b4';
import { httpB4Journey } from '../../src/lib/api/http/b4-journey';
import { httpPk } from '../../src/lib/api/http/pk';
import { B5_METHODS, httpB5 } from '../../src/lib/api/http/b5';
import { B6_METHODS, httpB6 } from '../../src/lib/api/http/b6';
import { httpB8 } from '../../src/lib/api/http/b8';
import { SUPPORT_METHODS, httpSupport } from '../../src/lib/api/http/support';
import { IAM_METHODS, httpIam } from '../../src/lib/api/http/iam';
import { httpMcpTasks } from '../../src/lib/api/http/mcp-tasks';

function walk(dir: string): string[] {
	return readdirSync(dir).flatMap((name) => {
		const p = join(dir, name);
		return statSync(p).isDirectory() ? walk(p) : [p];
	});
}

const stripComments = (src: string) =>
	src.replace(/\/\*[\s\S]*?\*\//g, '').replace(/(^|[^:])\/\/.*$/gm, '$1');

const SESSION = join('src', 'lib', 'auth', 'session.ts');
const HTTP = join('src', 'lib', 'api', 'http-client.ts');
/** What each allowed file may do, and nothing more. */
const ALLOWED: Record<string, string[]> = { [SESSION]: ['Keycloak init'], [HTTP]: ['fetch('] };

const FORBIDDEN: [string, RegExp][] = [
	['fetch(', /\bfetch\s*\(/],
	['EventSource(', /\bEventSource\s*\(/],
	['Keycloak init', /keycloak-js|new\s+Keycloak\s*\(|\bkeycloak\s*\.\s*init\s*\(|\binitAuth\s*\(/i],
];

/** `req<…>('VERB', `/api/…`)` calls: [verb, path with `${…}` as one segment] — and how many `req(` calls there are. */
function reqCalls(src: string): { literal: [string, string][]; total: number } {
	const code = stripComments(src);
	const total = [...code.matchAll(/\breq(?:\.(?:stream|form))?\s*(?:<[\s\S]*?>)?\(\s*'/g)].length;
	const literal = [
		...code.matchAll(
			/\breq(?:\.(?:stream|form))?\s*(?:<[\s\S]*?>)?\(\s*'(GET|POST|PUT|PATCH|DELETE)',\s*([`'])(\/api[^`']*)\2/g,
		),
	].map((m) => [m[1]!, m[3]!.replace(/\$\{[^}]*\}/g, 'x')] as [string, string]);
	return { literal, total };
}

/** Every method's doc comment in areas/*.ts (`/** … *\/` right before `name(` in the area's type). */
function docComments(): Map<string, string> {
	const out = new Map<string, string>();
	const dir = join('src', 'lib', 'api', 'areas');
	for (const f of readdirSync(dir).filter((x) => x.endsWith('.ts'))) {
		const src = readFileSync(join(dir, f), 'utf8');
		for (const m of src.matchAll(/\/\*\*((?:(?!\*\/)[\s\S])*)\*\/\s*\n\s*(\w+)\s*\(/g))
			out.set(m[2]!, `${out.get(m[2]!) ?? ''} ${m[1]!.replace(/\s*\n\s*\*\s?/g, ' ')}`);
	}
	return out;
}

describe('live-client guard', () => {
	const files = walk('src').filter((f) => /\.(ts|js|svelte|html)$/.test(f));

	it('scans the source tree', () => {
		expect(files.length).toBeGreaterThan(50);
		expect(files).toContain(join('src', 'lib', 'api', 'client.ts'));
	});

	it('(1) has no fetch(, EventSource( or Keycloak init outside the two allowed files', () => {
		const hits: string[] = [];
		for (const f of files) {
			const raw = readFileSync(f, 'utf8');
			const src = f === join('src', 'lib', 'api', 'client.ts') || ALLOWED[f] ? stripComments(raw) : raw;
			for (const [name, re] of FORBIDDEN)
				if (re.test(src) && !ALLOWED[f]?.includes(name)) hits.push(`${f}: ${name}`);
		}
		expect(hits).toEqual([]);
	});

	it('(1) the allow-list is exactly session.ts (Keycloak) and http-client.ts (fetch), and both still exist', () => {
		expect(Object.keys(ALLOWED).sort()).toEqual([HTTP, SESSION].sort());
		expect(files).toContain(SESSION);
		expect(files).toContain(HTTP);
		const session = stripComments(readFileSync(SESSION, 'utf8'));
		expect(session).not.toMatch(/\bfetch\s*\(/);
		expect(session).toMatch(/from 'keycloak-js'/);
		expect(session).toMatch(/pkceMethod: 'S256'/);
		expect(session).toMatch(/onLoad: 'login-required'/);
		expect(stripComments(readFileSync(HTTP, 'utf8'))).not.toMatch(/keycloak-js|EventSource/);
	});

	it('(2) every req() in src/lib/api/http/*.ts is a literal /api path that matches a ROUTES template', () => {
		const dir = join('src', 'lib', 'api', 'http');
		const mods = readdirSync(dir).filter((f) => f.endsWith('.ts') && f !== 'routes.ts');
		expect(mods).toContain('b1.ts');
		let seen = 0;
		for (const f of mods) {
			const { literal, total } = reqCalls(readFileSync(join(dir, f), 'utf8'));
			expect(literal.length, `${f}: every req( call has a literal '/api…' path`).toBe(total);
			for (const [verb, path] of literal)
				expect(isRouted(verb as Verb, path), `${f}: ${verb} ${path}`).toBe(true);
			seen += literal.length;
		}
		expect(seen).toBeGreaterThanOrEqual(Object.keys(ROUTES).length);
	});

	it('(3) every ROUTES key is an area method and its routes are in that method’s doc comment', () => {
		const docs = docComments();
		for (const [m, routes] of Object.entries(ROUTES)) {
			expect(docs.has(m), `${m} is an area method`).toBe(true);
			for (const r of routes as readonly string[])
				expect(docs.get(m) ?? '', `${m}: doc comment names ${r}`).toContain(r.split('?')[0]!);
		}
	});

	it('(4) httpB1 ∪ httpInteraction ∪ httpB2 ∪ httpB3 ∪ httpB7 ∪ httpB12 ∪ httpB4 ∪ httpB4Journey ∪ httpPk ∪ httpB5 ∪ httpB6 ∪ httpB8 implement exactly the ROUTES keys', () => {
		const stub = (() => Promise.resolve(undefined)) as never;
		const keys = [
			...Object.keys(httpB1(stub)),
			...Object.keys(httpInteraction(stub)),
			...Object.keys(httpB2(stub, () => 'job')),
			...Object.keys(httpB3(stub, () => 'job')),
			...Object.keys(httpB7(stub)),
			...Object.keys(httpB12(stub, () => 'job')),
			...Object.keys(httpB4(stub, () => 'job')),
			...Object.keys(httpB4Journey(stub, () => 'job')),
			...Object.keys(httpPk(stub)),
			...Object.keys(httpB5(stub, () => 'job')),
			...Object.keys(httpB6(stub)),
			...Object.keys(httpB8(stub, () => 'job')),
			...Object.keys(httpSupport(stub)),
			...Object.keys(httpIam(stub, () => 'job')),
			...Object.keys(httpMcpTasks(stub, () => 'job')),
		];
		expect(new Set(keys).size, 'no method in two modules').toBe(keys.length);
		expect(keys.sort()).toEqual(Object.keys(ROUTES).sort());
	});

	it('pins the staff test chat (B4 T10a, M1): the 5 /chat methods and the conversation SSE are live', () => {
		expect(ROUTES).toMatchObject({
			chatProfile: ['GET /api/jobs/{job}/chat/profile'],
			startChat: ['POST /api/jobs/{job}/conversations'],
			chatConversation: ['GET /api/jobs/{job}/conversations/{id}'],
			sendChatMessage: ['POST /api/jobs/{job}/conversations/{id}/messages'],
			conversationEvents: ['GET /api/jobs/{job}/conversations/{id}/events'],
		});
		// (4) pins httpB4's method set exactly: the /chat 5 (T10a), the case methods (T12) and T21a's runtime + usage
		// methods, and the console Bus stream's backlog (B7). A method added to httpB4 without a line here fails.
		expect(Object.keys(httpB4((() => Promise.resolve(undefined)) as never, () => 'job')).sort()).toEqual([
			'agentFleet',
			'agentStatus',
			'busAgents',
			'busBacklog',
			'busFlow',
			'caseDetail',
			'caseload',
			'cases',
			'chatConversation',
			'chatProfile',
			'checkUnknownOutcome',
			'claimStuck',
			'deadLetters',
			'discardDeadLetter',
			'handOverCase',
			'inbox',
			'jobAnalytics',
			'jobOverview',
			'operationCalls',
			'operationStats',
			'replayDeadLetter',
			'searchCases',
			'sendChatMessage',
			'startChat',
			'streamEntries',
			'tenantUsage',
			'unknownOutcomes',
		]);
		expect(isRouted('GET', '/api/jobs/j/conversations/c/events')).toBe(true);
		expect(isRouted('POST', '/api/jobs/j/conversations/c/messages')).toBe(true);
	});

	it('pins the B4 T21a runtime methods: operation calls (T13), bus admin (T14), tenant usage (T11), J1/J2 (T15)', () => {
		// Replaces the pre-T13 pin that kept the operation-call reads off the wire: agent-api serves them since #43.
		expect(ROUTES).toMatchObject({
			operationCalls: [
				'GET /api/jobs/{job}/operation-calls?op=&case_id=&route=&outcome=&env=&instance=&handling=&before=',
			],
			operationStats: ['GET /api/jobs/{job}/operation-stats'],
			unknownOutcomes: ['GET /api/jobs/{job}/operation-calls/unknown'],
			checkUnknownOutcome: ['POST /api/jobs/{job}/operation-calls/{call_id}/check'],
			busAgents: ['GET /api/jobs/{job}/agents'],
			agentFleet: ['GET /api/jobs/{job}/agents/fleet'],
			busFlow: ['GET /api/jobs/{job}/bus-flow?env='],
			streamEntries: ['GET /api/jobs/{job}/streams/{key}/entries?limit=&env='],
			deadLetters: ['GET /api/jobs/{job}/dead-letters'],
			claimStuck: ['POST /api/jobs/{job}/streams/{key}/groups/{group}/claim?env='],
			replayDeadLetter: ['POST /api/jobs/{job}/dead-letters/{id}/replay'],
			discardDeadLetter: ['DELETE /api/jobs/{job}/dead-letters/{id}'],
			tenantUsage: ['GET /api/tenant/usage?window=&group='],
			jobOverview: ['GET /api/jobs/{job}/overview'],
			jobAnalytics: ['GET /api/jobs/{job}/analytics?days='],
		});
		// the systems page and the providers map read them for a real caller now: nothing of theirs stays pending
		// J2 analytics also calls jobGates: live since B5 T13, so that page shows real data too
		expect(ROUTES).toHaveProperty('jobGates');
	});

	it('pins the B4 T21b methods: J21 policy + hours and feedback, tenant calendar, /chat rating, test cases', () => {
		// (4) pins httpB4Journey's method set exactly. A method added to it without a line here fails.
		expect(
			Object.keys(httpB4Journey((() => Promise.resolve(undefined)) as never, () => 'job')).sort(),
		).toEqual([
			'draftHandoverProposal',
			'handoverFeedback',
			'handoverPolicy',
			'saveHandoverPolicy',
			'saveTenantCalendar',
			'saveTestCase',
			'submitCsat',
			'tenantCalendar',
			'testCases',
		]);
		// the path set, as crates/agent-api/src/routes/{handover,tenant_calendar,test_cases}.rs and
		// conversations/routes.rs serve it
		expect(ROUTES).toMatchObject({
			handoverPolicy: ['GET /api/jobs/{job}/handover/policy'],
			saveHandoverPolicy: ['PUT /api/jobs/{job}/handover/policy'],
			handoverFeedback: ['POST /api/jobs/{job}/handover/{id}/feedback'],
			tenantCalendar: ['GET /api/tenant/calendar'],
			saveTenantCalendar: ['PUT /api/tenant/calendar'],
			submitCsat: ['POST /api/jobs/{job}/conversations/{id}/csat'],
			testCases: ['GET /api/jobs/{job}/test-cases'],
			saveTestCase: ['POST /api/jobs/{job}/test-cases'],
		});
		// the proposal draft is live since B8 T7 (the `activity` draft kind): nothing of the handover page is pending
		expect(ROUTES).toHaveProperty('draftHandoverProposal', [
			'POST /api/jobs/{job}/handover/{id}/feedback/draft',
		]);
		// /chat has nothing pending any more: its test cases and rating are live
	});

	it('pins the B5 T13 job design methods: J11 ontology, J12 procedures, J7 gates, J8 toolsets (31)', () => {
		// (4) pins httpB5's method set exactly. A method added to it without a line here fails.
		expect(Object.keys(httpB5((() => Promise.resolve(undefined)) as never, () => 'job')).sort()).toEqual(
			[...B5_METHODS].sort(),
		);
		expect(B5_METHODS.length).toBe(31);
		// the route table of the B5 plan, as crates/agent-api/src/routes/job_design/*.rs serves it;
		// saveContextSpaceRequired writes through B2's context-space PUT (R12) and reads the list back
		expect(ROUTES).toMatchObject({
			ontologyEntities: ['GET /api/jobs/{job}/ontology/entities?include_hidden=true'],
			ontologyRelations: ['GET /api/jobs/{job}/ontology/relations'],
			createOntologyEntity: ['POST /api/jobs/{job}/ontology/entities'],
			addOntologySynonym: ['POST /api/jobs/{job}/ontology/entities/{name}/synonyms'],
			addOntologyAttribute: ['POST /api/jobs/{job}/ontology/entities/{name}/attributes'],
			setOntologyEntityHidden: ['PUT /api/jobs/{job}/ontology/entities/{name}/hidden'],
			createOntologyRelation: ['POST /api/jobs/{job}/ontology/relations'],
			deleteOntologyRelation: ['DELETE /api/jobs/{job}/ontology/relations'],
			contextSpaces: ['GET /api/jobs/{job}/context-spaces'],
			saveContextSpaceRequired: [
				'PUT /api/jobs/{job}/context-spaces/{key}',
				'GET /api/jobs/{job}/context-spaces',
			],
			procedures: ['GET /api/jobs/{job}/procedures'],
			procedureVersions: ['GET /api/jobs/{job}/procedures/{name}'],
			createProcedureVersion: ['POST /api/jobs/{job}/procedures'],
			approveProcedure: ['POST /api/jobs/{job}/procedures/{name}/versions/{v}/approve'],
			setProcedureStatus: ['POST /api/jobs/{job}/procedures/{name}/versions/{v}/status'],
			jobGates: ['GET /api/jobs/{job}/gates'],
			refineGateOption: ['PUT /api/jobs/{job}/gates/{gate}/options/{option}'],
			setGateThreshold: ['PUT /api/jobs/{job}/gates/{gate}/threshold'],
			setGateRule: ['PUT /api/jobs/{job}/gates/{gate}/rule'],
			addGateQuestion: ['POST /api/jobs/{job}/gates'],
			deleteGateQuestion: ['DELETE /api/jobs/{job}/gates/{gate}'],
			setGateCustomerReply: ['PUT /api/jobs/{job}/gates/{gate}/customer-reply'],
			setGateEnabled: ['PUT /api/jobs/{job}/gates/{gate}/enabled'],
			reviewGates: ['POST /api/jobs/{job}/gates/review'],
			evaluateGates: ['POST /api/jobs/{job}/gates/evaluate'],
			gateVerdicts: ['GET /api/jobs/{job}/gates/verdicts?limit=50'],
			toolsetMeta: ['GET /api/jobs/{job}/toolsets'],
			narrowToolsetGrant: ['POST /api/jobs/{job}/toolsets/{role}/narrowings'],
			restoreToolsetGrant: ['DELETE /api/jobs/{job}/toolsets/{role}/narrowings'],
			toolsetOverview: ['GET /api/jobs/{job}/toolsets/overview?env='],
			setAgentRoleEnabled: ['PUT /api/jobs/{job}/toolsets/{role}/enabled'],
		});
		// B5 registers no context-space override route of its own (B2 owns the PUT)
		expect(isRouted('PUT', '/api/jobs/j/context-spaces/k/override')).toBe(false);
	});

	it('pins the B6b platform model methods: every one maps to its declared verb and path', async () => {
		expect(Object.keys(httpB6((() => Promise.resolve(undefined)) as never)).sort()).toEqual(
			[...B6_METHODS].sort(),
		);
		expect(B6_METHODS.length).toBe(69);
		const sel = emptySelection();
		const rule = { fire_when: 'gte' as const, threshold: 0.6, fire_values: null, min_confidence: null };
		const edit = {
			identity_level: '1' as const,
			side_effect: 'W' as const,
			confirm_back: true,
			verify_read: null,
			idempotency: 'key' as const,
			input_fields: [],
			output_fields: [],
			description: '',
			extra_errors: [],
		};
		// method → [args, the one route it may call (ROUTES template), the concrete request it sends]
		const TABLE: Record<(typeof B6_METHODS)[number], [unknown[], string, string]> = {
			layerCatalog: [[], 'GET /api/layers', 'GET /api/layers'],
			layerRows: [
				['L1', { include_retired: true }],
				'GET /api/layers/{id}/rows',
				'GET /api/layers/L1/rows?include_retired=true',
			],
			layerRowRevisions: [
				['L1', 'gate_question', 'GA'],
				'GET /api/layers/{id}/rows/{kind}/{key}/revisions',
				'GET /api/layers/L1/rows/gate_question/GA/revisions',
			],
			findLayerRow: [['r1'], 'GET /api/layer-rows/{row_id}', 'GET /api/layer-rows/r1'],
			layerSummaries: [[], 'GET /api/layers/summary', 'GET /api/layers/summary'],
			layerOverrideTargets: [['L1'], 'GET /api/layers/{id}/overrides', 'GET /api/layers/L1/overrides'],
			createLayer: [
				[{ scope: 'industry', industry: 'retail', region: 'nl', label: 'NL' }],
				'POST /api/layers',
				'POST /api/layers',
			],
			writeLayerRow: [
				['L1', { kind: 'instruction', key: 'tone', op: 'patch', fields: {}, expected_rev: 2 }],
				'POST /api/layers/{id}/rows',
				'POST /api/layers/L1/rows',
			],
			compileOptions: [[], 'GET /api/compile/options', 'GET /api/compile/options'],
			compilePreview: [[{ selection: sel, job: null }], 'POST /api/compile', 'POST /api/compile'],
			compileReferences: [[], 'GET /api/compile/references', 'GET /api/compile/references'],
			businessAxes: [[], 'GET /api/axes', 'GET /api/axes'],
			addAxisValue: [
				['party', { key: 'b2b', label: 'B2B', parent: null }],
				'POST /api/axes/{key}/values',
				'POST /api/axes/party/values',
			],
			createAxis: [
				[{ key: 'tier', label: 'Tier', description: '', cardinality: 'one', object: 'Party' }],
				'POST /api/axes',
				'POST /api/axes',
			],
			retireAxisValue: [
				['party', 'b2b'],
				'POST /api/axes/{key}/values/{value}/retire',
				'POST /api/axes/party/values/b2b/retire',
			],
			setAxisCardinality: [['party', 'many'], 'PATCH /api/axes/{key}', 'PATCH /api/axes/party'],
			axisRules: [[], 'GET /api/axis-rules', 'GET /api/axis-rules'],
			createAxisRule: [
				[
					{
						key: 'r',
						priority: 1,
						condition: { eq: ['party', 'b2b'] },
						adds: { key: 'Account', label: '' },
						layer: 'base',
					},
				],
				'POST /api/axis-rules',
				'POST /api/axis-rules',
			],
			businessProfiles: [[], 'GET /api/profiles', 'GET /api/profiles'],
			saveBusinessProfile: [
				[{ key: 'p', label: 'P', industry: null, description: '', selection: {} }],
				'PUT /api/profiles/{key}',
				'PUT /api/profiles/p',
			],
			renewalPresets: [[], 'GET /api/axes/renewal-presets', 'GET /api/axes/renewal-presets'],
			platformDimensions: [[], 'GET /api/dimensions', 'GET /api/dimensions'],
			departmentArchetypes: [[], 'GET /api/department-archetypes', 'GET /api/department-archetypes'],
			platformIndustries: [[], 'GET /api/platform/industries', 'GET /api/platform/industries'],
			createIndustry: [
				[{ industry: 'telecom', label: 'Telecom' }],
				'POST /api/platform/industries',
				'POST /api/platform/industries',
			],
			renameIndustry: [
				['telecom', 'Tele'],
				'PATCH /api/platform/industries/{industry}',
				'PATCH /api/platform/industries/telecom',
			],
			retireIndustry: [
				['telecom'],
				'POST /api/platform/industries/{industry}/retire',
				'POST /api/platform/industries/telecom/retire',
			],
			restoreIndustry: [
				['telecom'],
				'POST /api/platform/industries/{industry}/restore',
				'POST /api/platform/industries/telecom/restore',
			],
			createDepartment: [
				['telecom', { department: 'support', label: 'Support' }],
				'POST /api/platform/industries/{industry}/departments',
				'POST /api/platform/industries/telecom/departments',
			],
			renameDepartment: [
				['telecom', 'support', 'Care'],
				'PATCH /api/platform/industries/{industry}/departments/{department}',
				'PATCH /api/platform/industries/telecom/departments/support',
			],
			retireDepartment: [
				['telecom', 'support'],
				'POST /api/platform/industries/{industry}/departments/{department}/retire',
				'POST /api/platform/industries/telecom/departments/support/retire',
			],
			restoreDepartment: [
				['telecom', 'support'],
				'POST /api/platform/industries/{industry}/departments/{department}/restore',
				'POST /api/platform/industries/telecom/departments/support/restore',
			],
			addDimensionValue: [
				['channel', { key: 'sms', label: 'SMS' }],
				'POST /api/dimensions/{key}/values',
				'POST /api/dimensions/channel/values',
			],
			createDimension: [
				[{ key: 'tier', label: 'Tier', description: '' }],
				'POST /api/dimensions',
				'POST /api/dimensions',
			],
			platformDimensionUsage: [[], 'GET /api/dimensions/usage', 'GET /api/dimensions/usage'],
			templateCatalog: [[], 'GET /api/platform/templates', 'GET /api/platform/templates'],
			templateTagDetail: [
				['industry_dept/retail/customer_service', 3],
				'GET /api/platform/templates/{layer}/v{n}',
				'GET /api/platform/templates/industry_dept.retail.customer_service/v3',
			],
			tagLayerRevisions: [
				['job/customer_care_agent', 'n'],
				'POST /api/platform/templates/{layer}/tags',
				'POST /api/platform/templates/job.customer_care_agent/tags',
			],
			retireTemplateTag: [
				['job/customer_care_agent', 4],
				'POST /api/platform/templates/{layer}/tags/v{n}/retire',
				'POST /api/platform/templates/job.customer_care_agent/tags/v4/retire',
			],
			restoreTemplateTag: [
				['job/customer_care_agent', 4],
				'POST /api/platform/templates/{layer}/tags/v{n}/restore',
				'POST /api/platform/templates/job.customer_care_agent/tags/v4/restore',
			],
			objectCatalog: [[], 'GET /api/platform/objects', 'GET /api/platform/objects'],
			createObject: [
				[{ object: 'widget', label: 'Widget', tier: 'base', description: '', layer_key: 'base' }],
				'POST /api/platform/objects',
				'POST /api/platform/objects',
			],
			objectModelMap: [[], 'GET /api/platform/objects/map', 'GET /api/platform/objects/map'],
			objectDetail: [['order'], 'GET /api/platform/objects/{object}', 'GET /api/platform/objects/order'],
			objectContext: [
				['order'],
				'GET /api/platform/objects/{object}/context',
				'GET /api/platform/objects/order/context',
			],
			saveObjectField: [
				[
					'order',
					{
						field: 'note',
						data_type: 'string',
						pii: false,
						enum_values: [],
						ref: null,
						required: false,
						description: '',
					},
				],
				'PUT /api/platform/objects/{object}/fields/{field}',
				'PUT /api/platform/objects/order/fields/note',
			],
			retireObjectField: [
				['order', 'note'],
				'POST /api/platform/objects/{object}/fields/{field}/retire',
				'POST /api/platform/objects/order/fields/note/retire',
			],
			saveObjectState: [
				['order', { status: 'held', label: 'Held', terminal: false, canonical: true }],
				'PUT /api/platform/objects/{object}/states/{status}',
				'PUT /api/platform/objects/order/states/held',
			],
			saveObjectTransition: [
				['order', { verb: 'order.cancel', from_status: 'placed', to_status: 'cancelled', guard_rule: null }],
				'PUT /api/platform/objects/{object}/transitions/{verb}/{from}',
				'PUT /api/platform/objects/order/transitions/order.cancel/placed',
			],
			deleteObjectTransition: [
				['order', 'order.cancel', 'placed'],
				'DELETE /api/platform/objects/{object}/transitions/{verb}/{from}',
				'DELETE /api/platform/objects/order/transitions/order.cancel/placed',
			],
			saveObjectRelation: [
				['order', 'has', 'order_line', '1:n'],
				'PUT /api/platform/objects/{object}/relations/{predicate}/{target}',
				'PUT /api/platform/objects/order/relations/has/order_line',
			],
			deleteObjectRelation: [
				['order', 'has', 'order_line'],
				'DELETE /api/platform/objects/{object}/relations/{predicate}/{target}',
				'DELETE /api/platform/objects/order/relations/has/order_line',
			],
			createOperation: [
				[
					{
						...edit,
						object: 'widget',
						verb: 'read',
						layer_key: 'base',
						modality: 'observe',
						op_kind: 'standard',
					},
				],
				'POST /api/platform/operations',
				'POST /api/platform/operations',
			],
			catalogOperations: [
				[{ object: 'order', builtin: false }],
				'GET /api/platform/operations',
				'GET /api/platform/operations?object=order&builtin=false',
			],
			saveOperation: [
				['case', 'create', 'return', edit],
				'PUT /api/platform/operations/{object}/{verb}',
				'PUT /api/platform/operations/case/create?variant=return',
			],
			errorStateDefs: [[], 'GET /api/platform/error-states', 'GET /api/platform/error-states'],
			gateLibrary: [[], 'GET /api/platform/gates', 'GET /api/platform/gates'],
			gateLibraryUsage: [[], 'GET /api/platform/gates/usage', 'GET /api/platform/gates/usage'],
			updateGateQuestion: [
				['GA', { question: 'Is this a refund?' }],
				'PATCH /api/platform/gates/{key}',
				'PATCH /api/platform/gates/GA',
			],
			addGateOption: [
				['GT', { label: 'Other', description: '' }],
				'POST /api/platform/gates/{key}/options',
				'POST /api/platform/gates/GT/options',
			],
			createGateQuestion: [
				[
					{
						key: 'GX',
						layer: 'base',
						group: 'base',
						point: 'P1',
						type: 'noul',
						question: 'Is it urgent?',
						options: [],
						threshold: 'p ≥ 0.6',
						rule,
					},
				],
				'POST /api/platform/gates',
				'POST /api/platform/gates',
			],
			retireGateQuestion: [
				['GX'],
				'POST /api/platform/gates/{key}/retire',
				'POST /api/platform/gates/GX/retire',
			],
			removeGateOption: [
				['GT', 'other'],
				'DELETE /api/platform/gates/{key}/options/{option}',
				'DELETE /api/platform/gates/GT/options/other',
			],
			agentRoleJobs: [[], 'GET /api/platform/agent-roles/jobs', 'GET /api/platform/agent-roles/jobs'],
			agentRoles: [
				['customer_care_agent'],
				'GET /api/platform/agent-roles',
				'GET /api/platform/agent-roles?job=customer_care_agent',
			],
			addAgentRoleGrant: [
				['customer_care_agent', 'qualifier', { op: 'order.read', variant: null, modalities: ['chat'] }],
				'POST /api/platform/agent-roles/{job}/{role}/grants',
				'POST /api/platform/agent-roles/customer_care_agent/qualifier/grants',
			],
			removeAgentRoleGrant: [
				['customer_care_agent', 'qualifier', 'case.create', 'return'],
				'DELETE /api/platform/agent-roles/{job}/{role}/grants/{object.verb}',
				'DELETE /api/platform/agent-roles/customer_care_agent/qualifier/grants/case.create?variant=return',
			],
			setAgentRoleMaxSideEffect: [
				['customer_care_agent', 'qualifier', 'R'],
				'PATCH /api/platform/agent-roles/{job}/{role}',
				'PATCH /api/platform/agent-roles/customer_care_agent/qualifier',
			],
			exportPlatformSeed: [
				[{ family: 'base', tag: 'v2' }],
				'GET /api/platform/export',
				'GET /api/platform/export?family=base&tag=v2',
			],
		};
		expect(Object.keys(TABLE).sort()).toEqual([...B6_METHODS].sort());
		for (const m of B6_METHODS) {
			const [args, route, sent] = TABLE[m];
			// the method's ROUTES entry is exactly this one route (query stripped)
			expect(
				(ROUTES[m] as readonly string[]).map((r) => r.split('?')[0]),
				m,
			).toEqual([route]);
			const calls: string[] = [];
			const req = ((
				verb: string,
				path: string,
				opts: { query?: Record<string, unknown>; response?: string } = {},
			) => {
				const q = new URLSearchParams();
				for (const [k, v] of Object.entries(opts.query ?? {}))
					if (v !== undefined && v !== null && v !== '') q.set(k, String(v));
				calls.push(`${verb} ${path}${q.size ? `?${q}` : ''}`);
				return Promise.resolve(
					opts.response === 'text'
						? { body: '{}', content_type: 'application/json', filename: '' }
						: { items: [] },
				);
			}) as never;
			await (httpB6(req)[m] as (...a: unknown[]) => Promise<unknown>)(...args);
			expect(calls, m).toEqual([sent]);
			const [verb, path] = sent.split(' ') as [Verb, string];
			expect(isRouted(verb, path), `${m}: ${sent}`).toBe(true);
		}
		// nothing else on the platform model planes is reachable
		expect(isRouted('DELETE', '/api/layers/L1')).toBe(false);
		expect(isRouted('PUT', '/api/platform/gates/GA')).toBe(false);
		expect(isRouted('GET', '/api/v1/layers')).toBe(false);
	});

	it('the guards themselves catch a violation', () => {
		expect(FORBIDDEN[0]![1].test("await fetch('/api/me')")).toBe(true);
		expect(FORBIDDEN[1]![1].test('new EventSource(url)')).toBe(true);
		expect(FORBIDDEN[2]![1].test("import Keycloak from 'keycloak-js'")).toBe(true);
		expect(FORBIDDEN[2]![1].test('await kc.init({}); new Keycloak({ url })')).toBe(true);
		expect(FORBIDDEN[2]![1].test('A Keycloak subject.')).toBe(false);
		expect(stripComments('// fetch( x )\nconst a = 1;')).not.toMatch(/fetch\(/);
		const calls = reqCalls("req<R<'me'>>('GET', `/api/tenants/${seg(id)}/users`); req('GET', path);");
		expect(calls).toEqual({ literal: [['GET', '/api/tenants/x/users']], total: 2 });
		expect(
			reqCalls("req.stream('POST', `/api/jobs/${j}/interactions`, o); req.form('POST', p, fd);"),
		).toEqual({
			literal: [['POST', '/api/jobs/x/interactions']],
			total: 2,
		});
		expect(isRouted('GET', '/api/tenants/x/users')).toBe(true);
		expect(isRouted('DELETE', '/api/tenants/x/users')).toBe(false);
		expect(isRouted('GET', '/api/v1/me')).toBe(false);
	});
});

describe('staff workspace methods (SW T8)', () => {
	it('pins httpSupport’s method set and the routes agent-api serves (SW T2-T7)', () => {
		expect(Object.keys(httpSupport((() => Promise.resolve(undefined)) as never)).sort()).toEqual(
			[...SUPPORT_METHODS].sort(),
		);
		expect(ROUTES).toMatchObject({
			supportInbox: ['GET /api/support/inbox?tab=&reason=&channel=&priority='],
			events: ['GET /api/support/events'],
			take: ['POST /api/jobs/{job}/conversations/{id}/take'],
			assign: ['POST /api/jobs/{job}/conversations/{id}/assign'],
			handBack: ['POST /api/jobs/{job}/conversations/{id}/hand-back'],
			send: ['POST /api/jobs/{job}/conversations/{id}/messages'],
			resolve: ['POST /api/jobs/{job}/handover/{id}/resolve'],
			presence: ['POST /api/jobs/{job}/conversations/{id}/presence'],
			canned: ['GET /api/jobs/{job}/canned-replies'],
			createCanned: ['POST /api/jobs/{job}/canned-replies'],
			deleteCanned: ['DELETE /api/jobs/{job}/canned-replies/{id}'],
			transcript: ['GET /api/jobs/{job}/conversations/{id}'],
		});
	});
});

describe('A2/A3 declaration methods', () => {
	it('pins httpIam’s method set to the routes agent-api serves', () => {
		expect(Object.keys(httpIam((() => Promise.resolve(undefined)) as never, () => 'job')).sort()).toEqual(
			[...IAM_METHODS].sort(),
		);
		expect(ROUTES).toMatchObject({
			iamProcedures: ['GET /api/iam-procedures'],
			saveToolPolicy: ['PUT /api/jobs/{job}/tools/{op}/policy'],
			attachIamProcedure: ['PUT /api/jobs/{job}/iam-procedures/{id}'],
		});
	});
});
