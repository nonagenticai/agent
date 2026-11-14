// Sign-in config, read from the environment at request time (never baked into the build; strata_configurator
// precedent). Everyone signs in through Keycloak; there is no fixture login.
import { PUBLIC_KEYCLOAK_URL, PUBLIC_KEYCLOAK_REALM, PUBLIC_KEYCLOAK_CLIENT_ID } from '$app/env/public';
import { authConfigFrom, type AuthConfig } from '$lib/auth/config';

export function load(): { config: AuthConfig } {
	return {
		config: authConfigFrom({
			PUBLIC_KEYCLOAK_URL,
			PUBLIC_KEYCLOAK_REALM,
			PUBLIC_KEYCLOAK_CLIENT_ID,
		}),
	};
}
