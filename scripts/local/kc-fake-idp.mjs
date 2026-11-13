#!/usr/bin/env node
// B4 known-customer web chat, LOCAL acceptance: in-memory stand-ins for the two admin clients agent-api calls when
// the (removed) tenant recreate script imported a tenant (POST /api/tenants/import with first_admin = an email):
//
//   Keycloak admin  (crates/agent-api/src/keycloak_admin.rs; KEYCLOAK_ADMIN_API_URL = http://127.0.0.1:<port>/admin/realms/portal)
//     GET    users?email=|username=&exact=true      user lookup (case-insensitive exact)
//     POST   users                                   create -> 201 + Location: .../users/<id>; 409 when email/username exists
//     GET|PUT|DELETE users/{id}                      read / merge (attributes, names) / delete
//     GET    users/{id}/credentials | federated-identity   [] (a new user has none -> set-password mail)
//     PUT    users/{id}/execute-actions-email        204, recorded (the invite/actions mail; nothing is sent)
//     GET    users/{id}/role-mappings/clients/{client}/composite   roles mapped to the user on that client
//     GET|POST|DELETE users/{id}/role-mappings/(realm|clients/{client})   realm / client role mapping
//     GET    roles/{name}, clients/{client}/roles/{name}, clients?clientId=   role / client lookups
//   Licences admin  (src/licenses_admin.rs; LICENSES_ADMIN_URL = http://127.0.0.1:<port>)
//     GET apps/{key}, apps/{key}/plans, regions; PUT tenants/{id}; POST licenses/user/{sub};
//     POST licenses/{id}/tokens; POST license/revoke; POST license/introspect
//   POST /token                                      inert service-account token (KEYCLOAK_ADMIN_TOKEN_URL / LICENSES_ADMIN_TOKEN_URL)
//   GET  /health, /__state                           liveness; users, mail log, licences (no tokens), for the operator
//
// It is a SEPARATE process from scripts/local/b8-fake-idp.mjs, which keeps serving the realm's JWKS and `/__mint`
// (the token issuer agent-api verifies) unchanged on its own port. Any bearer is accepted; this is a local fake.
//
// Env: KC_IDP_ADMIN_PORT (required), KC_PLAN_CODE (default byo, = LICENSE_PLAN_CODE).
import { createServer } from 'node:http';
import { randomUUID, randomBytes } from 'node:crypto';
import { existsSync, readFileSync, writeFileSync } from 'node:fs';
import { signBundleToken } from './ps-bundle.mjs';

const PORT = Number(process.env.KC_IDP_ADMIN_PORT ?? 0);
if (!PORT) throw new Error('KC_IDP_ADMIN_PORT is required');
const PLAN = process.env.KC_PLAN_CODE ?? 'byo';
// bundle-token minter (agent-api tenants/app_licence.rs): enabled when PS_BUNDLE_SECRET is set
const BUNDLE_SECRET = process.env.PS_BUNDLE_SECRET ?? '';
const BUNDLE_APPS = new Set([
	'inference-router',
	'strata-rag',
	'strata-ingest',
	// Opt-in extras for a local acceptance whose tenant system mints another app key (the conveyor's
	// `app=conveyor`, licenses#317). Unset = exactly the three platform apps above.
	...(process.env.KC_BUNDLE_APPS ?? '')
		.split(',')
		.map((s) => s.trim())
		.filter(Boolean),
]);
const bundles = new Set(); // `${tenant}|${bundle}` ensured
const UUID = /^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}$/;

const users = new Map(); // id -> representation
const mails = []; // {user, actions, client_id}
const mappings = new Map(); // `${user}|${scope}` -> [{id,name}]   scope = realm | client:<uuid>
const APP = process.env.KC_APP_KEY ?? 'agent'; // agent-api's LICENSE_APP_KEY default
// licences and their tokens survive a restart (the tenant keeps its sealed token across `up`): KC_FAKE_STATE file
const STATE = process.env.KC_FAKE_STATE ?? '';
const saved = STATE && existsSync(STATE) ? JSON.parse(readFileSync(STATE, 'utf8')) : {};
const licences = new Map(saved.licences ?? []); // license_id -> {sub, tenant, revoked}
const tokens = new Map(saved.tokens ?? []); // raw token -> license_id
const persist = () => STATE && writeFileSync(STATE, JSON.stringify({ licences: [...licences], tokens: [...tokens] }), { mode: 0o600 });
const tenants = new Map();
const subjects = new Map(); // holder sub -> licences subject id
const roleId = (n) => `role-${n}`;

const json = (res, status, body, headers = {}) => {
	res.writeHead(status, { 'content-type': 'application/json', ...headers });
	res.end(body === undefined ? '' : JSON.stringify(body));
};
async function readJson(req) {
	const chunks = [];
	for await (const c of req) chunks.push(c);
	try {
		return JSON.parse(Buffer.concat(chunks).toString() || '{}');
	} catch {
		return {};
	}
}
const lc = (s) => String(s ?? '').toLowerCase();

function mappingList(user, scope) {
	return mappings.get(`${user}|${scope}`) ?? [];
}

function keycloak(req, res, u, seg, body) {
	// seg = path after /admin/realms/<realm>/
	const m = req.method;
	if (seg[0] === 'users' && seg.length === 1) {
		if (m === 'GET') {
			const q = u.searchParams;
			const field = q.has('email') ? 'email' : q.has('username') ? 'username' : null;
			const want = lc(q.get(field ?? ''));
			const hits = [...users.values()].filter((x) => !field || lc(x[field]) === want);
			return json(res, 200, hits.slice(0, Number(q.get('max') ?? 100)));
		}
		if (m === 'POST') {
			const taken = [...users.values()].some(
				(x) => lc(x.email) === lc(body.email) || lc(x.username) === lc(body.username ?? body.email),
			);
			if (taken) return json(res, 409, { errorMessage: 'User exists with same username or email' });
			const id = randomUUID();
			users.set(id, { id, enabled: true, emailVerified: false, requiredActions: [], attributes: {}, ...body });
			return json(res, 201, undefined, { location: `http://127.0.0.1:${PORT}${u.pathname.replace(/\/$/, '')}/${id}` });
		}
	}
	if (seg[0] === 'users' && seg[1]) {
		const user = users.get(seg[1]);
		const rest = seg.slice(2).join('/');
		if (!user) return json(res, 404, { error: 'User not found' });
		if (rest === '') {
			if (m === 'GET') return json(res, 200, user);
			if (m === 'PUT') {
				Object.assign(user, body, { id: user.id });
				return json(res, 204);
			}
			if (m === 'DELETE') {
				users.delete(user.id);
				return json(res, 204);
			}
		}
		if (m === 'GET' && (rest === 'credentials' || rest === 'federated-identity')) return json(res, 200, []);
		if (m === 'PUT' && rest === 'execute-actions-email') {
			mails.push({
				user: user.id,
				email: user.email,
				actions: body,
				client_id: u.searchParams.get('client_id'),
			});
			return json(res, 204);
		}
		const rm = /^role-mappings\/(realm|clients\/([^/]+))(\/composite)?$/.exec(rest);
		if (rm) {
			const scope = rm[1] === 'realm' ? 'realm' : `client:${rm[2]}`;
			if (m === 'GET') return json(res, 200, mappingList(user.id, scope));
			if (m === 'POST') {
				const cur = mappingList(user.id, scope);
				for (const r of Array.isArray(body) ? body : []) if (!cur.some((c) => c.name === r.name)) cur.push({ id: r.id ?? roleId(r.name), name: r.name });
				mappings.set(`${user.id}|${scope}`, cur);
				return json(res, 204);
			}
			if (m === 'DELETE') {
				const drop = new Set((Array.isArray(body) ? body : []).map((r) => r.name));
				mappings.set(`${user.id}|${scope}`, mappingList(user.id, scope).filter((r) => !drop.has(r.name)));
				return json(res, 204);
			}
		}
	}
	if (m === 'GET' && seg[0] === 'roles' && seg[1]) return json(res, 200, { id: roleId(seg[1]), name: seg[1] });
	if (m === 'GET' && seg[0] === 'clients' && seg.length === 1)
		return json(res, 200, [{ id: 'kc-inert-client', clientId: u.searchParams.get('clientId') ?? 'agent' }]);
	if (m === 'GET' && seg[0] === 'clients' && seg[2] === 'roles' && seg[3]) return json(res, 200, { id: roleId(seg[3]), name: seg[3] });
	return json(res, 404, { error: 'not_found', path: u.pathname });
}

function licencesAdmin(req, res, u, seg, body) {
	const m = req.method;
	// POST /internal/tenants/{id}/bundles/{key}/ensure {plans} -> 200 {refused: []}  (licenses_admin.rs ensure_bundle)
	if (m === 'POST' && seg[0] === 'internal' && seg[1] === 'tenants' && seg[3] === 'bundles' && seg[5] === 'ensure') {
		bundles.add(`${seg[2]}|${seg[4]}`);
		return json(res, 200, { refused: [] });
	}
	// POST /internal/tenants/{tenant}/bundle-token?app=&bundle=&ttl_minutes= -> 201 {token, expires_at, tenant_id, bundle,
	// app, license_id, token_id} (app_licence.rs BundleMinter::mint); 401 without the minter client's bearer
	if (m === 'POST' && seg[0] === 'internal' && seg[1] === 'tenants' && seg[3] === 'bundle-token') {
		if (!/^Bearer \S+/.test(req.headers.authorization ?? '')) return json(res, 401, { detail: { error: 'unauthorized' } });
		if (!BUNDLE_SECRET) return json(res, 503, { detail: { error: 'minter_not_configured' } });
		const tenant = seg[2];
		const app = u.searchParams.get('app') ?? '';
		const bundle = u.searchParams.get('bundle') ?? 'agent';
		const ttl = Math.min(60, Math.max(1, Number(u.searchParams.get('ttl_minutes') ?? 15)));
		if (!UUID.test(tenant)) return json(res, 400, { detail: { error: 'invalid_tenant' } });
		if (!BUNDLE_APPS.has(app)) return json(res, 400, { detail: { error: 'app_not_in_bundle' } });
		const { token, exp } = signBundleToken(BUNDLE_SECRET, app, tenant, ttl * 60);
		return json(res, 201, {
			token,
			expires_at: new Date(exp * 1000).toISOString(),
			tenant_id: tenant,
			bundle,
			app,
			license_id: `bundle-${tenant}-${app}`,
			token_id: randomUUID(),
		});
	}
	if (m === 'GET' && seg[0] === 'apps' && seg.length === 2) return json(res, 200, { id: `app-${seg[1]}`, key: seg[1] });
	if (m === 'GET' && seg[0] === 'apps' && seg[2] === 'plans')
		return json(res, 200, [PLAN, 'free', 'pro'].map((code) => ({ id: `plan-${code}`, code })));
	if (m === 'GET' && seg[0] === 'regions')
		return json(res, 200, ['eu', 'us'].map((code) => ({ id: `region-${code}`, code })));
	if (m === 'PUT' && seg[0] === 'tenants' && seg[1]) {
		tenants.set(seg[1], body.name ?? '');
		return json(res, 200, { id: seg[1], name: body.name });
	}
	if (m === 'POST' && seg[0] === 'licenses' && seg[1] === 'user' && seg[2]) {
		const sub = decodeURIComponent(seg[2]);
		const tenant = req.headers['x-tenant-id'] ?? null;
		// idempotent per tenant + holder, as licences is
		const hit = [...licences.entries()].find(([, l]) => l.sub === sub && l.tenant === tenant && !l.revoked);
		const license_id = hit?.[0] ?? randomUUID();
		if (!subjects.has(sub)) subjects.set(sub, randomUUID());
		if (!hit) licences.set(license_id, { sub, tenant, revoked: false });
		persist();
		return json(res, 200, { license_id, id: license_id, subject_id: subjects.get(sub) });
	}
	if (m === 'POST' && seg[0] === 'licenses' && seg[2] === 'tokens') {
		const token = `lic_${randomBytes(18).toString('base64url')}`;
		tokens.set(token, seg[1]);
		persist();
		return json(res, 200, { token });
	}
	if (m === 'POST' && seg[0] === 'license' && seg[1] === 'revoke') {
		const l = licences.get(body.license_id);
		if (l) l.revoked = true;
		persist();
		return json(res, 200, { revoked: Boolean(l) });
	}
	// licences' TokenIntrospectResponse: the token's licence, its tenant, app and plan (agent's verdict checks all four)
	if (m === 'POST' && seg[0] === 'license' && seg[1] === 'introspect') {
		const license_id = tokens.get(body.token);
		const l = license_id && licences.get(license_id);
		if (!l) return json(res, 200, { active: false, reason: 'token_not_found' });
		if (l.revoked) return json(res, 200, { active: false, reason: 'token_invalid' });
		return json(res, 200, { active: true, status: 'active', app: { key: APP }, tenant_id: l.tenant, license_id, plan: { code: PLAN } });
	}
	return json(res, 404, { error: 'not_found', path: u.pathname });
}

createServer(async (req, res) => {
	const u = new URL(req.url, `http://127.0.0.1:${PORT}`);
	const seg = u.pathname.split('/').filter(Boolean).map(decodeURIComponent);
	try {
		if (u.pathname === '/health') return json(res, 200, { ok: true });
		if (u.pathname === '/__state')
			return json(res, 200, {
				users: [...users.values()].map(({ id, email, username, firstName, lastName }) => ({ id, email, username, firstName, lastName })),
				mails,
				mappings: Object.fromEntries(mappings),
				tenants: Object.fromEntries(tenants),
				licences: [...licences.entries()].map(([id, l]) => ({ id, ...l })),
			});
		if (u.pathname === '/token') {
			await readJson(req);
			return json(res, 200, { access_token: 'inert', expires_in: 300, token_type: 'Bearer' });
		}
		const body = ['POST', 'PUT', 'DELETE'].includes(req.method) ? await readJson(req) : {};
		if (seg[0] === 'admin' && seg[1] === 'realms') return keycloak(req, res, u, seg.slice(3), body);
		return licencesAdmin(req, res, u, seg, body);
	} catch (e) {
		json(res, 500, { error: String(e) });
	}
}).listen(PORT, '127.0.0.1', () => console.log(`kc-fake-idp (keycloak admin + licences stubs) on http://127.0.0.1:${PORT}`));
