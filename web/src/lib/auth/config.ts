// The sign-in config `+layout.server.ts` reads from the environment and hands the browser (no Keycloak adapter here, so
// the server never loads the adapter).

export interface AuthConfig {
	keycloakUrl: string;
	realm: string;
	clientId: string;
}

export const DEFAULT_AUTH_CONFIG: AuthConfig = {
	keycloakUrl: 'https://auth.nonagentic.ai',
	realm: 'portal',
	clientId: 'agent',
};

/** The sign-in config from the public environment (`$app/env/public`), the defaults where a value is unset. */
export function authConfigFrom(pub: Record<string, string | undefined>): AuthConfig {
	return {
		keycloakUrl: pub.PUBLIC_KEYCLOAK_URL || DEFAULT_AUTH_CONFIG.keycloakUrl,
		realm: pub.PUBLIC_KEYCLOAK_REALM || DEFAULT_AUTH_CONFIG.realm,
		clientId: pub.PUBLIC_KEYCLOAK_CLIENT_ID || DEFAULT_AUTH_CONFIG.clientId,
	};
}
