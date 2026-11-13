// The web's one HTTP door (B1 T17): same-origin `/api` with the Keycloak bearer. `makeRequest(ctx)` builds the `req`
// every per-plan module (`http/b1.ts`, later `b2.ts` … `b8.ts`) calls; it refuses, before sending, any verb + path
// that matches no template in `ROUTES` (`http/routes.ts`), so a page can only reach a route a live method names.
// One of two files allowed to touch the network (tests/unit/guard.test.ts).
import { getToken } from '$lib/auth/session';
import { ApiError, LicenceError, isLicenceRefusal } from './errors';
import { ROUTES } from './http/routes';
import { createSseParser, type SseEvent } from './sse';
import type { Me } from './types/core';

export type Verb = 'GET' | 'POST' | 'PUT' | 'PATCH' | 'DELETE';
export type Surface = 'admin' | 'console' | 'chat' | 'support';

export interface HttpContext {
	/** The super_admin's chosen tenant, sent as `X-Active-Tenant-Id` (null = none sent). */
	activeTenant(): string | null;
	/** Which surface the caller is on (`X-Agent-Surface`; the API records it on the member's session). */
	surface(): Surface;
	/** A bearer token; `force` refreshes it first. Default: the Keycloak adapter (`$lib/auth/session`). */
	token?: (force: boolean) => Promise<string>;
	/** Injected in tests; the browser's `fetch` otherwise. */
	fetchImpl?: typeof fetch;
	/** Called with every licence refusal (a `LicenceError`) before it is thrown: the shell's licence state. */
	onLicenceError?: (e: LicenceError) => void;
	/**
	 * True while a super_admin is in the platform view (no tenant): a tenant-only call is refused here, before it
	 * is sent, with the API's own `409 active_tenant_required` — never a network 409 from a page left mounted.
	 */
	platformOnly?: () => boolean;
}

/** Routes that only answer inside a tenant (`tenancy.rs` `ActiveTenant`): never sent from the platform view. */
export const TENANT_ONLY_PREFIXES = ['/api/jobs', '/api/inbox', '/api/tenant/'] as const;

/** True when `path` (query ignored) is a tenant-only route. */
export function isTenantOnly(path: string): boolean {
	const p = path.split('?')[0]!;
	return TENANT_ONLY_PREFIXES.some((pre) => p === pre || p.startsWith(pre.endsWith('/') ? pre : `${pre}/`));
}

export type Query = Record<string, string | number | boolean | null | undefined>;
/**
 * `headers`: extra request headers a method sends (B7 `systemCatalogue`: `X-Job`); never overrides the standard ones.
 * `response: 'text'`: the 2xx body is a file, not JSON — the call resolves a `TextFile` (B7 T13a `exportOperationSpec`);
 * an error answer is still the JSON envelope → `ApiError`.
 */
export type ReqOpts = {
	query?: Query;
	body?: unknown;
	headers?: Record<string, string>;
	response?: 'json' | 'text' | 'blob';
};

/** A `response: 'text'` answer: the body, its `Content-Type`, and the `Content-Disposition` file name ('' if none). */
export interface TextFile {
	body: string;
	content_type: string;
	filename: string;
}

/**
 * A streamed call: the body is read as server-sent events (`text/event-stream`) and each event is handed to
 * `onEvent` as it arrives. Resolves when the stream ends; a non-2xx answer throws the same `ApiError` as `req`.
 * `headers`: extra request headers (a reconnecting feed's `Last-Event-ID`); `onOpen` runs once the server has
 * answered 2xx with a body, before the first event (a feed is `live` from then on).
 */
export type StreamReq = (
	verb: Verb,
	path: string,
	opts: {
		body?: unknown;
		onEvent: (e: SseEvent) => void;
		signal?: AbortSignal;
		headers?: Record<string, string>;
		onOpen?: () => void;
	},
) => Promise<void>;
/** A multipart call (`FormData` body, the browser sets the boundary); the answer is JSON, as for `req`. */
export type FormReq = <T>(
	verb: Verb,
	path: string,
	form: FormData,
	opts?: { signal?: AbortSignal },
) => Promise<T>;

export interface Req {
	<T>(verb: Verb, path: string, opts?: ReqOpts): Promise<T>;
	/** The same door, the body read as SSE (the interaction API's `stream: true`). */
	stream: StreamReq;
	/** The same door with a multipart body (voice turns). */
	form: FormReq;
}

/** `GET /api/tenants/{id}` → `^GET /api/tenants/[^/]+$` (a `{param}` is one non-empty path segment). */
function templateRe(route: string): RegExp {
	const esc = route.replace(/[.*+?^$()|[\]\\]/g, '\\$&').replace(/\\?\{[^}/]+\\?\}/g, '[^/]+');
	return new RegExp(`^${esc}$`);
}
const ROUTE_RES: RegExp[] = Object.values(ROUTES as Record<string, readonly string[]>)
	.flat()
	.map((r) => templateRe(r.split('?')[0]!));

/** True when `VERB /path` (no query) matches a template in `ROUTES`. */
export function isRouted(verb: Verb, path: string): boolean {
	const line = `${verb} ${path.split('?')[0]}`;
	return ROUTE_RES.some((re) => re.test(line));
}

/** A path segment, percent-encoded (`a b` → `a%20b`). */
export const seg = (s: string) => encodeURIComponent(s);

function queryString(q: Query | undefined): string {
	if (!q) return '';
	const p = new URLSearchParams();
	for (const [k, v] of Object.entries(q)) if (v !== null && v !== undefined && v !== '') p.set(k, String(v));
	const s = p.toString();
	return s ? `?${s}` : '';
}

/**
 * The `ApiError` for a non-2xx answer whose body parsed to `value` (or did not parse: `undefined`). A licence gate
 * refusal (`isLicenceRefusal`: a 402 licence code, or a gate-marked 403) is a `LicenceError` (still an `ApiError`).
 */
export function errorOf(status: number, value: unknown): ApiError {
	const err = (value as { error?: Record<string, unknown> } | undefined)?.error;
	if (err && typeof err.code === 'string') {
		const { code, message, ...details } = err;
		const text = typeof message === 'string' ? message : `Request failed (${status})`;
		if (isLicenceRefusal(status, code, details)) return new LicenceError(code, text, details, status);
		return new ApiError(status, code, text, details);
	}
	return new ApiError(status, 'request_failed', `Request failed (${status})`);
}

/** A JSON answer: 204 → `undefined`, an error body → `ApiError`, a 2xx body that is not JSON → `bad_response`. */
async function readJson<T>(res: Response): Promise<T> {
	const text = res.status === 204 ? '' : await res.text().catch(() => '');
	let value: unknown = undefined;
	let parsed = !text;
	if (text) {
		try {
			value = JSON.parse(text);
			parsed = true;
		} catch {
			value = undefined;
		}
	}
	if (!res.ok) throw errorOf(res.status, value);
	if (!parsed)
		throw new ApiError(res.status, 'bad_response', 'The server sent an answer this app cannot read.');
	return value as T;
}

/** `attachment; filename="a b.csv"` / `filename*=UTF-8''a%20b.csv` → `a b.csv` ('' when absent). */
export function filenameOf(disposition: string | null): string {
	if (!disposition) return '';
	const ext = /filename\*\s*=\s*(?:UTF-8|utf-8)''([^;]+)/.exec(disposition);
	if (ext) {
		try {
			return decodeURIComponent(ext[1]!.trim());
		} catch {
			/* fall through to the plain form */
		}
	}
	const plain = /filename\s*=\s*(?:"([^"]*)"|([^;\s]+))/.exec(disposition);
	return plain ? (plain[1] ?? plain[2] ?? '') : '';
}

/** A file answer: an error body → `ApiError` (as `readJson`), a 2xx → `{body, content_type, filename}`. */
async function readText(res: Response): Promise<TextFile> {
	const text = await res.text().catch(() => '');
	if (!res.ok) {
		let value: unknown = undefined;
		try {
			value = text ? JSON.parse(text) : undefined;
		} catch {
			value = undefined;
		}
		throw errorOf(res.status, value);
	}
	return {
		body: text,
		content_type: res.headers.get('Content-Type') ?? '',
		filename: filenameOf(res.headers.get('Content-Disposition')),
	};
}

/** A binary answer (an attachment's bytes): an error body → `ApiError`, a 2xx → the `Blob`. */
async function readBlob(res: Response): Promise<Blob> {
	if (!res.ok) {
		const text = await res.text().catch(() => '');
		let value: unknown = undefined;
		try {
			value = text ? JSON.parse(text) : undefined;
		} catch {
			value = undefined;
		}
		throw errorOf(res.status, value);
	}
	return res.blob();
}

interface Wire {
	body?: BodyInit;
	contentType?: string;
	accept: string;
	signal?: AbortSignal;
	/** Extra headers (`ReqOpts.headers`); the standard ones below always win. */
	headers?: Record<string, string>;
}

/**
 * One call: bearer + `X-Active-Tenant-Id` + `X-Agent-Surface`, JSON in/out, one retry with a forced token refresh on
 * 401, 202/204 are successes (a 204 resolves `undefined`). An error body `{"error":{code,message,…}}` becomes
 * `ApiError(status, code, message, details = the other fields)`; no answer at all becomes `ApiError(0,
 * 'network_error')` and a 2xx body that is not JSON `ApiError(status, 'bad_response')` (both "uncertain" writes).
 * `req.stream` and `req.form` share the headers, the route check and the 401 retry. A licence refusal (a
 * `LicenceError`) is also handed to `ctx.onLicenceError` before it is thrown.
 */
export function makeRequest(ctx: HttpContext): Req {
	const doFetch = ctx.fetchImpl ?? ((input: RequestInfo | URL, init?: RequestInit) => fetch(input, init));
	const token = ctx.token ?? getToken;
	const reported = async <T>(run: () => Promise<T>): Promise<T> => {
		try {
			return await run();
		} catch (e) {
			if (e instanceof LicenceError) ctx.onLicenceError?.(e);
			throw e;
		}
	};
	const routed = (verb: Verb, path: string) => {
		if (!isRouted(verb, path)) throw new Error(`http-client may not call ${verb} ${path}`);
		if (ctx.platformOnly?.() && isTenantOnly(path))
			throw new ApiError(409, 'active_tenant_required', 'Choose a tenant first: this acts in one tenant.');
	};
	const sendWire = async (verb: Verb, url: string, wire: Wire): Promise<Response> => {
		const send = async (force: boolean) => {
			const headers: Record<string, string> = {
				...wire.headers,
				Authorization: `Bearer ${await token(force)}`,
				Accept: wire.accept,
				'X-Agent-Surface': ctx.surface(),
			};
			const tenant = ctx.activeTenant();
			if (tenant) headers['X-Active-Tenant-Id'] = tenant;
			if (wire.contentType) headers['Content-Type'] = wire.contentType;
			const init: RequestInit = { method: verb, headers, body: wire.body, credentials: 'same-origin' };
			if (wire.signal) init.signal = wire.signal;
			try {
				return await doFetch(url, init);
			} catch (e) {
				if (wire.signal?.aborted) throw e;
				throw new ApiError(0, 'network_error', 'Could not reach the server. Check your connection.');
			}
		};
		const res = await send(false);
		return res.status === 401 ? send(true) : res;
	};

	const call = <T>(verb: Verb, path: string, opts: ReqOpts = {}): Promise<T> =>
		reported(() => callJson<T>(verb, path, opts));
	const callJson = async <T>(verb: Verb, path: string, opts: ReqOpts): Promise<T> => {
		routed(verb, path);
		const text = opts.response === 'text';
		const res = await sendWire(verb, path + queryString(opts.query), {
			headers: opts.headers,
			accept:
				opts.response === 'blob'
					? 'image/*, application/pdf, application/json;q=0.5'
					: text
						? 'text/csv, text/markdown, text/plain, application/json;q=0.5'
						: 'application/json',
			body: opts.body === undefined ? undefined : JSON.stringify(opts.body),
			contentType: opts.body === undefined ? undefined : 'application/json',
		});
		if (opts.response === 'blob') return readBlob(res) as Promise<T>;
		return text ? (readText(res) as Promise<T>) : readJson<T>(res);
	};

	const stream: StreamReq = (verb, path, opts) => reported(() => streamed(verb, path, opts));
	const streamed: StreamReq = async (verb, path, opts) => {
		routed(verb, path);
		const res = await sendWire(verb, path, {
			headers: opts.headers,
			accept: 'text/event-stream',
			body: opts.body === undefined ? undefined : JSON.stringify(opts.body),
			contentType: opts.body === undefined ? undefined : 'application/json',
			signal: opts.signal,
		});
		if (!res.ok) {
			const text = await res.text().catch(() => '');
			let value: unknown = undefined;
			try {
				value = text ? JSON.parse(text) : undefined;
			} catch {
				value = undefined;
			}
			throw errorOf(res.status, value);
		}
		if (!res.body) throw new ApiError(res.status, 'bad_response', 'The server sent no stream.');
		opts.onOpen?.();
		const parser = createSseParser(opts.onEvent);
		const reader = res.body.getReader();
		const decoder = new TextDecoder();
		let finished = false;
		try {
			for (;;) {
				const { done, value } = await reader.read();
				if (done) break;
				parser.push(decoder.decode(value, { stream: true }));
			}
			finished = true;
			parser.push(decoder.decode());
			parser.end();
		} catch (e) {
			if (opts.signal?.aborted || e instanceof ApiError) throw e;
			if (!finished)
				throw new ApiError(0, 'network_error', 'The connection dropped while the reply was streaming.');
			throw e;
		} finally {
			if (!finished) reader.cancel().catch(() => {});
		}
	};

	const form: FormReq = async <T>(
		verb: Verb,
		path: string,
		data: FormData,
		opts: { signal?: AbortSignal } = {},
	) => {
		routed(verb, path);
		return reported(async () => {
			const res = await sendWire(verb, path, { accept: 'application/json', body: data, signal: opts.signal });
			return readJson<T>(res);
		});
	};

	return Object.assign(call, { stream, form }) as Req;
}

/**
 * The surface a path belongs to: `/console…` → console, `/chat…` → chat, `/support…` → support (the staff workspace:
 * support routing counts a member there as online), everything else admin.
 */
export function surfaceOf(pathname: string): Surface {
	if (pathname.startsWith('/console')) return 'console';
	if (pathname.startsWith('/chat')) return 'chat';
	if (pathname.startsWith('/support')) return 'support';
	return 'admin';
}

function browserSurface(): Surface {
	try {
		return surfaceOf(globalThis.location?.pathname ?? '/');
	} catch {
		return 'admin';
	}
}

/** `GET /api/me` before the app has a client (boot): the real caller, no active tenant. */
export function fetchMe(opts: Pick<HttpContext, 'fetchImpl' | 'token'> = {}): Promise<Me> {
	return makeRequest({ activeTenant: () => null, surface: browserSurface, ...opts })<Me>('GET', '/api/me');
}

/** The `req` the running app uses: the active tenant and surface read at call time; licence refusals reported. */
export function appRequest(
	activeTenant: () => string | null,
	onLicenceError?: (e: LicenceError) => void,
	platformOnly?: () => boolean,
): Req {
	return makeRequest({ activeTenant, surface: browserSurface, onLicenceError, platformOnly });
}
