import { defineConfig, devices } from '@playwright/test';

// Tenant-admin channel journey on the REAL local stack (docs/runbooks/acceptance-tenant-admin-channels.md).
// No webServer: scripts/local/web-e2e.sh boots the platform-stack instance, builds the journey and sets PS_E2E_*; no
// mocks in the browser. One worker, tests share one database and run in order.
export default defineConfig({
	testDir: 'tests/acceptance',
	testMatch: /tenant-admin-channels\.spec\.ts/,
	globalSetup: './tests/e2e/global-setup.ts',
	outputDir: 'test-results/acceptance',
	fullyParallel: false,
	workers: 1,
	retries: 0,
	timeout: 240_000,
	expect: { timeout: 20_000 },
	reporter: [['list'], ['html', { open: 'never', outputFolder: 'test-results/acceptance-report' }]],
	use: {
		baseURL: process.env.PS_E2E_WEB ?? 'http://127.0.0.1:3300',
		trace: 'retain-on-failure',
		screenshot: 'only-on-failure',
	},
	projects: [
		{
			name: 'tenant-admin-channels',
			use: { ...devices['Desktop Chrome'], viewport: { width: 1440, height: 900 } },
		},
	],
});
