// Real sign-in: keycloak-js PKCE login-required + silent refresh + Keycloak logout (session.ts).
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const kc = vi.hoisted(() => ({
	instances: [] as Record<string, unknown>[],
	init: vi.fn(async () => true),
	updateToken: vi.fn(async () => true),
	login: vi.fn(async () => undefined),
	logout: vi.fn(async () => undefined),
}));

vi.mock('keycloak-js', () => ({
	default: class {
		token = 'access-1';
		idToken = 'id-1';
		tokenParsed = { name: 'Robin Vale', preferred_username: 'robin', sub: 's-1' };
		onTokenExpired?: () => void;
		init = kc.init;
		updateToken = kc.updateToken;
		login = kc.login;
		logout = kc.logout;
		constructor(public cfg: Record<string, unknown>) {
			kc.instances.push(this as unknown as Record<string, unknown>);
		}
	},
}));

import { getToken, initAuth, resetAuth } from '../../src/lib/auth/session';
import { DEFAULT_AUTH_CONFIG } from '../../src/lib/auth/config';
import { logout, setAuthMode } from '../../src/lib/auth/session.svelte';
import { session } from '../../src/lib/auth/session.svelte';

beforeEach(() => {
	kc.instances.length = 0;
	for (const f of [kc.init, kc.updateToken, kc.login, kc.logout]) f.mockClear();
});
afterEach(() => {
	vi.useRealTimers();
	resetAuth();
	setAuthMode(null);
});

describe('session.ts (keycloak-js)', () => {
	it('defaults: auth.nonagentic.ai, realm portal, client agent', () => {
		expect(DEFAULT_AUTH_CONFIG).toEqual({
			keycloakUrl: 'https://auth.nonagentic.ai',
			realm: 'portal',
			clientId: 'agent',
		});
	});

	it('initAuth: login-required with PKCE S256, no login iframe; resolves the name', async () => {
		const who = await initAuth(DEFAULT_AUTH_CONFIG);
		expect(who.name).toBe('Robin Vale');
		expect(kc.instances[0]!.cfg).toEqual({
			url: 'https://auth.nonagentic.ai',
			realm: 'portal',
			clientId: 'agent',
		});
		expect(kc.init).toHaveBeenCalledWith({
			onLoad: 'login-required',
			pkceMethod: 'S256',
			checkLoginIframe: false,
			redirectUri: location.origin + location.pathname + location.search,
		});
	});

	it('a deep link with its own fragment signs in with a fragment-free redirect and keeps the fragment', async () => {
		history.replaceState(null, '', '/admin/jobs/j1/release#conflicts');
		// the round trip to Keycloak drops the fragment; the callback page has none of its own
		kc.init.mockImplementationOnce(async () => {
			history.replaceState(null, '', '/admin/jobs/j1/release');
			return true;
		});
		await initAuth(DEFAULT_AUTH_CONFIG);
		const opts = (kc.init.mock.calls[0] as unknown as [{ redirectUri: string }])[0];
		expect(opts.redirectUri).toBe(`${location.origin}/admin/jobs/j1/release`);
		expect(location.hash).toBe('#conflicts');
		history.replaceState(null, '', '/');
	});

	it('refreshes silently: on token expiry and on a timer', async () => {
		vi.useFakeTimers();
		await initAuth(DEFAULT_AUTH_CONFIG);
		(kc.instances[0]!.onTokenExpired as () => void)();
		expect(kc.updateToken).toHaveBeenCalledTimes(1);
		await vi.advanceTimersByTimeAsync(60_000);
		expect(kc.updateToken).toHaveBeenCalledTimes(2);
		expect(kc.updateToken).toHaveBeenLastCalledWith(70);
	});

	it('a failed refresh sends the user to sign in again', async () => {
		await initAuth(DEFAULT_AUTH_CONFIG);
		kc.updateToken.mockRejectedValueOnce(new Error('expired'));
		await expect(getToken()).rejects.toThrow('Your session expired');
		expect(kc.login).toHaveBeenCalled();
	});

	it('getToken: refreshed token; force refreshes regardless', async () => {
		await expect(getToken()).rejects.toThrow('Please sign in');
		await initAuth(DEFAULT_AUTH_CONFIG);
		expect(await getToken()).toBe('access-1');
		await getToken(true);
		expect(kc.updateToken).toHaveBeenLastCalledWith(-1);
	});

	it('Sign out uses the Keycloak adapter logout, back to the app root', async () => {
		await initAuth(DEFAULT_AUTH_CONFIG);
		const navigate = vi.fn();
		logout({ navigate, origin: 'https://agent.nonagentic.ai' });
		expect(kc.logout).toHaveBeenCalledWith({ redirectUri: 'https://agent.nonagentic.ai/' });
		expect(navigate).not.toHaveBeenCalled();
		expect(session.me).toBeNull();
	});
});
