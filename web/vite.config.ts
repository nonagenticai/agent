import adapter from '@sveltejs/adapter-node';
import { sveltekit } from '@sveltejs/kit/vite';
import { vitePreprocess } from '@sveltejs/vite-plugin-svelte';
import { defineConfig } from 'vitest/config';

export default defineConfig({
	plugins: [
		// SvelteKit configuration lives here since kit 3 — `svelte.config.js` is no
		// longer supported (https://svelte.dev/e/kit/config_file_unsupported).
		sveltekit({
			// Kit 3 removed the built-in `$lib` alias (https://svelte.dev/e/kit/module_removed_lib);
			// this is kit's own documented compatibility path, avoiding a repo-wide rewrite
			// to `#lib` subpath imports.
			alias: { $lib: 'src/lib' },
			preprocess: vitePreprocess(),
			adapter: adapter({ out: 'build' }),
		}),
	],
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
