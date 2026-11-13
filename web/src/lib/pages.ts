// The page registry: all 61 pages of spec 14 §2 (P1–P20, T1–T14, J1–J25, L1–L2) plus L4 (Talk to a job). It drives the nav, the page
// titles and milestone chips, the route access guard, and tests/unit/coverage.test.ts (every route has a file).
// Owned by the foundation. Area agents read it (via PageHead / PagePlaceholder) and never edit it.
import type { Milestone, Role } from './api/types/core';
import type { Session } from './auth/session.svelte';

/** The areas of the product a page belongs to. */
export type Area =
	| 'runtime'
	| 'tenant'
	| 'platform'
	| 'model'
	| 'connectors'
	| 'release'
	| 'journey'
	| 'ontology'
	| 'live';

export type NavGroup = 'platform' | 'tenant' | 'job' | 'live';
/** Where a page sits in the redesigned shell: one scope per sidebar. `dev` is the super-admin-only tools. */
export type NavScope = 'workspace' | 'agent' | 'studio' | 'dev';
/** The task groups of the agent scope (and the collapsed `more` of the workspace scope). */
export type NavTask = 'operate' | 'build' | 'test_release' | 'advanced' | 'more';
/**
 * `effectiveRoles` is optional so callers that only gate on superAdmin/member (accessFor, devPages, role-less test
 * literals) keep compiling; navFor treats an absent set as "no role information" and does not narrow by role. Every
 * real caller passes the Session, which always has it.
 */
export type NavSession = Pick<Session, 'superAdmin' | 'member'> & { effectiveRoles?: readonly Role[] };

export interface PageRoute {
	/** SvelteKit route id, e.g. `/admin/jobs/[job]/release`. Its `+page.svelte` must exist. */
	route: string;
	/** Fragment of a page that is a tab of another page's route (J16 = `…/release#conflicts`). */
	hash?: string;
	/** Nav label when it differs from the page title (J25's second route). */
	label?: string;
	/** Task group of this route in the scoped nav (set from NAV_ENTRIES). */
	task?: NavTask;
	/** Label in the scoped nav when it differs from `label ?? title`. */
	navLabel?: string;
	/** False = reachable only from inside another page (detail routes, tabs). Default true. */
	inNav?: boolean;
	/**
	 * Any-of: the effective roles (held ∩ view-as) that can open this route. Set ONLY where the backend refuses a
	 * caller without the role on the route's own read (`Requires<R>` / `JobRequires<R>` on a GET, or the STAFF_ROLES
	 * check). The backend has no tenant_admin bypass (roles are independent, §R.1), so none is applied here. Unset =
	 * every member sees the route (reads are open to observers).
	 */
	roles?: readonly Role[];
}

export interface PageDef extends PageRoute {
	id: string;
	title: string;
	group: NavGroup;
	/** Nav sub-heading inside the group. */
	section: string;
	/** Which area agent owns the page's route file(s) (web/AREAS.md). */
	area: Area;
	/** First milestone that builds it (spec 14 §2 "M"). */
	milestone: Milestone;
	/** The full milestone text when it is a range or has a caveat (`M1–M3`, `M1 (suspend/offboard M2)`). */
	milestoneNote?: string;
	/** Extra routes of the same page id (detail routes, J25's second page). */
	also?: PageRoute[];
	/** Values for route params other than `[job]`, so a parameterised page can appear in the nav. */
	navParams?: Record<string, string>;
	/** Hidden from the nav (detail pages reached from a list). */
	detail?: boolean;
	/** Which sidebar the page belongs to. */
	scope: NavScope;
	/** Who sees it in the nav (and may open it). */
	show: (s: NavSession) => boolean;
	icon: string;
}

/** conversations/routes.rs:534 STAFF_ROLES: who may work a conversation (the staff workspace's gate). */
const STAFF_ROLES: readonly Role[] = ['support_agent', 'support_manager', 'operator', 'job_designer'];

const SHOW: Record<NavGroup, (s: NavSession) => boolean> = {
	platform: (s) => s.superAdmin,
	tenant: (s) => s.member,
	job: (s) => s.member,
	live: (s) => s.member,
};

type Row = Omit<PageDef, 'show' | 'group' | 'icon' | 'scope'> & { icon?: string };

const GROUP_ICON: Record<NavGroup, string> = {
	platform: 'layers',
	tenant: 'tenant',
	job: 'jobs',
	live: 'console',
};

const GROUP_SCOPE: Record<NavGroup, NavScope> = {
	platform: 'studio',
	tenant: 'workspace',
	job: 'agent',
	live: 'agent',
};

function group(g: NavGroup, rows: Row[]): PageDef[] {
	return rows.map((r) => ({
		...r,
		group: g,
		scope: GROUP_SCOPE[g],
		show: SHOW[g],
		icon: r.icon ?? GROUP_ICON[g],
	}));
}

const J = '/admin/jobs/[job]';

const RAW_PAGES: PageDef[] = [
	...group('platform', [
		// Tenants & access
		{
			id: 'P1',
			title: 'Tenants',
			route: '/platform/tenants',
			section: 'Tenants & access',
			area: 'platform',
			milestone: 'M1',
			milestoneNote: 'M1 (suspend/offboard M2)',
			icon: 'tenant',
		},
		{
			id: 'P2',
			title: 'Tenant detail',
			route: '/platform/tenants/[id]',
			section: 'Tenants & access',
			area: 'platform',
			milestone: 'M1',
			milestoneNote: 'M1–M3',
			detail: true,
			icon: 'tenant',
		},
		{
			id: 'P3',
			title: 'Signups',
			route: '/platform/signups',
			section: 'Tenants & access',
			area: 'platform',
			milestone: 'M2',
			milestoneNote: 'M2 (dark)',
			icon: 'signup',
		},
		{
			id: 'P20',
			title: 'Licensing & keys',
			route: '/platform/licensing',
			section: 'Tenants & access',
			area: 'platform',
			milestone: 'M2',
			icon: 'key',
		},
		{
			id: 'P19',
			title: 'Platform audit',
			route: '/platform/audit',
			section: 'Tenants & access',
			area: 'platform',
			milestone: 'M2',
			icon: 'audit',
		},
		// Authoring
		{
			id: 'P21',
			title: 'Industries',
			route: '/platform/industries',
			section: 'Authoring',
			area: 'platform',
			milestone: 'M4',
			icon: 'journey',
		},
		{
			id: 'P4',
			title: 'Layers',
			route: '/platform/layers',
			section: 'Authoring',
			area: 'platform',
			milestone: 'M1',
			icon: 'layers',
			also: [{ route: '/platform/knowledge', label: 'Platform knowledge' }],
		},
		{
			id: 'P5',
			title: 'Compile preview',
			route: '/platform/compile',
			section: 'Authoring',
			area: 'platform',
			milestone: 'M1',
			icon: 'compile',
		},
		{
			id: 'P12',
			title: 'Business models',
			route: '/platform/business-models',
			section: 'Authoring',
			area: 'platform',
			milestone: 'M1',
			milestoneNote: 'M1 read / M4 axis split',
			icon: 'axes',
		},
		{
			id: 'P13',
			title: 'Dimensions',
			route: '/platform/dimensions',
			section: 'Authoring',
			area: 'platform',
			milestone: 'M1',
			icon: 'axes',
		},
		// Templates & model
		{
			id: 'P6',
			title: 'Templates',
			route: '/platform/templates',
			section: 'Templates & model',
			area: 'model',
			milestone: 'M4',
			icon: 'template',
		},
		{
			id: 'P7',
			title: 'Template tag',
			route: '/platform/templates/[layer]/[v]',
			section: 'Templates & model',
			area: 'model',
			milestone: 'M4',
			detail: true,
			icon: 'template',
		},
		{
			id: 'P8',
			title: 'Journey designer',
			route: '/platform/journeys/[industry]',
			section: 'Templates & model',
			area: 'journey',
			milestone: 'M4',
			navParams: { industry: 'retail' },
			icon: 'journey',
		},
		{
			id: 'P9',
			title: 'Objects',
			route: '/platform/objects',
			section: 'Templates & model',
			area: 'model',
			milestone: 'M4',
			icon: 'object',
		},
		{
			id: 'P10',
			title: 'Object detail',
			route: '/platform/objects/[object]',
			section: 'Templates & model',
			area: 'model',
			milestone: 'M4',
			detail: true,
			icon: 'object',
		},
		{
			id: 'P11',
			title: 'Operations catalogue',
			route: '/platform/operations',
			section: 'Templates & model',
			area: 'model',
			milestone: 'M4',
			icon: 'operation',
		},
		// Agents & connectors
		{
			id: 'P14',
			title: 'Gate library',
			route: '/platform/gates',
			section: 'Agents & connectors',
			area: 'ontology',
			milestone: 'M4',
			icon: 'gate',
		},
		{
			id: 'P15',
			title: 'Agent roles',
			route: '/platform/agent-roles',
			section: 'Agents & connectors',
			area: 'ontology',
			milestone: 'M4',
			icon: 'agents',
		},
		{
			id: 'P16',
			title: 'Connector templates',
			route: '/platform/connector-templates',
			section: 'Agents & connectors',
			area: 'connectors',
			milestone: 'M4',
			icon: 'backends',
		},
		{
			id: 'P17',
			title: 'Connector template detail',
			route: '/platform/connector-templates/[key]/[v]',
			section: 'Agents & connectors',
			area: 'connectors',
			milestone: 'M4',
			detail: true,
			icon: 'backends',
		},
		{
			id: 'P18',
			title: 'Discovery generator',
			route: '/platform/generator',
			section: 'Agents & connectors',
			area: 'journey',
			milestone: 'M4',
			also: [{ route: '/platform/generator/[run]', label: 'Generator run' }],
			icon: 'generator',
		},
	]),
	...group('tenant', [
		{
			id: 'T2',
			title: 'Workspace home',
			route: '/admin',
			section: 'Tenant',
			area: 'tenant',
			milestone: 'M1',
			milestoneNote: 'M1–M3',
			icon: 'dashboard',
		},
		{
			id: 'T14',
			title: 'Company',
			route: '/admin/company',
			section: 'Tenant',
			area: 'tenant',
			milestone: 'M1',
			icon: 'tenant',
		},
		{
			id: 'T15',
			title: 'Identity procedures',
			route: '/admin/identity',
			section: 'Tenant',
			area: 'tenant',
			milestone: 'M4',
			icon: 'key',
		},
		{
			id: 'T1',
			title: 'Jobs',
			route: '/admin/jobs',
			section: 'Tenant',
			area: 'tenant',
			milestone: 'M1',
			icon: 'jobs',
		},
		{
			id: 'T3',
			title: 'Users',
			route: '/admin/users',
			section: 'Tenant',
			area: 'tenant',
			milestone: 'M1',
			milestoneNote: 'M1 add by sub / M2 invite',
			icon: 'users',
		},
		{
			id: 'T5',
			title: 'Usage',
			route: '/admin/usage',
			section: 'Tenant',
			area: 'tenant',
			milestone: 'M1',
			icon: 'analytics',
		},
		{
			id: 'T4',
			title: 'Audit log',
			route: '/admin/audit',
			section: 'Tenant',
			area: 'tenant',
			milestone: 'M1',
			icon: 'audit',
		},
		{
			id: 'T7',
			title: 'Knowledge stores',
			route: '/admin/knowledge/stores',
			section: 'Knowledge',
			area: 'tenant',
			milestone: 'M1',
			milestoneNote: 'M1 / M3',
			also: [
				{ route: '/admin/knowledge/eval', label: 'Retrieval eval' },
				{ route: '/admin/knowledge/model-changes', label: 'Model changes' },
				{ route: '/admin/knowledge/platform', label: 'Platform knowledge' },
			],
			icon: 'stores',
		},
		{
			id: 'T8',
			title: 'Ingest jobs',
			route: '/admin/knowledge/jobs',
			section: 'Knowledge',
			area: 'tenant',
			milestone: 'M1',
			icon: 'jobs',
		},
		{
			id: 'T6',
			title: 'Providers',
			route: '/admin/providers',
			section: 'Integrations',
			area: 'tenant',
			milestone: 'M1',
			icon: 'models',
		},
		{
			id: 'T10',
			title: 'Systems',
			route: '/admin/connections/systems',
			section: 'Integrations',
			area: 'connectors',
			milestone: 'M4',
			icon: 'backends',
		},
		{
			id: 'T11',
			title: 'System detail',
			route: '/admin/connections/systems/[id]',
			section: 'Integrations',
			area: 'connectors',
			milestone: 'M4',
			detail: true,
			icon: 'backends',
		},
		{
			id: 'T12',
			title: 'Tenant connector templates',
			route: '/admin/connections/templates',
			section: 'Integrations',
			area: 'connectors',
			milestone: 'M4',
			icon: 'template',
		},
		{
			id: 'T13',
			title: 'Channels',
			route: '/admin/channels',
			section: 'Integrations',
			area: 'tenant',
			milestone: 'M2',
			milestoneNote: 'M2+',
			icon: 'chat',
			// B13: the tenant's customers across channels (party identity) and the identity policy.
			also: [
				// GET /api/parties is Requires<SupportAgent> (party/routes.rs:46)
				{ route: '/admin/parties', label: 'Customers', roles: ['support_agent'] },
				{ route: '/admin/parties/[id]', label: 'Customer' },
			],
		},
	]),
	...group('job', [
		// Overview
		{
			id: 'J1',
			title: 'Dashboard',
			route: J,
			section: 'Overview',
			area: 'runtime',
			milestone: 'M1',
			icon: 'dashboard',
		},
		{
			id: 'J2',
			title: 'Analytics',
			route: `${J}/analytics`,
			section: 'Overview',
			area: 'runtime',
			milestone: 'M1',
			icon: 'analytics',
		},
		{
			id: 'J3',
			title: 'Go-live checklist',
			route: `${J}/readiness`,
			section: 'Overview',
			area: 'release',
			milestone: 'M4',
			milestoneNote: 'M4 (M1 shows W5 slots only)',
			icon: 'check',
		},
		// Design
		{
			id: 'J4',
			title: 'Journey',
			route: `${J}/journey`,
			section: 'Design',
			area: 'journey',
			milestone: 'M4',
			icon: 'journey',
		},
		{
			id: 'J5',
			title: 'Activity detail',
			route: `${J}/journey/[activity]`,
			section: 'Design',
			area: 'journey',
			milestone: 'M4',
			detail: true,
			icon: 'journey',
		},
		{
			id: 'J6',
			title: 'Operations & routes',
			route: `${J}/operations`,
			section: 'Design',
			area: 'connectors',
			milestone: 'M4',
			icon: 'operation',
		},
		{
			id: 'J7',
			title: 'Gates',
			route: `${J}/gates`,
			section: 'Design',
			area: 'ontology',
			milestone: 'M4',
			icon: 'gate',
		},
		{
			id: 'J8',
			title: 'Toolsets',
			route: `${J}/toolsets`,
			section: 'Design',
			area: 'ontology',
			milestone: 'M4',
			icon: 'tools',
		},
		{
			id: 'J9',
			title: 'Business rules',
			route: `${J}/rules`,
			section: 'Design',
			area: 'release',
			milestone: 'M4',
			icon: 'rule',
		},
		{
			id: 'J10',
			title: 'Knowledge kinds',
			route: `${J}/knowledge-kinds`,
			section: 'Design',
			area: 'ontology',
			milestone: 'M3',
			milestoneNote: 'M3/M4',
			icon: 'knowledge',
		},
		{
			id: 'J11',
			title: 'Ontology',
			route: `${J}/ontology`,
			section: 'Design',
			area: 'ontology',
			milestone: 'M1',
			icon: 'ontology',
		},
		{
			id: 'J12',
			title: 'Procedures',
			route: `${J}/procedures`,
			section: 'Design',
			area: 'ontology',
			milestone: 'M1',
			also: [{ route: `${J}/procedures/[name]`, label: 'Procedure' }],
			icon: 'procedures',
		},
		{
			id: 'T9',
			title: 'Sources',
			route: `${J}/sources`,
			section: 'Design',
			area: 'tenant',
			milestone: 'M1',
			icon: 'knowledge',
		},
		// Release
		{
			id: 'J13',
			title: 'Base & refinements',
			route: `${J}/refinements`,
			section: 'Release',
			area: 'release',
			milestone: 'M1',
			icon: 'procedures',
		},
		{
			id: 'J14',
			title: 'Releases',
			route: `${J}/release`,
			section: 'Release',
			area: 'release',
			milestone: 'M1',
			icon: 'release',
		},
		{
			id: 'J15',
			title: 'Release detail',
			route: `${J}/release/[id]`,
			section: 'Release',
			area: 'release',
			milestone: 'M1',
			detail: true,
			icon: 'release',
		},
		{
			id: 'J16',
			title: 'Conflicts',
			route: `${J}/release`,
			hash: 'conflicts',
			section: 'Release',
			area: 'release',
			milestone: 'M1',
			icon: 'warn',
		},
		{
			id: 'J17',
			title: 'Validation grid',
			route: `${J}/validation`,
			section: 'Release',
			area: 'journey',
			milestone: 'M4',
			icon: 'check',
		},
		{
			id: 'J18',
			title: 'Discovery',
			route: `${J}/discovery`,
			section: 'Release',
			area: 'journey',
			milestone: 'M4',
			icon: 'search',
		},
		// Runtime
		{
			id: 'J19',
			title: 'Cases',
			route: `${J}/cases`,
			section: 'Runtime',
			area: 'runtime',
			milestone: 'M1',
			icon: 'cases',
		},
		{
			id: 'J20',
			title: 'Case detail',
			route: `${J}/cases/[id]`,
			section: 'Runtime',
			area: 'runtime',
			milestone: 'M1',
			milestoneNote: 'M1 (ops/gates M4)',
			detail: true,
			icon: 'cases',
		},
		{
			id: 'J21',
			title: 'Handover queue',
			route: `${J}/handover`,
			section: 'Runtime',
			area: 'journey',
			milestone: 'M4',
			milestoneNote: 'M4 (concept)',
			icon: 'users',
			// B12: the job's console conversations and its channel bindings (customer + worker side).
			also: [
				{ route: `${J}/conversations`, label: 'Conversations' },
				// GET /api/jobs/{job}/channel-bindings is JobRequires<IntegrationsManager> (conversations/bindings.rs:889)
				{ route: `${J}/channels`, label: 'Channel bindings', roles: ['integrations_manager'] },
			],
		},
		{
			id: 'J22',
			title: 'Operation calls',
			route: `${J}/operation-calls`,
			section: 'Runtime',
			area: 'runtime',
			milestone: 'M4',
			icon: 'operation',
		},
		{
			id: 'J23',
			title: 'Agents',
			route: `${J}/agents`,
			section: 'Runtime',
			area: 'runtime',
			milestone: 'M1',
			icon: 'agents',
		},
		{
			id: 'J24',
			title: 'Buses & topics',
			route: `${J}/buses`,
			section: 'Runtime',
			area: 'runtime',
			milestone: 'M1',
			icon: 'buses',
		},
		{
			id: 'J25',
			title: 'Tools',
			route: `${J}/tools`,
			section: 'Runtime',
			area: 'runtime',
			milestone: 'M4',
			also: [{ route: `${J}/backends`, label: 'Backend systems' }],
			icon: 'tools',
		},
		{
			id: 'J26',
			title: 'Tasks',
			route: `${J}/tasks`,
			section: 'Runtime',
			area: 'runtime',
			milestone: 'M4',
			icon: 'tools',
		},
	]),
	...group('live', [
		{
			id: 'L1',
			title: 'Console',
			route: '/console',
			section: 'Live',
			area: 'live',
			milestone: 'M1',
			icon: 'console',
		},
		{
			// Not in spec 14 §2: the staff workspace for handovers (SW plan T10) — tenant-wide inbox, one conversation
			// per URL (`/support/{job}/c/{conversationId}`, SW-8), context. Every member opens it (SW-7).
			id: 'L5',
			title: 'Support',
			route: '/support',
			section: 'Live',
			area: 'live',
			milestone: 'M1',
			icon: 'chat',
			// The inbox + event stream call require_worker: any STAFF_ROLES role (support/mod.rs:28, inbox.rs:388)
			roles: STAFF_ROLES,
			also: [{ route: '/support/[job]/c/[id]', label: 'Support conversation' }],
		},
		{
			id: 'L2',
			title: 'Test chat',
			route: '/chat',
			section: 'Live',
			area: 'live',
			milestone: 'M1',
			milestoneNote: 'M1 (edge M2)',
			icon: 'chat',
		},
		{
			// Not in spec 14 §2: the interaction API demo (text, streamed text and voice to any job of the tenant).
			id: 'L4',
			title: 'Talk to a job',
			route: '/interact',
			section: 'Live',
			area: 'live',
			milestone: 'M1',
			icon: 'chat',
		},
	]),
];

export const TASK_LABEL: Record<NavTask, string> = {
	operate: 'Operate',
	build: 'Build',
	test_release: 'Test & release',
	advanced: 'Advanced',
	more: 'More',
};

/** One sidebar entry. `route` defaults to the page's main route; the order of this list is the order in the sidebar. */
interface NavEntry {
	scope: Exclude<NavScope, 'studio'>;
	task?: NavTask;
	id: string;
	route?: string;
	label: string;
}

const e = (
	scope: NavEntry['scope'],
	task: NavTask | undefined,
	id: string,
	label: string,
	route?: string,
): NavEntry => ({ scope, task, id, label, route });

const NAV_ENTRIES: NavEntry[] = [
	e('agent', 'operate', 'J1', 'Home'),
	e('agent', 'operate', 'J19', 'Cases'),
	e('agent', 'operate', 'J21', 'Handovers'),
	e('agent', 'operate', 'J21', 'Conversations', `${J}/conversations`),
	e('agent', 'operate', 'J26', 'Tasks'),
	e('agent', 'operate', 'L1', 'Live console'),
	e('agent', 'operate', 'J2', 'Performance'),
	e('agent', 'build', 'J4', 'Journey'),
	e('agent', 'build', 'T9', 'Knowledge'),
	e('agent', 'build', 'J9', 'Rules & policies'),
	e('agent', 'build', 'J6', 'Actions & systems'),
	e('agent', 'test_release', 'L2', 'Test chat'),
	e('agent', 'test_release', 'J17', 'Test suite'),
	e('agent', 'test_release', 'J14', 'Releases'),
	e('agent', 'test_release', 'J3', 'Go-live checklist'),
	e('agent', 'advanced', 'J7', 'Gates'),
	e('agent', 'advanced', 'J11', 'Ontology'),
	e('agent', 'advanced', 'J12', 'Procedures'),
	e('agent', 'advanced', 'J10', 'Knowledge types'),
	e('agent', 'advanced', 'J8', 'Toolsets'),
	e('agent', 'advanced', 'J23', 'Specialists'),
	e('agent', 'advanced', 'J24', 'Messaging'),
	e('agent', 'advanced', 'J22', 'Action log'),
	e('agent', 'advanced', 'J18', 'Discovery'),
	e('agent', 'advanced', 'J25', 'Tools'),
	e('agent', 'advanced', 'J25', 'Backend systems', `${J}/backends`),
	e('agent', 'advanced', 'J21', 'Channel bindings', `${J}/channels`),
	e('workspace', undefined, 'T2', 'Home'),
	e('workspace', undefined, 'T14', 'Company'),
	e('workspace', undefined, 'T1', 'Agents'),
	e('workspace', undefined, 'L5', 'Support'),
	e('workspace', undefined, 'T7', 'Knowledge library'),
	e('workspace', undefined, 'T10', 'Connections'),
	e('workspace', undefined, 'T13', 'Channels'),
	e('workspace', undefined, 'T13', 'Customers', '/admin/parties'),
	e('workspace', undefined, 'T5', 'Usage'),
	e('workspace', undefined, 'T3', 'Team & access'),
	e('workspace', undefined, 'T4', 'Activity log'),
	e('workspace', 'more', 'T8', 'Ingest jobs'),
	e('workspace', 'more', 'T7', 'Retrieval eval', '/admin/knowledge/eval'),
	e('workspace', 'more', 'T7', 'Model changes', '/admin/knowledge/model-changes'),
	e('workspace', 'more', 'T7', 'Platform knowledge', '/admin/knowledge/platform'),
	e('workspace', 'more', 'T6', 'Providers'),
	e('workspace', 'more', 'T12', 'Connector templates'),
	e('dev', undefined, 'L4', 'Talk to a job'),
];

const entryRoute = (en: NavEntry, p: PageDef) => en.route ?? p.route;

/** The registry with the scoped-nav fields filled in from NAV_ENTRIES (a route without an entry is not in the nav). */
export const PAGES: PageDef[] = RAW_PAGES.map((p) => {
	const mine = NAV_ENTRIES.filter((en) => en.id === p.id);
	const main = mine.find((en) => entryRoute(en, p) === p.route);
	const mark = (r: PageRoute, isMain: boolean): PageRoute => {
		const en = mine.find((x) => entryRoute(x, p) === r.route);
		if (!en || (r.hash && !isMain)) return { ...r, inNav: false };
		const base = r.label ?? (isMain ? p.title : r.route);
		return { ...r, task: en.task, navLabel: en.label !== base ? en.label : undefined, inNav: true };
	};
	const scope: NavScope = p.group === 'platform' ? 'studio' : (mine[0]?.scope ?? GROUP_SCOPE[p.group]);
	const own = mark({ route: p.route, hash: p.hash, label: p.label, roles: p.roles }, true);
	const studio = scope === 'studio';
	return {
		...p,
		scope,
		task: own.task,
		navLabel: own.navLabel,
		inNav: studio ? !p.detail : !!main,
		also: p.also?.map((r) => (studio ? { ...r, inNav: !r.route.includes('[') } : mark(r, false))),
	};
});

/** Every page id of spec 14 §2, in table order (plus L4, the interaction API demo, and L5, the staff workspace). */
export const PAGE_IDS: string[] = [
	...Array.from({ length: 20 }, (_, i) => `P${i + 1}`),
	// P21 Industries (plan 2026-10-30-super-admin-platform-fixes T3): industries, departments and their knowledge.
	'P21',
	...Array.from({ length: 15 }, (_, i) => `T${i + 1}`),
	...Array.from({ length: 26 }, (_, i) => `J${i + 1}`),
	// L3 (demo data) left with the demo backends page.
	'L1',
	'L2',
	'L4',
	'L5',
];

export function pageById(id: string): PageDef | undefined {
	return PAGES.find((p) => p.id === id);
}

/** Every route of a page: its own plus `also`. */
export function routesOf(p: PageDef): PageRoute[] {
	return [{ route: p.route, hash: p.hash, label: p.label, roles: p.roles }, ...(p.also ?? [])];
}

/** The page a SvelteKit route id belongs to (the first page claiming it, so J14 wins `…/release` over J16). */
export function pageForRoute(routeId: string | null): PageDef | undefined {
	if (!routeId) return undefined;
	return PAGES.find((p) => routesOf(p).some((r) => r.route === routeId && !r.hash));
}

/**
 * The document title of a SvelteKit route: its own `label` when it is an `also` route of a page (`…/backends` →
 * "Backend systems", `/admin/knowledge/eval` → "Retrieval eval"), else the page title. Undefined off the registry.
 */
export function titleForRoute(routeId: string | null): string | undefined {
	const p = pageForRoute(routeId);
	if (!p) return undefined;
	const r = routesOf(p).find((x) => x.route === routeId && !x.hash);
	return r?.label ?? p.title;
}

/**
 * The URL of a page route: `[job]` → `job`, other params from `navParams`, `#hash` appended. Null when a param is
 * still unresolved (detail routes need a concrete id from a list).
 */
export function hrefOf(p: PageDef, r: PageRoute, job: string): string | null {
	let missing = false;
	const path = r.route.replace(/\[([^\]]+)\]/g, (_, name: string) => {
		const v = name === 'job' ? job : p.navParams?.[name];
		if (!v) missing = true;
		return encodeURIComponent(v ?? '');
	});
	if (missing) return null;
	return r.hash ? `${path}#${r.hash}` : path;
}

export interface NavItem {
	id: string;
	label: string;
	href: string;
	icon: string;
	milestone: Milestone;
	milestoneNote?: string;
}
export interface NavSection {
	section: string;
	items: NavItem[];
}
export interface NavGroupView {
	group: NavTask | 'workspace' | 'studio';
	label: string;
	/** True for `advanced` and `more`: the sidebar folds them behind a toggle. */
	collapsed: boolean;
	sections: NavSection[];
}

/** Which sidebar a SvelteKit route id belongs to. */
export function scopeForRoute(routeId: string | null): 'workspace' | 'agent' | 'studio' {
	if (!routeId) return 'workspace';
	if (routeId === '/platform' || routeId.startsWith('/platform/')) return 'studio';
	if (routeId === '/admin/jobs/[job]' || routeId.startsWith('/admin/jobs/[job]/')) return 'agent';
	if (['/chat', '/console'].some((p) => routeId === p || routeId.startsWith(`${p}/`))) return 'agent';
	return 'workspace';
}

function itemOf(p: PageDef, r: PageRoute, label: string, job: string): NavItem | null {
	if (/\[(?!job\])/.test(r.route) && !p.navParams) return null;
	const href = hrefOf(p, r, job);
	if (!href) return null;
	return { id: p.id, label, href, icon: p.icon, milestone: p.milestone, milestoneNote: p.milestoneNote };
}

/** A route with `roles` needs one of them in the EFFECTIVE set (so "View as…" narrows the nav like the inbox). */
function mayUse(s: NavSession, r: PageRoute): boolean {
	return !r.roles || !s.effectiveRoles || r.roles.some((x) => s.effectiveRoles!.includes(x));
}

const TASK_ORDER: NavTask[] = ['operate', 'build', 'test_release', 'advanced', 'more'];

/**
 * The sidebar for one scope: visible groups → sections → links. The agent scope is grouped by task, the workspace
 * scope is one list plus a collapsed More, the studio keeps the platform sections. An agent scope with no `job`
 * (a tenant with zero jobs) is empty. Detail routes and tab pages are left out.
 */
export function navFor(s: NavSession, job: string, scope: 'workspace' | 'agent' | 'studio'): NavGroupView[] {
	if (scope === 'studio') {
		const sections: NavSection[] = [];
		for (const p of PAGES) {
			if (p.scope !== 'studio' || !p.show(s)) continue;
			for (const r of routesOf(p)) {
				if (r.inNav === false) continue;
				const item = itemOf(p, r, r.label ?? p.title, job);
				if (!item) continue;
				let sec = sections.find((x) => x.section === p.section);
				if (!sec) sections.push((sec = { section: p.section, items: [] }));
				sec.items.push(item);
			}
		}
		return sections.length ? [{ group: 'studio', label: 'Studio', collapsed: false, sections }] : [];
	}
	if (scope === 'agent' && !job) return [];
	const byGroup = new Map<NavGroupView['group'], NavItem[]>();
	for (const en of NAV_ENTRIES) {
		if (en.scope !== scope) continue;
		const p = pageById(en.id);
		if (!p || !p.show(s)) continue;
		const r = routesOf(p).find((x) => x.route === entryRoute(en, p));
		if (!r || !mayUse(s, r)) continue;
		const item = itemOf(p, r, en.label, job);
		if (!item) continue;
		const g = en.task ?? 'workspace';
		byGroup.set(g, [...(byGroup.get(g) ?? []), item]);
	}
	const order: NavGroupView['group'][] = scope === 'agent' ? TASK_ORDER : ['workspace', 'more'];
	return order.flatMap((g) => {
		const items = byGroup.get(g);
		if (!items) return [];
		const label = g === 'workspace' ? 'Workspace' : TASK_LABEL[g as NavTask];
		return [
			{
				group: g,
				label,
				collapsed: g === 'advanced' || g === 'more',
				sections: [{ section: label, items }],
			},
		];
	});
}

/** The super-admin-only developer pages (Demo data, Talk to a job). */
export function devPages(s: NavSession): NavItem[] {
	if (!s.superAdmin) return [];
	return NAV_ENTRIES.filter((en) => en.scope === 'dev').flatMap((en) => {
		const p = pageById(en.id);
		const item = p && itemOf(p, p, en.label, '');
		return item ? [item] : [];
	});
}

export type Access = 'ok' | 'super_admin_only' | 'pick_tenant' | 'no_access';

/**
 * Whether `s` may open the route (07 §4.2 E3, T6): `/platform/*` is super_admin only (rendered without any API
 * call); tenant, job and live routes need an active tenant — a super_admin without one gets the picker inline.
 */
export function accessFor(routeId: string | null, s: NavSession): Access {
	if (!routeId || routeId === '/') return 'ok';
	if (routeId === '/platform' || routeId.startsWith('/platform/'))
		return s.superAdmin ? 'ok' : 'super_admin_only';
	const tenantScoped = ['/admin', '/console', '/chat', '/interact', '/support'].some(
		(p) => routeId === p || routeId.startsWith(`${p}/`),
	);
	if (!tenantScoped) return 'ok';
	if (s.member) return 'ok';
	return s.superAdmin ? 'pick_tenant' : 'no_access';
}

/** Where `/` sends the caller (07 §4.3): super_admin without a tenant → Platform › Tenants; others → `/admin`. */
export function landingFor(s: NavSession): string | null {
	if (s.member) return '/admin';
	if (s.superAdmin) return '/platform/tenants';
	return null;
}
