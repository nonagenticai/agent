// Platform-stack harness: the fake bundle token shared by the fake minter (kc-fake-idp.mjs), the fake inference router
// (kc-fake-llm.mjs) and the fake strata (kc-fake-strata.mjs / b8-fake-strata.mjs). A token is self-verifying, so the
// three upstreams need no shared state: bnd.<app>.<tenant-uuid>.<expiry-epoch-s>.<hmac-sha256(secret, app|tenant|exp)>.
// Printable ASCII only (agent-api accepts 1..4096 chars in 33..=126). Enforcement is opt-in: PS_BUNDLE_SECRET unset
// leaves every other acceptance script's fakes exactly as they were.
import { createHmac, randomBytes } from 'node:crypto';

const sig = (secret, app, tenant, exp) => createHmac('sha256', secret).update(`${app}|${tenant}|${exp}`).digest('base64url');

export function signBundleToken(secret, app, tenant, ttlSeconds) {
	const exp = Math.floor(Date.now() / 1000) + ttlSeconds;
	return { token: `bnd.${app}.${tenant}.${exp}.${sig(secret, app, tenant, exp)}.${randomBytes(4).toString('hex')}`, exp };
}

/** The tenant a valid, unexpired token was minted for `app` (any app when `app` is null), else null. */
export function verifyBundleToken(secret, token, app = null) {
	const m = /^bnd\.([a-z-]+)\.([0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12})\.(\d+)\.([A-Za-z0-9_-]+)\.[0-9a-f]{8}$/.exec(String(token ?? ''));
	if (!m || !secret) return null;
	const [, a, tenant, exp, s] = m;
	if (app && a !== app) return null;
	if (Number(exp) < Date.now() / 1000) return null;
	return s === sig(secret, a, tenant, exp) ? tenant : null;
}

export const bearerOf = (req) => /^Bearer (.+)$/.exec(req.headers.authorization ?? '')?.[1] ?? '';
