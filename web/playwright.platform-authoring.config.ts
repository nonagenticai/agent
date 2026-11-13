// Bootstrap acceptance starts with an empty isolated platform, before the tenant smoke creates its journey.
// Start scripts/local/platform-stack.sh on isolated ports, then set PS_E2E_WEB and PS_E2E_IDP for this config.
import { defineConfig, devices } from '@playwright/test';

export default defineConfig({
	testDir: 'tests/e2e',
	testMatch: 'platform-creation.spec.ts',
	outputDir: 'test-results/platform-authoring',
	workers: 1,
	timeout: 120_000,
	expect: { timeout: 15_000 },
	use: { baseURL: process.env.PS_E2E_WEB, trace: 'retain-on-failure' },
	projects: [
		{ name: 'desktop', use: { ...devices['Desktop Chrome'], viewport: { width: 1440, height: 1000 } } },
	],
});
