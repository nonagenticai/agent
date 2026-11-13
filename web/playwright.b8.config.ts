import { defineConfig, devices } from '@playwright/test';

// B8 T17 local acceptance (A1-A15). Needs the live stack: scripts/local/b8-acceptance.sh starts it and sets the
// B8_* variables. No webServer here: the web dev server, agent-api, the fake IdP and the fake LLM are started by
// that script. Tests share one database and run in order, one worker.
export default defineConfig({
	testDir: 'tests/e2e',
	testMatch: /b8-acceptance\.spec\.ts/,
	outputDir: 'test-results/b8-acceptance',
	fullyParallel: false,
	workers: 1,
	retries: 0,
	timeout: 180_000,
	expect: { timeout: 15_000 },
	reporter: [['list'], ['html', { open: 'never', outputFolder: 'test-results/b8-acceptance-report' }]],
	use: {
		baseURL: process.env.B8_WEB_URL ?? 'http://127.0.0.1:3000',
		trace: 'retain-on-failure',
		screenshot: 'only-on-failure',
	},
	projects: [
		{
			name: 'b8-acceptance',
			use: { ...devices['Desktop Chrome'], viewport: { width: 1440, height: 900 } },
		},
	],
});
