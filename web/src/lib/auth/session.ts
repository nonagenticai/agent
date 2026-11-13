// Real sign-in (early access, 2026-10-19): keycloak-js against realm `portal`, public PKCE client `agent`,
// `login-required`, tokens refreshed silently before they expire (after strata_configurator web/src/lib/auth.ts).
// The config comes from the server at runtime (`+layout.server.ts`), never baked into the build. One of two files allowed to touch Keycloak or
// the network (tests/unit/guard.test.ts).
import Keycloak from 'keycloak-js';
import { logoutRedirect, setAuthMode } from './session.svelte';

import type { AuthConfig } from './config';
export { DEFAULT_AUTH_CONFIG, type AuthConfig } from './config';

/** Refresh when fewer than this many seconds of the access token are left. */
const MIN_VALIDITY_S = 70;
/** How often the silent refresh checks the token. */
const REFRESH_EVERY_MS = 60_000;

let keycloak: Keycloak | undefined;
let timer: ReturnType<typeof setInterval> | undefined;

async function refresh(): Promise<void> {
	if (!keycloak) return;
	try {
		await keycloak.updateToken(MIN_VALIDITY_S);
	} catch {
		// The refresh token is gone (session ended in Keycloak): sign in again.
		await keycloak.login();
	}
}

const HASH_KEY = 'agent-auth-return-hash';
function remember(hash: string): void {
	try {
		sessionStorage.setItem(HASH_KEY, hash);
	} catch {
		/* storage blocked: the fragment is lost, sign-in still works */
	}
}
function recall(): string | null {
	try {
		const h = sessionStorage.getItem(HASH_KEY);
		sessionStorage.removeItem(HASH_KEY);
		return h;
	} catch {
		return null;
	}
}

/**
 * Signs in (redirects to Keycloak when there is no session; resolves once back with tokens), starts the silent
 * refresh and points sign-out at Keycloak's logout. Resolves the signed-in user's display name.
 */
export async function initAuth(cfg: AuthConfig): Promise<{ name: string }> {
	const kc = new Keycloak({ url: cfg.keycloakUrl, realm: cfg.realm, clientId: cfg.clientId });
	kc.onTokenExpired = () => void refresh();
	// keycloak-js would send location.href as the redirect: a deep link with its own fragment (`/release#conflicts`)
	// then comes back as `#conflicts#state=…&code=…`, which it cannot read, so it logs in again with an ever longer
	// URL until the IdP answers 431. Send a fragment-free redirect and put the page's fragment back after sign-in.
	const back = location.origin + location.pathname + location.search;
	const ownHash = location.hash && !/[#&]state=/.test(location.hash) ? location.hash : '';
	if (ownHash) remember(ownHash);
	await kc.init({ onLoad: 'login-required', pkceMethod: 'S256', checkLoginIframe: false, redirectUri: back });
	const hash = recall();
	if (hash && !location.hash) history.replaceState(history.state, '', back + hash);
	keycloak = kc;
	if (timer) clearInterval(timer);
	timer = setInterval(() => void refresh(), REFRESH_EVERY_MS);
	setAuthMode({
		kind: 'keycloak',
		realm: { url: cfg.keycloakUrl, realm: cfg.realm, clientId: cfg.clientId, idToken: kc.idToken ?? null },
		signOut: (origin) => kc.logout({ redirectUri: logoutRedirect(origin) }),
	});
	const t = kc.tokenParsed as { name?: string; preferred_username?: string; sub?: string } | undefined;
	return { name: t?.name || t?.preferred_username || t?.sub || 'Signed in' };
}

/** A valid access token (refreshed first when close to expiry; `force` refreshes regardless). */
export async function getToken(force = false): Promise<string> {
	if (!keycloak) throw new Error('Please sign in to continue.');
	try {
		await keycloak.updateToken(force ? -1 : MIN_VALIDITY_S);
	} catch {
		await keycloak.login();
		throw new Error('Your session expired. Please sign in again.');
	}
	if (!keycloak.token) throw new Error('Please sign in to continue.');
	return keycloak.token;
}

/** Tests: forget the adapter and stop the refresh timer. */
export function resetAuth(): void {
	if (timer) clearInterval(timer);
	timer = undefined;
	keycloak = undefined;
}
