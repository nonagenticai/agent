import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vitest/config';

export default defineConfig({
	plugins: [sveltekit()],
	// Local live runs only (scripts/local/b8-acceptance.sh): the app calls same-origin /api, which production's
	// ingress routes to agent-api. Unset everywhere else, so builds and the CI e2e servers are unchanged.
	server: process.env.AGENT_API_PROXY
		? { proxy: { '/api': { target: process.env.AGENT_API_PROXY, changeOrigin: false } } }
		: undefined,
	test: {
		environment: 'jsdom',
		include: ['tests/unit/**/*.test.ts'],
		setupFiles: [],
	},
	resolve: process.env.VITEST ? { conditions: ['browser'] } : undefined,
});
