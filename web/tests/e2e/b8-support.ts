// Helpers for the B8 local acceptance run (b8-acceptance.spec.ts). Everything here talks to the LIVE stack that
// scripts/local/b8-acceptance.sh starts: the web dev server (which proxies /api to agent-api), the fake IdP and the
// fake LLM. Nothing is mocked in the browser; the only fake parts are the IdP (real RS256 tokens, real PKCE flow) and
// the model server.
import { execFileSync } from 'node:child_process';
import {
	expect,
	request as pwRequest,
	type APIRequestContext,
	type Page,
	type Response,
} from '@playwright/test';

export const WEB = process.env.B8_WEB_URL ?? 'http://127.0.0.1:3000';
export const IDP = process.env.B8_IDP_URL ?? 'http://127.0.0.1:9352';
export const LLM = process.env.B8_LLM_URL ?? 'http://127.0.0.1:9353';
export const DEMO_TENANT = process.env.B8_DEMO_TENANT ?? '';
export const OTHER_TENANT = process.env.B8_OTHER_TENANT ?? '';
export const PLATFORM_TENANT = process.env.B8_PLATFORM_TENANT ?? '';
/** Owner connection of the acceptance database (psql); only used for fixture rows the API has no route for. */
export const OWNER_URL = process.env.B8_OWNER_URL ?? '';

/** The seeded personas (scripts/local/b8-seed.sql): sub `b8-<persona>`; only `sa` is super_admin. */
export type Persona = 'sa' | 'ta' | 'tu' | 'jd' | 'rm' | 'op' | 'ta2';

export interface Called<T = any> {
	status: number;
	body: T;
}

async function bodyOf(r: Response | { json(): Promise<unknown>; text(): Promise<string> }): Promise<any> {
	// a 204 has no body to fetch (the browser protocol errors on it)
	const text = await r.text().catch(() => '');
	try {
		return text ? JSON.parse(text) : null;
	} catch {
		return text;
	}
}

/** Sign in as `persona` through the real PKCE flow (the IdP reads the persona from a cookie), landing on `path`. */
export async function signIn(page: Page, persona: Persona, path: string): Promise<void> {
	await page.context().addCookies([{ name: 'b8_persona', value: persona, url: IDP }]);
	const me = page.waitForResponse((r) => new URL(r.url()).pathname === '/api/me');
	await page.goto(path);
	const r = await me;
	expect(r.status(), `GET /api/me as ${persona}`).toBe(200);
}

let tokens: Record<string, string> = {};
export async function tokenOf(persona: Persona): Promise<string> {
	if (!tokens[persona]) {
		const ctx = await pwRequest.newContext();
		const r = await ctx.get(`${IDP}/__mint?persona=${persona}`);
		tokens[persona] = (await r.json()).access_token;
		await ctx.dispose();
	}
	return tokens[persona]!;
}

/** Direct API calls (through the web origin, so the same proxy as the browser) as `persona`. */
export class Api {
	private constructor(
		private ctx: APIRequestContext,
		private token: string,
		private tenant?: string,
	) {}
	static async as(persona: Persona, tenant?: string): Promise<Api> {
		return new Api(await pwRequest.newContext({ baseURL: WEB }), await tokenOf(persona), tenant);
	}
	async call(
		method: string,
		path: string,
		opts: { json?: unknown; multipart?: Record<string, any> } = {},
	): Promise<Called> {
		const headers: Record<string, string> = { authorization: `Bearer ${this.token}` };
		if (this.tenant) headers['x-active-tenant-id'] = this.tenant;
		const r = await this.ctx.fetch(path, {
			method,
			headers,
			...(opts.json !== undefined ? { data: opts.json } : {}),
			...(opts.multipart ? { multipart: opts.multipart } : {}),
		});
		return { status: r.status(), body: await bodyOf(r) };
	}
	get = (p: string) => this.call('GET', p);
	post = (p: string, json?: unknown) => this.call('POST', p, { json: json ?? {} });
	put = (p: string, json?: unknown) => this.call('PUT', p, { json: json ?? {} });
	patch = (p: string, json?: unknown) => this.call('PATCH', p, { json: json ?? {} });
	del = (p: string) => this.call('DELETE', p);
	file(p: string, name: string, buffer: Buffer, fields: Record<string, string> = {}, mime = 'text/csv') {
		return this.call('POST', p, { multipart: { ...fields, file: { name, mimeType: mime, buffer } } });
	}
	async dispose() {
		await this.ctx.dispose();
	}
}

/**
 * Run `action` (a UI interaction) and return the real `/api` response it caused: the first response to `method` on a
 * path matching `path`. The browser's own network call is what is asserted, not a re-request.
 */
export async function viaUi(
	page: Page,
	method: string,
	path: RegExp,
	action: () => Promise<unknown>,
): Promise<Called> {
	const wait = page.waitForResponse(
		(r) => r.request().method() === method && path.test(new URL(r.url()).pathname),
		{ timeout: 30_000 },
	);
	await action();
	const r = await wait;
	return { status: r.status(), body: await bodyOf(r) };
}

/** Passes when the call was a 2xx. The label and body are in the message so a failure explains itself. */
export function ok(c: Called, label: string): any {
	expect(c.status, `${label}: ${JSON.stringify(c.body).slice(0, 400)}`).toBeGreaterThanOrEqual(200);
	expect(c.status, `${label}: ${JSON.stringify(c.body).slice(0, 400)}`).toBeLessThan(300);
	return c.body;
}

export function errorCode(c: Called): string {
	return c.body?.error?.code ?? '';
}

/** Poll `read` until `done` holds (or fail with the last value). */
export async function until<T>(
	label: string,
	read: () => Promise<T>,
	done: (v: T) => boolean,
	timeoutMs = 120_000,
	everyMs = 1500,
): Promise<T> {
	const end = Date.now() + timeoutMs;
	let last: T;
	for (;;) {
		last = await read();
		if (done(last)) return last;
		if (Date.now() > end)
			throw new Error(
				`${label}: not reached in ${timeoutMs} ms; last: ${JSON.stringify(last).slice(0, 600)}`,
			);
		await new Promise((r) => setTimeout(r, everyMs));
	}
}

/** One psql statement as the database owner (fixtures only: the API has no route for them). */
export function sql(statement: string): string {
	if (!OWNER_URL) throw new Error('B8_OWNER_URL is not set (scripts/local/b8-acceptance.sh sets it)');
	return execFileSync('psql', [OWNER_URL, '-v', 'ON_ERROR_STOP=1', '-qtA', '-c', statement], {
		encoding: 'utf8',
	}).trim();
}

/** Counters of the fake LLM (proof that a real model call happened). */
export async function llmCalls(): Promise<Record<string, number>> {
	const ctx = await pwRequest.newContext();
	const r = await ctx.get(`${LLM}/__calls`);
	const j = await r.json();
	await ctx.dispose();
	return j;
}

/** The list rows of a route that answers either an array or `{items: []}`. */
export function rowsOf(body: any): any[] {
	return Array.isArray(body) ? body : (body?.items ?? []);
}
