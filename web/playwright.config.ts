import { defineConfig, devices } from '@playwright/test';

// The e2e specs run on the REAL local platform stack, never on a fixture app (the in-memory fixture world was removed
// with the seed, D131). scripts/local/web-e2e.sh boots its own platform-stack instance (shifted ports, own databases),
// runs the platform-stack smoke so every product object exists through the product's own routes, and runs Playwright
// under the machine's browser lease with PS_E2E_WEB / PS_E2E_IDP / PS_E2E_EDGE / PS_E2E_JOURNEY set. Port 8000 is
// never used. The B8 local acceptance run has its own stack and config (playwright.b8.config.ts).
const WEB = process.env.PS_E2E_WEB ?? 'http://127.0.0.1:3300';

export default defineConfig({
	testDir: 'tests/e2e',
	// b8-acceptance and platform-creation (an EMPTY platform) run under their own configs
	testIgnore: [/b8-acceptance\.spec\.ts/, /platform-creation\.spec\.ts/],
	outputDir: 'test-results/playwright',
	globalSetup: './tests/e2e/global-setup.ts',
	// one stack, one journey: specs that change it (a Take, a reply) must not race each other
	workers: 1,
	timeout: 90_000,
	expect: { timeout: 15_000 },
	use: { baseURL: WEB, trace: 'retain-on-failure' },
	projects: [
		{ name: 'desktop', use: { ...devices['Desktop Chrome'], viewport: { width: 1440, height: 900 } } },
		{ name: 'phone', use: { ...devices['Pixel 7'], viewport: { width: 390, height: 844 } } },
	],
});
