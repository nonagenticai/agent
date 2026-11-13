import type { Api } from './api/client';
import { ApiError, LicenceError, isLicenceCode, type LicenceCode } from './api/errors';
import type { JobSummary, Me, Role, TenantSummary } from './api/types/core';
import { session } from './auth/session.svelte';
import { toast } from './ui/toaster.svelte';
import { CHECKING_TEXT, classifyWriteError, forbiddenText, type WriteOutcome } from './write-outcome';
import { initAuth } from './auth/session';
import type { AuthConfig } from './auth/config';
import { appRequest, fetchMe, type Req } from './api/http-client';
import { createLiveClient } from './api/live-client';
import { httpSignups } from './api/http/b1';
import type { SignupsApi } from './api/types/signup-approvals';

/**
 * Why the app cannot show its pages for this caller: `not_provisioned` (no agent tenant, not a super_admin),
 * `tenant_suspended` (a member; a super_admin may enter), `tenant_archived` (terminal, everyone). `null` = ready.
 * A super_admin with no tenant is NOT blocked: `app.platformOnly` (07 §4.4, T6).
 */
export type Blocked = 'not_provisioned' | 'tenant_suspended' | 'tenant_archived' | null;

/**
 * Licence gate 2 refused the tenant (a `LicenceError` from any live call — a 402 `license_*` or a gate-marked 403 —
 * or `GET /api/license/status` saying `blocked`, whose code is always the gate's): the shell shows one blocking
 * licence state instead of the page; the topbar (who am I, sign out) stays.
 */
export interface LicenceBlock {
	code: LicenceCode;
	message: string;
}

/** No job until the tenant's jobs are read (a tenant with none acts in no job). */
export const DEFAULT_JOB = '';

/**
 * `job` is the job the job-scoped routes act in; `activeTenant` is a super_admin's chosen tenant (sent as
 * `X-Active-Tenant-Id`). On a job page (`/admin/jobs/{job}/…`) the job layout makes `job` the URL's job before the
 * page renders.
 */
export const app = $state<{
	api: Api | null;
	ready: boolean;
	job: string;
	jobs: JobSummary[];
	activeTenant: string | null;
	blocked: Blocked;
	/** Licence gate 2 refused the active tenant (`null` = not refused, or the gate is off). */
	licence: LicenceBlock | null;
	/** A super_admin with no active tenant: Platform group only; tenant/job routes render the picker inline. */
	platformOnly: boolean;
	/** Every live tenant, for a super_admin's tenant picker (empty for everyone else, or when the list failed). */
	tenants: TenantSummary[];
	/** A suspended tenant a super_admin chose to enter anyway. */
	enteredSuspended: string | null;
	/** Bumped on tenant switch / view-as / licence retry so every page remounts. */
	epoch: number;
}>({
	api: null,
	ready: false,
	job: DEFAULT_JOB,
	jobs: [],
	activeTenant: null,
	blocked: null,
	licence: null,
	platformOnly: false,
	tenants: [],
	enteredSuspended: null,
	epoch: 0,
});

export function api(): Api {
	if (!app.api) throw new Error('API used before sign-in finished');
	return app.api;
}

const JOB_KEY = 'agent-job';
/** A real super_admin's active tenant, remembered across reloads (§R.6). */
const TENANT_KEY = 'agent-active-tenant';
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

function readStored(store: 'session' | 'local', key: string): string | null {
	try {
		return (store === 'local' ? localStorage : sessionStorage).getItem(key);
	} catch {
		return null;
	}
}
function writeStored(store: 'session' | 'local', key: string, value: string | null) {
	try {
		const s = store === 'local' ? localStorage : sessionStorage;
		if (value === null) s.removeItem(key);
		else s.setItem(key, value);
	} catch {
		/* storage blocked: the choice lasts for this page only */
	}
}

/** A bare, non-nil uuid, as the API accepts in X-Active-Tenant-Id. */
export function isTenantId(s: string): boolean {
	return UUID.test(s) && !/^[0-]+$/.test(s);
}

/** The blocked shell for `me`; `entered` = the suspended tenant a super_admin chose to enter. */
export function blockedFor(me: Me, entered: string | null = null): Blocked {
	if (me.super_admin) {
		if (!me.tenant) return null;
		if (me.tenant.status === 'archived') return 'tenant_archived';
		if (me.tenant.status === 'suspended' && entered !== me.tenant.id) return 'tenant_suspended';
		return null;
	}
	if (!me.tenant) return 'not_provisioned';
	if (me.tenant.status === 'archived') return 'tenant_archived';
	if (me.tenant.status === 'suspended') return 'tenant_suspended';
	return null;
}

/** The stored job when the tenant has it, else the first job, else the default job. */
export function pickJob(jobs: JobSummary[], stored: string | null): string {
	if (stored && jobs.some((j) => j.key === stored)) return stored;
	return jobs[0]?.key ?? DEFAULT_JOB;
}

/** The job the job-scoped pages, the console and the demo act in from now on (kept for this browser tab). */
export function selectJob(key: string) {
	app.job = key;
	writeStored('session', JOB_KEY, key);
}

async function loadJobs(a: Api) {
	try {
		app.jobs = (await a.jobs()).items;
	} catch (e) {
		if (!(e instanceof ApiError)) throw e;
		app.jobs = [];
		return;
	}
	app.job = pickJob(app.jobs, readStored('session', JOB_KEY));
	writeStored('session', JOB_KEY, app.job);
}

/** A super_admin's tenant list (`GET /api/tenants`). Resolves an error message instead of throwing. */
export async function loadTenants(): Promise<string | null> {
	try {
		app.tenants = (await api().tenants()).items;
		return null;
	} catch (e) {
		return e instanceof Error ? e.message : String(e);
	}
}

// ---- the live client ----------------------------------------------------------------------------------------------

/** The authenticated request function (Keycloak bearer, `X-Active-Tenant-Id`); set by `bootKeycloak`. */
let liveReq: Req | null = null;

function makeClient(req: Req): Api {
	return createLiveClient(req, () => app.job);
}

// ---- licence gate 2 (spec 04 §4b) --------------------------------------------------------------------------------

/** A live call answered a licence refusal: show the licence state (the `HttpContext.onLicenceError` hook). */
export function licenceBlocked(e: LicenceError): void {
	app.licence = { code: e.licence, message: e.message };
}

/**
 * Asks `GET /api/license/status` (never 402) and sets the licence state from it: blocked with a licence code →
 * `app.licence`, anything else → cleared. A failed check leaves the state as it was. Resolves whether it is blocked.
 */
export async function checkLicence(): Promise<boolean> {
	try {
		const s = await api().licenseStatus();
		app.licence =
			s.state === 'blocked' && isLicenceCode(s.code) ? { code: s.code, message: s.message ?? '' } : null;
	} catch (e) {
		if (!(e instanceof ApiError)) throw e;
	}
	return app.licence !== null;
}

/** The licence state's Retry: re-checks, and when the tenant is admitted again re-reads the scope and remounts. */
export async function retryLicence(): Promise<boolean> {
	if (await checkLicence()) return false;
	await refreshScope();
	app.epoch++;
	return true;
}

/**
 * Re-reads `me`, the blocked state, the tenant list (SA) and the jobs. The licence state starts cleared: the jobs
 * read (a tenant route) sets it again through `licenceBlocked` while gate 2 refuses the tenant.
 */
async function refreshScope(): Promise<void> {
	const a = api();
	const me = await a.me();
	session.set({ sub: me.sub, name: me.display_name, actor: me.actor }, me);
	app.blocked = blockedFor(me, app.enteredSuspended);
	app.licence = null;
	app.platformOnly = me.super_admin && !me.tenant;
	if (me.super_admin) await loadTenants();
	else app.tenants = [];
	if (!app.blocked && !app.platformOnly) await loadJobs(a);
	else app.jobs = [];
}

/**
 * A super_admin acts in `tenantId` from now on; `null` returns to the platform-only shell. Resolves an error
 * message when the id is malformed or names no tenant (the previous tenant stays active), else null.
 */
export async function switchTenant(tenantId: string | null): Promise<string | null> {
	if (tenantId === null) {
		app.activeTenant = null;
		app.enteredSuspended = null;
		writeStored('local', TENANT_KEY, null);
		await refreshScope();
		return null;
	}
	const id = tenantId.trim().toLowerCase();
	if (!isTenantId(id)) return 'A tenant id is a uuid such as 0195f48b-d000-7000-8000-000000000099.';
	const prev = app.activeTenant;
	app.activeTenant = id;
	let me: Me;
	try {
		me = await api().me();
	} catch (e) {
		app.activeTenant = prev;
		return (e as Error).message;
	}
	if (me.active_tenant !== id) {
		app.activeTenant = prev;
		return 'No tenant with that id.';
	}
	app.enteredSuspended = null;
	writeStored('local', TENANT_KEY, id);
	await refreshScope();
	return null;
}

/**
 * D79 view-as: narrow the caller's own roles (server-side row; survives a reload). `[]` = view as an observer.
 * Pages remount so every control re-reads the effective roles. Resolves an error message, else null.
 */
export async function viewAs(roles: Role[], ttlSeconds?: number): Promise<string | null> {
	try {
		const me = await api().setViewAs({ roles, ttl_seconds: ttlSeconds });
		session.set(session.user, me);
	} catch (e) {
		return (e as Error).message;
	}
	app.epoch++;
	return null;
}

/** Ends view-as (idempotent; always allowed) and remounts the pages with the held roles. */
export async function exitViewAs(): Promise<void> {
	const me = await api().clearViewAs();
	session.set(session.user, me);
	app.epoch++;
}

/** A super_admin enters the suspended tenant they picked (members stay blocked). */
export async function enterSuspended(): Promise<void> {
	if (!session.superAdmin || !app.activeTenant) return;
	app.enteredSuspended = app.activeTenant;
	await refreshScope();
}

const UNCERTAIN_TEXT =
	'The write outcome is uncertain: no answer came back. Reload before trying again; the server may have accepted it.';

/**
 * Runs a write, reports it as a toast and says what happened: `ok`, `rejected` (the server refused it; nothing
 * changed) or `uncertain` (no answer came back; it may have been stored). With `reconcile: true` the caller
 * re-reads the resource itself after an uncertain outcome, so the toast says a check is running instead of
 * asking for a reload.
 */
export async function guardOutcome(
	action: () => Promise<unknown>,
	okMsg: string | (() => string),
	failPrefix = 'Not saved',
	opts: { reconcile?: boolean } = {},
): Promise<WriteOutcome> {
	try {
		await action();
		toast(typeof okMsg === 'function' ? okMsg() : okMsg);
		return { kind: 'ok' };
	} catch (e) {
		if (e instanceof ApiError && e.status === 403) {
			toast(
				`${forbiddenText(e)} ${session.viewAs ? 'You are viewing as fewer roles; exit view-as to do this.' : 'Ask a tenant admin for it.'}`,
				'err',
			);
			return { kind: 'rejected', error: e };
		}
		if (classifyWriteError(e) === 'uncertain') {
			toast(opts.reconcile ? CHECKING_TEXT : UNCERTAIN_TEXT, 'err');
			return { kind: 'uncertain', error: e };
		}
		toast(`${failPrefix}: ${(e as Error).message}`, 'err');
		return { kind: 'rejected', error: e };
	}
}

/**
 * Boolean form of `guardOutcome`: resolves true only after success, so callers continue (close the drawer, clear
 * the form) only on `ok`; after any error the edits are kept.
 */
export async function guard(
	action: () => Promise<unknown>,
	okMsg: string | (() => string),
	failPrefix = 'Not saved',
): Promise<boolean> {
	return (await guardOutcome(action, okMsg, failPrefix)).kind === 'ok';
}

// ---- real sign-in (Keycloak) ------------------------------------------------------------------------------------

export interface BootDeps {
	/** Keycloak sign-in (`./auth/session` in the app). */
	signIn: (cfg: AuthConfig) => Promise<{ name: string }>;
	fetchMe: () => Promise<Me>;
	/** The live `req` (tests inject a fake fetch); default: same-origin with the Keycloak bearer. */
	request?: Req;
}

/**
 * Real sign-in: Keycloak (login-required), then the real `GET /api/me`. Everyone runs on the live client — the
 * methods in `http/routes.ts` over HTTP — and sees their own tenant's data. A super_admin works as themself in the
 * platform shell or the tenant they last picked (identity comes from `GET /api/me`); a member of an active tenant
 * lands in their tenant; anyone else (no tenant, suspended or archived tenant) gets the blocked shell.
 */
export async function bootKeycloak(
	cfg: AuthConfig,
	deps: BootDeps = { signIn: initAuth, fetchMe: () => fetchMe() },
): Promise<void> {
	const who = await deps.signIn(cfg);
	const me = await deps.fetchMe();
	liveReq =
		deps.request ??
		appRequest(
			() => app.activeTenant,
			licenceBlocked,
			() => app.platformOnly,
		);
	app.api = makeClient(liveReq);
	app.activeTenant = null;
	app.enteredSuspended = null;
	if (me.super_admin) {
		await refreshScope();
		const stored = readStored('local', TENANT_KEY);
		if (stored && isTenantId(stored) && (await switchTenant(stored))) writeStored('local', TENANT_KEY, null);
		app.epoch++;
		app.ready = true;
		return;
	}
	const blocked = blockedFor(me);
	if (blocked || !me.tenant) {
		session.set({ sub: me.sub, name: me.display_name || who.name, actor: me.actor }, me);
		app.blocked = blocked ?? 'not_provisioned';
		app.platformOnly = false;
		app.tenants = [];
		app.jobs = [];
		app.ready = true;
		return;
	}
	await refreshScope();
	app.epoch++;
	app.ready = true;
}

/** The signup queue (`/platform/signups`): the real API. */
export function signupsApi(): SignupsApi {
	return httpSignups(
		liveReq ??
			appRequest(
				() => app.activeTenant,
				licenceBlocked,
				() => app.platformOnly,
			),
	);
}
