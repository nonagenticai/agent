import { defineEnvVars } from '@sveltejs/kit/env';

// Public sign-in config, read at runtime (never baked into the build; strata_configurator precedent). Everyone signs in
// through Keycloak. Unset values fall back to the defaults in $lib/auth/config.ts, so each variable is optional.
export const variables = defineEnvVars({
	PUBLIC_KEYCLOAK_URL: { public: true, schema: (value) => value ?? '' },
	PUBLIC_KEYCLOAK_REALM: { public: true, schema: (value) => value ?? '' },
	PUBLIC_KEYCLOAK_CLIENT_ID: { public: true, schema: (value) => value ?? '' },
});
