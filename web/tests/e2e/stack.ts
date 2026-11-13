// The real local platform stack the e2e specs run on (scripts/local/web-e2e.sh starts it: scripts/local/platform-stack.sh
// on its own ports, then the platform-stack smoke, which makes every product object through the product's own routes:
// signup -> approved tenant + tenant_admin -> job from a template -> knowledge + procedure -> integration -> web chat
// binding + staff agent -> test -> publish -> a live customer chat handed to a person). Nothing in the browser is
// mocked: the web dev server proxies /api to agent-api, the IdP is the fake one (real RS256 tokens, real PKCE flow), the
// model is the fake LLM. The specs read the journey the smoke wrote (journey.json) and add their own data through the
// API as the identity a person would use.
import { createHmac, randomBytes, randomUUID } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { crc32, deflateSync } from 'node:zlib';
import { expect, request as pwRequest, type APIRequestContext, type Page } from '@playwright/test';

export { viaUi, ok, errorCode, until, rowsOf, type Called } from './b8-support';
import type { Called } from './b8-support';

const need = (k: string): string => {
	const v = process.env[k];
	if (!v) throw new Error(`${k} is not set: run the e2e through scripts/local/web-e2e.sh`);
	return v.replace(/\/$/, '');
};

export const WEB = () => need('PS_E2E_WEB');
export const IDP = () => need('PS_E2E_IDP');
export const EDGE = () => need('PS_E2E_EDGE');
export const KCADMIN = () => need('PS_E2E_KCADMIN');

/** What the smoke built (journey.json): the tenant, its published job and web chat key, and its two real users. */
export interface Journey {
	tenant: string;
	job: string;
	widget_key: string;
	/** The job's published web chat binding, and its identity key + secret (the tenant's site signs with them). */
	binding: string;
	id_kid: string;
	id_secret: string;
	issuer: string;
	/** Known customers of the tenant's own external system. */
	customers: Customer[];
	admin: { sub: string; email: string };
	agent: { sub: string; email: string };
}

export interface Customer {
	customer_id: string;
	email: string;
	name: string;
}

let journey: Journey | null = null;
export function J(): Journey {
	journey ??= JSON.parse(readFileSync(need('PS_E2E_JOURNEY'), 'utf8')) as Journey;
	return journey!;
}

/**
 * Who signs in: `sa` is the platform admin (a persona of the fake IdP); `admin` (the tenant's tenant_admin, made by the
 * signup approval) and `agent` (the support agent the tenant admin invited onto the job's staff team) are the product's
 * own identities, so they sign in with their sub and email.
 */
export type Who = 'sa' | 'admin' | 'agent';

function identity(who: Who): { persona: string; sub?: string; email?: string } {
	if (who === 'sa') return { persona: 'sa' };
	const u = who === 'admin' ? J().admin : J().agent;
	return { persona: who === 'admin' ? 'ta' : 'ag', sub: u.sub, email: u.email };
}

/** Sign in as `who` through the real PKCE flow, landing on `path` (the IdP reads the identity from its cookies). */
export async function signIn(page: Page, who: Who, path: string): Promise<void> {
	const id = identity(who);
	const url = IDP();
	const cookies = [{ name: 'b8_persona', value: id.persona, url }];
	if (id.sub) cookies.push({ name: 'b8_sub', value: encodeURIComponent(id.sub), url });
	if (id.email) cookies.push({ name: 'b8_email', value: encodeURIComponent(id.email), url });
	await page.context().addCookies(cookies);
	const me = page.waitForResponse((r) => new URL(r.url()).pathname === '/api/me', { timeout: 60_000 });
	await page.goto(path);
	const r = await me;
	expect(r.status(), `GET /api/me as ${who}`).toBe(200);
}

const tokens: Partial<Record<Who, string>> = {};
/** An access token for `who` from the fake IdP (API-level setup, as the person would act). */
export async function tokenOf(who: Who): Promise<string> {
	if (!tokens[who]) {
		const id = identity(who);
		const q = new URLSearchParams({ persona: id.persona });
		if (id.sub) q.set('sub', id.sub);
		if (id.email) q.set('email', id.email);
		const ctx = await pwRequest.newContext();
		const r = await ctx.get(`${IDP()}/__mint?${q}`);
		expect(r.status(), `mint ${who}`).toBe(200);
		tokens[who] = (await r.json()).access_token;
		await ctx.dispose();
	}
	return tokens[who]!;
}

/** Direct API calls through the web origin (the browser's own proxy) as `who`. */
export class Api {
	private constructor(
		private ctx: APIRequestContext,
		private token: string,
	) {}
	static async as(who: Who): Promise<Api> {
		return new Api(await pwRequest.newContext({ baseURL: WEB() }), await tokenOf(who));
	}
	static async fromToken(token: string): Promise<Api> {
		return new Api(await pwRequest.newContext({ baseURL: WEB() }), token);
	}
	async call(method: string, path: string, json?: unknown): Promise<Called> {
		const r = await this.ctx.fetch(path, {
			method,
			headers: { authorization: `Bearer ${this.token}` },
			...(json !== undefined ? { data: json } : {}),
		});
		const text = await r.text().catch(() => '');
		let body: unknown = text;
		try {
			body = text ? JSON.parse(text) : null;
		} catch {
			/* not json */
		}
		return { status: r.status(), body };
	}
	get = (p: string) => this.call('GET', p);
	post = (p: string, json?: unknown) => this.call('POST', p, json ?? {});
	put = (p: string, json?: unknown) => this.call('PUT', p, json ?? {});
	del = (p: string) => this.call('DELETE', p);
	async dispose() {
		await this.ctx.dispose();
	}
}

// ---- the customer side: the tenant's logged-in site opening its web chat (the edge, same-origin like the widget) ----

async function edge(method: string, path: string, body?: unknown, headers: Record<string, string> = {}) {
	const origin = new URL(EDGE()).origin.replace('127.0.0.1', 'localhost');
	const r = await fetch(`${EDGE()}${path}`, {
		method,
		headers: { 'content-type': 'application/json', origin, ...headers },
		body: body === undefined ? undefined : JSON.stringify(body),
	});
	const text = await r.text();
	let json: any = null;
	try {
		json = text ? JSON.parse(text) : null;
	} catch {
		/* not json */
	}
	return { status: r.status, json, text };
}

let ipSeq = (Date.now() / 1000) & 0xffff;
/** A fresh customer address in 198.18.0.0/15 (reserved for benchmarking: never a real customer's). */
function nextIp(): string {
	ipSeq = (ipSeq + 1) & 0x1ffff;
	return `198.${18 + (ipSeq >> 16)}.${(ipSeq >> 8) & 255}.${ipSeq & 255}`;
}

/** The HS256 assertion the tenant's site signs for a signed-in customer (conversations/webchat_identity.rs). */
export function assertionFor(c: Customer): string {
	const j = J();
	const b64 = (o: unknown) => Buffer.from(JSON.stringify(o)).toString('base64url');
	const now = Math.floor(Date.now() / 1000);
	const head = b64({ alg: 'HS256', typ: 'JWT', kid: j.id_kid });
	const claims = b64({
		iss: j.issuer,
		aud: j.binding,
		sub: String(c.customer_id),
		email: c.email,
		name: c.name,
		iat: now,
		exp: now + 120,
		jti: randomUUID().replace(/-/g, ''),
	});
	const sig = createHmac('sha256', j.id_secret).update(`${head}.${claims}`).digest('base64url');
	return `${head}.${claims}.${sig}`;
}

/** A known customer chatting on the job's published web chat, as the widget does. */
export interface CustomerChat {
	/** agent-api's conversation id: the one the staff workspace, console and case pages use. */
	id: string;
	/** The web chat's own conversation id (the widget's). */
	conversation_id: string;
	/** The first answer's text. */
	first: string;
	/** Send another message; the agent's (or nobody's, when a person holds it) answer text. */
	say(text: string): Promise<{ status: number; text: string }>;
	/** Upload `file` and send it in a message (the widget's paperclip); the message's status. */
	sendFile(
		file: { name: string; mimeType: string; buffer: Buffer },
		text?: string,
	): Promise<{ status: number; body: string }>;
	/** What the customer's own chat window holds (the edge's history): every line's role and text. */
	history(): Promise<{ role: string; text: string }[]>;
}

/**
 * `cust` signs in on the tenant's site and writes `opening` in its web chat (a NEW conversation per call); `null` is
 * a visitor who is not signed in.
 */
export async function customerChat(cust: Customer | null, opening: string): Promise<CustomerChat> {
	const key = J().widget_key;
	// every customer is on their own address, as Cloudflare tells the edge in production (cf-connecting-ip); the edge
	// and agent-api limit sessions and messages per address, which one test machine would otherwise exhaust
	const ip = { 'cf-connecting-ip': nextIp() };
	const s = await edge(
		'POST',
		'/api/webchat/sessions',
		{ widget_key: key, ...(cust ? { customer_assertion: assertionFor(cust) } : {}) },
		ip,
	);
	expect(s.status, `web chat session for ${cust?.name ?? 'a visitor'}: ${s.text.slice(0, 300)}`).toBe(201);
	const bearer = { authorization: `Bearer ${s.json.session_token}`, ...ip };
	const visitor = randomBytes(12).toString('hex');
	let conv: { conversation_id?: string; token?: string } = {};
	const send = (text: string, extra: Record<string, unknown> = {}) =>
		edge(
			'POST',
			'/api/webchat/messages',
			{ widget_key: key, visitor_id: visitor, messages: [{ role: 'user', text }], ...conv, ...extra },
			bearer,
		);
	const f = await send(opening);
	expect(f.status, `first web chat message: ${f.text.slice(0, 300)}`).toBe(200);
	expect(f.json?.conversation_id, `no conversation in ${f.text.slice(0, 300)}`).toBeTruthy();
	// later messages go to the same conversation, with the conversation token the first answer handed out
	const token = String(f.json.token ?? '');
	conv = { conversation_id: f.json.conversation_id, token };
	// the conversation token is `<agent-api conversation id>~<binding id>.<mac>` (channels/webchat.rs issue_token)
	const id = token.slice(0, token.lastIndexOf('.')).split('~')[0]!;
	expect(id, `no agent-api conversation in the token`).toMatch(/^conv-/);
	const origin = new URL(EDGE()).origin.replace('127.0.0.1', 'localhost');
	return {
		id,
		conversation_id: f.json.conversation_id,
		first: String(f.json.text ?? ''),
		say: async (text) => {
			const r = await send(text);
			return { status: r.status, text: String(r.json?.text ?? '') };
		},
		// a file-only message carries the widget's stand-in text (webchat_widget.js: "(photo)" / "(file)")
		sendFile: async (file, text = file.mimeType.startsWith('image/') ? '(photo)' : '(file)') => {
			const form = new FormData();
			form.set('conversation', f.json.conversation_id);
			form.set('token', token);
			form.set('file', new Blob([new Uint8Array(file.buffer)], { type: file.mimeType }), file.name);
			const up = await fetch(`${EDGE()}/api/webchat/attachments`, {
				method: 'POST',
				headers: { origin, ...bearer },
				body: form,
			});
			const upText = await up.text();
			expect(up.status, `upload ${file.name}: ${upText.slice(0, 300)}`).toBe(201);
			const r = await send(text, { attachments: [JSON.parse(upText).id] });
			return { status: r.status, body: r.text };
		},
		history: async () => {
			const h = await edge(
				'GET',
				`/api/webchat/conversations/${f.json.conversation_id}/messages?token=${encodeURIComponent(token)}`,
				undefined,
				bearer,
			);
			expect(h.status, `customer history: ${h.text.slice(0, 300)}`).toBe(200);
			const rows: { role?: string; text?: string }[] = Array.isArray(h.json)
				? h.json
				: (h.json?.messages ?? h.json?.items ?? []);
			return rows.map((m) => ({ role: String(m.role ?? ''), text: String(m.text ?? '') }));
		},
	};
}

/** A conversation handed to the staff queue: the customer asks for a person (as a real one would). */
export async function queuedConversation(
	cust: Customer | null,
	opening = 'Where is my parcel?',
): Promise<CustomerChat> {
	const chat = await customerChat(cust, opening);
	const r = await chat.say('I would like to speak to a person, please.');
	expect(r.status, 'asking for a person').toBe(200);
	return chat;
}

// ---- staff: the tenant admin invites a colleague onto the job's staff team (as smoke step 4 does) ----

/** Invite one more support agent onto the job's staff team; sign them in with `signInAs(page, colleague)`. */
export async function inviteColleague(name: string): Promise<{ sub: string; email: string; name: string }> {
	const email = `ps-colleague-${Date.now().toString(36)}@platform-stack.example.test`;
	const admin = await Api.as('admin');
	const inv = await admin.post('/api/tenant/invites', {
		email,
		roles: ['support_agent'],
		display_name: name,
	});
	expect([200, 201, 202], `invite ${email}: ${JSON.stringify(inv.body).slice(0, 300)}`).toContain(inv.status);
	const st = await (await fetch(`${KCADMIN()}/__state`)).json();
	const kc = (st.users ?? []).find((u: { email: string }) => u.email.toLowerCase() === email);
	expect(kc?.id, 'the invite made an identity').toBeTruthy();
	const colleague = { sub: kc.id as string, email, name };
	// the first sign-in seats the membership
	const me = await (await asIdentity('ag', colleague)).get('/api/me');
	expect(me.status, 'GET /api/me as the colleague').toBe(200);
	const staff = await admin.get(`/api/jobs/${J().job}/staff`);
	const members: string[] = (staff.body?.members ?? []).map((m: { sub?: string } | string) =>
		typeof m === 'string' ? m : (m.sub ?? ''),
	);
	const put = await admin.put(`/api/jobs/${J().job}/staff`, {
		members: [...new Set([...members.filter(Boolean), J().agent.sub, colleague.sub])],
		routing: { mode: 'off' },
	});
	expect(put.status, `job staff: ${JSON.stringify(put.body).slice(0, 300)}`).toBe(200);
	await admin.dispose();
	return colleague;
}

/** An Api for an ad-hoc product identity (a colleague the spec invited). */
export async function asIdentity(persona: string, id: { sub: string; email: string }): Promise<Api> {
	const q = new URLSearchParams({ persona, sub: id.sub, email: id.email });
	const ctx = await pwRequest.newContext();
	const r = await ctx.get(`${IDP()}/__mint?${q}`);
	const token = (await r.json()).access_token as string;
	await ctx.dispose();
	return Api.fromToken(token);
}

/** Sign an ad-hoc product identity in (persona `ag`: a staff member). */
export async function signInAs(page: Page, id: { sub: string; email: string }, path: string): Promise<void> {
	const url = IDP();
	await page.context().addCookies([
		{ name: 'b8_persona', value: 'ag', url },
		{ name: 'b8_sub', value: encodeURIComponent(id.sub), url },
		{ name: 'b8_email', value: encodeURIComponent(id.email), url },
	]);
	const me = page.waitForResponse((r) => new URL(r.url()).pathname === '/api/me', { timeout: 60_000 });
	await page.goto(path);
	expect((await me).status()).toBe(200);
}

/** A real `w`×`h` RGB PNG of one colour (the server re-encodes uploads, so the bytes must be a valid image). */
export function pngOf(w = 64, h = 48, rgb: [number, number, number] = [31, 111, 92]): Buffer {
	const chunk = (type: string, data: Buffer) => {
		const len = Buffer.alloc(4);
		len.writeUInt32BE(data.length);
		const td = Buffer.concat([Buffer.from(type, 'ascii'), data]);
		const crc = Buffer.alloc(4);
		crc.writeUInt32BE(crc32(td) >>> 0);
		return Buffer.concat([len, td, crc]);
	};
	const ihdr = Buffer.alloc(13);
	ihdr.writeUInt32BE(w, 0);
	ihdr.writeUInt32BE(h, 4);
	ihdr.set([8, 2, 0, 0, 0], 8); // 8-bit RGB, no interlace
	const row = Buffer.concat([Buffer.from([0]), Buffer.from(Array.from({ length: w }, () => rgb).flat())]);
	const raw = Buffer.concat(Array.from({ length: h }, () => row));
	return Buffer.concat([
		Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
		chunk('IHDR', ihdr),
		chunk('IDAT', deflateSync(raw)),
		chunk('IEND', Buffer.alloc(0)),
	]);
}

// ---- view-as (D79): the avatar menu's "View as…", shared by the specs that narrow a session's roles ----

/** Open the "View as…" panel: in the user menu on desktop (under Developer for a super admin), the nav drawer on a phone. */
export async function openViewAs(page: Page): Promise<void> {
	await expect(page.locator('main h1').first()).toBeVisible();
	const menu = page.getByTestId('user-menu');
	if (await menu.isVisible()) await menu.click();
	else await page.getByRole('button', { name: 'Open navigation' }).click();
	const open = page.getByTestId('view-as-open');
	const dev = page.getByTestId('developer-toggle');
	if (!(await open.isVisible()) && (await dev.count())) await dev.click();
	await open.click();
	await expect(page.getByTestId('view-as-panel')).toBeVisible();
}

/** View as only `role` (every other held role unticked) and apply. */
export async function viewAsOnly(page: Page, role: string): Promise<void> {
	await openViewAs(page);
	const panel = page.getByTestId('view-as-panel');
	for (const box of await panel.getByRole('checkbox').all()) {
		const name = (await box.getAttribute('value')) ?? '';
		if (name !== role && (await box.isChecked())) await box.uncheck();
	}
	await panel.getByTestId('view-as-apply').click();
	await expect(panel).toBeHidden();
}

/** Clear any view-as row the people of the journey hold (the row is the API's and outlives a failed test). */
export async function clearViewAs(): Promise<void> {
	for (const who of ['admin', 'agent', 'sa'] as const) {
		const a = await Api.as(who);
		await a.del('/api/me/view-as');
		await a.dispose();
	}
}

/** Every `/api` call the page makes, as "VERB /path?query". */
export function recordApi(page: Page): string[] {
	const calls: string[] = [];
	page.on('request', (r) => {
		const u = new URL(r.url());
		if (u.pathname.startsWith('/api/')) calls.push(`${r.method()} ${u.pathname}${u.search}`);
	});
	return calls;
}

/** Console errors and uncaught page errors. */
export function watchConsole(page: Page): string[] {
	const errors: string[] = [];
	page.on('console', (m) => {
		if (m.type() === 'error') errors.push(m.text());
	});
	page.on('pageerror', (e) => errors.push(String(e)));
	return errors;
}
