// Sign-in config, read from the environment at request time (never baked into the build; strata_configurator
// precedent). Everyone signs in through Keycloak; there is no fixture login.
import { env as pub } from '$env/dynamic/public';
import { authConfigFrom, type AuthConfig } from '$lib/auth/config';

export function load(): { config: AuthConfig } {
	return { config: authConfigFrom(pub) };
}
