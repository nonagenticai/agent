#!/usr/bin/env node
// B8 local acceptance: a tiny stand-in for Keycloak realm `portal` (public PKCE client `agent`).
//
// agent-api verifies RS256 tokens against KEYCLOAK_JWKS_URL and the web signs in with keycloak-js; there is no
// dev auth bypass, so the local run needs a real issuer. This one signs real tokens with a persisted RSA key.
//
//   GET  /realms/portal/protocol/openid-connect/auth   -> 302 back to redirect_uri with a code (the persona comes
//                                                         from the `b8_persona` cookie or query; default `sa`)
//   POST /realms/portal/protocol/openid-connect/token  -> authorization_code and refresh_token grants
//   GET  /realms/portal/protocol/openid-connect/certs  -> the JWKS
//   GET  /realms/portal/protocol/openid-connect/logout -> 302 to post_logout_redirect_uri
//   GET|POST /__mint?persona=ta[&sub=<id>&email=<e>]    -> {access_token} for API-level test setup (sub/email override the
//                                                         persona's fixed identity; /auth takes cookies/query b8_sub, b8_email)
//   POST /token, /admin/**                              -> inert Keycloak-admin / licences stubs (boot needs the URLs)
//
// Persona -> token: sub and preferred_username `b8-<persona>`; only `sa` carries the client role `super_admin`.
// Tenant roles are NOT in the token (agent reads them from its `users` table; scripts/local/b8-seed.sql).
//
// Env: B8_IDP_PORT (required), B8_IDP_KEY_FILE (PEM, created when missing), B8_IDP_AUDIENCE (default agent).
import { createServer } from 'node:http';
import { createPrivateKey, createPublicKey, createSign, generateKeyPairSync, randomUUID } from 'node:crypto';
import { existsSync, readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { dirname } from 'node:path';

const PORT = Number(process.env.B8_IDP_PORT ?? 0);
if (!PORT) throw new Error('B8_IDP_PORT is required');
const AUD = process.env.B8_IDP_AUDIENCE ?? 'agent';
const KEY_FILE = process.env.B8_IDP_KEY_FILE ?? '';
const ISSUER = `http://127.0.0.1:${PORT}/realms/portal`;
const OIDC = '/realms/portal/protocol/openid-connect';
const KID = 'b8-acceptance';

function loadKey() {
	if (KEY_FILE && existsSync(KEY_FILE)) return createPrivateKey(readFileSync(KEY_FILE));
	const { privateKey } = generateKeyPairSync('rsa', { modulusLength: 2048 });
	if (KEY_FILE) {
		mkdirSync(dirname(KEY_FILE), { recursive: true });
		writeFileSync(KEY_FILE, privateKey.export({ type: 'pkcs8', format: 'pem' }), { mode: 0o600 });
	}
	return privateKey;
}
const privateKey = loadKey();
const jwk = { ...createPublicKey(privateKey).export({ format: 'jwk' }), kid: KID, alg: 'RS256', use: 'sig' };

const b64 = (o) => Buffer.from(typeof o === 'string' ? o : JSON.stringify(o)).toString('base64url');
function jwt(claims) {
	const head = b64({ alg: 'RS256', typ: 'JWT', kid: KID });
	const body = b64(claims);
	const sig = createSign('RSA-SHA256').update(`${head}.${body}`).sign(privateKey).toString('base64url');
	return `${head}.${body}.${sig}`;
}

const PERSONA = /^[a-z0-9_]{1,16}$/;
// ov = {sub?, email?}: an identity another fake created (a Keycloak-admin user id), e.g. a tenant's first admin made by
// POST /api/tenants. Absent: the persona's fixed `b8-<persona>`. Additive: b8-acceptance never passes it.
function claimsOf(persona, nonce, ttl = 3600, ov = {}) {
	const now = Math.floor(Date.now() / 1000);
	const sub = ov.sub || `b8-${persona}`;
	// `intake` = the website/chat intake's client-credentials token (the product's signup door; SIGNUP_INTAKE_AZP and
	// SIGNUP_INTAKE_ROLE defaults): own azp + aud, a REALM role, never super_admin. Additive: b8-acceptance never asks for it.
	if (persona === 'intake') {
		return { iss: ISSUER, aud: [AUD, 'agent-signup-intake'], azp: 'agent-signup-intake', sub: 'b8-intake', preferred_username: 'service-account-agent-signup-intake', iat: now, exp: now + ttl, typ: 'Bearer', realm_access: { roles: ['agent-signup-intake'] } };
	}
	return {
		iss: ISSUER,
		aud: AUD,
		azp: AUD,
		sub,
		preferred_username: sub,
		name: `B8 ${persona.toUpperCase()}`,
		email: ov.email || `${sub}@b8.example.test`,
		email_verified: true,
		sid: `sid-${persona}`,
		iat: now,
		exp: now + ttl,
		typ: 'Bearer',
		...(nonce ? { nonce } : {}),
		resource_access: { [AUD]: { roles: persona === 'sa' ? ['super_admin'] : [] } },
	};
}

function cors(req, res) {
	const origin = req.headers.origin;
	if (origin) {
		res.setHeader('access-control-allow-origin', origin);
		res.setHeader('access-control-allow-credentials', 'true');
		res.setHeader('access-control-allow-headers', 'content-type, authorization');
		res.setHeader('access-control-allow-methods', 'GET, POST, OPTIONS');
	}
}
const json = (res, status, body) => {
	res.writeHead(status, { 'content-type': 'application/json' });
	res.end(JSON.stringify(body));
};
async function readForm(req) {
	const chunks = [];
	for await (const c of req) chunks.push(c);
	const raw = Buffer.concat(chunks).toString();
	return Object.fromEntries(new URLSearchParams(raw));
}
const cookieOf = (req, name) =>
	(req.headers.cookie ?? '')
		.split(';')
		.map((c) => c.trim().split('='))
		.find(([k]) => k === name)?.[1];

function tokens(persona, nonce, ov = {}) {
	const access = jwt(claimsOf(persona, undefined, 3600, ov));
	return {
		access_token: access,
		id_token: jwt(claimsOf(persona, nonce, 3600, ov)),
		// keycloak-js decodes the refresh token as a JWT, so it must be one (p/n carry the persona and nonce).
		refresh_token: jwt({ ...claimsOf(persona, undefined, 3600, ov), typ: 'Refresh', p: persona, n: nonce ?? null, s: ov.sub ?? null, e: ov.email ?? null, r: randomUUID() }),
		token_type: 'Bearer',
		expires_in: 3600,
		refresh_expires_in: 7200,
		session_state: `sid-${persona}`,
		scope: 'openid',
	};
}

createServer(async (req, res) => {
	cors(req, res);
	const u = new URL(req.url, `http://127.0.0.1:${PORT}`);
	if (req.method === 'OPTIONS') return res.writeHead(204).end();
	try {
		if (u.pathname === '/health') return json(res, 200, { ok: true });
		if (u.pathname === `${OIDC}/certs`) return json(res, 200, { keys: [jwk] });
		if (u.pathname === '/realms/portal/.well-known/openid-configuration') {
			return json(res, 200, {
				issuer: ISSUER,
				authorization_endpoint: `${ISSUER}/protocol/openid-connect/auth`,
				token_endpoint: `${ISSUER}/protocol/openid-connect/token`,
				jwks_uri: `${ISSUER}/protocol/openid-connect/certs`,
			});
		}
		if (u.pathname === `${OIDC}/auth`) {
			const persona = u.searchParams.get('b8_persona') ?? cookieOf(req, 'b8_persona') ?? 'sa';
			if (!PERSONA.test(persona)) return json(res, 400, { error: 'bad persona' });
			const redirect = u.searchParams.get('redirect_uri');
			if (!redirect) return json(res, 400, { error: 'redirect_uri' });
			const sub = u.searchParams.get('b8_sub') ?? cookieOf(req, 'b8_sub');
			const email = u.searchParams.get('b8_email') ?? cookieOf(req, 'b8_email');
			const code = b64({ p: persona, n: u.searchParams.get('nonce'), s: sub ? decodeURIComponent(sub) : null, e: email ? decodeURIComponent(email) : null });
			const sep = u.searchParams.get('response_mode') === 'query' ? '?' : '#';
			const back = `${redirect}${sep}state=${encodeURIComponent(u.searchParams.get('state') ?? '')}&session_state=sid-${persona}&code=${code}`;
			res.writeHead(302, { location: back });
			return res.end();
		}
		if (u.pathname === `${OIDC}/token`) {
			const f = await readForm(req);
			const src = f.grant_type === 'refresh_token' ? f.refresh_token : f.code;
			const part = f.grant_type === 'refresh_token' ? (src ?? '').split('.')[1] : src;
			const { p, n, s: osub, e: oemail } = JSON.parse(Buffer.from(part ?? '', 'base64url').toString() || '{}');
			if (!p || !PERSONA.test(p)) return json(res, 400, { error: 'invalid_grant' });
			return json(res, 200, tokens(p, n, { sub: osub ?? undefined, email: oemail ?? undefined }));
		}
		if (u.pathname === `${OIDC}/logout`) {
			res.writeHead(302, { location: u.searchParams.get('post_logout_redirect_uri') ?? '/' });
			return res.end();
		}
		if (u.pathname === '/__mint') {
			const persona = u.searchParams.get('persona') ?? '';
			if (!PERSONA.test(persona)) return json(res, 400, { error: 'bad persona' });
			return json(res, 200, { access_token: jwt(claimsOf(persona, undefined, 3600, { sub: u.searchParams.get('sub') ?? undefined, email: u.searchParams.get('email') ?? undefined })) });
		}
		// Keycloak-admin and licences clients (required config; B8 never calls them): an inert token endpoint.
		if (u.pathname === '/token') return json(res, 200, { access_token: 'inert', expires_in: 300, token_type: 'Bearer' });
		json(res, 404, { error: 'not_found' });
	} catch (e) {
		json(res, 500, { error: String(e) });
	}
}).listen(PORT, '127.0.0.1', () => console.log(`b8-fake-idp listening on ${ISSUER}`));
