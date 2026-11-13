import type { Me, Role, ViewAs } from '$lib/api/types/core';

/** D88 tenant roles from agent's `users` table, reported by `GET /api/me`. Token roles grant nothing in the web. */
export type { Me, Role };
/** `actor` is what the API records as the actor: preferred_username, else sub. In U1 the persona's subject. */
export type User = { sub: string; name: string; actor?: string };

export class Session {
	user = $state<User | null>(null);
	me = $state<Me | null>(null);
	set(user: User | null, me: Me | null) {
		this.user = user;
		this.me = me;
	}
	get superAdmin(): boolean {
		return this.me?.super_admin === true;
	}
	/**
	 * The caller acts in a tenant: a member of their home tenant (with or without roles — no role is an observer),
	 * or a super_admin with an active tenant. Nobody else (unprovisioned, super_admin with no tenant) reads tenant
	 * pages.
	 */
	get member(): boolean {
		return !!this.me?.tenant;
	}
	/** Roles held in the active tenant (§R.1 order; a super_admin acting in a tenant holds all). */
	get roles(): Role[] {
		return this.me?.roles ?? [];
	}
	/** The roles every check uses: held ∩ view-as (D79). */
	get effectiveRoles(): Role[] {
		return this.me?.effective_roles ?? [];
	}
	/** Active view-as, else null. */
	get viewAs(): ViewAs | null {
		return this.me?.view_as ?? null;
	}
	/** May use the writes §R.1 gives `role` (reads the EFFECTIVE roles, so a narrowed view hides what it narrows). */
	has(role: Role): boolean {
		return this.effectiveRoles.includes(role);
	}
}

export const session = new Session();

// ---- sign out ---------------------------------------------------------------------------------------------------

/**
 * Where sign-out lands: the app root (UX reference: care-bus-agents). An explicit root, never the current URL — a
 * Keycloak redirect back to a URL with a `#state=…` fragment matches no registered post-logout URI (400).
 */
export function logoutRedirect(origin: string): string {
	return `${origin.replace(/\/+$/, '')}/`;
}

/** The realm a real build signs in against (the future boot sets it; U1 has none and stays `fixture`). */
export interface KeycloakLogout {
	url: string;
	realm: string;
	clientId: string;
	/** The signed-in id token, sent as `id_token_hint` so Keycloak skips its "Do you want to log out?" page. */
	idToken?: string | null;
}

/** OpenID end-session URL — what the Keycloak adapter's `logout({ redirectUri })` navigates to. */
export function keycloakLogoutUrl(kc: KeycloakLogout, origin: string): string {
	const q = new URLSearchParams({ client_id: kc.clientId, post_logout_redirect_uri: logoutRedirect(origin) });
	if (kc.idToken) q.set('id_token_hint', kc.idToken);
	return `${kc.url.replace(/\/+$/, '')}/realms/${encodeURIComponent(kc.realm)}/protocol/openid-connect/logout?${q}`;
}

/**
 * The Keycloak realm signed in against (set by `initAuth` in `./session.ts`; `null` before sign-in). `signOut` is
 * the Keycloak adapter's `logout`; without it sign-out builds the same end-session URL itself.
 */
let authMode: { kind: 'keycloak'; realm: KeycloakLogout; signOut?: (origin: string) => unknown } | null =
	null;
export function setAuthMode(mode: typeof authMode): void {
	authMode = mode;
}

/** Storage key the app keeps the active job under (`$lib/app.svelte`); sign-out forgets it. */
export const SIGN_IN_KEYS = { job: 'agent-job' } as const;

export interface LogoutDeps {
	/** Full navigation (a reload): the app boots again from the root with nobody remembered. */
	navigate: (href: string) => void;
	origin: string;
	local?: Pick<Storage, 'removeItem'> | null;
	tab?: Pick<Storage, 'removeItem'> | null;
}

function browserDeps(): LogoutDeps {
	const store = (get: () => Storage) => {
		try {
			return get();
		} catch {
			return null; // storage blocked: nothing was remembered either
		}
	};
	return {
		navigate: (href) => window.location.assign(href),
		origin: window.location.origin,
		local: store(() => localStorage),
		tab: store(() => sessionStorage),
	};
}

/**
 * Sign out: clears the session and redirects to Keycloak's end-session endpoint, which returns to the app root.
 * Before sign-in finished (no realm known) it forgets the active job and reloads at the app root.
 */
export function logout(deps: LogoutDeps = browserDeps()): void {
	session.set(null, null);
	if (authMode) {
		if (authMode.signOut) void authMode.signOut(deps.origin);
		else deps.navigate(keycloakLogoutUrl(authMode.realm, deps.origin));
		return;
	}
	try {
		deps.tab?.removeItem(SIGN_IN_KEYS.job);
	} catch {
		/* storage blocked */
	}
	deps.navigate(logoutRedirect(deps.origin));
}
